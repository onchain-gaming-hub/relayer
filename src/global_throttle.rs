//! `GlobalThrottle` — singleton DO (`id_from_name("global")`), spec §4.4 RelayController step 3.
//! Fixed one-minute window counter plus a 30 s cached relayer balance (circuit breaker).
//! Counter and cache live in memory: an eviction resets them, which is acceptable.

use std::cell::{Cell, RefCell};

use futures_util::future::{FutureExt, LocalBoxFuture, Shared};
use serde_json::json;
use worker::*;

use crate::{
    config::{BALANCE_CACHE_MS, Config},
    error::ApiError,
    relayer_keypair_pubkey, rpc,
};

const WINDOW_MS: f64 = 60_000.0;

type BalanceFuture = Shared<LocalBoxFuture<'static, std::result::Result<u64, String>>>;

#[durable_object]
pub struct GlobalThrottle {
    env: Env,
    window_start: Cell<f64>,
    count: Cell<u32>,
    balance: Cell<Option<(u64, f64)>>,
    balance_inflight: RefCell<Option<BalanceFuture>>,
}

impl DurableObject for GlobalThrottle {
    fn new(_state: State, env: Env) -> Self {
        Self {
            env,
            window_start: Cell::new(0.0),
            count: Cell::new(0),
            balance: Cell::new(None),
            balance_inflight: RefCell::new(None),
        }
    }

    async fn fetch(&self, _req: Request) -> Result<Response> {
        let now = Date::now().as_millis() as f64;
        let cfg = match Config::from_env(&self.env) {
            Ok(c) => c,
            Err(e) => return ApiError::internal(e).into_response(),
        };

        // Window check + increment happen synchronously, before the first await.
        if now - self.window_start.get() >= WINDOW_MS {
            self.window_start.set(now);
            self.count.set(0);
        }
        if self.count.get() >= cfg.global_rpm {
            return ApiError::global_throttle(self.window_start.get() + WINDOW_MS - now)
                .into_response();
        }
        self.count.set(self.count.get() + 1);

        let balance = match self.balance.get() {
            Some((lamports, at)) if now - at < BALANCE_CACHE_MS => Ok(lamports),
            _ => self.refresh_balance(&cfg).await,
        };
        match balance {
            Err(e) => ApiError::upstream(e).into_response(),
            Ok(lamports) if lamports < cfg.min_relayer_balance_lamports => {
                ApiError::low_balance().into_response()
            }
            Ok(_) => Response::from_json(&json!({ "admitted": true })),
        }
    }
}

impl GlobalThrottle {
    /// Single-flight: concurrent cache misses share one `getBalance` call.
    async fn refresh_balance(&self, cfg: &Config) -> std::result::Result<u64, String> {
        let fut = {
            let mut slot = self.balance_inflight.borrow_mut();
            match slot.as_ref() {
                Some(f) => f.clone(),
                None => {
                    let pubkey = relayer_keypair_pubkey(&self.env)
                        .map_err(|e| e.reason.unwrap_or(e.message))?;
                    let (url, timeout) = (cfg.rpc_url.clone(), cfg.rpc_timeout_ms);
                    let f = async move { rpc::get_balance(&url, timeout, &pubkey).await }
                        .boxed_local()
                        .shared();
                    *slot = Some(f.clone());
                    f
                }
            }
        };
        let result = fut.await;
        self.balance_inflight.borrow_mut().take();
        if let Ok(lamports) = result {
            self.balance
                .set(Some((lamports, Date::now().as_millis() as f64)));
        }
        result
    }
}
