//! `RelayController` — one DO per user wallet (spec §4.4, stateful part).
//!
//! Ordering matters: DO input gates only close during storage operations, so any `await`
//! on a fetch (GlobalThrottle, RPC) lets another request for the same wallet in. The
//! in-flight check-and-set below therefore runs synchronously before the first `await`.

use std::{cell::Cell, time::Duration};

use futures_util::future::{Either, select};

use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::*;

use crate::{
    config::Config,
    error::ApiError,
    rpc::{self, SendOutcome},
};

const COOLDOWN_KEY: &str = "cooldown_until";
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
            Err(e) => return ApiError::internal(e).into_response(),
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
        let admit_budget = Duration::from_millis(cfg.rpc_timeout_ms + ADMIT_MARGIN_MS);
        match select(Box::pin(self.admit()), Box::pin(Delay::from(admit_budget))).await {
            Either::Left((result, _)) => result?,
            Either::Right(_) => return Err(ApiError::upstream("global admit timeout")),
        }

        let outcome = rpc::send_transaction(&cfg.rpc_url, cfg.rpc_timeout_ms, &job.tx).await;
        let done = Date::now().as_millis() as f64;
        let (cooldown_ms, audit) = match &outcome {
            SendOutcome::Accepted(_) | SendOutcome::AlreadyProcessed => {
                (cfg.cooldown_ms, ("success", None))
            }
            SendOutcome::Rejected { reason, http_429 } => {
                if *http_429 {
                    console_warn!("rpc http 429 (R6) for {}", job.user);
                }
                (
                    cfg.reject_cooldown_ms,
                    ("rpc_error", Some(format!("rejected: {reason}"))),
                )
            }
            SendOutcome::Unknown(reason) => (
                cfg.cooldown_ms,
                ("rpc_error", Some(format!("unknown: {reason}"))),
            ),
        };
        if let Err(e) = storage.put(COOLDOWN_KEY, done + cooldown_ms).await {
            // The tx may already be on its way; never turn that into an error response.
            console_error!("cooldown put failed for {}: {e}", job.user);
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
                Err(ApiError::rpc_rejected(reason, cfg.reject_cooldown_ms))
            }
            SendOutcome::Unknown(reason) => Err(ApiError::rpc_unknown(reason, job.signature)),
        }
    }

    /// Asks the GlobalThrottle DO for capacity + balance; fail-closed on any error.
    async fn admit(&self) -> std::result::Result<(), ApiError> {
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
            return Ok(());
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
