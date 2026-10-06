//! Gasless relayer — Cloudflare Worker (Rust/Wasm). Spec: docs/plans/2026-10-05_gasless-relayer-poc-spec.md
//!
//! Worker (stateless): HTTP hardening → decode → policy → idToken → co-sign + verify → DO.
//! RelayController DO (stateful, per wallet): in-flight lock → cooldown → GlobalThrottle → RPC → audit.

mod config;
mod error;
mod global_throttle;
mod jwt;
mod policy;
mod relay_controller;
mod rpc;

use std::cell::{Cell, RefCell};

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use worker::*;

use config::{
    Config, DEVNET_GENESIS_HASH, JWKS_FAILURE_BACKOFF_MS, JWKS_TTL_MS, JWKS_UNKNOWN_KID_REFETCH_MS,
    MAX_BODY_BYTES, MAX_DRAIN_BYTES, WalletBinding,
};
use error::ApiError;

pub use global_throttle::GlobalThrottle;
pub use relay_controller::RelayController;

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
    static JWKS: RefCell<Option<(Vec<jwt::Jwk>, f64)>> = const { RefCell::new(None) };
    static RELAYER_KEYPAIR: RefCell<Option<Keypair>> = const { RefCell::new(None) };
    static JWKS_LAST_FORCED_REFETCH: Cell<f64> = const { Cell::new(f64::NEG_INFINITY) };
    static JWKS_LAST_FAILURE: Cell<f64> = const { Cell::new(f64::NEG_INFINITY) };
    static DEVNET_CONFIRMED: Cell<bool> = const { Cell::new(false) };
}

/// Decoded once per isolate (secrets do not change within an isolate's lifetime).
fn relayer_keypair(env: &Env) -> std::result::Result<Keypair, ApiError> {
    if let Some(kp) = RELAYER_KEYPAIR.with(|c| c.borrow().as_ref().map(Keypair::insecure_clone)) {
        return Ok(kp);
    }
    let secret = env
        .secret("RELAYER_PRIVATE_KEY")
        .map_err(|_| ApiError::config("missing RELAYER_PRIVATE_KEY"))?
        .to_string();
    let kp = Keypair::try_from_base58_string(secret.trim())
        .map_err(|_| ApiError::config("RELAYER_PRIVATE_KEY is not a valid base58 keypair"))?;
    RELAYER_KEYPAIR.with(|c| *c.borrow_mut() = Some(kp.insecure_clone()));
    Ok(kp)
}

pub(crate) fn relayer_keypair_pubkey(env: &Env) -> std::result::Result<String, ApiError> {
    relayer_keypair(env).map(|kp| kp.pubkey().to_string())
}

fn now_ms() -> f64 {
    Date::now().as_millis() as f64
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let origin = req.headers().get("Origin")?;
    let cfg = match Config::from_env(&env) {
        Ok(c) => c,
        Err(e) => {
            // Keep CORS working so the browser surfaces the 500: preflights must get a 2xx,
            // otherwise POST /relay would only ever show a CORS failure.
            let allowed = env.var("ALLOWED_ORIGIN").ok().map(|v| v.to_string());
            let cors = origin
                .as_deref()
                .filter(|o| allowed.as_deref() == Some(*o) && *o != "*");
            if cors.is_some() && req.method() == Method::Options {
                return with_cors(Response::empty()?.with_status(204), cors);
            }
            return with_cors(ApiError::config(e).into_response()?, cors);
        }
    };
    let origin_ok = origin.as_deref() == Some(cfg.allowed_origin.as_str());
    let path = req.path();

    let result = match (req.method(), path.as_str()) {
        (Method::Options, "/relayer" | "/relay") if origin_ok => {
            Ok(Response::empty()?.with_status(204))
        }
        (Method::Options, "/relayer" | "/relay") => Err(ApiError::origin()),
        // Browsers send Origin on cross-origin GETs; non-browser clients may omit it.
        (Method::Get, "/relayer") if origin.is_none() || origin_ok => relayer_info(&env, &cfg),
        (Method::Get, "/relayer") => Err(ApiError::origin()),
        (Method::Post, "/relay") if origin_ok => handle_relay(&mut req, &env, &cfg).await,
        (Method::Post, "/relay") => Err(ApiError::origin()),
        (_, "/relayer" | "/relay") => Err(ApiError::method()),
        _ => Err(ApiError::not_found()),
    };
    let resp = match result {
        Ok(r) => r,
        Err(e) => e.into_response()?,
    };
    with_cors(resp, origin_ok.then_some(cfg.allowed_origin.as_str()))
}

fn with_cors(mut resp: Response, origin: Option<&str>) -> Result<Response> {
    if let Some(origin) = origin {
        let h = resp.headers_mut();
        h.set("Access-Control-Allow-Origin", origin)?;
        h.set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
        h.set(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type",
        )?;
        h.set("Access-Control-Expose-Headers", "Retry-After")?;
        h.set("Vary", "Origin")?;
    }
    Ok(resp)
}

fn relayer_info(env: &Env, cfg: &Config) -> std::result::Result<Response, ApiError> {
    let pubkey = relayer_keypair_pubkey(env)?;
    Response::from_json(&json!({
        "pubkey": pubkey,
        // Whole milliseconds (spec §4.2 shows an integer, not 10000.0).
        "cooldown_ms": cfg.cooldown_ms.round() as u64,
        "allowlist": [policy::MEMO_V2.to_string()],
        "max_memo_bytes": cfg.max_memo_bytes,
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
    // Cheap fail-fast; the token itself is verified after policy (step 4).
    let token = bearer_token(req)?;
    let body = read_body_limited(req, MAX_BODY_BYTES).await?;

    // 2. Decode
    let body: RelayBody =
        serde_json::from_slice(&body).map_err(|_| ApiError::validation("invalid JSON body"))?;
    let raw = B64
        .decode(body.transaction.as_bytes())
        .map_err(|_| ApiError::validation("transaction is not base64"))?;
    let mut tx = txcore::decode(&raw).map_err(ApiError::validation)?;

    // 3. Policy
    let relayer = relayer_keypair(env)?;
    let user = policy::validate(&tx, &relayer.pubkey(), cfg.max_memo_bytes)
        .map_err(ApiError::validation)?;

    // 4. Identity
    authenticate(&token, cfg, &user).await?;

    // 5. Co-sign + verify every slot
    txcore::cosign_and_verify(&mut tx, &relayer).map_err(ApiError::validation)?;
    let signature = tx.signatures[0].to_string();
    let signed = txcore::encode(&tx).map_err(ApiError::internal)?;

    ensure_devnet(cfg).await?;

    // 6. Hand off to the wallet's RelayController
    let job = json!({ "tx": B64.encode(signed), "user": user.to_string(), "signature": signature });
    let stub = env
        .durable_object("RELAY_CONTROLLER")
        .and_then(|ns| ns.id_from_name(&user.to_string())?.get_stub())
        .map_err(|e| ApiError::upstream(format!("relay stub: {e}")))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(job.to_string().into()));
    let do_req = Request::new_with_init("https://relay/relay", &init)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut resp = stub
        .fetch_with_request(do_req)
        .await
        .map_err(|e| ApiError::upstream(format!("relay fetch: {e}")))?;
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
    user: &Pubkey,
) -> std::result::Result<(), ApiError> {
    let now_s = now_ms() / 1000.0;
    // Reject malformed / non-ES256 / kid-less tokens before touching the network.
    jwt::precheck(token).map_err(|e| ApiError::unauthorized(format!("{e:?}")))?;

    let mut jwks = load_jwks(cfg, false).await?;
    let mut result = jwt::verify(
        token,
        &jwks,
        now_s,
        &cfg.web3auth_client_id,
        &cfg.web3auth_issuer,
    );
    if matches!(result, Err(jwt::AuthError::UnknownKid(_))) {
        // Key rotation: refetch at most once per window per isolate, else reject.
        let last = JWKS_LAST_FORCED_REFETCH.with(Cell::get);
        if now_ms() - last >= JWKS_UNKNOWN_KID_REFETCH_MS {
            // Stamp before the attempt so a failing JWKS endpoint is not hammered by
            // random-kid floods; an unknown kid stays a 401 either way (spec §4.3).
            JWKS_LAST_FORCED_REFETCH.with(|c| c.set(now_ms()));
            jwks = load_jwks(cfg, true)
                .await
                .map_err(|_| ApiError::unauthorized("UnknownKid (JWKS refetch failed)"))?;
            result = jwt::verify(
                token,
                &jwks,
                now_s,
                &cfg.web3auth_client_id,
                &cfg.web3auth_issuer,
            );
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

async fn load_jwks(cfg: &Config, force: bool) -> std::result::Result<Vec<jwt::Jwk>, ApiError> {
    let cached = JWKS.with(|c| c.borrow().clone());
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
    let recently_failed =
        !force && now_ms() - JWKS_LAST_FAILURE.with(Cell::get) < JWKS_FAILURE_BACKOFF_MS;
    let fetched = if recently_failed {
        Err("jwks: recently failed".to_string())
    } else {
        match rpc::get_text(&cfg.web3auth_jwks_url, cfg.rpc_timeout_ms).await {
            Ok(text) => jwt::parse_jwks(&text),
            Err(e) => Err(format!("jwks: {e}")),
        }
    };
    match fetched {
        Ok(keys) => {
            JWKS.with(|c| *c.borrow_mut() = Some((keys.clone(), now_ms())));
            Ok(keys)
        }
        Err(e) => {
            if !recently_failed {
                JWKS_LAST_FAILURE.with(|c| c.set(now_ms()));
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

/// Lazy per-isolate devnet guard: only a confirmed match is cached; fetch failures retry.
async fn ensure_devnet(cfg: &Config) -> std::result::Result<(), ApiError> {
    if DEVNET_CONFIRMED.with(Cell::get) {
        return Ok(());
    }
    let hash = rpc::get_genesis_hash(&cfg.rpc_url, cfg.rpc_timeout_ms)
        .await
        .map_err(ApiError::upstream)?;
    if hash != DEVNET_GENESIS_HASH {
        return Err(ApiError::upstream("RPC_URL is not Solana devnet"));
    }
    DEVNET_CONFIRMED.with(|c| c.set(true));
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
