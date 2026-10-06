//! `ProjectThrottle` — one DO per project (`id_from_name(aud)`, spec v2 §4.4).
//! Project requests per minute + a 30 s cached balance of the project's fee payer (circuit
//! breaker), then the cluster-wide admit. Phase 2 adds the persistent daily budget.
//! Limits arrive with each admit (from the registry snapshot, ≤ REGISTRY_CACHE_MS old).

use std::cell::RefCell;

use futures_util::future::{FutureExt, LocalBoxFuture, Shared};
use serde::Deserialize;
use serde_json::{Value, json};
use worker::*;

use crate::{
    config::{BALANCE_CACHE_MS, BALANCE_FAILURE_BACKOFF_MS, Cluster, Config, rpc_url},
    error::ApiError,
    rpc,
    window::FixedWindow,
};

type BalanceFuture = Shared<LocalBoxFuture<'static, std::result::Result<u64, String>>>;

#[derive(Deserialize)]
struct Admit {
    cluster: String,
    rpm: u32,
    fee_payer: String,
    min_balance: u64,
}

#[derive(Deserialize)]
struct Refund {
    cluster: String,
    window: f64,
    cluster_window: f64,
}

#[durable_object]
pub struct ProjectThrottle {
    env: Env,
    rpm: FixedWindow,
    /// (fee payer, lamports, fetched_at): keyed by pubkey so a key rotation never reuses it.
    balance: RefCell<Option<(String, u64, f64)>>,
    balance_inflight: RefCell<Option<(String, BalanceFuture)>>,
    /// Last getBalance failure (fee payer, message, at): answered from here for
    /// BALANCE_FAILURE_BACKOFF_MS so a degraded RPC is not hit again by every request (R6).
    balance_failure: RefCell<Option<(String, String, f64)>>,
}

impl DurableObject for ProjectThrottle {
    fn new(_state: State, env: Env) -> Self {
        Self {
            env,
            rpm: FixedWindow::default(),
            balance: RefCell::new(None),
            balance_inflight: RefCell::new(None),
            balance_failure: RefCell::new(None),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let now = Date::now().as_millis() as f64;
        let cfg = match Config::from_env(&self.env) {
            Ok(c) => c,
            Err(e) => return ApiError::config(e).into_response(),
        };
        // Roll first so a refund for an expired window reports refunded:false.
        self.rpm.roll(now);
        let path = req.path();
        let body: Value = match req.json().await {
            Ok(b) => b,
            Err(e) => return ApiError::internal(format!("project body: {e}")).into_response(),
        };
        let result = match path.as_str() {
            "/admit" => match serde_json::from_value::<Admit>(body) {
                Ok(a) => self.admit(a, &cfg, now).await,
                Err(e) => Err(ApiError::internal(format!("admit body: {e}"))),
            },
            "/refund" => match serde_json::from_value::<Refund>(body) {
                Ok(r) => self.refund(r, &cfg).await,
                Err(e) => Err(ApiError::internal(format!("refund body: {e}"))),
            },
            _ => Err(ApiError::not_found()),
        };
        match result {
            Ok(r) => Ok(r),
            Err(e) => e.into_response(),
        }
    }
}

impl ProjectThrottle {
    async fn admit(
        &self,
        a: Admit,
        cfg: &Config,
        now: f64,
    ) -> std::result::Result<Response, ApiError> {
        let cluster =
            Cluster::parse(&a.cluster).ok_or_else(|| ApiError::internal("bad cluster"))?;
        if a.rpm == 0 {
            return Err(ApiError::internal("project rpm 0"));
        }
        let url = rpc_url(&self.env, cluster).map_err(ApiError::config)?;
        // Capacity check + increment happen synchronously, before the first await.
        let window = self
            .rpm
            .take(a.rpm, now)
            .map_err(ApiError::project_throttle)?;

        match self.balance(&a.fee_payer, &url, cfg, now).await {
            Err(e) => {
                self.rpm.release(window);
                return Err(ApiError::upstream(e));
            }
            Ok(lamports) if lamports < a.min_balance => {
                self.rpm.release(window);
                return Err(ApiError::low_balance());
            }
            Ok(_) => {}
        }

        let call = cluster_call(&self.env, "admit", json!({ "cluster": cluster.as_str() }));
        match rpc::with_timeout(call, cfg.cluster_hop_timeout_ms).await {
            Some(Ok((200, body))) => {
                let cluster_window = body
                    .get("window")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| ApiError::upstream("cluster admit: missing window"))?;
                Response::from_json(&json!({ "window": window, "cluster_window": cluster_window }))
                    .map_err(|e| ApiError::internal(e.to_string()))
            }
            Some(Ok((status, body))) => {
                self.rpm.release(window);
                Err(ApiError::relayed(status, &body))
            }
            Some(Err(e)) => {
                self.rpm.release(window);
                Err(ApiError::upstream(format!("cluster admit: {e}")))
            }
            // The cluster may have counted it: keep our slot too (fail-closed, R-v2-5).
            None => Err(ApiError::upstream("cluster admit timeout")),
        }
    }

    /// Best-effort: gives back this project's slot and forwards the refund to the cluster.
    async fn refund(&self, r: Refund, cfg: &Config) -> std::result::Result<Response, ApiError> {
        let refunded = self.rpm.release(r.window);
        let call = cluster_call(
            &self.env,
            "refund",
            json!({ "cluster": r.cluster, "window": r.cluster_window }),
        );
        let cluster_refunded = match rpc::with_timeout(call, cfg.cluster_hop_timeout_ms).await {
            Some(Ok((200, body))) => body.get("refunded").and_then(Value::as_bool) == Some(true),
            Some(Ok((status, _))) => {
                console_error!("cluster refund: status {status}");
                false
            }
            Some(Err(e)) => {
                console_error!("cluster refund failed: {e}");
                false
            }
            None => {
                console_error!("cluster refund timed out");
                false
            }
        };
        Response::from_json(&json!({ "refunded": refunded, "cluster_refunded": cluster_refunded }))
            .map_err(|e| ApiError::internal(e.to_string()))
    }

    /// Cached balance with failure backoff and single-flight on a miss.
    async fn balance(
        &self,
        fee_payer: &str,
        url: &str,
        cfg: &Config,
        now: f64,
    ) -> std::result::Result<u64, String> {
        if let Some((key, lamports, at)) = self.balance.borrow().as_ref()
            && key == fee_payer
            && now - at < BALANCE_CACHE_MS
        {
            return Ok(*lamports);
        }
        if let Some((key, msg, at)) = self.balance_failure.borrow().as_ref()
            && key == fee_payer
            && now - at < BALANCE_FAILURE_BACKOFF_MS
        {
            return Err(format!("{msg} (cached)"));
        }
        let fut = {
            let mut slot = self.balance_inflight.borrow_mut();
            match slot.as_ref() {
                Some((key, f)) if key == fee_payer => f.clone(),
                _ => {
                    let (url, timeout, pk) =
                        (url.to_string(), cfg.rpc_timeout_ms, fee_payer.to_string());
                    let f = async move { rpc::get_balance(&url, timeout, &pk).await }
                        .boxed_local()
                        .shared();
                    *slot = Some((fee_payer.to_string(), f.clone()));
                    f
                }
            }
        };
        let result = fut.clone().await;
        // Clear the slot only if it still holds *this* future (a newer one may have replaced it).
        let mut slot = self.balance_inflight.borrow_mut();
        if slot.as_ref().is_some_and(|(_, f)| f.ptr_eq(&fut)) {
            slot.take();
        }
        drop(slot);
        let at = Date::now().as_millis() as f64;
        match &result {
            Ok(lamports) => {
                *self.balance.borrow_mut() = Some((fee_payer.to_string(), *lamports, at));
                self.balance_failure.borrow_mut().take();
            }
            Err(e) => {
                *self.balance_failure.borrow_mut() = Some((fee_payer.to_string(), e.clone(), at))
            }
        }
        result
    }
}

/// POSTs to the `ClusterThrottle` named in `body.cluster`; returns (status, JSON body).
async fn cluster_call(
    env: &Env,
    action: &str,
    body: Value,
) -> std::result::Result<(u16, Value), worker::Error> {
    let name = body
        .get("cluster")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let stub = env
        .durable_object("CLUSTER_THROTTLE")?
        .id_from_name(&name)?
        .get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(body.to_string().into()));
    let req = Request::new_with_init(&format!("https://cluster/{action}"), &init)?;
    let mut resp = stub.fetch_with_request(req).await?;
    let status = resp.status_code();
    let json = resp.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, json))
}
