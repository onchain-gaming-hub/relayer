//! `UserController` — one DO per user per project (`id_from_name("<aud>:<wallet>")`,
//! spec v2 V2, §4.4). In-flight lock, cooldown and reject backoff as in v1, plus the user's
//! daily tx quota (SQLite, `daily.rs`) checked before any other DO is asked.
//!
//! Ordering matters: DO input gates only close during storage operations, so any `await`
//! on a fetch (ProjectThrottle, RPC) lets another request for the same wallet in. The
//! in-flight check-and-set below therefore runs synchronously before the first `await`.

use std::cell::Cell;

use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::*;

use crate::{
    config::{ADMIT_MARGIN_MS, Cluster, Config, REFUND_TIMEOUT_MS, reject_backoff_ms, rpc_url},
    daily,
    error::ApiError,
    inner_ix::{self, Expect, Verdict},
    rpc::{self, SendOutcome},
};

const COOLDOWN_KEY: &str = "cooldown_until";
const REJECT_STREAK_KEY: &str = "reject_streak";

/// Written atomically with `put_multiple`; field names are the storage keys.
#[derive(serde::Serialize)]
struct CooldownState {
    cooldown_until: f64,
    reject_streak: u32,
}

#[derive(Deserialize)]
pub struct RelayJob {
    /// Fully signed wire tx, base64.
    pub tx: String,
    pub user: String,
    pub signature: String,
    pub aud: String,
    pub cluster: String,
    /// Lamports the relayer pays for this tx (spec v2 V7).
    pub cost: u64,
    pub project_rpm: u32,
    pub project_daily_budget: u64,
    pub fee_payer: String,
    pub min_balance: u64,
    pub user_daily_quota: u32,
    /// Present for program actions (spec v3): two-step admission + simulation (W14, §4.3).
    #[serde(default)]
    pub program: Option<ProgramJob>,
}

/// What the program path needs beyond a memo job (spec v3).
#[derive(Deserialize)]
pub struct ProgramJob {
    pub fee_cost: u64,
    pub declared_rent: u64,
    pub needs_simulation: bool,
    /// No payer slot: the fee payer may not lose more than the fee (W4b).
    pub refund_only: bool,
    /// Programs registered by the project (allowed owners of rent-funded accounts).
    pub registered: Vec<String>,
    /// Static account keys of the tx (Compiled inner-ix shape) and the writable non-signer
    /// keys whose post-state the simulation returns.
    pub static_keys: Vec<String>,
    pub requested: Vec<String>,
}

#[derive(Deserialize)]
struct QuotaQuery {
    limit: u32,
}

/// Where this request was counted, so a tx-fault reject can give the slots back (D17).
struct Admitted {
    window: f64,
    day: i64,
    cluster_window: f64,
    cluster_day: i64,
}

#[durable_object]
pub struct UserController {
    state: State,
    env: Env,
    inflight_since: Cell<Option<f64>>,
}

/// Clears the in-flight flag on every exit path — but only if it still holds *this*
/// request's stamp. A stale lock taken over by a newer request must survive the old guard.
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

impl DurableObject for UserController {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            inflight_since: Cell::new(None),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let now = Date::now().as_millis() as f64;
        let cfg = match Config::from_env(&self.env) {
            Ok(c) => c,
            Err(e) => return ApiError::config(e).into_response(),
        };
        match req.path().as_str() {
            // Read-only: no lock (GET /quota must work while a relay is in flight).
            "/quota" => {
                return match req.json::<QuotaQuery>().await {
                    Ok(q) => match self.quota(q.limit, now) {
                        Ok(v) => Response::from_json(&v),
                        Err(e) => e.into_response(),
                    },
                    Err(e) => ApiError::internal(format!("quota body: {e}")).into_response(),
                };
            }
            "/relay" => {}
            _ => return ApiError::not_found().into_response(),
        }

        // --- synchronous check-and-set: no await above this line ---
        if let Some(since) = self.inflight_since.get()
            && now - since < cfg.inflight_stale_ms()
        {
            return ApiError::in_progress().into_response();
        }
        self.inflight_since.set(Some(now));
        let _guard = InflightGuard {
            flag: &self.inflight_since,
            owner: now,
        };

        match self.relay(req, &cfg, now).await {
            Ok(resp) => Ok(resp),
            Err(e) => e.into_response(),
        }
    }
}

impl UserController {
    /// `{ used, limit, resets_at }` for today (UTC).
    fn quota(&self, limit: u32, now: f64) -> std::result::Result<serde_json::Value, ApiError> {
        let used = daily::used(&self.state.storage().sql(), daily::day_of(now))
            .map_err(|e| ApiError::internal(format!("quota read: {e}")))?;
        Ok(json!({ "used": used, "limit": limit, "resets_at": daily::next_day_start(now) as u64 }))
    }

    fn refund_user(&self, day: i64) {
        if let Err(e) = daily::refund(&self.state.storage().sql(), day, 1) {
            console_error!("user quota refund failed: {e}");
        }
    }

    async fn relay(
        &self,
        mut req: Request,
        cfg: &Config,
        now: f64,
    ) -> std::result::Result<Response, ApiError> {
        let job: RelayJob = req
            .json()
            .await
            .map_err(|e| ApiError::internal(format!("job: {e}")))?;
        let storage = self.state.storage();
        let cluster =
            Cluster::parse(&job.cluster).ok_or_else(|| ApiError::internal("job: bad cluster"))?;
        // Before admit: a missing RPC URL must not consume any slot.
        let url = rpc_url(&self.env, cluster).map_err(ApiError::config)?;

        let cooldown_until: f64 = storage
            .get(COOLDOWN_KEY)
            .await
            .map_err(|e| ApiError::internal(format!("storage get: {e}")))?
            .unwrap_or(0.0);
        if now < cooldown_until {
            return Err(ApiError::cooldown(cooldown_until - now));
        }

        // User daily quota (V6/V8): counted before asking any other DO, so a user at quota
        // never consumes project or cluster capacity.
        let day = daily::day_of(now);
        match daily::try_add(&storage.sql(), day, 1, job.user_daily_quota as i64) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(ApiError::user_quota(daily::ms_until_next_day(now))),
            Err(e) => return Err(ApiError::internal(format!("user quota: {e}"))),
        }

        let mut rent_simulated = None;
        let admitted = if let Some(p) = &job.program {
            // Program action (v3 W14): RPM → simulate + verify → budget. Refunds the user's
            // count itself on every refusal.
            let (a, rent) = self.admit_program(&job, p, &url, cfg, day).await?;
            rent_simulated = rent;
            a
        } else {
            // Bounded: ProjectThrottle may wait one RPC timeout on getBalance plus the cluster hop.
            match rpc::with_timeout(self.admit(&job), cfg.rpc_timeout_ms + ADMIT_MARGIN_MS).await {
                Some(Ok(admitted)) => admitted,
                // Refused or unreachable: only our own count is known to be safe to return
                // (an unreachable ProjectThrottle keeps whatever it counted: fail-closed).
                Some(Err(e)) => {
                    self.refund_user(day);
                    return Err(e);
                }
                None => {
                    // The throttles may still count this request (slots held until the window
                    // ends, R9 / R-v2-5), so the user's count is kept too (fail-closed). A short
                    // cooldown stops retries from leaking more slots.
                    let until = Date::now().as_millis() as f64 + cfg.reject_cooldown_ms;
                    if let Err(e) = storage.put(COOLDOWN_KEY, until).await {
                        console_error!(
                            "cooldown put after admit timeout failed for {}: {e}",
                            job.user
                        );
                    }
                    return Err(ApiError::upstream("project admit timeout"));
                }
            }
        };

        let outcome = rpc::send_transaction(&url, cfg.rpc_timeout_ms, &job.tx).await;
        let done = Date::now().as_millis() as f64;

        // D17: only a rejection caused by the transaction itself (JSON-RPC error except node-side
        // codes -32603/-32005/-32004, HTTP 400/413)
        // returns the global slot and grows this wallet's backoff. An overloaded RPC (429, other
        // 4xx) is not the user's fault and must not free capacity to hit it again.
        let tx_fault = matches!(outcome, SendOutcome::Rejected { tx_fault: true, .. });
        let streak = if tx_fault {
            self.next_reject_streak(&job.user).await
        } else {
            None
        };
        let (cooldown_ms, audit) = match &outcome {
            SendOutcome::Accepted(_) => (cfg.cooldown_ms, ("success", None)),
            // Replays of a landed tx: keep them distinguishable from real sends in the audit log.
            SendOutcome::AlreadyProcessed => (
                cfg.cooldown_ms,
                ("success", Some("already processed".to_string())),
            ),
            SendOutcome::Rejected {
                reason, http_429, ..
            } => {
                if *http_429 {
                    console_warn!("rpc http 429 (R6) for {}", job.user);
                }
                (
                    reject_backoff_ms(
                        cfg.reject_cooldown_ms,
                        streak.unwrap_or(1),
                        cfg.reject_cooldown_max_ms,
                    ),
                    ("rpc_error", Some(format!("rejected: {reason}"))),
                )
            }
            SendOutcome::Unknown(reason) => (
                cfg.cooldown_ms,
                ("rpc_error", Some(format!("unknown: {reason}"))),
            ),
        };

        // Persist the cooldown (and streak) first: if the DO dies before the refund below,
        // we lose a global slot (fail-closed) rather than the wallet's cooldown.
        let stored = match (&outcome, streak) {
            (_, Some(reject_streak)) => {
                storage
                    .put_multiple(CooldownState {
                        cooldown_until: done + cooldown_ms,
                        reject_streak,
                    })
                    .await
            }
            // Only a real acceptance clears the streak (AlreadyProcessed can be replayed);
            // one atomic write so a crash cannot leave the old streak behind.
            (SendOutcome::Accepted(_), _) => {
                storage
                    .put_multiple(CooldownState {
                        cooldown_until: done + cooldown_ms,
                        reject_streak: 0,
                    })
                    .await
            }
            _ => storage.put(COOLDOWN_KEY, done + cooldown_ms).await,
        };
        if let Err(e) = stored {
            // The tx may already be on its way; never turn that into an error response.
            console_error!("cooldown put failed for {}: {e}", job.user);
        }
        if tx_fault {
            // Nothing was sent: the user's count, the project and the cluster budgets go back.
            self.refund_user(day);
            self.refund(&job, &admitted).await;
        }
        self.audit(&job, audit.0, audit.1, done, rent_simulated);
        // `done`, not `now`: a request that crosses UTC midnight reports the new day.
        let quota = self.quota(job.user_daily_quota, done).ok();

        match outcome {
            SendOutcome::Accepted(sig) => {
                Response::from_json(&json!({ "signature": sig, "quota": quota }))
                    .map_err(|e| ApiError::internal(e.to_string()))
            }
            // The same signed tx already landed: report it as the success it is.
            SendOutcome::AlreadyProcessed => {
                Response::from_json(&json!({ "signature": job.signature, "quota": quota }))
                    .map_err(|e| ApiError::internal(e.to_string()))
            }
            SendOutcome::Rejected { reason, .. } => {
                Err(ApiError::rpc_rejected(reason, cooldown_ms))
            }
            SendOutcome::Unknown(reason) => Err(ApiError::rpc_unknown(reason, job.signature)),
        }
    }

    /// Program-action admission (spec v3 W14): RPM first (every RPC call is counted), then
    /// simulation + inner-ix verification (§4.3), then the budget (fee + declared rent; rent
    /// shares the fee budget until Phase 3 splits it). Returns where it was counted and the
    /// rent the simulation saw. On every refusal the user's daily count is given back; the RPM
    /// slot is kept (1 call = 1 slot).
    async fn admit_program(
        &self,
        job: &RelayJob,
        p: &ProgramJob,
        url: &str,
        cfg: &Config,
        day: i64,
    ) -> std::result::Result<(Admitted, Option<u64>), ApiError> {
        let storage = self.state.storage();
        // Step 1: RPM (in memory at both throttles; bounded by the cluster hop).
        let rpm = json!({ "cluster": job.cluster, "rpm": job.project_rpm });
        let (window, cluster_window) = match rpc::with_timeout(
            self.project_call(&job.aud, "admit_rpm", rpm),
            ADMIT_MARGIN_MS,
        )
        .await
        {
            Some(Ok((200, b))) => {
                let get = |k: &str| b.get(k).and_then(serde_json::Value::as_f64);
                match (get("window"), get("cluster_window")) {
                    (Some(w), Some(c)) => (w, c),
                    _ => {
                        self.refund_user(day);
                        return Err(ApiError::upstream("admit_rpm: missing window"));
                    }
                }
            }
            Some(Ok((status, b))) => {
                self.refund_user(day);
                return Err(ApiError::relayed(status, &b));
            }
            Some(Err(e)) => {
                self.refund_user(day);
                return Err(ApiError::upstream(format!("admit_rpm: {e}")));
            }
            None => {
                self.short_cooldown(cfg, &job.user).await;
                return Err(ApiError::upstream("admit_rpm timeout"));
            }
        };

        // Step 2: simulate + verify (§4.3), unless every program in the tx opted out (W7).
        let mut rent_simulated = None;
        if p.needs_simulation {
            let value = match rpc::simulate(url, cfg.rpc_timeout_ms, &job.tx, &p.requested).await {
                Ok(v) => v,
                Err(e) => {
                    self.refund_user(day);
                    return Err(ApiError::upstream(format!("simulate: {e}")));
                }
            };
            let keys: Vec<_> = p
                .static_keys
                .iter()
                .filter_map(|k| k.parse().ok())
                .collect();
            let requested: Vec<_> = p.requested.iter().filter_map(|k| k.parse().ok()).collect();
            let registered: Vec<_> = p.registered.iter().filter_map(|k| k.parse().ok()).collect();
            let (fee_payer, user) = match (job.fee_payer.parse(), job.user.parse()) {
                (Ok(f), Ok(u))
                    if keys.len() == p.static_keys.len()
                        && requested.len() == p.requested.len()
                        && registered.len() == p.registered.len() =>
                {
                    (f, u)
                }
                _ => {
                    self.refund_user(day);
                    return Err(ApiError::internal("program job: bad pubkeys"));
                }
            };
            let sim = inner_ix::parse_simulation(&value, &keys, &requested);
            let verdict = inner_ix::verify(
                &sim,
                Expect {
                    fee_payer: &fee_payer,
                    user: &user,
                    registered: &registered,
                    fee_cost: p.fee_cost,
                    declared_rent: p.declared_rent,
                    refund_only: p.refund_only,
                },
            );
            match verdict {
                Verdict::Ok { rent_spent } => rent_simulated = Some(rent_spent),
                Verdict::ProgramError(reason) => {
                    self.refund_user(day);
                    let wait = self.penalize(&storage, cfg, &job.user).await;
                    return Err(ApiError::simulation_failed(reason, wait));
                }
                Verdict::Incomplete(field) => {
                    self.refund_user(day);
                    return Err(ApiError::upstream(format!("simulation missing {field}")));
                }
                Verdict::Rejected(reason) => {
                    console_error!("inner_ix_rejected for {} ({}): {reason}", job.user, job.aud);
                    self.refund_user(day);
                    let wait = self.penalize(&storage, cfg, &job.user).await;
                    return Err(ApiError::inner_ix_rejected(reason, wait));
                }
            }
        }

        // Step 3: budget + balance (bounded like the memo admit: getBalance + cluster hop).
        let budget = json!({
            "cluster": job.cluster,
            "cost": job.cost,
            "budget": job.project_daily_budget,
            "fee_payer": job.fee_payer,
            "min_balance": job.min_balance,
        });
        let admit = self.project_call(&job.aud, "admit_budget", budget);
        let (bday, cluster_day) =
            match rpc::with_timeout(admit, cfg.rpc_timeout_ms + ADMIT_MARGIN_MS).await {
                Some(Ok((200, b))) => match (
                    b.get("day").and_then(serde_json::Value::as_i64),
                    b.get("cluster_day").and_then(serde_json::Value::as_i64),
                ) {
                    (Some(d), Some(c)) => (d, c),
                    _ => {
                        self.refund_user(day);
                        return Err(ApiError::upstream("admit_budget: missing day"));
                    }
                },
                Some(Ok((status, b))) => {
                    self.refund_user(day);
                    return Err(ApiError::relayed(status, &b));
                }
                Some(Err(e)) => {
                    self.refund_user(day);
                    return Err(ApiError::upstream(format!("admit_budget: {e}")));
                }
                None => {
                    self.short_cooldown(cfg, &job.user).await;
                    return Err(ApiError::upstream("admit_budget timeout"));
                }
            };
        Ok((
            Admitted {
                window,
                day: bday,
                cluster_window,
                cluster_day,
            },
            rent_simulated,
        ))
    }

    /// Grows the reject streak and sets the backoff cooldown (D17) — for simulation failures,
    /// which cost the relayer an RPC call but no fee. Returns the cooldown in ms.
    async fn penalize(&self, storage: &Storage, cfg: &Config, user: &str) -> f64 {
        let streak = self.next_reject_streak(user).await;
        let wait = reject_backoff_ms(
            cfg.reject_cooldown_ms,
            streak.unwrap_or(1),
            cfg.reject_cooldown_max_ms,
        );
        let until = Date::now().as_millis() as f64 + wait;
        let stored = match streak {
            Some(reject_streak) => {
                storage
                    .put_multiple(CooldownState {
                        cooldown_until: until,
                        reject_streak,
                    })
                    .await
            }
            None => storage.put(COOLDOWN_KEY, until).await,
        };
        if let Err(e) = stored {
            console_error!("cooldown put after simulation failure failed for {user}: {e}");
        }
        wait
    }

    /// After an admit timeout: the throttles may have counted it; stop immediate retries.
    async fn short_cooldown(&self, cfg: &Config, user: &str) {
        let until = Date::now().as_millis() as f64 + cfg.reject_cooldown_ms;
        if let Err(e) = self.state.storage().put(COOLDOWN_KEY, until).await {
            console_error!("cooldown put after admit timeout failed for {user}: {e}");
        }
    }

    /// Next consecutive-reject count. `None` if storage could not be read: the caller then
    /// uses the base cooldown and does not overwrite a (possibly higher) stored streak.
    async fn next_reject_streak(&self, user: &str) -> Option<u32> {
        match self.state.storage().get::<u32>(REJECT_STREAK_KEY).await {
            Ok(v) => Some(v.unwrap_or(0).saturating_add(1)),
            Err(e) => {
                console_error!("reject streak get failed for {user}: {e}");
                None
            }
        }
    }

    /// Best-effort, bounded: a failed refund only means the slots stay used until the window ends.
    async fn refund(&self, job: &RelayJob, admitted: &Admitted) {
        let body = json!({
            "cluster": job.cluster,
            "cost": job.cost,
            "window": admitted.window,
            "day": admitted.day,
            "cluster_window": admitted.cluster_window,
            "cluster_day": admitted.cluster_day,
        });
        match rpc::with_timeout(
            self.project_call(&job.aud, "refund", body),
            REFUND_TIMEOUT_MS,
        )
        .await
        {
            Some(Ok((200, _))) => {}
            Some(Ok((status, _))) => console_error!("project refund: status {status}"),
            Some(Err(e)) => console_error!("project refund failed: {e}"),
            None => console_error!("project refund timed out"),
        }
    }

    /// Asks the project's ProjectThrottle (which asks ClusterThrottle) for capacity; fail-closed.
    async fn admit(&self, job: &RelayJob) -> std::result::Result<Admitted, ApiError> {
        let body = json!({
            "cluster": job.cluster,
            "cost": job.cost,
            "rpm": job.project_rpm,
            "budget": job.project_daily_budget,
            "fee_payer": job.fee_payer,
            "min_balance": job.min_balance,
        });
        let (status, body) = self
            .project_call(&job.aud, "admit", body)
            .await
            .map_err(|e| ApiError::upstream(format!("project admit: {e}")))?;
        if status != 200 {
            return Err(ApiError::relayed(status, &body));
        }
        let get = |k: &str| body.get(k).and_then(serde_json::Value::as_f64);
        let get_day = |k: &str| body.get(k).and_then(serde_json::Value::as_i64);
        match (
            get("window"),
            get_day("day"),
            get("cluster_window"),
            get_day("cluster_day"),
        ) {
            (Some(window), Some(day), Some(cluster_window), Some(cluster_day)) => Ok(Admitted {
                window,
                day,
                cluster_window,
                cluster_day,
            }),
            _ => Err(ApiError::upstream("project admit: missing window")),
        }
    }

    async fn project_call(
        &self,
        aud: &str,
        action: &str,
        body: serde_json::Value,
    ) -> std::result::Result<(u16, serde_json::Value), worker::Error> {
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
        let json = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        Ok((status, json))
    }

    /// Best-effort D1 write (spec D13): never affects the response.
    fn audit(
        &self,
        job: &RelayJob,
        status: &'static str,
        reason: Option<String>,
        at: f64,
        rent_simulated: Option<u64>,
    ) {
        let db = match self.env.d1("DB") {
            Ok(db) => db,
            Err(e) => {
                console_error!("audit: no D1 binding: {e}");
                return;
            }
        };
        let (wallet, sig, aud, cluster, cost) = (
            job.user.clone(),
            job.signature.clone(),
            job.aud.clone(),
            job.cluster.clone(),
            job.cost as f64,
        );
        self.state.wait_until(async move {
            let reason = reason.map(JsValue::from).unwrap_or(JsValue::NULL);
            let stmt = db.prepare(
                "INSERT INTO relay_log (wallet_address, tx_signature, status, reason, created_at, aud, cluster, cost_lamports, rent_lamports) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            );
            let result = match stmt.bind(&[
                wallet.into(),
                sig.into(),
                status.into(),
                reason,
                JsValue::from_f64(at),
                aud.into(),
                cluster.into(),
                JsValue::from_f64(cost),
                rent_simulated.map(|r| JsValue::from_f64(r as f64)).unwrap_or(JsValue::NULL),
            ]) {
                Ok(bound) => bound.run().await.map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                console_error!("audit insert failed: {e}");
            }
        });
    }
}
