//! Gasless relayer — Cloudflare Worker (Rust/Wasm), multi-project.
//! Spec: docs/plans/2026-10-06_multi-project-relayer-v2-spec.md (extends the v1 POC spec).
//!
//! Worker (stateless): HTTP hardening → decode + structural policy → idToken (project = `aud`)
//! → project status / origin / policy → co-sign with the project's fee payer → UserController.
//! UserController DO ("<aud>:<wallet>"): lock → cooldown → ProjectThrottle → ClusterThrottle
//! → RPC → audit.

mod cluster_throttle;
mod config;
mod error;
mod jwt;
mod policy;
mod project_throttle;
mod registry;
mod rpc;
mod user_controller;
mod window;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use worker::*;

use config::{
    Cluster, Config, JWKS_FAILURE_BACKOFF_MS, JWKS_TTL_MS, JWKS_UNKNOWN_KID_REFETCH_MS,
    MAX_BODY_BYTES, MAX_DRAIN_BYTES, WalletBinding, Web3AuthEndpoints,
};
use error::ApiError;
use registry::{Project, Snapshot, Status};

pub use cluster_throttle::ClusterThrottle;
pub use project_throttle::ProjectThrottle;
pub use user_controller::UserController;

pub mod txcore {
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::versioned::VersionedTransaction;

    /// Decodes a wire-format transaction, rejecting trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<VersionedTransaction, String> {
        wincode::deserialize_exact(bytes).map_err(|e| format!("decode: {e}"))
    }

    pub fn encode(tx: &VersionedTransaction) -> Result<Vec<u8>, String> {
        wincode::serialize(tx).map_err(|e| format!("encode: {e}"))
    }

    /// Signs slot 0 as fee payer, then verifies every signature slot at once.
    /// `verify_and_hash_message` checks all slots, so slot 0 must be filled first.
    pub fn cosign_and_verify(
        tx: &mut VersionedTransaction,
        relayer: &Keypair,
    ) -> Result<(), String> {
        let fee_payer = tx
            .message
            .static_account_keys()
            .first()
            .ok_or("no account keys")?;
        if *fee_payer != relayer.pubkey() {
            return Err("fee payer is not the relayer".into());
        }
        let slot0 = tx.signatures.first_mut().ok_or("no signature slots")?;
        *slot0 = relayer.sign_message(&tx.message.serialize());
        tx.verify_and_hash_message()
            .map(|_| ())
            .map_err(|e| format!("verify: {e:?}"))
    }
}

thread_local! {
    /// Per-isolate caches. Wasm workers are single-threaded.
    /// JWKS per URL: (keys, fetched_at).
    static JWKS: RefCell<HashMap<String, (Vec<jwt::Jwk>, f64)>> = RefCell::new(HashMap::new());
    static JWKS_LAST_FORCED_REFETCH: RefCell<HashMap<String, f64>> = RefCell::new(HashMap::new());
    static JWKS_LAST_FAILURE: RefCell<HashMap<String, f64>> = RefCell::new(HashMap::new());
    /// Fee payer keypairs by project slug (secrets do not change within an isolate).
    static FEE_PAYERS: RefCell<HashMap<String, Keypair>> = RefCell::new(HashMap::new());
    static CLUSTERS_CONFIRMED: RefCell<HashSet<Cluster>> = RefCell::new(HashSet::new());
}

/// The project's fee payer from `FEE_PAYER_<SLUG>`; must match `fee_payer_pubkey` in D1 (V3).
fn fee_payer(env: &Env, project: &Project) -> std::result::Result<Keypair, ApiError> {
    let cached = FEE_PAYERS.with(|c| c.borrow().get(&project.slug).map(Keypair::insecure_clone));
    let kp = match cached {
        Some(kp) => kp,
        None => {
            // Re-validate before building a binding name from registry data (V3).
            if !registry::valid_slug(&project.slug) {
                return Err(ApiError::config(format!(
                    "project {}: bad slug",
                    project.aud
                )));
            }
            let name = format!("FEE_PAYER_{}", project.slug);
            // Secret and Var are the same binding type at runtime: the code cannot tell a
            // `[vars]` value from a secret, so keys must never be put in wrangler.toml.
            let secret = env
                .secret(&name)
                .map(|s| s.to_string())
                .map_err(|_| ApiError::config(format!("missing {name}")))?;
            let kp = Keypair::try_from_base58_string(secret.trim())
                .map_err(|_| ApiError::config(format!("{name} is not a valid base58 keypair")))?;
            FEE_PAYERS.with(|c| {
                c.borrow_mut()
                    .insert(project.slug.clone(), kp.insecure_clone())
            });
            kp
        }
    };
    if kp.pubkey() != project.fee_payer {
        return Err(ApiError::config(format!(
            "project {}: FEE_PAYER_{} does not match fee_payer_pubkey",
            project.aud, project.slug
        )));
    }
    Ok(kp)
}

fn now_ms() -> f64 {
    Date::now().as_millis() as f64
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let origin = req.headers().get("Origin")?;
    let cfg = match Config::from_env(&env) {
        Ok(c) => c,
        Err(e) => return ApiError::config(e).into_response(),
    };
    let path = req.path();
    if !matches!(path.as_str(), "/relayer" | "/relay") {
        return ApiError::not_found().into_response();
    }
    // Like the config 500 above, this 503 carries no CORS headers: allowed origins come from
    // the registry, so a cold isolate that cannot read D1 has no origin to echo (accepted;
    // the browser reports a CORS failure instead of the JSON body).
    let snap = match registry::snapshot(&env, &cfg, now_ms()).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    // CORS: echo the Origin when it belongs to any active project (V9). The project-specific
    // check for POST /relay happens after the token names the project.
    let cors_origin = origin.as_deref().filter(|o| snap.origins.contains(*o));

    let result = match (req.method(), path.as_str()) {
        (Method::Options, _) if cors_origin.is_some() => Ok(Response::empty()?.with_status(204)),
        (Method::Options, _) => Err(ApiError::origin()),
        // Browsers send Origin on cross-origin GETs; non-browser clients may omit it.
        (Method::Get, "/relayer") if origin.is_none() || cors_origin.is_some() => {
            relayer_info(&req, &snap, &cfg)
        }
        (Method::Get, "/relayer") => Err(ApiError::origin()),
        (Method::Post, "/relay") => match origin.as_deref() {
            Some(o) if cors_origin.is_some() => handle_relay(&mut req, &env, &cfg, &snap, o).await,
            _ => Err(ApiError::origin()),
        },
        _ => Err(ApiError::method()),
    };
    let resp = match result {
        Ok(r) => r,
        Err(e) => e.into_response()?,
    };
    with_cors(resp, cors_origin)
}

fn with_cors(mut resp: Response, origin: Option<&str>) -> Result<Response> {
    let h = resp.headers_mut();
    h.set("Vary", "Origin")?;
    if let Some(origin) = origin {
        h.set("Access-Control-Allow-Origin", origin)?;
        h.set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
        h.set(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type",
        )?;
        h.set("Access-Control-Expose-Headers", "Retry-After")?;
    }
    Ok(resp)
}

/// Whether a project may serve traffic right now (V5, V13, V4 max stale).
fn project_gate(
    project: &Project,
    snap: &Snapshot,
    cfg: &Config,
) -> std::result::Result<(), ApiError> {
    if project.status != Status::Active {
        return Err(ApiError::project_paused());
    }
    if project.cluster == Cluster::Mainnet {
        if !snap.mainnet_enabled {
            return Err(ApiError::cluster_disabled());
        }
        // A stale snapshot could hold an old `mainnet_enabled = true`: fail closed.
        if snap.age(now_ms()) > cfg.registry_max_stale_ms {
            return Err(ApiError::upstream(
                "registry snapshot too stale for mainnet",
            ));
        }
    }
    Ok(())
}

/// `GET /relayer?project=<clientId>` (spec v2 §4.2). Unknown, inactive, or disabled → 404.
fn relayer_info(
    req: &Request,
    snap: &Snapshot,
    cfg: &Config,
) -> std::result::Result<Response, ApiError> {
    let url = req.url().map_err(|e| ApiError::internal(e.to_string()))?;
    let aud = url
        .query_pairs()
        .find(|(k, _)| k == "project")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(ApiError::not_found)?;
    let project = match snap.project(&aud) {
        Some(Ok(p)) if project_gate(p, snap, cfg).is_ok() => p,
        _ => return Err(ApiError::not_found()),
    };
    let mut memo = json!({ "type": "memo", "max_bytes": project.memo.max_bytes });
    if let Some(prefix) = &project.memo.prefix {
        memo["prefix"] = json!(prefix);
    }
    Response::from_json(&json!({
        "pubkey": project.fee_payer.to_string(),
        "cluster": project.cluster.as_str(),
        "policy": { "actions": [memo] },
        // Whole milliseconds; ceil like retry_after_ms.
        "cooldown_ms": cfg.cooldown_ms.ceil() as u64,
        "user_daily_quota": project.user_daily_quota,
        "wallet_binding": match cfg.wallet_binding {
            WalletBinding::Wallets => "wallets",
            WalletBinding::Off => "off",
        },
    }))
    .map_err(|e| ApiError::internal(e.to_string()))
}

#[derive(Deserialize)]
struct RelayBody {
    transaction: String,
}

async fn handle_relay(
    req: &mut Request,
    env: &Env,
    cfg: &Config,
    snap: &Snapshot,
    origin: &str,
) -> std::result::Result<Response, ApiError> {
    // 1. HTTP hardening
    let content_type = req
        .headers()
        .get("Content-Type")
        .ok()
        .flatten()
        .unwrap_or_default();
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if !media_type.eq_ignore_ascii_case("application/json") {
        return Err(ApiError::content_type());
    }
    // Cheap fail-fast; the token itself is verified in step 3.
    let token = bearer_token(req)?;
    let body = read_body_limited(req, MAX_BODY_BYTES).await?;

    // 2. Decode + project-independent policy (before spending an ES256 verify)
    let body: RelayBody =
        serde_json::from_slice(&body).map_err(|_| ApiError::validation("invalid JSON body"))?;
    let raw = B64
        .decode(body.transaction.as_bytes())
        .map_err(|_| ApiError::validation("transaction is not base64"))?;
    let mut tx = txcore::decode(&raw).map_err(ApiError::validation)?;
    let shape = policy::structural(&tx).map_err(ApiError::validation)?;

    // 3. Identity (+ wallet binding): the unverified `aud` only picks the project;
    //    verify re-checks it.
    jwt::precheck(&token).map_err(|e| ApiError::unauthorized(format!("{e:?}")))?;
    let aud = jwt::unverified_aud(&token).map_err(|e| ApiError::unauthorized(format!("{e:?}")))?;
    let project = match snap.project(&aud) {
        Some(Ok(p)) => p,
        // Logged once per registry load (registry.rs), not per request.
        Some(Err(_)) => return Err(ApiError::misconfigured_quiet()),
        // Same answer as any bad token: do not reveal which clientIds are registered.
        None => return Err(ApiError::unauthorized("UnknownProject")),
    };
    authenticate(&token, cfg, project, &shape.user).await?;

    // 4. Project status / cluster switch
    project_gate(project, snap, cfg)?;
    // 5. Origin must belong to this token's project (Origin is hygiene; `aud` is the gate)
    if !project.allowed_origins.iter().any(|o| o == origin) {
        return Err(ApiError::origin());
    }
    // 6. Project-dependent policy → cost (V7)
    let cost = policy::for_project(&shape, &project.fee_payer, &project.memo)
        .map_err(ApiError::validation)?;

    // 7. Co-sign with the project's fee payer + verify every slot
    let relayer = fee_payer(env, project)?;
    txcore::cosign_and_verify(&mut tx, &relayer).map_err(ApiError::validation)?;
    let signature = tx.signatures[0].to_string();
    let signed = txcore::encode(&tx).map_err(ApiError::internal)?;

    ensure_cluster(env, cfg, project.cluster).await?;

    // 8. Hand off to the user's controller for this project
    let user = shape.user.to_string();
    let job = json!({
        "tx": B64.encode(signed),
        "user": user,
        "signature": signature,
        "aud": project.aud,
        "cluster": project.cluster.as_str(),
        "cost": cost,
        "project_rpm": project.project_rpm,
        "fee_payer": project.fee_payer.to_string(),
        "min_balance": project.min_balance_lamports,
    });
    let stub = env
        .durable_object("USER_CONTROLLER")
        .and_then(|ns| {
            ns.id_from_name(&format!("{}:{user}", project.aud))?
                .get_stub()
        })
        .map_err(|e| ApiError::upstream(format!("user stub: {e}")))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(job.to_string().into()));
    let do_req = Request::new_with_init("https://user/relay", &init)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut resp = stub
        .fetch_with_request(do_req)
        .await
        .map_err(|e| ApiError::upstream(format!("user fetch: {e}")))?;
    passthrough(&mut resp).await
}

/// Rebuilds the DO response so the Worker can attach CORS headers.
async fn passthrough(resp: &mut Response) -> std::result::Result<Response, ApiError> {
    let status = resp.status_code();
    let retry_after = resp.headers().get("Retry-After").ok().flatten();
    let body = resp
        .text()
        .await
        .map_err(|e| ApiError::upstream(format!("relay body: {e}")))?;
    let mut out = Response::ok(body)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .with_status(status);
    let h = out.headers_mut();
    let _ = h.set("Content-Type", "application/json");
    if let Some(v) = retry_after {
        let _ = h.set("Retry-After", &v);
    }
    Ok(out)
}

async fn read_body_limited(
    req: &mut Request,
    limit: usize,
) -> std::result::Result<Vec<u8>, ApiError> {
    if let Some(len) = req.headers().get("Content-Length").ok().flatten() {
        match len.trim().parse::<usize>() {
            Ok(n) if n > limit => return Err(ApiError::too_large()),
            Ok(_) => {}
            Err(_) => return Err(ApiError::validation("invalid Content-Length")),
        }
    }
    // Content-Length can be absent (chunked); cap what is actually read as well.
    let mut stream = req
        .stream()
        .map_err(|_| ApiError::validation("missing body"))?;
    let mut buf = Vec::new();
    let mut seen = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::validation("unreadable body"))?;
        seen += chunk.len();
        if seen > limit {
            // Drain a bounded amount before answering: abandoning a chunked upload
            // mid-stream makes some proxies (e.g. wrangler dev) drop the connection,
            // turning the 413 into a 500. Past MAX_DRAIN we stop reading regardless.
            if seen > MAX_DRAIN_BYTES {
                return Err(ApiError::too_large());
            }
            continue;
        }
        buf.extend_from_slice(&chunk);
    }
    if seen > limit {
        return Err(ApiError::too_large());
    }
    Ok(buf)
}

fn bearer_token(req: &Request) -> std::result::Result<String, ApiError> {
    let header = req
        .headers()
        .get("Authorization")
        .ok()
        .flatten()
        .unwrap_or_default();
    parse_bearer(&header).ok_or_else(|| ApiError::unauthorized("missing bearer token"))
}

/// `Bearer <token>` with a case-insensitive scheme (RFC 9110 §11.1). Char-boundary safe:
/// `get(..7)` is `None` for headers shorter than 7 bytes or split inside a UTF-8 char.
fn parse_bearer(header: &str) -> Option<String> {
    header
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("bearer "))
        .and_then(|_| header.get(7..))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

async fn authenticate(
    token: &str,
    cfg: &Config,
    project: &Project,
    user: &Pubkey,
) -> std::result::Result<(), ApiError> {
    let now_s = now_ms() / 1000.0;
    let endpoints = cfg.web3auth(project.network).map_err(ApiError::config)?;

    let mut jwks = load_jwks(endpoints, cfg, false).await?;
    let mut result = jwt::verify(token, &jwks, now_s, &project.aud, &endpoints.issuer);
    if matches!(result, Err(jwt::AuthError::UnknownKid(_))) {
        // Key rotation: refetch at most once per window per isolate and JWKS URL, else reject.
        let url = &endpoints.jwks_url;
        let last = JWKS_LAST_FORCED_REFETCH
            .with(|m| m.borrow().get(url).copied())
            .unwrap_or(f64::NEG_INFINITY);
        if now_ms() - last >= JWKS_UNKNOWN_KID_REFETCH_MS {
            // Stamp before the attempt so a failing JWKS endpoint is not hammered by
            // random-kid floods; an unknown kid stays a 401 either way.
            JWKS_LAST_FORCED_REFETCH.with(|m| m.borrow_mut().insert(url.clone(), now_ms()));
            jwks = load_jwks(endpoints, cfg, true)
                .await
                .map_err(|_| ApiError::unauthorized("UnknownKid (JWKS refetch failed)"))?;
            result = jwt::verify(token, &jwks, now_s, &project.aud, &endpoints.issuer);
        }
    }
    let claims = result.map_err(|e| ApiError::unauthorized(format!("{e:?}")))?;

    match cfg.wallet_binding {
        WalletBinding::Wallets if !jwt::token_binds_wallet(&claims, user) => Err(
            ApiError::unauthorized("token does not belong to the transaction signer"),
        ),
        WalletBinding::Wallets => Ok(()),
        WalletBinding::Off => {
            // R5 fallback: any valid Web3Auth user can relay for any keypair.
            console_warn!("WALLET_BINDING=off: idToken not bound to signer {user}");
            Ok(())
        }
    }
}

async fn load_jwks(
    endpoints: &Web3AuthEndpoints,
    cfg: &Config,
    force: bool,
) -> std::result::Result<Vec<jwt::Jwk>, ApiError> {
    let url = &endpoints.jwks_url;
    let cached = JWKS.with(|c| c.borrow().get(url).cloned());
    if !force
        && let Some((keys, fetched_at)) = &cached
        && now_ms() - fetched_at < JWKS_TTL_MS
    {
        return Ok(keys.clone());
    }
    // No cross-request single-flight here: the Workers runtime forbids awaiting I/O created by
    // another request in the same isolate ("Cannot perform I/O on behalf of a different
    // request" — verified on wrangler dev). A cold isolate may fetch JWKS once per concurrent
    // request; accepted (Web3Auth JWKS is a static, cacheable document).
    // Negative cache: after a failed fetch, don't retry for JWKS_FAILURE_BACKOFF_MS.
    // A forced refetch is already rate-limited by its own window, so it always hits the network.
    let last_failure = JWKS_LAST_FAILURE
        .with(|m| m.borrow().get(url).copied())
        .unwrap_or(f64::NEG_INFINITY);
    let recently_failed = !force && now_ms() - last_failure < JWKS_FAILURE_BACKOFF_MS;
    let fetched = if recently_failed {
        Err("jwks: recently failed".to_string())
    } else {
        match rpc::get_text(url, cfg.rpc_timeout_ms).await {
            Ok(text) => jwt::parse_jwks(&text),
            Err(e) => Err(format!("jwks: {e}")),
        }
    };
    match fetched {
        Ok(keys) => {
            JWKS.with(|c| c.borrow_mut().insert(url.clone(), (keys.clone(), now_ms())));
            Ok(keys)
        }
        Err(e) => {
            if !recently_failed {
                JWKS_LAST_FAILURE.with(|m| m.borrow_mut().insert(url.clone(), now_ms()));
            }
            // Prefer stale keys over an outage: Web3Auth keys rotate rarely.
            match cached {
                Some((keys, _)) => {
                    console_warn!("{e}; serving stale JWKS");
                    Ok(keys)
                }
                None => Err(ApiError::upstream(e)),
            }
        }
    }
}

/// Lazy per-isolate cluster guard (V5): the RPC of each cluster must report that cluster's
/// genesis hash. Only a confirmed match is cached; fetch failures retry.
async fn ensure_cluster(
    env: &Env,
    cfg: &Config,
    cluster: Cluster,
) -> std::result::Result<(), ApiError> {
    if CLUSTERS_CONFIRMED.with(|c| c.borrow().contains(&cluster)) {
        return Ok(());
    }
    let url = config::rpc_url(env, cluster).map_err(ApiError::config)?;
    let hash = rpc::get_genesis_hash(&url, cfg.rpc_timeout_ms)
        .await
        .map_err(ApiError::upstream)?;
    if hash != cluster.genesis_hash() {
        return Err(ApiError::upstream(format!(
            "RPC_URL_{} is not Solana {}",
            cluster.as_str().to_uppercase(),
            cluster.as_str()
        )));
    }
    CLUSTERS_CONFIRMED.with(|c| c.borrow_mut().insert(cluster));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::txcore;
    use crate::policy::tests::good_tx;
    use solana_keypair::Keypair;
    use solana_signature::Signature;
    use solana_signer::Signer;

    #[test]
    fn wire_roundtrip_then_cosign_verifies() {
        let relayer = Keypair::new();
        let user = Keypair::new();
        let tx = good_tx(&relayer.pubkey(), &user);
        assert!(
            tx.verify_and_hash_message().is_err(),
            "slot 0 empty must fail full verify"
        );
        let bytes = txcore::encode(&tx).unwrap();
        let mut decoded = txcore::decode(&bytes).unwrap();
        assert_eq!(decoded, tx);
        txcore::cosign_and_verify(&mut decoded, &relayer).unwrap();
    }

    #[test]
    fn parses_bearer_scheme() {
        use super::parse_bearer;
        assert_eq!(
            parse_bearer("Bearer abc.def.ghi").as_deref(),
            Some("abc.def.ghi")
        );
        assert_eq!(parse_bearer("bearer abc").as_deref(), Some("abc"));
        assert_eq!(parse_bearer("BEARER   abc  ").as_deref(), Some("abc"));
        assert_eq!(parse_bearer("Bearer"), None); // no space, shorter than 7 bytes
        assert_eq!(parse_bearer("Bearer    "), None); // empty token
        assert_eq!(parse_bearer("BearerXabc"), None);
        assert_eq!(parse_bearer("Basic dXNlcjpwYXNz"), None);
        assert_eq!(parse_bearer(""), None);
        // Byte 7 falls inside a multi-byte char -> get(..7) is None (no panic).
        assert!(!"Bearer€x".is_char_boundary(7));
        assert_eq!(parse_bearer("Bearer€x"), None);
        assert!(!"Beareré".is_char_boundary(7));
        assert_eq!(parse_bearer("Beareré"), None);
    }

    #[test]
    fn rejects_trailing_bytes() {
        let relayer = Keypair::new();
        let mut bytes = txcore::encode(&good_tx(&relayer.pubkey(), &Keypair::new())).unwrap();
        bytes.push(0);
        assert!(txcore::decode(&bytes).is_err());
    }

    #[test]
    fn rejects_tampered_user_signature() {
        let relayer = Keypair::new();
        let mut tx = good_tx(&relayer.pubkey(), &Keypair::new());
        tx.signatures[1] = Signature::default();
        assert!(txcore::cosign_and_verify(&mut tx, &relayer).is_err());
    }
}
