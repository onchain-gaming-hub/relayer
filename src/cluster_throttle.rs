//! `ClusterThrottle` — one DO per cluster (`id_from_name("devnet" | "mainnet")`, spec v2 §4.4).
//! Requests per minute across every project on the cluster, protecting the RPC plan.
//! Phase 2 adds the persistent daily budget.

use serde::Deserialize;
use serde_json::json;
use worker::*;

use crate::{
    config::{Cluster, Config},
    error::ApiError,
    window::FixedWindow,
};

#[derive(Deserialize)]
struct Body {
    cluster: String,
    #[serde(default)]
    window: Option<f64>,
}

#[durable_object]
pub struct ClusterThrottle {
    env: Env,
    rpm: FixedWindow,
}

impl DurableObject for ClusterThrottle {
    fn new(_state: State, env: Env) -> Self {
        Self {
            env,
            rpm: FixedWindow::default(),
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
        let body: Body = match req.json().await {
            Ok(b) => b,
            Err(e) => return ApiError::internal(format!("cluster body: {e}")).into_response(),
        };
        let Some(cluster) = Cluster::parse(&body.cluster) else {
            return ApiError::internal("cluster body: unknown cluster").into_response();
        };
        match path.as_str() {
            "/admit" => match self.rpm.take(cfg.cluster_rpm(cluster), now) {
                Ok(window) => Response::from_json(&json!({ "window": window })),
                Err(retry) => ApiError::cluster_throttle(retry).into_response(),
            },
            "/refund" => {
                let refunded = body.window.is_some_and(|w| self.rpm.release(w));
                Response::from_json(&json!({ "refunded": refunded }))
            }
            _ => ApiError::not_found().into_response(),
        }
    }
}
