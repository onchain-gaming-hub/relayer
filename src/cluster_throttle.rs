//! `ClusterThrottle` — one DO per cluster (`id_from_name("devnet" | "mainnet")`, spec v2 §4.4).
//! Requests per minute across every project on the cluster (memory, protects the RPC plan) and
//! the cluster-wide daily fee and rent budgets (SQLite, `daily.rs`, v3 W9) — the
//! catastrophic stop (V12).

use serde::Deserialize;
use serde_json::json;
use worker::*;

use crate::{
    config::{Cluster, Config},
    daily,
    error::ApiError,
    window::FixedWindow,
};

#[derive(Deserialize)]
struct Body {
    cluster: String,
    /// Fee lamports.
    cost: u64,
    /// Declared rent lamports (v3 W9; 0 for memo txs).
    #[serde(default)]
    rent: u64,
    #[serde(default)]
    window: Option<f64>,
    #[serde(default)]
    day: Option<i64>,
}

#[durable_object]
pub struct ClusterThrottle {
    state: State,
    env: Env,
    rpm: FixedWindow,
}

impl DurableObject for ClusterThrottle {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
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
        let sql = self.state.storage().sql();
        let cost = i64::try_from(body.cost).unwrap_or(i64::MAX);
        let rent = i64::try_from(body.rent).unwrap_or(i64::MAX);
        match path.as_str() {
            "/admit" => {
                // Both checks are synchronous (memory + one SQLite statement, V8).
                let window = match self.rpm.take(cfg.cluster_rpm(cluster), now) {
                    Ok(w) => w,
                    Err(retry) => return ApiError::cluster_throttle(retry).into_response(),
                };
                let day = daily::day_of(now);
                let budget = i64::try_from(cfg.cluster_daily_budget(cluster)).unwrap_or(i64::MAX);
                match daily::try_add(&sql, daily::FEE, day, daily::FEE, cost, budget) {
                    Ok(Some(_)) => Response::from_json(&json!({ "window": window, "day": day })),
                    Ok(None) => {
                        self.rpm.release(window);
                        ApiError::cluster_budget(daily::ms_until_next_day(now), daily::FEE)
                            .into_response()
                    }
                    Err(e) => {
                        self.rpm.release(window);
                        ApiError::internal(format!("cluster budget: {e}")).into_response()
                    }
                }
            }
            // v3 program path (W14): RPM before simulation, budget after verification.
            "/admit_rpm" => match self.rpm.take(cfg.cluster_rpm(cluster), now) {
                Ok(window) => Response::from_json(&json!({ "window": window })),
                Err(retry) => ApiError::cluster_throttle(retry).into_response(),
            },
            "/admit_budget" => {
                let day = daily::day_of(now);
                match admit_budgets(&sql, &cfg, cluster, day, cost, rent) {
                    Ok(()) => Response::from_json(&json!({ "day": day })),
                    Err(Some(kind)) => {
                        ApiError::cluster_budget(daily::ms_until_next_day(now), kind)
                            .into_response()
                    }
                    Err(None) => ApiError::internal("cluster budget").into_response(),
                }
            }
            "/refund" => {
                let refunded = body.window.is_some_and(|w| self.rpm.release(w));
                if let Some(day) = body.day {
                    for (kind, amount) in [(daily::FEE, cost), (daily::RENT, rent)] {
                        if let Err(e) = daily::refund(&sql, daily::FEE, day, kind, amount) {
                            console_error!("cluster {kind} budget refund failed: {e}");
                        }
                    }
                }
                Response::from_json(&json!({ "refunded": refunded }))
            }
            _ => ApiError::not_found().into_response(),
        }
    }
}

/// Fee, then rent (W9): both synchronous SQLite statements, so the pair is atomic within this
/// DO. A rent refusal gives the fee back. `Err(Some(kind))` = that budget is exhausted,
/// `Err(None)` = storage error (logged).
fn admit_budgets(
    sql: &SqlStorage,
    cfg: &Config,
    cluster: Cluster,
    day: i64,
    fee: i64,
    rent: i64,
) -> std::result::Result<(), Option<&'static str>> {
    let limit = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let log = |e: worker::Error| {
        console_error!("cluster budget: {e}");
        None
    };
    match daily::try_add(
        sql,
        daily::FEE,
        day,
        daily::FEE,
        fee,
        limit(cfg.cluster_daily_budget(cluster)),
    ) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(Some(daily::FEE)),
        Err(e) => return Err(log(e)),
    }
    if rent == 0 {
        return Ok(());
    }
    let refused = match daily::try_add(
        sql,
        daily::FEE,
        day,
        daily::RENT,
        rent,
        limit(cfg.cluster_daily_rent_budget(cluster)),
    ) {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => Some(daily::RENT),
        Err(e) => log(e),
    };
    if let Err(e) = daily::refund(sql, daily::FEE, day, daily::FEE, fee) {
        console_error!("cluster fee refund after rent refusal failed: {e}");
    }
    Err(refused)
}
