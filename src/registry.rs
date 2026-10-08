//! Project registry (spec v2 V4, §4.5): the whole `projects` + `settings` tables are loaded
//! as one snapshot per isolate and looked up in memory, so D1 load never depends on the
//! `aud` / `?project=` values a client sends.
//!
//! Refresh is stale-while-revalidate. Workers forbid awaiting I/O started by another request,
//! so there is no shared promise: the first request that sees an expired snapshot stamps
//! `REFRESHING_SINCE` and queries; the others keep serving the old snapshot. A failed refresh
//! keeps the stamp, which doubles as a retry backoff. A cold isolate queries once per
//! concurrent request (bounded by concurrency, like JWKS).

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
};

use serde::Deserialize;
use serde_json::Value;
use solana_pubkey::Pubkey;
use worker::{Env, console_log, console_warn};

use crate::{
    config::{Cluster, Config, Web3AuthNetwork},
    error::ApiError,
    policy::{MAX_MEMO_BYTES_CAP, MemoRule},
    program_policy::{self, ProgramPolicy},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    Paused,
    Retired,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub aud: String,
    pub slug: String,
    pub status: Status,
    pub cluster: Cluster,
    pub network: Web3AuthNetwork,
    pub allowed_origins: Vec<String>,
    pub fee_payer: Pubkey,
    /// Memo action (v2 shape). `None` = memo txs are not sponsored.
    pub memo: Option<MemoRule>,
    /// Program actions (spec v3). `None` = program txs are not sponsored.
    pub program: Option<ProgramPolicy>,
    pub user_daily_quota: u32,
    pub project_rpm: u32,
    pub project_daily_budget_lamports: u64,
    /// Rent the fee payer may front per UTC day (v3 W9; enforced from Phase 3). 0 = none.
    #[allow(dead_code)]
    pub project_daily_rent_budget_lamports: u64,
    /// Per-project timing overrides (v3 W10; enforced from Phase 3). `None` = Worker default.
    #[allow(dead_code)]
    pub timing: Timing,
    pub min_balance_lamports: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Timing {
    pub cooldown_ms: Option<f64>,
    pub reject_cooldown_ms: Option<f64>,
    pub reject_cooldown_max_ms: Option<f64>,
    pub retry_cooldown_ms: Option<f64>,
}

/// One `projects` row as D1 returns it (numbers arrive as JS numbers).
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectRow {
    pub aud: String,
    pub slug: String,
    pub status: String,
    pub cluster: String,
    pub web3auth_network: String,
    pub allowed_origins: String,
    pub fee_payer_pubkey: String,
    pub policy: String,
    pub user_daily_quota: f64,
    pub project_rpm: f64,
    pub project_daily_budget_lamports: f64,
    pub min_balance_lamports: f64,
    pub project_daily_rent_budget_lamports: f64,
    #[serde(default)]
    pub cooldown_ms: Option<f64>,
    #[serde(default)]
    pub reject_cooldown_ms: Option<f64>,
    #[serde(default)]
    pub reject_cooldown_max_ms: Option<f64>,
    #[serde(default)]
    pub retry_cooldown_ms: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SettingRow {
    key: String,
    value: String,
}

pub fn valid_slug(slug: &str) -> bool {
    (1..=32).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// An exact `scheme://host[:port]` origin: no wildcard, path, or trailing slash.
fn valid_origin(o: &str) -> bool {
    let Some((scheme, rest)) = o.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https")
        && !rest.is_empty()
        && !rest.contains(['/', '*', '?', '#', ' '])
}

fn whole<T: TryFrom<u64>>(v: f64, name: &str, min: u64) -> Result<T, String> {
    if !v.is_finite() || v.fract() != 0.0 || v < min as f64 || v > 9_007_199_254_740_991.0 {
        return Err(format!("{name}: must be an integer >= {min}"));
    }
    T::try_from(v as u64).map_err(|_| format!("{name}: out of range"))
}

fn parse_memo(a: &Value) -> Result<MemoRule, String> {
    let max_bytes = a
        .get("max_bytes")
        .and_then(Value::as_u64)
        .ok_or("policy: memo.max_bytes required")? as usize;
    if !(1..=MAX_MEMO_BYTES_CAP).contains(&max_bytes) {
        return Err(format!(
            "policy: memo.max_bytes must be 1..={MAX_MEMO_BYTES_CAP}"
        ));
    }
    let prefix = match a.get("prefix") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) if !p.is_empty() && p.len() <= max_bytes => Some(p.clone()),
        Some(_) => return Err("policy: memo.prefix must be a short non-empty string".into()),
    };
    Ok(MemoRule { max_bytes, prefix })
}

/// Parses `projects.policy` (v2 V11 + v3 §3). Unknown action types fail closed.
fn parse_policy(json: &str) -> Result<(Option<MemoRule>, Option<ProgramPolicy>), String> {
    let v: Value = serde_json::from_str(json).map_err(|_| "policy: invalid JSON".to_string())?;
    let actions = v
        .get("actions")
        .and_then(Value::as_array)
        .ok_or("policy: missing actions")?;
    let mut memo = None;
    let mut programs: Vec<program_policy::ProgramRule> = Vec::new();
    for a in actions {
        match a.get("type").and_then(Value::as_str) {
            Some("memo") if memo.is_none() => memo = Some(parse_memo(a)?),
            Some("memo") => return Err("policy: duplicate memo action".into()),
            Some("program") => {
                let p = program_policy::parse_program(a).map_err(|e| format!("policy: {e}"))?;
                if programs.iter().any(|q| q.program_id == p.program_id) {
                    return Err(format!("policy: program {} registered twice", p.program_id));
                }
                programs.push(p);
            }
            other => return Err(format!("policy: unknown action type {other:?}")),
        }
    }
    let program = if programs.is_empty() {
        if v.get("limits").is_some_and(|l| !l.is_null()) {
            return Err("policy: limits without a program action".into());
        }
        None
    } else {
        Some(ProgramPolicy {
            programs,
            limits: program_policy::parse_limits(v.get("limits"))
                .map_err(|e| format!("policy: {e}"))?,
        })
    };
    if memo.is_none() && program.is_none() {
        return Err("policy: no actions".into());
    }
    Ok((memo, program))
}

fn opt_ms(v: Option<f64>, name: &str) -> Result<Option<f64>, String> {
    match v {
        None => Ok(None),
        Some(x) if x.is_finite() && (0.0..=3_600_000.0).contains(&x) => Ok(Some(x)),
        Some(_) => Err(format!("{name}: must be 0..=3600000 ms")),
    }
}

pub fn parse_project(row: &ProjectRow) -> Result<Project, String> {
    if !valid_slug(&row.slug) {
        return Err("slug must match [A-Z0-9_]{1,32}".into());
    }
    let status = match row.status.as_str() {
        "active" => Status::Active,
        "paused" => Status::Paused,
        "retired" => Status::Retired,
        _ => return Err("invalid status".into()),
    };
    let cluster = Cluster::parse(&row.cluster).ok_or("invalid cluster")?;
    let network =
        Web3AuthNetwork::parse(&row.web3auth_network).ok_or("invalid web3auth_network")?;
    // A mismatched pair would verify devnet logins for a mainnet wallet (or vice versa).
    let expected = match cluster {
        Cluster::Devnet => Web3AuthNetwork::SapphireDevnet,
        Cluster::Mainnet => Web3AuthNetwork::SapphireMainnet,
    };
    if network != expected {
        return Err("web3auth_network does not match cluster".into());
    }
    let allowed_origins: Vec<String> = serde_json::from_str(&row.allowed_origins)
        .map_err(|_| "allowed_origins: must be a JSON array of strings")?;
    if allowed_origins.is_empty() || !allowed_origins.iter().all(|o| valid_origin(o)) {
        return Err("allowed_origins: exact http(s) origins only, no '*'".into());
    }
    let fee_payer: Pubkey = row
        .fee_payer_pubkey
        .parse()
        .map_err(|_| "fee_payer_pubkey: invalid")?;
    let (memo, program) = parse_policy(&row.policy)?;
    let min_balance_lamports: u64 = whole(row.min_balance_lamports, "min_balance_lamports", 0)?;
    // A payer-slot tx must never pass the breaker without the lamports to pay its rent (§3).
    if let Some(p) = &program {
        let max_rent = p.max_rent_per_ix();
        if max_rent > 0 {
            let need = (p.limits.max_instructions as u64)
                .saturating_mul(max_rent)
                .saturating_add(1_000_000);
            if min_balance_lamports < need {
                return Err(format!(
                    "min_balance_lamports must be >= {need} for payer slots"
                ));
            }
        }
    }
    let timing = Timing {
        cooldown_ms: opt_ms(row.cooldown_ms, "cooldown_ms")?,
        reject_cooldown_ms: opt_ms(row.reject_cooldown_ms, "reject_cooldown_ms")?,
        reject_cooldown_max_ms: opt_ms(row.reject_cooldown_max_ms, "reject_cooldown_max_ms")?,
        retry_cooldown_ms: opt_ms(row.retry_cooldown_ms, "retry_cooldown_ms")?,
    };
    Ok(Project {
        aud: row.aud.clone(),
        slug: row.slug.clone(),
        status,
        cluster,
        network,
        allowed_origins,
        fee_payer,
        memo,
        program,
        project_daily_rent_budget_lamports: whole(
            row.project_daily_rent_budget_lamports,
            "project_daily_rent_budget_lamports",
            0,
        )?,
        timing,
        user_daily_quota: whole(row.user_daily_quota, "user_daily_quota", 1)?,
        project_rpm: whole(row.project_rpm, "project_rpm", 1)?,
        project_daily_budget_lamports: whole(
            row.project_daily_budget_lamports,
            "project_daily_budget_lamports",
            0,
        )?,
        min_balance_lamports,
    })
}

pub struct Snapshot {
    /// A row that fails to parse stays as `Err` so only that project answers 500.
    projects: HashMap<String, Result<Project, String>>,
    /// Union of `allowed_origins` of active projects (CORS preflight, V9).
    pub origins: HashSet<String>,
    pub mainnet_enabled: bool,
    pub loaded_at: f64,
}

impl Snapshot {
    /// Builds from raw rows. A row that does not even deserialize (e.g. text typed into a
    /// number column, which SQLite accepts) becomes an `Err` for its `aud` only, so one bad
    /// row never takes the whole registry down.
    pub fn build(rows: Vec<Value>, settings: &[(String, String)], loaded_at: f64) -> Self {
        let mut projects = HashMap::new();
        let mut origins = HashSet::new();
        for raw in rows {
            let Some(aud) = raw.get("aud").and_then(Value::as_str).map(str::to_string) else {
                console_warn_native("registry: skipping a projects row without a text aud");
                continue;
            };
            let parsed = serde_json::from_value::<ProjectRow>(raw)
                .map_err(|e| format!("row does not match the schema: {e}"))
                .and_then(|row| parse_project(&row));
            if let Ok(p) = &parsed
                && p.status == Status::Active
            {
                origins.extend(p.allowed_origins.iter().cloned());
            }
            projects.insert(aud, parsed);
        }
        let mainnet_enabled = settings
            .iter()
            .any(|(k, v)| k == "mainnet_enabled" && v == "true");
        Self {
            projects,
            origins,
            mainnet_enabled,
            loaded_at,
        }
    }

    pub fn project(&self, aud: &str) -> Option<&Result<Project, String>> {
        self.projects.get(aud)
    }

    pub fn age(&self, now: f64) -> f64 {
        now - self.loaded_at
    }
}

/// `console_warn!` only exists on wasm; unit tests run natively.
fn console_warn_native(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    console_warn!("{msg}");
    #[cfg(not(target_arch = "wasm32"))]
    let _ = msg;
}

thread_local! {
    static SNAPSHOT: RefCell<Option<Rc<Snapshot>>> = const { RefCell::new(None) };
    static REFRESHING_SINCE: Cell<f64> = const { Cell::new(f64::NEG_INFINITY) };
}

/// The registry snapshot for this request (V4).
pub async fn snapshot(env: &Env, cfg: &Config, now: f64) -> Result<Rc<Snapshot>, ApiError> {
    let cached = SNAPSHOT.with(|s| s.borrow().clone());
    let backoff = cfg.cluster_hop_timeout_ms as f64 * 5.0;
    // Cold isolate whose last load failed: answer 503 without hitting D1 again for a while.
    if cached.is_none() && now - REFRESHING_SINCE.with(Cell::get) < backoff {
        return Err(ApiError::upstream_relayed());
    }
    if let Some(s) = &cached {
        if s.age(now) < cfg.registry_cache_ms {
            return Ok(s.clone());
        }
        // Someone else is refreshing (or a recent refresh failed): serve stale meanwhile.
        if now - REFRESHING_SINCE.with(Cell::get) < backoff {
            return Ok(s.clone());
        }
    }
    // Stamped for cold loads too: a failure then backs off instead of querying per request.
    REFRESHING_SINCE.with(|r| r.set(now));
    match load(env, now).await {
        Ok(fresh) => {
            let fresh = Rc::new(fresh);
            SNAPSHOT.with(|s| *s.borrow_mut() = Some(fresh.clone()));
            REFRESHING_SINCE.with(|r| r.set(f64::NEG_INFINITY));
            Ok(fresh)
        }
        Err(e) => match cached {
            // Mainnet projects refuse a snapshot older than REGISTRY_MAX_STALE_MS (lib.rs).
            Some(s) => {
                console_warn!("registry refresh failed, serving stale snapshot: {e}");
                Ok(s)
            }
            None => Err(ApiError::upstream(format!("registry: {e}"))),
        },
    }
}

async fn load(env: &Env, now: f64) -> Result<Snapshot, String> {
    let db = env.d1("DB").map_err(|e| e.to_string())?;
    // Raw values, not `results::<ProjectRow>()`: that fails the whole query on one bad row.
    let rows: Vec<Value> = db
        .prepare(
            "SELECT aud, slug, status, cluster, web3auth_network, allowed_origins, fee_payer_pubkey, \
             policy, user_daily_quota, project_rpm, project_daily_budget_lamports, min_balance_lamports, \
             project_daily_rent_budget_lamports, cooldown_ms, reject_cooldown_ms, reject_cooldown_max_ms, \
             retry_cooldown_ms FROM projects",
        )
        .all()
        .await
        .map_err(|e| format!("projects: {e}"))?
        .results()
        .map_err(|e| format!("projects rows: {e}"))?;
    let settings: Vec<SettingRow> = db
        .prepare("SELECT key, value FROM settings")
        .all()
        .await
        .map_err(|e| format!("settings: {e}"))?
        .results()
        .map_err(|e| format!("settings rows: {e}"))?;
    let settings: Vec<(String, String)> = settings.into_iter().map(|r| (r.key, r.value)).collect();
    let snap = Snapshot::build(rows, &settings, now);
    // One line per D1 load: e2e counts these to prove client input cannot trigger queries.
    console_log!("registry: loaded {} projects", snap.projects.len());
    for (aud, p) in &snap.projects {
        if let Err(e) = p {
            console_warn!("registry: project {aud} is misconfigured: {e}");
        }
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_value(r: ProjectRow) -> Value {
        serde_json::json!({
            "aud": r.aud, "slug": r.slug, "status": r.status, "cluster": r.cluster,
            "web3auth_network": r.web3auth_network, "allowed_origins": r.allowed_origins,
            "fee_payer_pubkey": r.fee_payer_pubkey, "policy": r.policy,
            "user_daily_quota": r.user_daily_quota, "project_rpm": r.project_rpm,
            "project_daily_budget_lamports": r.project_daily_budget_lamports,
            "min_balance_lamports": r.min_balance_lamports,
            "project_daily_rent_budget_lamports": r.project_daily_rent_budget_lamports,
            "cooldown_ms": r.cooldown_ms,
        })
    }

    #[test]
    fn one_malformed_row_only_breaks_its_own_project() {
        let mut bad = to_value(row());
        bad["aud"] = "client-bad".into();
        bad["user_daily_quota"] = "2O".into(); // text in a number column (SQLite accepts it)
        let mut good_b = row();
        good_b.aud = "client-b".into();
        let no_aud = serde_json::json!({ "aud": 42 });
        let s = Snapshot::build(
            vec![bad, to_value(row()), to_value(good_b), no_aud],
            &[],
            0.0,
        );
        assert!(s.project("client-bad").unwrap().is_err());
        assert!(s.project("client-a").unwrap().is_ok());
        assert!(s.project("client-b").unwrap().is_ok());
        assert_eq!(s.projects.len(), 3);
    }

    pub fn row() -> ProjectRow {
        ProjectRow {
            aud: "client-a".into(),
            slug: "POC_1".into(),
            status: "active".into(),
            cluster: "devnet".into(),
            web3auth_network: "sapphire_devnet".into(),
            allowed_origins: r#"["http://localhost:5173"]"#.into(),
            fee_payer_pubkey: "7uA1QxYTnzXzNBoVaE4gbLvXzxtPLjzvWAQqvmcMU5Cc".into(),
            policy: r#"{"actions":[{"type":"memo","max_bytes":128}]}"#.into(),
            user_daily_quota: 20.0,
            project_rpm: 60.0,
            project_daily_budget_lamports: 20_000_000.0,
            min_balance_lamports: 5_000_000.0,
            project_daily_rent_budget_lamports: 0.0,
            cooldown_ms: None,
            reject_cooldown_ms: None,
            reject_cooldown_max_ms: None,
            retry_cooldown_ms: None,
        }
    }

    fn program_policy_json(extra: &str) -> String {
        format!(
            r#"{{"actions":[{{"type":"program","program_id":"Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW","deploy_slot":1,
               "instructions":[{{"name":"create","discriminator":"181ec828051c0777","payer_index":1,"max_rent_lamports":1100000,
               "min_accounts":4,"max_accounts":4,"max_data_len":8}}]}}]{extra}}}"#
        )
    }

    #[test]
    fn program_policy_parses_with_memo_optional_and_min_balance_rule() {
        let mut r = row();
        r.policy = program_policy_json("");
        // 4 × 1_100_000 + 1_000_000 = 5_400_000 > 5_000_000
        assert!(
            parse_project(&r)
                .unwrap_err()
                .contains("min_balance_lamports")
        );
        r.min_balance_lamports = 5_400_000.0;
        let p = parse_project(&r).unwrap();
        assert!(p.memo.is_none());
        assert_eq!(p.program.as_ref().unwrap().programs.len(), 1);
        r.policy = program_policy_json(r#","limits":{"max_instructions":2}"#);
        r.min_balance_lamports = 3_200_000.0; // 2 × 1_100_000 + 1_000_000
        assert!(parse_project(&r).is_ok());
        // Same program twice, limits without programs.
        r.policy = r#"{"actions":[{"type":"memo","max_bytes":8}],"limits":{"max_cu":1}}"#.into();
        assert!(parse_project(&r).is_err());
        let twice = program_policy_json("").replace(r#"}]}]"#, r#"}]},{"type":"program","program_id":"Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW","deploy_slot":1,"instructions":[{"name":"x","discriminator":"01","min_accounts":0,"max_accounts":1,"max_data_len":8}]}]"#);
        r.policy = twice;
        r.min_balance_lamports = 9_000_000.0;
        assert!(parse_project(&r).unwrap_err().contains("twice"));
    }

    #[test]
    fn timing_overrides_are_bounded() {
        let mut r = row();
        r.cooldown_ms = Some(500.0);
        assert_eq!(parse_project(&r).unwrap().timing.cooldown_ms, Some(500.0));
        r.cooldown_ms = Some(-1.0);
        assert!(parse_project(&r).is_err());
        r.cooldown_ms = None;
        r.retry_cooldown_ms = Some(f64::NAN);
        assert!(parse_project(&r).is_err());
    }

    #[test]
    fn parses_a_good_row() {
        let p = parse_project(&row()).unwrap();
        assert_eq!(p.cluster, Cluster::Devnet);
        assert_eq!(p.memo.as_ref().unwrap().max_bytes, 128);
        assert!(p.program.is_none());
        assert_eq!(p.project_daily_budget_lamports, 20_000_000);
    }

    #[test]
    fn rejects_bad_slugs_like_the_db_check() {
        for bad in ["", "poc", "A-bad", "A B", "ÀBC", &"A".repeat(33)] {
            assert!(!valid_slug(bad), "{bad}");
            let mut r = row();
            r.slug = bad.to_string();
            assert!(parse_project(&r).is_err(), "{bad}");
        }
        for good in ["POC_1", "1ABC", &"A".repeat(32)] {
            assert!(valid_slug(good), "{good}");
        }
    }

    #[test]
    fn rejects_wildcard_or_malformed_origins() {
        for bad in [
            r#"["*"]"#,
            r#"[]"#,
            r#"["http://localhost:5173/"]"#,
            r#"["localhost:5173"]"#,
            r#"["https://*.example.com"]"#,
            r#""http://localhost:5173""#,
            "not json",
        ] {
            let mut r = row();
            r.allowed_origins = bad.into();
            assert!(parse_project(&r).is_err(), "{bad}");
        }
    }

    #[test]
    fn policy_unknown_type_or_bad_limits_fail_closed() {
        for bad in [
            r#"{"actions":[{"type":"spl_transfer"}]}"#,
            r#"{"actions":[{"type":"memo","max_bytes":0}]}"#,
            r#"{"actions":[{"type":"memo","max_bytes":129}]}"#,
            r#"{"actions":[{"type":"memo"}]}"#,
            r#"{"actions":[]}"#,
            r#"{"actions":[{"type":"memo","max_bytes":10},{"type":"memo","max_bytes":10}]}"#,
            r#"{"actions":[{"type":"memo","max_bytes":4,"prefix":"toolong"}]}"#,
            r#"{}"#,
        ] {
            assert!(parse_policy(bad).is_err(), "{bad}");
        }
        let (rule, program) =
            parse_policy(r#"{"actions":[{"type":"memo","max_bytes":64,"prefix":"app:"}]}"#)
                .unwrap();
        assert_eq!(rule.unwrap().prefix.as_deref(), Some("app:"));
        assert!(program.is_none());
    }

    #[test]
    fn rejects_cluster_network_mismatch_and_bad_numbers() {
        let mut r = row();
        r.web3auth_network = "sapphire_mainnet".into();
        assert!(parse_project(&r).is_err());
        let mut r = row();
        r.user_daily_quota = 0.0;
        assert!(parse_project(&r).is_err());
        let mut r = row();
        r.project_rpm = 1.5;
        assert!(parse_project(&r).is_err());
        let mut r = row();
        r.min_balance_lamports = -1.0;
        assert!(parse_project(&r).is_err());
        let mut r = row();
        r.fee_payer_pubkey = "nope".into();
        assert!(parse_project(&r).is_err());
    }

    #[test]
    fn snapshot_unions_only_active_origins_and_reads_kill_switch() {
        let mut paused = row();
        paused.aud = "client-p".into();
        paused.slug = "PAUSED".into();
        paused.status = "paused".into();
        paused.allowed_origins = r#"["https://paused.test"]"#.into();
        let mut broken = row();
        broken.aud = "client-x".into();
        broken.policy = "{}".into();
        broken.allowed_origins = r#"["https://broken.test"]"#.into();
        let s = Snapshot::build(
            vec![to_value(row()), to_value(paused), to_value(broken)],
            &[("mainnet_enabled".into(), "true".into())],
            0.0,
        );
        assert!(s.origins.contains("http://localhost:5173"));
        assert!(!s.origins.contains("https://paused.test"));
        assert!(!s.origins.contains("https://broken.test"));
        assert!(s.project("client-x").unwrap().is_err());
        assert!(s.project("missing").is_none());
        assert!(s.mainnet_enabled);
        let off = Snapshot::build(vec![], &[("mainnet_enabled".into(), "TRUE".into())], 0.0);
        assert!(!off.mainnet_enabled);
    }
}
