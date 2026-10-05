//! Error contract (spec §4.3). Every non-2xx response goes through `ApiError`.

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
        Self::new(404, "not_found", "no such route")
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
    pub fn global_throttle(retry_after_ms: f64) -> Self {
        Self::new(429, "global_throttle", "relayer is at capacity").with_retry_after(retry_after_ms)
    }
    pub fn internal(reason: impl Into<String>) -> Self {
        Self::new(500, "internal", "internal error").with_reason(reason)
    }
    pub fn rpc_rejected(reason: impl Into<String>, retry_after_ms: f64) -> Self {
        Self::new(502, "rpc_rejected", "RPC rejected the transaction")
            .with_reason(reason)
            .with_retry_after(retry_after_ms)
    }
    pub fn low_balance() -> Self {
        Self::new(503, "low_balance", "relayer balance below floor")
    }
    pub fn upstream(reason: impl Into<String>) -> Self {
        Self::new(503, "upstream", "upstream dependency unavailable").with_reason(reason)
    }
    pub fn rpc_unknown(reason: impl Into<String>, signature: String) -> Self {
        Self::new(
            504,
            "rpc_unknown",
            "RPC outcome unknown; poll the signature",
        )
        .with_reason(reason)
        .with_signature(signature)
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
