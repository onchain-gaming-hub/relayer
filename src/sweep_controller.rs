//! `SweepController` — one DO per project (`id_from_name(aud)`), spec v3 W13 / §5. Closes
//! accounts whose rent the project's fee payer fronted, so the rent comes back.
//!
//! One run at a time (synchronous in-flight flag, as in `UserController`); a cursor per sweep in
//! storage so the head of the list cannot starve the tail. Every RPC call takes one project +
//! cluster RPM slot first (§5 step 6); each send takes the fee budget. A simulation that fails
//! (not closable yet), breaks §4.3, or does not bring rent back is never sent.

use std::cell::Cell;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use wasm_bindgen::JsValue;
use worker::*;

use crate::{
    config::{ADMIT_MARGIN_MS, Cluster, Config, REFUND_TIMEOUT_MS, rpc_url},
    error::ApiError,
    inner_ix::{self, Expect, Verdict},
    rpc::{self, SendOutcome},
    sweep::{
        SWEEP_FEE_LAMPORTS, SweepJob, SweepSpec, build_tx, order_from_cursor, rent_came_back,
        rent_returned,
    },
};

/// A run older than this is presumed dead (the scheduled event's own wall limit is 15 min).
pub const SWEEP_STALE_MS: f64 = 15.0 * 60_000.0;
/// A blockhash is valid for ~60–90 s; refresh well before (each refresh takes an RPM slot).
const BLOCKHASH_MAX_AGE_MS: f64 = 30_000.0;
/// RPC failures (transport, missing fields, unknown send outcomes) before the run stops.
const MAX_RPC_ERRORS: u32 = 3;

#[durable_object]
pub struct SweepController {
    state: State,
    env: Env,
    inflight_since: Cell<Option<f64>>,
    /// `getMinimumBalanceForRentExemption(0)` per cluster (never a hardcoded rate).
    min_rent0: Cell<Option<(Cluster, u64)>>,
}

struct InflightGuard<'a> {
    flag: &'a Cell<Option<f64>>,
    owner: f64,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        if self.flag.get() == Some(self.owner) {
            self.flag.set(None);
        }
    }
}

/// Per-run state shared by every sweep of the project.
struct Run {
    url: String,
    cluster: Cluster,
    fee_payer: solana_keypair::Keypair,
    registered: Vec<Pubkey>,
    /// (blockhash, fetched_at): refreshed when older than BLOCKHASH_MAX_AGE_MS.
    blockhash: Option<(Hash, f64)>,
    rpc_errors: u32,
    /// Why the run stopped early (budget, throttle, RPC errors); `None` = ran to its limits.
    stopped: Option<String>,
}

impl DurableObject for SweepController {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            inflight_since: Cell::new(None),
            min_rent0: Cell::new(None),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let now = Date::now().as_millis() as f64;
        if req.path() != "/run" {
            return ApiError::not_found().into_response();
        }
        // --- synchronous check-and-set: no await above this line ---
        if let Some(since) = self.inflight_since.get()
            && now - since < SWEEP_STALE_MS
        {
            return Response::from_json(&json!({ "skipped": "previous run still in progress" }));
        }
        self.inflight_since.set(Some(now));
        let _guard = InflightGuard {
            flag: &self.inflight_since,
            owner: now,
        };
        let job: SweepJob = match req.json().await {
            Ok(j) => j,
            Err(e) => return ApiError::internal(format!("sweep job: {e}")).into_response(),
        };
        match self.run(&job).await {
            Ok(v) => Response::from_json(&v),
            Err(e) => e.into_response(),
        }
    }
}

impl SweepController {
    async fn run(&self, job: &SweepJob) -> std::result::Result<Value, ApiError> {
        let cfg = Config::from_env(&self.env).map_err(ApiError::config)?;
        let cluster =
            Cluster::parse(&job.cluster).ok_or_else(|| ApiError::internal("bad cluster"))?;
        let expected: Pubkey = job
            .fee_payer
            .parse()
            .map_err(|_| ApiError::internal("bad fee payer"))?;
        let mut run = Run {
            url: rpc_url(&self.env, cluster).map_err(ApiError::config)?,
            cluster,
            fee_payer: crate::fee_payer_key(&self.env, &job.slug, &job.aud, &expected)?,
            registered: job
                .registered
                .iter()
                .filter_map(|k| k.parse().ok())
                .collect(),
            blockhash: None,
            rpc_errors: 0,
            stopped: None,
        };
        let mut reports = Vec::new();
        for spec in &job.sweeps {
            if run.stopped.is_some() {
                break;
            }
            reports.push(self.sweep(job, spec, &cfg, &mut run).await);
        }
        Ok(json!({ "sweeps": reports, "stopped": run.stopped }))
    }

    /// One sweep template: list → resume after the cursor → simulate / verify / send.
    async fn sweep(&self, job: &SweepJob, spec: &SweepSpec, cfg: &Config, run: &mut Run) -> Value {
        let (mut simulated, mut sent, mut skipped, mut rejected) = (0u32, 0u32, 0u32, 0u32);
        let report = |found: usize, s: u32, n: u32, k: u32, r: u32| {
            json!({ "name": spec.name, "program_id": spec.program_id, "found": found,
                    "simulated": s, "sent": n, "skipped": k, "rejected": r })
        };
        if let Err(e) = self.rpm(job).await {
            run.stopped = Some(e);
            return report(0, 0, 0, 0, 0);
        }
        let filters = [
            (0, spec.account_discriminator.clone()),
            (spec.rent_payer_offset, job.fee_payer.clone()),
        ];
        let mut addrs = match rpc::get_program_accounts(
            &run.url,
            cfg.rpc_timeout_ms,
            &spec.program_id,
            &filters,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                self.rpc_error(run, &e);
                return report(0, 0, 0, 0, 0);
            }
        };
        let storage = self.state.storage();
        let key = spec.cursor_key();
        let cursor: Option<String> = storage.get(&key).await.ok().flatten();
        let start = order_from_cursor(&mut addrs, cursor.as_deref());
        let mut reached_end = true;
        'addrs: for addr in &addrs[start..] {
            let Ok(swept) = addr.parse::<Pubkey>() else {
                skipped += 1;
                continue;
            };
            // Simulate, retrying once with a fresh blockhash if the cached one expired (a
            // long run outlives it; `BlockhashNotFound` says nothing about the account).
            let mut retried = false;
            let (tx, b64, sim) = loop {
                if simulated >= spec.max_simulations_per_run || sent >= spec.max_sends_per_run {
                    reached_end = false;
                    break 'addrs;
                }
                let Ok((blockhash, _)) = self.prepare(job, cfg, run).await else {
                    reached_end = false;
                    break 'addrs;
                };
                let tx = match build_tx(spec, &swept, &run.fee_payer, &blockhash) {
                    Ok(t) => t,
                    Err(e) => {
                        run.stopped = Some(format!("sweep {} template: {e}", spec.name));
                        reached_end = false;
                        break 'addrs;
                    }
                };
                let Ok(wire) = crate::txcore::encode(&tx) else {
                    run.stopped = Some("encode".into());
                    reached_end = false;
                    break 'addrs;
                };
                let b64 = B64.encode(wire);
                let static_keys = tx.message.static_account_keys().to_vec();
                let requested = crate::writable_unsigned_keys(&tx);
                let requested_s: Vec<String> = requested.iter().map(Pubkey::to_string).collect();
                if let Err(e) = self.rpm(job).await {
                    run.stopped = Some(e);
                    reached_end = false;
                    break 'addrs;
                }
                let sim_result =
                    rpc::simulate(&run.url, cfg.rpc_timeout_ms, &b64, &requested_s).await;
                simulated += 1;
                let value = match sim_result {
                    Ok(v) => v,
                    Err(e) => {
                        // Advance anyway: an address that always fails must not pin the cursor
                        // (it is listed again when the cursor wraps). 3 errors stop the run.
                        self.rpc_error(run, &e);
                        skipped += 1;
                        self.advance(&key, addr).await;
                        if run.stopped.is_some() {
                            reached_end = false;
                            break 'addrs;
                        }
                        continue 'addrs;
                    }
                };
                if value.get("err").and_then(Value::as_str) == Some("BlockhashNotFound") {
                    run.blockhash = None;
                    if !retried {
                        retried = true;
                        continue;
                    }
                    self.rpc_error(run, "BlockhashNotFound twice");
                    skipped += 1;
                    self.advance(&key, addr).await;
                    if run.stopped.is_some() {
                        reached_end = false;
                        break 'addrs;
                    }
                    continue 'addrs;
                }
                break (
                    tx,
                    b64,
                    inner_ix::parse_simulation(&value, &static_keys, &requested),
                );
            };
            let min_rent0 = self.min_rent0.get().map(|(_, v)| v).unwrap_or(u64::MAX);
            let payer = run.fee_payer_pubkey();
            let verdict = inner_ix::verify(
                &sim,
                Expect {
                    fee_payer: &payer,
                    user: &payer,
                    registered: &run.registered,
                    fee_cost: SWEEP_FEE_LAMPORTS,
                    declared_rent: 0,
                    refund_only: true,
                },
            );
            let signature = tx.signatures[0].to_string();
            match verdict {
                Verdict::Ok { .. } => {}
                // Not closable yet (e.g. RetentionNotReached): skip, no fee spent.
                Verdict::ProgramError(_) => {
                    skipped += 1;
                    self.advance(&key, addr).await;
                    continue;
                }
                Verdict::FeePayerUnfunded => {
                    run.stopped = Some("low_balance".into());
                    reached_end = false;
                    break;
                }
                Verdict::Incomplete(field) => {
                    self.rpc_error(run, &format!("simulation missing {field}"));
                    skipped += 1;
                    self.advance(&key, addr).await;
                    if run.stopped.is_some() {
                        reached_end = false;
                        break;
                    }
                    continue;
                }
                Verdict::Rejected(reason) => {
                    rejected += 1;
                    console_error!("sweep_rejected {} {addr}: {reason}", job.aud);
                    self.audit(
                        job,
                        addr,
                        &signature,
                        "rpc_error",
                        format!("sweep_rejected: {reason}"),
                        0,
                        None,
                    )
                    .await;
                    self.advance(&key, addr).await;
                    continue;
                }
            }
            // Ok guarantees both balances. The fee the cluster reported, if any.
            let (pre, post) = (
                sim.pre_fee_payer.unwrap_or(0),
                sim.post_fee_payer.unwrap_or(0),
            );
            let fee = sim.fee.unwrap_or(SWEEP_FEE_LAMPORTS);
            if !rent_came_back(pre, post, fee, min_rent0) {
                rejected += 1;
                let reason = format!("rent did not come back (pre {pre}, post {post})");
                console_error!("sweep_rejected {} {addr}: {reason}", job.aud);
                self.audit(
                    job,
                    addr,
                    &signature,
                    "rpc_error",
                    format!("sweep_rejected: {reason}"),
                    0,
                    None,
                )
                .await;
                self.advance(&key, addr).await;
                continue;
            }

            if let Err(e) = self.rpm(job).await {
                run.stopped = Some(e);
                reached_end = false;
                break;
            }
            let (day, cluster_day) = match self.budget(job, cfg).await {
                Ok(d) => d,
                Err(e) => {
                    run.stopped = Some(e);
                    reached_end = false;
                    break;
                }
            };
            sent += 1;
            let rent = rent_returned(pre, post, fee);
            match rpc::send_transaction(&run.url, cfg.rpc_timeout_ms, &b64, false).await {
                SendOutcome::Accepted(sig) => {
                    self.audit(
                        job,
                        addr,
                        &sig,
                        "sweep",
                        spec.name.clone(),
                        SWEEP_FEE_LAMPORTS,
                        Some(rent),
                    )
                    .await;
                }
                // Landed earlier (an earlier run's send): nothing new was spent or returned now.
                SendOutcome::AlreadyProcessed => {
                    self.refund(job, day, cluster_day).await;
                    self.audit(
                        job,
                        addr,
                        &signature,
                        "rpc_error",
                        "sweep already processed".into(),
                        0,
                        None,
                    )
                    .await;
                }
                SendOutcome::Rejected {
                    reason, tx_fault, ..
                } => {
                    self.refund(job, day, cluster_day).await;
                    self.audit(
                        job,
                        addr,
                        &signature,
                        "rpc_error",
                        format!("sweep rejected: {reason}"),
                        0,
                        None,
                    )
                    .await;
                    if !tx_fault {
                        self.rpc_error(run, &reason);
                    }
                }
                SendOutcome::Unknown(reason) => {
                    // May have landed: keep the budget (fail-closed) and count it as spent.
                    self.audit(
                        job,
                        addr,
                        &signature,
                        "rpc_error",
                        format!("sweep unknown: {reason}"),
                        SWEEP_FEE_LAMPORTS,
                        None,
                    )
                    .await;
                    self.rpc_error(run, &reason);
                }
            }
            self.advance(&key, addr).await;
            if run.stopped.is_some() {
                reached_end = false;
                break;
            }
        }
        if reached_end {
            // Whole list examined: the next run starts from the beginning again.
            if let Err(e) = storage.delete(&key).await {
                console_error!("sweep cursor reset failed: {e}");
            }
        }
        report(addrs.len(), simulated, sent, skipped, rejected)
    }

    /// Blockhash (once per run, refreshed after BLOCKHASH_MAX_AGE_MS) and the zero-byte rent minimum (cached per cluster), each RPC
    /// call behind an RPM slot. `Err` = the run stopped (reason in `run.stopped`) or should
    /// retry this address next run.
    async fn prepare(
        &self,
        job: &SweepJob,
        cfg: &Config,
        run: &mut Run,
    ) -> std::result::Result<(Hash, u64), ()> {
        let min_rent0 = match self.min_rent0.get() {
            Some((c, v)) if c == run.cluster => v,
            _ => {
                if let Err(e) = self.rpm(job).await {
                    run.stopped = Some(e);
                    return Err(());
                }
                match rpc::get_minimum_balance_for_rent_exemption(&run.url, cfg.rpc_timeout_ms, 0)
                    .await
                {
                    Ok(v) => {
                        self.min_rent0.set(Some((run.cluster, v)));
                        v
                    }
                    Err(e) => {
                        self.rpc_error(run, &e);
                        return Err(());
                    }
                }
            }
        };
        let now = Date::now().as_millis() as f64;
        let blockhash = match &run.blockhash {
            Some((h, at)) if now - at < BLOCKHASH_MAX_AGE_MS => h.clone(),
            _ => {
                if let Err(e) = self.rpm(job).await {
                    run.stopped = Some(e);
                    return Err(());
                }
                let parsed = rpc::get_latest_blockhash(&run.url, cfg.rpc_timeout_ms)
                    .await
                    .and_then(|s| s.parse::<Hash>().map_err(|_| "bad blockhash".into()));
                match parsed {
                    Ok(h) => {
                        run.blockhash = Some((h.clone(), now));
                        h
                    }
                    Err(e) => {
                        self.rpc_error(run, &e);
                        return Err(());
                    }
                }
            }
        };
        Ok((blockhash, min_rent0))
    }

    fn rpc_error(&self, run: &mut Run, e: &str) {
        console_error!("sweep rpc error: {e}");
        run.rpc_errors += 1;
        if run.rpc_errors >= MAX_RPC_ERRORS {
            run.stopped = Some(format!("{MAX_RPC_ERRORS} RPC errors"));
        }
    }

    async fn advance(&self, key: &str, addr: &str) {
        if let Err(e) = self.state.storage().put(key, addr).await {
            console_error!("sweep cursor put failed: {e}");
        }
    }

    /// One project + cluster RPM slot per RPC call (§5 step 6). `Err` = stop the run.
    async fn rpm(&self, job: &SweepJob) -> std::result::Result<(), String> {
        let body = json!({ "cluster": job.cluster, "rpm": job.project_rpm });
        match rpc::with_timeout(
            self.project_call(&job.aud, "admit_rpm", body),
            ADMIT_MARGIN_MS,
        )
        .await
        {
            Some(Ok((200, _))) => Ok(()),
            Some(Ok((_, b))) => Err(format!(
                "rpm: {}",
                b.get("code").and_then(Value::as_str).unwrap_or("refused")
            )),
            Some(Err(e)) => Err(format!("rpm: {e}")),
            None => Err("rpm: timeout".into()),
        }
    }

    /// The fee of one sweep tx against the project's fee budget (no rent) + balance breaker.
    async fn budget(
        &self,
        job: &SweepJob,
        cfg: &Config,
    ) -> std::result::Result<(i64, i64), String> {
        let body = json!({
            "cluster": job.cluster,
            "cost": SWEEP_FEE_LAMPORTS,
            "rent": 0,
            "budget": job.project_daily_budget,
            "rent_budget": 0,
            "fee_payer": job.fee_payer,
            "min_balance": job.min_balance,
        });
        match rpc::with_timeout(
            self.project_call(&job.aud, "admit_budget", body),
            cfg.rpc_timeout_ms + ADMIT_MARGIN_MS,
        )
        .await
        {
            Some(Ok((200, b))) => match (
                b.get("day").and_then(Value::as_i64),
                b.get("cluster_day").and_then(Value::as_i64),
            ) {
                (Some(d), Some(c)) => Ok((d, c)),
                _ => Err("budget: missing day".into()),
            },
            Some(Ok((_, b))) => Err(format!(
                "budget: {} {}",
                b.get("code").and_then(Value::as_str).unwrap_or("refused"),
                b.get("reason").and_then(Value::as_str).unwrap_or("")
            )
            .trim_end()
            .to_string()),
            Some(Err(e)) => Err(format!("budget: {e}")),
            None => Err("budget: timeout".into()),
        }
    }

    /// A rejected send cost nothing: the fee goes back to both budgets (RPM slots stay).
    async fn refund(&self, job: &SweepJob, day: i64, cluster_day: i64) {
        let body = json!({
            "cluster": job.cluster,
            "cost": SWEEP_FEE_LAMPORTS,
            "rent": 0,
            "keep_rpm": true,
            "window": 0.0,
            "day": day,
            "cluster_window": 0.0,
            "cluster_day": cluster_day,
        });
        match rpc::with_timeout(
            self.project_call(&job.aud, "refund", body),
            REFUND_TIMEOUT_MS,
        )
        .await
        {
            Some(Ok((200, _))) => {}
            other => console_error!("sweep refund failed: {:?}", other.map(|r| r.map(|x| x.0))),
        }
    }

    async fn project_call(
        &self,
        aud: &str,
        action: &str,
        body: Value,
    ) -> std::result::Result<(u16, Value), worker::Error> {
        let stub = self
            .env
            .durable_object("PROJECT_THROTTLE")?
            .id_from_name(aud)?
            .get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(body.to_string().into()));
        let req = Request::new_with_init(&format!("https://project/{action}"), &init)?;
        let mut resp = stub.fetch_with_request(req).await?;
        let status = resp.status_code();
        let json = resp.json::<Value>().await.unwrap_or(Value::Null);
        Ok((status, json))
    }

    /// `relay_log` row: wallet = the swept account, status `sweep` (sent) or `rpc_error`.
    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        job: &SweepJob,
        swept: &str,
        signature: &str,
        status: &str,
        reason: String,
        cost: u64,
        rent: Option<u64>,
    ) {
        let db = match self.env.d1("DB") {
            Ok(db) => db,
            Err(e) => {
                console_error!("sweep audit: no D1 binding: {e}");
                return;
            }
        };
        let stmt = db.prepare(
            "INSERT INTO relay_log (wallet_address, tx_signature, status, reason, created_at, aud, cluster, cost_lamports, rent_lamports) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        );
        let at = Date::now().as_millis() as f64;
        let result = match stmt.bind(&[
            swept.into(),
            signature.into(),
            status.into(),
            reason.into(),
            JsValue::from_f64(at),
            job.aud.as_str().into(),
            job.cluster.as_str().into(),
            JsValue::from_f64(cost as f64),
            rent.map(|r| JsValue::from_f64(r as f64))
                .unwrap_or(JsValue::NULL),
        ]) {
            Ok(bound) => bound.run().await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            console_error!("sweep audit insert failed: {e}");
        }
    }
}

impl Run {
    fn fee_payer_pubkey(&self) -> Pubkey {
        use solana_signer::Signer;
        self.fee_payer.pubkey()
    }
}
