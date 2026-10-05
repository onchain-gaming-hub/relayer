//! Runtime configuration (spec §4.1). Read per request; values are cheap env lookups.

use worker::Env;

pub const DEVNET_GENESIS_HASH: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
pub const MAX_BODY_BYTES: usize = 4 * 1024;
pub const BALANCE_CACHE_MS: f64 = 30_000.0;
pub const INFLIGHT_STALE_MARGIN_MS: f64 = 5_000.0;
pub const JWKS_TTL_MS: f64 = 60.0 * 60_000.0;
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
        Ok(Self {
            rpc_url: var("RPC_URL").unwrap_or_else(|| "https://api.devnet.solana.com".into()),
            rpc_timeout_ms: num(var("RPC_TIMEOUT_MS"), "RPC_TIMEOUT_MS", 15_000)?,
            allowed_origin: required("ALLOWED_ORIGIN")?,
            web3auth_client_id: required("WEB3AUTH_CLIENT_ID")?,
            web3auth_jwks_url: var("WEB3AUTH_JWKS_URL")
                .unwrap_or_else(|| "https://api-auth.web3auth.io/jwks".into()),
            web3auth_issuer: required("WEB3AUTH_ISSUER")?,
            wallet_binding,
            cooldown_ms: num(var("COOLDOWN_MS"), "COOLDOWN_MS", 10_000.0)?,
            reject_cooldown_ms: num(var("REJECT_COOLDOWN_MS"), "REJECT_COOLDOWN_MS", 3_000.0)?,
            global_rpm: num(var("GLOBAL_RPM"), "GLOBAL_RPM", 60)?,
            min_relayer_balance_lamports: num(
                var("MIN_RELAYER_BALANCE_LAMPORTS"),
                "MIN_RELAYER_BALANCE_LAMPORTS",
                50_000_000,
            )?,
            max_memo_bytes: num(var("MAX_MEMO_BYTES"), "MAX_MEMO_BYTES", 128)?,
        })
    }

    /// Longest a healthy request can hold the lock: bounded admit (getBalance timeout +
    /// margin) followed by sendTransaction (timeout), plus a safety margin.
    pub fn inflight_stale_ms(&self) -> f64 {
        2.0 * self.rpc_timeout_ms as f64 + 2_000.0 + INFLIGHT_STALE_MARGIN_MS
    }
}
