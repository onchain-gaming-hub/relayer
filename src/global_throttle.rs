//! `GlobalThrottle` — singleton DO (`id_from_name("global")`), spec §4.4 RelayController step 3.
//! Fixed one-minute window counter plus a 30 s cached relayer balance (circuit breaker).
//! Counter and cache live in memory: an eviction resets them, which is acceptable.

use std::cell::{Cell, RefCell};

use futures_util::future::{FutureExt, LocalBoxFuture, Shared};
use serde_json::json;
use worker::*;

use crate::{
    config::{BALANCE_CACHE_MS, BALANCE_FAILURE_BACKOFF_MS, Config},
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
    /// Last getBalance failure (message, at): answered from here for BALANCE_FAILURE_BACKOFF_MS
    /// so a degraded RPC is not hit again by every request (R6).
    balance_failure: RefCell<Option<(String, f64)>>,
}

impl DurableObject for GlobalThrottle {
    fn new(_state: State, env: Env) -> Self {
        Self {
            env,
            window_start: Cell::new(0.0),
            count: Cell::new(0),
            balance: Cell::new(None),
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
        // Misconfigured keypair is a 500 config error, never a cached "balance failure".
        let pubkey = match relayer_keypair_pubkey(&self.env) {
            Ok(p) => p,
            Err(e) => return e.into_response(),
        };

        // Roll the window first so a refund for an expired window reports refunded:false.
        // (Synchronous, before the first await.)
        if now - self.window_start.get() >= WINDOW_MS {
            self.window_start.set(now);
            self.count.set(0);
        }

        if req.path() == "/refund" {
            // A request the RPC rejected sent nothing on-chain: give its slot back (D17),
            // but only within the window it was counted in.
            let window = req
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("window").and_then(|w| w.as_f64()));
            let refunded = window.is_some_and(|w| self.release(w));
            return Response::from_json(&json!({ "refunded": refunded }));
        }

        // Capacity check + increment happen synchronously, before the first await.
        if self.count.get() >= cfg.global_rpm {
            return ApiError::global_throttle(self.window_start.get() + WINDOW_MS - now)
                .into_response();
        }
        self.count.set(self.count.get() + 1);
        // Captured synchronously: the window may roll over while we await the balance below,
        // and a later refund must target the window this request was actually counted in.
        let window = self.window_start.get();

        let recent_failure = self
            .balance_failure
            .borrow()
            .as_ref()
            .filter(|(_, at)| now - at < BALANCE_FAILURE_BACKOFF_MS)
            .map(|(msg, _)| msg.clone());
        let balance = match (self.balance.get(), recent_failure) {
            (Some((lamports, at)), _) if now - at < BALANCE_CACHE_MS => Ok(lamports),
            (_, Some(msg)) => Err(format!("{msg} (cached)")),
            _ => self.refresh_balance(&cfg, pubkey).await,
        };
        match balance {
            Err(e) => {
                self.release(window);
                ApiError::upstream(e).into_response()
            }
            Ok(lamports) if lamports < cfg.min_relayer_balance_lamports => {
                self.release(window);
                ApiError::low_balance().into_response()
            }
            Ok(_) => Response::from_json(&json!({ "admitted": true, "window": window })),
        }
    }
}

impl GlobalThrottle {
    /// Gives back one slot, but only in the window it was taken from and never below zero.
    fn release(&self, window: f64) -> bool {
        let ok = window == self.window_start.get() && self.count.get() > 0;
        if ok {
            self.count.set(self.count.get() - 1);
        }
        ok
    }

    /// Single-flight: concurrent cache misses share one `getBalance` call.
    async fn refresh_balance(
        &self,
        cfg: &Config,
        pubkey: String,
    ) -> std::result::Result<u64, String> {
        let fut = {
            let mut slot = self.balance_inflight.borrow_mut();
            match slot.as_ref() {
                Some(f) => f.clone(),
                None => {
                    let (url, timeout) = (cfg.rpc_url.clone(), cfg.rpc_timeout_ms);
                    let f = async move { rpc::get_balance(&url, timeout, &pubkey).await }
                        .boxed_local()
                        .shared();
                    *slot = Some(f.clone());
                    f
                }
            }
        };
        let result = fut.clone().await;
        // Clear the slot only if it still holds *this* future (a newer one may have replaced it).
        let mut slot = self.balance_inflight.borrow_mut();
        if slot.as_ref().is_some_and(|f| f.ptr_eq(&fut)) {
            slot.take();
        }
        drop(slot);
        let at = Date::now().as_millis() as f64;
        match &result {
            Ok(lamports) => {
                self.balance.set(Some((*lamports, at)));
                self.balance_failure.borrow_mut().take();
            }
            Err(e) => *self.balance_failure.borrow_mut() = Some((e.clone(), at)),
        }
        result
    }
}
