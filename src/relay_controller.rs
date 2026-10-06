//! `RelayController` — one DO per user wallet (spec §4.4, stateful part).
//!
//! Ordering matters: DO input gates only close during storage operations, so any `await`
//! on a fetch (GlobalThrottle, RPC) lets another request for the same wallet in. The
//! in-flight check-and-set below therefore runs synchronously before the first `await`.

use std::cell::Cell;

use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::*;

use crate::{
    config::{Config, reject_backoff_ms},
    error::ApiError,
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
const ADMIT_MARGIN_MS: u64 = 2_000;

#[derive(Deserialize)]
pub struct RelayJob {
    /// Fully signed wire tx, base64.
    pub tx: String,
    pub user: String,
    pub signature: String,
}

#[durable_object]
pub struct RelayController {
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

impl DurableObject for RelayController {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            inflight_since: Cell::new(None),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let now = Date::now().as_millis() as f64;
        let cfg = match Config::from_env(&self.env) {
            Ok(c) => c,
            Err(e) => return ApiError::config(e).into_response(),
        };

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

impl RelayController {
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

        let cooldown_until: f64 = storage
            .get(COOLDOWN_KEY)
            .await
            .map_err(|e| ApiError::internal(format!("storage get: {e}")))?
            .unwrap_or(0.0);
        if now < cooldown_until {
            return Err(ApiError::cooldown(cooldown_until - now));
        }

        // Bounded: GlobalThrottle may itself wait up to one RPC timeout on getBalance.
        let window = rpc::with_timeout(self.admit(), cfg.rpc_timeout_ms + ADMIT_MARGIN_MS)
            .await
            .ok_or_else(|| ApiError::upstream("global admit timeout"))??;

        let outcome = rpc::send_transaction(&cfg.rpc_url, cfg.rpc_timeout_ms, &job.tx).await;
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
            (SendOutcome::Accepted(_), _) => {
                // Only a real acceptance clears the streak (AlreadyProcessed can be replayed).
                match storage.put(COOLDOWN_KEY, done + cooldown_ms).await {
                    Ok(()) => storage.delete(REJECT_STREAK_KEY).await.map(|_| ()),
                    Err(e) => Err(e),
                }
            }
            _ => storage.put(COOLDOWN_KEY, done + cooldown_ms).await,
        };
        if let Err(e) = stored {
            // The tx may already be on its way; never turn that into an error response.
            console_error!("cooldown put failed for {}: {e}", job.user);
        }
        if tx_fault {
            self.refund(window, cfg).await;
        }
        self.audit(&job, audit.0, audit.1, done);

        match outcome {
            SendOutcome::Accepted(sig) => Response::from_json(&json!({ "signature": sig }))
                .map_err(|e| ApiError::internal(e.to_string())),
            // The same signed tx already landed: report it as the success it is.
            SendOutcome::AlreadyProcessed => {
                Response::from_json(&json!({ "signature": job.signature }))
                    .map_err(|e| ApiError::internal(e.to_string()))
            }
            SendOutcome::Rejected { reason, .. } => {
                Err(ApiError::rpc_rejected(reason, cooldown_ms))
            }
            SendOutcome::Unknown(reason) => Err(ApiError::rpc_unknown(reason, job.signature)),
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

    /// Best-effort, bounded: a failed refund only means the slot stays used until the window ends.
    async fn refund(&self, window: f64, cfg: &Config) {
        let call = async {
            let stub = self
                .env
                .durable_object("GLOBAL_THROTTLE")
                .and_then(|ns| ns.id_from_name("global")?.get_stub())?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_body(Some(json!({ "window": window }).to_string().into()));
            let req = Request::new_with_init("https://global/refund", &init)?;
            let resp = stub.fetch_with_request(req).await?;
            if resp.status_code() != 200 {
                console_error!(
                    "global refund: GlobalThrottle returned {}",
                    resp.status_code()
                );
            }
            Ok::<(), worker::Error>(())
        };
        match rpc::with_timeout(call, cfg.rpc_timeout_ms).await {
            Some(Ok(())) => {}
            Some(Err(e)) => console_error!("global refund failed: {e}"),
            None => console_error!("global refund timed out"),
        }
    }

    /// Asks the GlobalThrottle DO for capacity + balance; fail-closed on any error.
    /// Returns the window the request was counted in (needed to refund it).
    async fn admit(&self) -> std::result::Result<f64, ApiError> {
        let stub = self
            .env
            .durable_object("GLOBAL_THROTTLE")
            .and_then(|ns| ns.id_from_name("global")?.get_stub())
            .map_err(|e| ApiError::upstream(format!("global stub: {e}")))?;
        let mut resp = stub
            .fetch_with_str("https://global/admit")
            .await
            .map_err(|e| ApiError::upstream(format!("global fetch: {e}")))?;
        if resp.status_code() == 200 {
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ApiError::upstream(format!("global body: {e}")))?;
            return body
                .get("window")
                .and_then(|w| w.as_f64())
                .ok_or_else(|| ApiError::upstream("global admit: missing window"));
        }
        let status = resp.status_code();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::upstream(format!("global body: {e}")))?;
        let retry = body
            .get("retry_after_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        Err(match (status, body.get("code").and_then(|v| v.as_str())) {
            (429, _) => ApiError::global_throttle(retry),
            (_, Some("low_balance")) => ApiError::low_balance(),
            // Today GlobalThrottle only returns 500 for config errors; keep the log generic.
            (500, Some("internal")) => ApiError::config("global throttle returned 500"),
            _ => ApiError::upstream(
                body.get("reason")
                    .and_then(|v| v.as_str())
                    .map_or_else(|| format!("global status {status}"), str::to_string),
            ),
        })
    }

    /// Best-effort D1 write (spec D13): never affects the response.
    fn audit(&self, job: &RelayJob, status: &'static str, reason: Option<String>, at: f64) {
        let db = match self.env.d1("DB") {
            Ok(db) => db,
            Err(e) => {
                console_error!("audit: no D1 binding: {e}");
                return;
            }
        };
        let (wallet, sig) = (job.user.clone(), job.signature.clone());
        self.state.wait_until(async move {
            let reason = reason.map(JsValue::from).unwrap_or(JsValue::NULL);
            let stmt = db.prepare(
                "INSERT INTO relay_log (wallet_address, tx_signature, status, reason, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            );
            let result = match stmt.bind(&[wallet.into(), sig.into(), status.into(), reason, JsValue::from_f64(at)]) {
                Ok(bound) => bound.run().await.map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                console_error!("audit insert failed: {e}");
            }
        });
    }
}
