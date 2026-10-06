//! Runtime configuration (spec §4.1). Read per request; values are cheap env lookups.

use worker::Env;

pub const DEVNET_GENESIS_HASH: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
pub const MAX_BODY_BYTES: usize = 4 * 1024;
/// Oversized bodies are read (and discarded) up to this many bytes before the 413.
pub const MAX_DRAIN_BYTES: usize = 64 * 1024;
pub const BALANCE_CACHE_MS: f64 = 30_000.0;
pub const BALANCE_FAILURE_BACKOFF_MS: f64 = 5_000.0;
pub const INFLIGHT_STALE_MARGIN_MS: f64 = 5_000.0;
pub const JWKS_TTL_MS: f64 = 60.0 * 60_000.0;
pub const JWKS_FAILURE_BACKOFF_MS: f64 = 30_000.0;
pub const JWT_LEEWAY_SECS: f64 = 30.0;
pub const JWKS_UNKNOWN_KID_REFETCH_MS: f64 = 5.0 * 60_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalletBinding {
    /// Token `wallets[]` must contain the tx signer (default).
    Wallets,
    /// Only verify the token itself (risk R5 fallback).
    Off,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub rpc_url: String,
    pub rpc_timeout_ms: u64,
    pub allowed_origin: String,
    pub web3auth_client_id: String,
    pub web3auth_jwks_url: String,
    pub web3auth_issuer: String,
    pub wallet_binding: WalletBinding,
    pub cooldown_ms: f64,
    pub reject_cooldown_ms: f64,
    pub reject_cooldown_max_ms: f64,
    pub global_rpm: u32,
    pub min_relayer_balance_lamports: u64,
    pub max_memo_bytes: usize,
}

impl Config {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let var = |name: &str| {
            env.var(name)
                .ok()
                .map(|v| v.to_string())
                .filter(|v| !v.is_empty())
        };
        let required = |name: &str| var(name).ok_or_else(|| format!("missing config {name}"));
        fn num<T: std::str::FromStr>(
            v: Option<String>,
            name: &str,
            default: T,
        ) -> Result<T, String> {
            match v {
                Some(s) => s.parse().map_err(|_| format!("invalid config {name}")),
                None => Ok(default),
            }
        }
        let wallet_binding = match var("WALLET_BINDING").as_deref() {
            None | Some("wallets") => WalletBinding::Wallets,
            Some("off") => WalletBinding::Off,
            Some(_) => return Err("invalid config WALLET_BINDING".into()),
        };
        let allowed_origin = required("ALLOWED_ORIGIN")?;
        if allowed_origin == "*" {
            return Err("ALLOWED_ORIGIN must be an exact origin, not *".into());
        }
        Self {
            rpc_url: var("RPC_URL").unwrap_or_else(|| "https://api.devnet.solana.com".into()),
            rpc_timeout_ms: num(var("RPC_TIMEOUT_MS"), "RPC_TIMEOUT_MS", 15_000)?,
            allowed_origin,
            web3auth_client_id: required("WEB3AUTH_CLIENT_ID")?,
            // Values seen on real Sapphire Devnet social-login tokens (spike 0.2, 2026-10-06);
            // the documented api-auth.web3auth.io issuer/JWKS do not match them.
            web3auth_jwks_url: var("WEB3AUTH_JWKS_URL").unwrap_or_else(|| {
                "https://api.web3auth.io/citadel-service/.well-known/jwks.json".into()
            }),
            web3auth_issuer: var("WEB3AUTH_ISSUER").unwrap_or_else(|| "web3auth.io".into()),
            wallet_binding,
            cooldown_ms: num(var("COOLDOWN_MS"), "COOLDOWN_MS", 10_000.0)?,
            reject_cooldown_ms: num(var("REJECT_COOLDOWN_MS"), "REJECT_COOLDOWN_MS", 3_000.0)?,
            reject_cooldown_max_ms: num(
                var("REJECT_COOLDOWN_MAX_MS"),
                "REJECT_COOLDOWN_MAX_MS",
                60_000.0,
            )?,
            global_rpm: num(var("GLOBAL_RPM"), "GLOBAL_RPM", 60)?,
            min_relayer_balance_lamports: num(
                var("MIN_RELAYER_BALANCE_LAMPORTS"),
                "MIN_RELAYER_BALANCE_LAMPORTS",
                50_000_000,
            )?,
            max_memo_bytes: num(var("MAX_MEMO_BYTES"), "MAX_MEMO_BYTES", 128)?,
        }
        .validated()
    }

    /// Rejects settings that silently disable or invert the cooldowns.
    fn validated(self) -> Result<Self, String> {
        for (name, v) in [
            ("COOLDOWN_MS", self.cooldown_ms),
            ("REJECT_COOLDOWN_MS", self.reject_cooldown_ms),
            ("REJECT_COOLDOWN_MAX_MS", self.reject_cooldown_max_ms),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(format!(
                    "invalid config {name}: must be a non-negative number"
                ));
            }
        }
        // 0 would time out every call; huge values overflow `rpc_timeout_ms + margin`.
        if !(1..=600_000).contains(&self.rpc_timeout_ms) {
            return Err("invalid config RPC_TIMEOUT_MS: must be 1..=600000".into());
        }
        // 0 would answer every request with global_throttle (an accidental kill switch).
        if self.global_rpm == 0 {
            return Err("invalid config GLOBAL_RPM: must be >= 1".into());
        }
        if self.reject_cooldown_max_ms < self.reject_cooldown_ms {
            return Err(
                "invalid config REJECT_COOLDOWN_MAX_MS: must be >= REJECT_COOLDOWN_MS".into(),
            );
        }
        Ok(self)
    }

    /// Longest a healthy request can hold the lock: bounded admit (getBalance timeout +
    /// margin), sendTransaction (timeout), and on rejection the refund call (timeout),
    /// plus a safety margin.
    pub fn inflight_stale_ms(&self) -> f64 {
        3.0 * self.rpc_timeout_ms as f64 + 2_000.0 + INFLIGHT_STALE_MARGIN_MS
    }
}

/// Cooldown after the `streak`-th consecutive RPC rejection (D17): base, 2×base, 4×base … capped.
pub fn reject_backoff_ms(base_ms: f64, streak: u32, max_ms: f64) -> f64 {
    let doublings = streak.saturating_sub(1).min(30) as i32;
    (base_ms * 2f64.powi(doublings)).min(max_ms)
}

#[cfg(test)]
mod tests {
    use super::{Config, WalletBinding, reject_backoff_ms};

    fn cfg(cooldown: f64, reject: f64, reject_max: f64) -> Config {
        Config {
            rpc_url: String::new(),
            rpc_timeout_ms: 15_000,
            allowed_origin: "http://localhost:5173".into(),
            web3auth_client_id: "c".into(),
            web3auth_jwks_url: String::new(),
            web3auth_issuer: String::new(),
            wallet_binding: WalletBinding::Wallets,
            cooldown_ms: cooldown,
            reject_cooldown_ms: reject,
            reject_cooldown_max_ms: reject_max,
            global_rpm: 60,
            min_relayer_balance_lamports: 0,
            max_memo_bytes: 128,
        }
    }

    #[test]
    fn validates_cooldown_settings() {
        assert!(cfg(10_000.0, 3_000.0, 60_000.0).validated().is_ok());
        assert!(cfg(0.0, 0.0, 0.0).validated().is_ok());
        assert!(cfg(-1.0, 3_000.0, 60_000.0).validated().is_err());
        assert!(cfg(10_000.0, -5.0, 60_000.0).validated().is_err());
        assert!(cfg(10_000.0, 3_000.0, 1_000.0).validated().is_err());
        assert!(cfg(f64::NAN, 3_000.0, 60_000.0).validated().is_err());
        assert!(cfg(f64::INFINITY, 3_000.0, 60_000.0).validated().is_err());
        let mut c = cfg(10_000.0, 3_000.0, 60_000.0);
        c.global_rpm = 0;
        assert!(c.validated().is_err());
        for bad in [0, u64::MAX] {
            let mut c = cfg(10_000.0, 3_000.0, 60_000.0);
            c.rpc_timeout_ms = bad;
            assert!(c.validated().is_err());
        }
    }

    #[test]
    fn reject_backoff_doubles_and_caps() {
        let seq: Vec<f64> = (1..=7)
            .map(|s| reject_backoff_ms(3_000.0, s, 60_000.0))
            .collect();
        assert_eq!(
            seq,
            [
                3_000.0, 6_000.0, 12_000.0, 24_000.0, 48_000.0, 60_000.0, 60_000.0
            ]
        );
        assert_eq!(reject_backoff_ms(3_000.0, 0, 60_000.0), 3_000.0);
        assert_eq!(reject_backoff_ms(3_000.0, u32::MAX, 60_000.0), 60_000.0);
    }
}
