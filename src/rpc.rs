//! Solana JSON-RPC over HTTP with a hard timeout (spec §4.4, §4.5).
//! Cloudflare `fetch` has no default timeout, so every call races a `Delay`.

use std::time::Duration;

use futures_util::future::{Either, select};
use serde_json::{Value, json};
use worker::{Delay, Fetch, Headers, Method, Request, RequestInit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// RPC returned a signature: the tx was forwarded.
    Accepted(String),
    /// JSON-RPC error or HTTP 4xx: the tx was definitely not forwarded.
    /// `tx_fault`: the rejection is about the transaction/request itself (JSON-RPC error,
    /// HTTP 400/413) rather than the RPC being unavailable (429, 403, 408, ...).
    Rejected {
        reason: String,
        http_429: bool,
        tx_fault: bool,
    },
    /// Timeout, transport error, HTTP 5xx, or unparseable 200: it may have been forwarded.
    Unknown(String),
    /// The exact same signed tx already landed (resubmission).
    AlreadyProcessed,
}

enum Raw {
    Http(u16, String),
    Transport(String),
    Timeout,
}

async fn post(url: &str, timeout_ms: u64, body: &Value) -> Raw {
    let call = async {
        let headers = Headers::new();
        headers
            .set("content-type", "application/json")
            .map_err(|e| e.to_string())?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(body.to_string().into()));
        let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status_code();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((status, text))
    };
    match with_timeout(call, timeout_ms).await {
        Some(Ok((status, text))) => Raw::Http(status, text),
        Some(Err(e)) => Raw::Transport(e),
        None => Raw::Timeout,
    }
}

/// Races `fut` against a `Delay`; `None` means the timeout won. The losing side is dropped:
/// `worker::Delay` clears its `setTimeout` on drop (worker 0.8.7 `delay.rs` `PinnedDrop`),
/// so a fast call leaves no timer behind.
pub async fn with_timeout<F: Future>(fut: F, timeout_ms: u64) -> Option<F::Output> {
    let timeout = Delay::from(Duration::from_millis(timeout_ms));
    match select(Box::pin(fut), Box::pin(timeout)).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
}

fn rpc_body(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

/// JSON-RPC errors raised by the node itself rather than by the transaction:
/// -32603 internal error, -32005 node unhealthy, -32004 block not available.
const NODE_SIDE_ERROR_CODES: [i64; 3] = [-32603, -32005, -32004];

pub fn classify_send(status: u16, text: &str) -> SendOutcome {
    if (400..500).contains(&status) {
        return SendOutcome::Rejected {
            reason: format!("http {status}"),
            http_429: status == 429,
            tx_fault: matches!(status, 400 | 413),
        };
    }
    if status != 200 {
        return SendOutcome::Unknown(format!("http {status}"));
    }
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return SendOutcome::Unknown("unparseable response".into());
    };
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("rpc error");
        if msg.contains("already been processed") || msg.contains("AlreadyProcessed") {
            return SendOutcome::AlreadyProcessed;
        }
        let code = err.get("code").and_then(Value::as_i64);
        return SendOutcome::Rejected {
            reason: truncate(msg, 200),
            http_429: false,
            // Node-side failures say nothing about the transaction (D17: treat like 429).
            // An error without a code cannot be attributed to the tx either -> not tx-fault.
            tx_fault: code.is_some_and(|c| !NODE_SIDE_ERROR_CODES.contains(&c)),
        };
    }
    match v.get("result").and_then(Value::as_str) {
        Some(sig) => SendOutcome::Accepted(sig.to_string()),
        None => SendOutcome::Unknown("missing result".into()),
    }
}

pub async fn send_transaction(url: &str, timeout_ms: u64, tx_base64: &str) -> SendOutcome {
    let body = rpc_body(
        "sendTransaction",
        json!([tx_base64, { "encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed" }]),
    );
    match post(url, timeout_ms, &body).await {
        Raw::Http(status, text) => classify_send(status, &text),
        Raw::Transport(e) => SendOutcome::Unknown(format!("transport: {}", truncate(&e, 200))),
        Raw::Timeout => SendOutcome::Unknown("timeout".into()),
    }
}

async fn call_result(
    url: &str,
    timeout_ms: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    match post(url, timeout_ms, &rpc_body(method, params)).await {
        Raw::Http(200, text) => {
            let v: Value =
                serde_json::from_str(&text).map_err(|_| format!("{method}: unparseable"))?;
            v.get("result")
                .cloned()
                .ok_or_else(|| format!("{method}: rpc error"))
        }
        Raw::Http(status, _) => Err(format!("{method}: http {status}")),
        Raw::Transport(e) => Err(format!("{method}: transport: {}", truncate(&e, 120))),
        Raw::Timeout => Err(format!("{method}: timeout")),
    }
}

pub async fn get_balance(url: &str, timeout_ms: u64, pubkey: &str) -> Result<u64, String> {
    let r = call_result(
        url,
        timeout_ms,
        "getBalance",
        json!([pubkey, { "commitment": "confirmed" }]),
    )
    .await?;
    r.get("value")
        .and_then(Value::as_u64)
        .ok_or_else(|| "getBalance: bad result".into())
}

pub async fn get_genesis_hash(url: &str, timeout_ms: u64) -> Result<String, String> {
    let r = call_result(url, timeout_ms, "getGenesisHash", json!([])).await?;
    r.as_str()
        .map(str::to_string)
        .ok_or_else(|| "getGenesisHash: bad result".into())
}

/// Plain GET with the same timeout semantics (used for the Web3Auth JWKS).
pub async fn get_text(url: &str, timeout_ms: u64) -> Result<String, String> {
    let call = async {
        let mut resp = Fetch::Url(url.parse().map_err(|_| "bad url".to_string())?)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status_code() != 200 {
            return Err(format!("http {}", resp.status_code()));
        }
        resp.text().await.map_err(|e| e.to_string())
    };
    with_timeout(call, timeout_ms)
        .await
        .unwrap_or_else(|| Err("timeout".into()))
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_send_outcomes() {
        assert_eq!(
            classify_send(200, r#"{"result":"sig1"}"#),
            SendOutcome::Accepted("sig1".into())
        );
        assert!(matches!(
            classify_send(
                200,
                r#"{"error":{"code":-32002,"message":"Blockhash not found"}}"#
            ),
            SendOutcome::Rejected {
                http_429: false,
                tx_fault: true,
                ..
            }
        ));
        assert_eq!(
            classify_send(
                200,
                r#"{"error":{"code":-32002,"message":"Transaction simulation failed: This transaction has already been processed"}}"#
            ),
            SendOutcome::AlreadyProcessed
        );
        assert!(matches!(
            classify_send(429, "Too many requests"),
            SendOutcome::Rejected {
                http_429: true,
                tx_fault: false,
                ..
            }
        ));
        for code in [-32603, -32005, -32004] {
            let body = format!(r#"{{"error":{{"code":{code},"message":"Node is unhealthy"}}}}"#);
            assert!(matches!(
                classify_send(200, &body),
                SendOutcome::Rejected {
                    tx_fault: false,
                    http_429: false,
                    ..
                }
            ));
        }
        assert!(matches!(
            classify_send(200, r#"{"error":{"message":"overloaded"}}"#),
            SendOutcome::Rejected {
                tx_fault: false,
                ..
            }
        ));
        assert!(matches!(
            classify_send(400, ""),
            SendOutcome::Rejected { tx_fault: true, .. }
        ));
        assert!(matches!(
            classify_send(408, ""),
            SendOutcome::Rejected {
                tx_fault: false,
                ..
            }
        ));
        assert!(matches!(
            classify_send(403, ""),
            SendOutcome::Rejected {
                http_429: false,
                tx_fault: false,
                ..
            }
        ));
        assert!(matches!(classify_send(503, ""), SendOutcome::Unknown(_)));
        assert!(matches!(
            classify_send(200, "<html>"),
            SendOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_send(200, r#"{"result":null}"#),
            SendOutcome::Unknown(_)
        ));
    }
}
