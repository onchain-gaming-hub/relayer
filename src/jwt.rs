//! Web3Auth idToken verification (spec §4.4 step 4). Pure functions; JWKS fetching and
//! caching live in `lib.rs`.
//!
//! Wallet binding (Web3Auth docs, embedded-wallets/authentication/id-token): social-login
//! tokens carry `wallets[] = { public_key: <hex>, type: "web3auth_app_key", curve: "ed25519" }`.
//! Still to be confirmed against a real token in spike 0.2.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64URL};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use serde::Deserialize;
use serde_json::Value;
use solana_pubkey::Pubkey;

use crate::config::JWT_LEEWAY_SECS;

#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    pub kid: String,
    #[serde(default)]
    pub kty: String,
    #[serde(default)]
    pub crv: String,
    pub x: String,
    pub y: String,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

pub fn parse_jwks(json: &str) -> Result<Vec<Jwk>, String> {
    let set: JwkSet = serde_json::from_str(json).map_err(|e| format!("jwks: {e}"))?;
    Ok(set
        .keys
        .into_iter()
        .filter(|k| k.kty == "EC" && k.crv == "P-256")
        .collect())
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Malformed,
    BadAlg,
    UnknownKid(String),
    BadSignature,
    Expired,
    NotYetValid,
    BadAudience,
    BadIssuer,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

/// Cheap structural check (3 parts, header `alg == ES256`, `kid` present) that needs no JWKS,
/// so malformed tokens are rejected before any network fetch.
pub fn precheck(token: &str) -> Result<(), AuthError> {
    let mut parts = token.split('.');
    let (Some(h), Some(_), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AuthError::Malformed);
    };
    let header: Header = decode_json(h)?;
    if header.alg != "ES256" {
        return Err(AuthError::BadAlg);
    }
    header.kid.map(|_| ()).ok_or(AuthError::Malformed)
}

/// Verifies an ES256 JWT against `jwks` and checks `exp`, `aud`, `iss`. Returns the claims.
pub fn verify(
    token: &str,
    jwks: &[Jwk],
    now_secs: f64,
    aud: &str,
    iss: &str,
) -> Result<Value, AuthError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AuthError::Malformed);
    };
    let header: Header = decode_json(h)?;
    if header.alg != "ES256" {
        return Err(AuthError::BadAlg);
    }
    let kid = header.kid.ok_or(AuthError::Malformed)?;
    let jwk = jwks
        .iter()
        .find(|k| k.kid == kid)
        .ok_or_else(|| AuthError::UnknownKid(kid.clone()))?;

    let key = verifying_key(jwk).ok_or(AuthError::BadSignature)?;
    let sig_bytes = B64URL.decode(s).map_err(|_| AuthError::Malformed)?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| AuthError::BadSignature)?;
    let signing_input = &token[..h.len() + 1 + p.len()];
    key.verify(signing_input.as_bytes(), &sig)
        .map_err(|_| AuthError::BadSignature)?;

    let claims: Value = decode_json(p)?;
    let exp = claims
        .get("exp")
        .and_then(Value::as_f64)
        .ok_or(AuthError::Malformed)?;
    if now_secs >= exp + JWT_LEEWAY_SECS {
        return Err(AuthError::Expired);
    }
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_f64)
        && nbf > now_secs + JWT_LEEWAY_SECS
    {
        return Err(AuthError::NotYetValid);
    }
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => a == aud,
        Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(aud)),
        _ => false,
    };
    if !aud_ok {
        return Err(AuthError::BadAudience);
    }
    if claims.get("iss").and_then(Value::as_str) != Some(iss) {
        return Err(AuthError::BadIssuer);
    }
    Ok(claims)
}

pub fn token_binds_wallet(claims: &Value, user: &Pubkey) -> bool {
    let Some(wallets) = claims.get("wallets").and_then(Value::as_array) else {
        return false;
    };
    wallets.iter().any(|w| {
        let curve = w.get("curve").and_then(Value::as_str).unwrap_or_default();
        let kind = w.get("type").and_then(Value::as_str).unwrap_or_default();
        if !curve.eq_ignore_ascii_case("ed25519") || kind != "web3auth_app_key" {
            return false;
        }
        // Social-login app keys carry `public_key` only (`address` belongs to external wallets).
        w.get("public_key")
            .and_then(Value::as_str)
            .is_some_and(|v| key_matches(v, user))
    })
}

fn key_matches(value: &str, user: &Pubkey) -> bool {
    // Web3Auth's own verifier strips an optional 0x prefix and compares hex case-insensitively
    // (`decode_hex` accepts both cases). Base58 is case-sensitive, so it uses the raw value.
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if hex.len() == 64
        && let Some(bytes) = decode_hex(hex)
    {
        return bytes == user.to_bytes();
    }
    value.parse::<Pubkey>().map(|p| p == *user).unwrap_or(false)
}

fn decode_hex(s: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

fn verifying_key(jwk: &Jwk) -> Option<VerifyingKey> {
    let x = B64URL.decode(&jwk.x).ok()?;
    let y = B64URL.decode(&jwk.y).ok()?;
    if x.len() != 32 || y.len() != 32 {
        return None;
    }
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    VerifyingKey::from_sec1_bytes(&sec1).ok()
}

fn decode_json<T: for<'de> Deserialize<'de>>(part: &str) -> Result<T, AuthError> {
    let bytes = B64URL.decode(part).map_err(|_| AuthError::Malformed)?;
    serde_json::from_slice(&bytes).map_err(|_| AuthError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{SigningKey, signature::Signer};
    use serde_json::json;
    use solana_keypair::Keypair;
    use solana_signer::Signer as _;

    const AUD: &str = "client-123";
    const ISS: &str = "https://issuer.test";

    fn key() -> (SigningKey, Jwk) {
        let sk = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let point = sk.verifying_key().to_encoded_point(false);
        let jwk = Jwk {
            kid: "k1".into(),
            kty: "EC".into(),
            crv: "P-256".into(),
            x: B64URL.encode(point.x().unwrap()),
            y: B64URL.encode(point.y().unwrap()),
        };
        (sk, jwk)
    }

    fn token(sk: &SigningKey, header: Value, claims: Value) -> String {
        let h = B64URL.encode(header.to_string());
        let p = B64URL.encode(claims.to_string());
        let sig: Signature = sk.sign(format!("{h}.{p}").as_bytes());
        format!("{h}.{p}.{}", B64URL.encode(sig.to_bytes()))
    }

    fn claims(exp: f64) -> Value {
        json!({ "aud": AUD, "iss": ISS, "exp": exp, "sub": "u1" })
    }

    #[test]
    fn verifies_valid_token() {
        let (sk, jwk) = key();
        let t = token(&sk, json!({"alg":"ES256","kid":"k1"}), claims(2000.0));
        assert!(verify(&t, &[jwk], 1000.0, AUD, ISS).is_ok());
    }

    #[test]
    fn rejects_expired_wrong_aud_wrong_iss() {
        let (sk, jwk) = key();
        let jwks = [jwk];
        let t = token(&sk, json!({"alg":"ES256","kid":"k1"}), claims(500.0));
        assert_eq!(verify(&t, &jwks, 1000.0, AUD, ISS), Err(AuthError::Expired));
        // within leeway
        assert!(verify(&t, &jwks, 520.0, AUD, ISS).is_ok());
        let t = token(
            &sk,
            json!({"alg":"ES256","kid":"k1"}),
            json!({"aud":AUD,"iss":ISS,"exp":2000.0,"nbf":1500.0}),
        );
        assert_eq!(
            verify(&t, &jwks, 1000.0, AUD, ISS),
            Err(AuthError::NotYetValid)
        );
        let t = token(
            &sk,
            json!({"alg":"ES256","kid":"k1"}),
            json!({"aud":"other","iss":ISS,"exp":2000.0}),
        );
        assert_eq!(
            verify(&t, &jwks, 1000.0, AUD, ISS),
            Err(AuthError::BadAudience)
        );
        let t = token(
            &sk,
            json!({"alg":"ES256","kid":"k1"}),
            json!({"aud":[AUD],"iss":"evil","exp":2000.0}),
        );
        assert_eq!(
            verify(&t, &jwks, 1000.0, AUD, ISS),
            Err(AuthError::BadIssuer)
        );
    }

    #[test]
    fn rejects_alg_none_unknown_kid_and_bad_signature() {
        let (sk, jwk) = key();
        let jwks = [jwk];
        let t = token(&sk, json!({"alg":"none","kid":"k1"}), claims(2000.0));
        assert_eq!(verify(&t, &jwks, 1000.0, AUD, ISS), Err(AuthError::BadAlg));
        let t = token(&sk, json!({"alg":"ES256","kid":"zzz"}), claims(2000.0));
        assert_eq!(
            verify(&t, &jwks, 1000.0, AUD, ISS),
            Err(AuthError::UnknownKid("zzz".into()))
        );
        let other = SigningKey::from_slice(&[9u8; 32]).unwrap();
        let t = token(&other, json!({"alg":"ES256","kid":"k1"}), claims(2000.0));
        assert_eq!(
            verify(&t, &jwks, 1000.0, AUD, ISS),
            Err(AuthError::BadSignature)
        );
        assert_eq!(
            verify("a.b", &jwks, 1000.0, AUD, ISS),
            Err(AuthError::Malformed)
        );
    }

    #[test]
    fn binds_wallet_by_hex_or_base58() {
        let user = Keypair::new().pubkey();
        let hex: String = user.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let w = |v: Value| json!({ "wallets": [v] });
        let app = "web3auth_app_key";
        assert!(token_binds_wallet(
            &w(json!({"type":app,"curve":"ed25519","public_key":hex})),
            &user
        ));
        assert!(token_binds_wallet(
            &w(
                json!({"type":app,"curve":"ed25519","public_key":format!("0x{}", hex.to_uppercase())})
            ),
            &user
        ));
        assert!(token_binds_wallet(
            &w(json!({"type":app,"curve":"ED25519","public_key":user.to_string()})),
            &user
        ));
        assert!(token_binds_wallet(
            &w(json!({"type":app,"curve":"ed25519","public_key":format!("0X{hex}")})),
            &user
        ));
        // `address` (external-wallet field) is not accepted for app keys.
        assert!(!token_binds_wallet(
            &w(json!({"type":app,"curve":"ed25519","address":user.to_string()})),
            &user
        ));
        assert!(!token_binds_wallet(
            &w(json!({"type":app,"curve":"secp256k1","public_key":hex})),
            &user
        ));
        assert!(!token_binds_wallet(
            &w(json!({"type":"other","curve":"ed25519","public_key":hex})),
            &user
        ));
        assert!(!token_binds_wallet(
            &w(json!({"curve":"ed25519","public_key":hex})),
            &user
        ));
        assert!(!token_binds_wallet(
            &w(
                json!({"type":app,"curve":"ed25519","public_key":Keypair::new().pubkey().to_string()})
            ),
            &user
        ));
        assert!(!token_binds_wallet(&json!({}), &user));
    }

    #[test]
    fn precheck_rejects_without_jwks() {
        let (sk, _) = key();
        assert_eq!(precheck("x"), Err(AuthError::Malformed));
        let none = token(&sk, json!({"alg":"none","kid":"k1"}), claims(2000.0));
        assert_eq!(precheck(&none), Err(AuthError::BadAlg));
        let no_kid = token(&sk, json!({"alg":"ES256"}), claims(2000.0));
        assert_eq!(precheck(&no_kid), Err(AuthError::Malformed));
        let ok = token(&sk, json!({"alg":"ES256","kid":"k1"}), claims(2000.0));
        assert!(precheck(&ok).is_ok());
    }

    #[test]
    fn parses_live_web3auth_jwks_shape() {
        let json = r#"{"keys":[{"kty":"EC","crv":"P-256","x":"PzwlL8X3P1SpXieB8i6z-KzqVEPI8Vu_JysBiGa7dgA","y":"7OtaCKJjbNvs3Oo5pmwGkWKm4oJZOiyl2oWb_odopKo","kid":"TYOgg_-5EOEblaY-VVRYqVaDAgptnfKV4535MZPC0w0","use":"sig","alg":"ES256"}]}"#;
        let keys = parse_jwks(json).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(verifying_key(&keys[0]).is_some());
    }
}
