//! Error contract (spec v1 §4.3 + v2 §4.3). Every non-2xx response goes through `ApiError`.

use serde::Serialize;
use worker::{Response, Result};

#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            reason: None,
            retry_after_ms: None,
            signature: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn with_retry_after(mut self, ms: f64) -> Self {
        self.retry_after_ms = Some(ms.max(0.0).ceil() as u64);
        self
    }

    pub fn with_signature(mut self, signature: String) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn validation(reason: impl Into<String>) -> Self {
        Self::new(400, "validation", "transaction rejected by policy").with_reason(reason)
    }
    pub fn unauthorized(reason: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", "invalid or missing idToken").with_reason(reason)
    }
    pub fn origin() -> Self {
        Self::new(403, "origin", "origin not allowed")
    }
    pub fn not_found() -> Self {
        Self::new(404, "not_found", "not found")
    }
    pub fn method() -> Self {
        Self::new(405, "method", "method not allowed")
    }
    pub fn in_progress() -> Self {
        Self::new(
            409,
            "in_progress",
            "a relay for this wallet is already in progress",
        )
    }
    pub fn too_large() -> Self {
        Self::new(413, "too_large", "request body too large")
    }
    pub fn content_type() -> Self {
        Self::new(415, "content_type", "content-type must be application/json")
    }
    pub fn cooldown(retry_after_ms: f64) -> Self {
        Self::new(429, "cooldown", "wallet is cooling down").with_retry_after(retry_after_ms)
    }
    pub fn project_throttle(retry_after_ms: f64) -> Self {
        Self::new(429, "project_throttle", "project is at capacity")
            .with_retry_after(retry_after_ms)
    }
    pub fn cluster_throttle(retry_after_ms: f64) -> Self {
        Self::new(429, "cluster_throttle", "relayer is at capacity")
            .with_retry_after(retry_after_ms)
    }
    pub fn user_quota(retry_after_ms: f64) -> Self {
        Self::new(429, "user_quota", "daily quota reached for this wallet")
            .with_retry_after(retry_after_ms)
    }
    pub fn project_budget(retry_after_ms: f64) -> Self {
        Self::new(503, "project_budget", "project daily budget exhausted")
            .with_retry_after(retry_after_ms)
    }
    pub fn cluster_budget(retry_after_ms: f64) -> Self {
        Self::new(503, "cluster_budget", "relayer daily budget exhausted")
            .with_retry_after(retry_after_ms)
    }
    pub fn project_paused() -> Self {
        Self::new(503, "project_paused", "project is not active")
    }
    pub fn cluster_disabled() -> Self {
        Self::new(503, "cluster_disabled", "cluster is disabled")
    }
    /// Misconfiguration: the detail (e.g. which env var is missing) goes to the log only.
    pub fn config(reason: impl Into<String>) -> Self {
        worker::console_error!("config error: {}", reason.into());
        Self::new(500, "internal", "relayer misconfigured")
    }
    /// Misconfigured project already logged once when the registry loaded: no per-request log
    /// (a forged, unverified token naming it must not be able to flood the log).
    pub fn misconfigured_quiet() -> Self {
        Self::new(500, "internal", "relayer misconfigured")
    }
    /// Unexpected failure: the detail goes to the log only, never to the client.
    pub fn internal(reason: impl Into<String>) -> Self {
        worker::console_error!("internal error: {}", reason.into());
        Self::new(500, "internal", "internal error")
    }
    pub fn rpc_rejected(reason: impl Into<String>, retry_after_ms: f64) -> Self {
        Self::new(502, "rpc_rejected", "RPC rejected the transaction")
            .with_reason(reason)
            .with_retry_after(retry_after_ms)
    }
    pub fn low_balance() -> Self {
        Self::new(503, "low_balance", "fee payer balance below floor")
    }
    /// Detail (may contain raw transport errors / upstream URLs) is logged, never returned.
    pub fn upstream(reason: impl Into<String>) -> Self {
        Self::upstream_relayed().logged(reason)
    }
    /// A 503 `upstream` passed through from another component that already logged the detail.
    pub fn upstream_relayed() -> Self {
        Self::new(503, "upstream", "upstream dependency unavailable")
    }
    pub fn rpc_unknown(reason: impl Into<String>, signature: String) -> Self {
        Self::new(
            504,
            "rpc_unknown",
            "RPC outcome unknown; poll the signature",
        )
        .with_signature(signature)
        .logged(reason)
    }

    /// Rebuilds an error answered by another Durable Object (ProjectThrottle / ClusterThrottle)
    /// so it reaches the client unchanged. Unknown shapes become a logged 503 `upstream`.
    pub fn relayed(status: u16, body: &serde_json::Value) -> Self {
        let retry = body
            .get("retry_after_ms")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        match (status, body.get("code").and_then(serde_json::Value::as_str)) {
            (429, Some("project_throttle")) => Self::project_throttle(retry),
            (429, Some("cluster_throttle")) => Self::cluster_throttle(retry),
            (503, Some("low_balance")) => Self::low_balance(),
            (503, Some("project_budget")) => Self::project_budget(retry),
            (503, Some("cluster_budget")) => Self::cluster_budget(retry),
            // The other DO already logged its own detail; don't log it twice.
            (503, Some("upstream")) => Self::upstream_relayed(),
            (500, Some("internal")) => Self::config("throttle DO returned 500"),
            _ => Self::upstream(format!("throttle DO status {status}")),
        }
    }

    /// Logs a detail that must not reach the client (transport errors can contain URLs).
    fn logged(self, reason: impl Into<String>) -> Self {
        worker::console_error!("{}: {}", self.code, reason.into());
        self
    }

    pub fn into_response(self) -> Result<Response> {
        let status = self.status;
        let retry_after = self.retry_after_ms;
        let mut resp = Response::from_json(&self)?.with_status(status);
        if let Some(ms) = retry_after {
            resp.headers_mut()
                .set("Retry-After", &ms.div_ceil(1000).to_string())?;
        }
        Ok(resp)
    }
}
