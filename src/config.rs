//! Worker-level runtime configuration (spec v2 §4.1). Per-project settings live in the D1
//! registry (`registry.rs`). Read per request; values are cheap env lookups.

use worker::Env;

pub const DEVNET_GENESIS_HASH: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
pub const MAINNET_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
pub const MAX_BODY_BYTES: usize = 4 * 1024;
/// Oversized bodies are read (and discarded) up to this many bytes before the 413.
pub const MAX_DRAIN_BYTES: usize = 64 * 1024;
pub const BALANCE_CACHE_MS: f64 = 30_000.0;
pub const BALANCE_FAILURE_BACKOFF_MS: f64 = 5_000.0;
pub const INFLIGHT_STALE_MARGIN_MS: f64 = 5_000.0;
/// UserController → ProjectThrottle admit budget on top of RPC_TIMEOUT_MS (getBalance):
/// the cluster hop (≤ CLUSTER_HOP_TIMEOUT_MS) plus margin (spec v2 §4.4 step 4).
pub const ADMIT_MARGIN_MS: u64 = 4_000;
/// UserController → ProjectThrottle refund budget (covers the forwarded cluster refund).
pub const REFUND_TIMEOUT_MS: u64 = 5_000;
pub const JWKS_TTL_MS: f64 = 60.0 * 60_000.0;
pub const JWKS_FAILURE_BACKOFF_MS: f64 = 30_000.0;
pub const JWT_LEEWAY_SECS: f64 = 30.0;
pub const JWKS_UNKNOWN_KID_REFETCH_MS: f64 = 5.0 * 60_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cluster {
    Devnet,
    Mainnet,
}

impl Cluster {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "devnet" => Some(Self::Devnet),
            "mainnet" => Some(Self::Mainnet),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Devnet => "devnet",
            Self::Mainnet => "mainnet",
        }
    }
    pub fn genesis_hash(self) -> &'static str {
        match self {
            Self::Devnet => DEVNET_GENESIS_HASH,
            Self::Mainnet => MAINNET_GENESIS_HASH,
        }
    }
    fn rpc_url_name(self) -> &'static str {
        match self {
            Self::Devnet => "RPC_URL_DEVNET",
            Self::Mainnet => "RPC_URL_MAINNET",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Web3AuthNetwork {
    SapphireDevnet,
    SapphireMainnet,
}

impl Web3AuthNetwork {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sapphire_devnet" => Some(Self::SapphireDevnet),
            "sapphire_mainnet" => Some(Self::SapphireMainnet),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalletBinding {
    /// Token `wallets[]` must contain the tx signer (default).
    Wallets,
    /// Only verify the token itself (risk R5 fallback).
    Off,
}

/// Issuer and JWKS URL of one Web3Auth network.
#[derive(Clone, Debug)]
pub struct Web3AuthEndpoints {
    pub issuer: String,
    pub jwks_url: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub rpc_timeout_ms: u64,
    pub wallet_binding: WalletBinding,
    pub cooldown_ms: f64,
    pub reject_cooldown_ms: f64,
    pub reject_cooldown_max_ms: f64,
    pub cluster_rpm_devnet: u32,
    pub cluster_rpm_mainnet: u32,
    /// Lamports per UTC day across every project on the cluster (V12 catastrophic stop).
    pub cluster_daily_budget_devnet: u64,
    pub cluster_daily_budget_mainnet: u64,
    pub cluster_hop_timeout_ms: u64,
    pub registry_cache_ms: f64,
    pub registry_max_stale_ms: f64,
    pub web3auth_devnet: Web3AuthEndpoints,
    /// Not verified yet (spec v2 spike 0.2): no default, so mainnet-network projects fail
    /// closed until both WEB3AUTH_MAINNET_ISSUER and WEB3AUTH_MAINNET_JWKS_URL are set.
    pub web3auth_mainnet: Option<Web3AuthEndpoints>,
}

/// A plain var or a secret: both are the same string binding at runtime (worker `env.rs`).
fn string_binding(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|v| v.to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let var = |name: &str| string_binding(env, name);
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
        let web3auth_mainnet = match (
            var("WEB3AUTH_MAINNET_ISSUER"),
            var("WEB3AUTH_MAINNET_JWKS_URL"),
        ) {
            (Some(issuer), Some(jwks_url)) => Some(Web3AuthEndpoints { issuer, jwks_url }),
            (None, None) => None,
            _ => {
                return Err(
                    "set both WEB3AUTH_MAINNET_ISSUER and WEB3AUTH_MAINNET_JWKS_URL, or neither"
                        .into(),
                );
            }
        };
        Self {
            rpc_timeout_ms: num(var("RPC_TIMEOUT_MS"), "RPC_TIMEOUT_MS", 15_000)?,
            wallet_binding,
            cooldown_ms: num(var("COOLDOWN_MS"), "COOLDOWN_MS", 10_000.0)?,
            reject_cooldown_ms: num(var("REJECT_COOLDOWN_MS"), "REJECT_COOLDOWN_MS", 3_000.0)?,
            reject_cooldown_max_ms: num(
                var("REJECT_COOLDOWN_MAX_MS"),
                "REJECT_COOLDOWN_MAX_MS",
                60_000.0,
            )?,
            cluster_rpm_devnet: num(var("CLUSTER_RPM_DEVNET"), "CLUSTER_RPM_DEVNET", 300)?,
            cluster_rpm_mainnet: num(var("CLUSTER_RPM_MAINNET"), "CLUSTER_RPM_MAINNET", 300)?,
            // Spec v2 §5: 10 SOL on devnet, 0.2 SOL on mainnet.
            cluster_daily_budget_devnet: num(
                var("CLUSTER_DAILY_BUDGET_DEVNET"),
                "CLUSTER_DAILY_BUDGET_DEVNET",
                10_000_000_000,
            )?,
            cluster_daily_budget_mainnet: num(
                var("CLUSTER_DAILY_BUDGET_MAINNET"),
                "CLUSTER_DAILY_BUDGET_MAINNET",
                200_000_000,
            )?,
            cluster_hop_timeout_ms: num(
                var("CLUSTER_HOP_TIMEOUT_MS"),
                "CLUSTER_HOP_TIMEOUT_MS",
                2_000,
            )?,
            registry_cache_ms: num(var("REGISTRY_CACHE_MS"), "REGISTRY_CACHE_MS", 60_000.0)?,
            registry_max_stale_ms: num(
                var("REGISTRY_MAX_STALE_MS"),
                "REGISTRY_MAX_STALE_MS",
                600_000.0,
            )?,
            // Values seen on real Sapphire Devnet social-login tokens (spike 0.2, 2026-10-06);
            // the documented api-auth.web3auth.io issuer/JWKS do not match them.
            web3auth_devnet: Web3AuthEndpoints {
                issuer: var("WEB3AUTH_DEVNET_ISSUER").unwrap_or_else(|| "web3auth.io".into()),
                jwks_url: var("WEB3AUTH_DEVNET_JWKS_URL").unwrap_or_else(|| {
                    "https://api.web3auth.io/citadel-service/.well-known/jwks.json".into()
                }),
            },
            web3auth_mainnet,
        }
        .validated()
    }

    /// Rejects settings that silently disable or invert the cooldowns and limits.
    fn validated(self) -> Result<Self, String> {
        for (name, v) in [
            ("COOLDOWN_MS", self.cooldown_ms),
            ("REJECT_COOLDOWN_MS", self.reject_cooldown_ms),
            ("REJECT_COOLDOWN_MAX_MS", self.reject_cooldown_max_ms),
            ("REGISTRY_CACHE_MS", self.registry_cache_ms),
            ("REGISTRY_MAX_STALE_MS", self.registry_max_stale_ms),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(format!(
                    "invalid config {name}: must be a non-negative number"
                ));
            }
        }
        // 0 would time out every call; huge values overflow `timeout + margin`.
        for (name, v) in [
            ("RPC_TIMEOUT_MS", self.rpc_timeout_ms),
            ("CLUSTER_HOP_TIMEOUT_MS", self.cluster_hop_timeout_ms),
        ] {
            if !(1..=600_000).contains(&v) {
                return Err(format!("invalid config {name}: must be 1..=600000"));
            }
        }
        // 0 would answer every request with cluster_throttle (an accidental kill switch).
        if self.cluster_rpm_devnet == 0 || self.cluster_rpm_mainnet == 0 {
            return Err("invalid config CLUSTER_RPM_*: must be >= 1".into());
        }
        if self.reject_cooldown_max_ms < self.reject_cooldown_ms {
            return Err(
                "invalid config REJECT_COOLDOWN_MAX_MS: must be >= REJECT_COOLDOWN_MS".into(),
            );
        }
        if self.registry_max_stale_ms < self.registry_cache_ms {
            return Err(
                "invalid config REGISTRY_MAX_STALE_MS: must be >= REGISTRY_CACHE_MS".into(),
            );
        }
        Ok(self)
    }

    pub fn cluster_rpm(&self, cluster: Cluster) -> u32 {
        match cluster {
            Cluster::Devnet => self.cluster_rpm_devnet,
            Cluster::Mainnet => self.cluster_rpm_mainnet,
        }
    }

    pub fn cluster_daily_budget(&self, cluster: Cluster) -> u64 {
        match cluster {
            Cluster::Devnet => self.cluster_daily_budget_devnet,
            Cluster::Mainnet => self.cluster_daily_budget_mainnet,
        }
    }

    pub fn web3auth(&self, network: Web3AuthNetwork) -> Result<&Web3AuthEndpoints, String> {
        match network {
            Web3AuthNetwork::SapphireDevnet => Ok(&self.web3auth_devnet),
            Web3AuthNetwork::SapphireMainnet => self
                .web3auth_mainnet
                .as_ref()
                .ok_or_else(|| "sapphire_mainnet issuer/JWKS not configured".into()),
        }
    }

    /// Longest a healthy request can hold the wallet lock (spec v2 §4.4): admit
    /// (RPC_TIMEOUT_MS + 4 s) + sendTransaction (RPC_TIMEOUT_MS) + refund (5 s) + 5 s margin.
    pub fn inflight_stale_ms(&self) -> f64 {
        2.0 * self.rpc_timeout_ms as f64
            + ADMIT_MARGIN_MS as f64
            + REFUND_TIMEOUT_MS as f64
            + INFLIGHT_STALE_MARGIN_MS
    }
}

/// RPC URL of a cluster (a secret: provider URLs carry API keys). Public RPCs block Workers.
pub fn rpc_url(env: &Env, cluster: Cluster) -> Result<String, String> {
    string_binding(env, cluster.rpc_url_name())
        .ok_or_else(|| format!("missing {}", cluster.rpc_url_name()))
}

/// Cooldown after the `streak`-th consecutive RPC rejection (D17): base, 2×base, 4×base … capped.
pub fn reject_backoff_ms(base_ms: f64, streak: u32, max_ms: f64) -> f64 {
    let doublings = streak.saturating_sub(1).min(30) as i32;
    (base_ms * 2f64.powi(doublings)).min(max_ms)
}

#[cfg(test)]
mod tests {
    use super::{Config, WalletBinding, Web3AuthEndpoints, Web3AuthNetwork, reject_backoff_ms};

    fn cfg(cooldown: f64, reject: f64, reject_max: f64) -> Config {
        Config {
            rpc_timeout_ms: 15_000,
            wallet_binding: WalletBinding::Wallets,
            cooldown_ms: cooldown,
            reject_cooldown_ms: reject,
            reject_cooldown_max_ms: reject_max,
            cluster_rpm_devnet: 300,
            cluster_rpm_mainnet: 300,
            cluster_daily_budget_devnet: 10_000_000_000,
            cluster_daily_budget_mainnet: 200_000_000,
            cluster_hop_timeout_ms: 2_000,
            registry_cache_ms: 60_000.0,
            registry_max_stale_ms: 600_000.0,
            web3auth_devnet: Web3AuthEndpoints {
                issuer: "web3auth.io".into(),
                jwks_url: "https://jwks.test".into(),
            },
            web3auth_mainnet: None,
        }
    }

    #[test]
    fn validates_settings() {
        assert!(cfg(10_000.0, 3_000.0, 60_000.0).validated().is_ok());
        assert!(cfg(0.0, 0.0, 0.0).validated().is_ok());
        assert!(cfg(-1.0, 3_000.0, 60_000.0).validated().is_err());
        assert!(cfg(10_000.0, -5.0, 60_000.0).validated().is_err());
        assert!(cfg(10_000.0, 3_000.0, 1_000.0).validated().is_err());
        assert!(cfg(f64::NAN, 3_000.0, 60_000.0).validated().is_err());
        assert!(cfg(f64::INFINITY, 3_000.0, 60_000.0).validated().is_err());
        let mut c = cfg(10_000.0, 3_000.0, 60_000.0);
        c.cluster_rpm_mainnet = 0;
        assert!(c.validated().is_err());
        for bad in [0, u64::MAX] {
            let mut c = cfg(10_000.0, 3_000.0, 60_000.0);
            c.rpc_timeout_ms = bad;
            assert!(c.validated().is_err());
            let mut c = cfg(10_000.0, 3_000.0, 60_000.0);
            c.cluster_hop_timeout_ms = bad;
            assert!(c.validated().is_err());
        }
        let mut c = cfg(10_000.0, 3_000.0, 60_000.0);
        c.registry_max_stale_ms = 1_000.0;
        assert!(c.validated().is_err());
    }

    #[test]
    fn inflight_stale_matches_spec_formula() {
        // 2 × RPC_TIMEOUT_MS + 14 s
        assert_eq!(cfg(0.0, 0.0, 0.0).inflight_stale_ms(), 44_000.0);
    }

    #[test]
    fn mainnet_web3auth_fails_closed_until_configured() {
        let c = cfg(0.0, 0.0, 0.0);
        assert!(c.web3auth(Web3AuthNetwork::SapphireDevnet).is_ok());
        assert!(c.web3auth(Web3AuthNetwork::SapphireMainnet).is_err());
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
