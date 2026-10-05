//! Spike 0.1 — proves the relayer stack builds to Wasm and runs on `wrangler dev`:
//! wire-tx decode, relayer co-sign of slot 0, full signature verify, and both
//! SQLite-backed Durable Object classes. Not the production pipeline (Phase 1).

use std::cell::Cell;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use serde_json::json;
use solana_keypair::Keypair;
use solana_signer::Signer;
use worker::*;

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
    pub fn cosign_and_verify(tx: &mut VersionedTransaction, relayer: &Keypair) -> Result<(), String> {
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

fn relayer_keypair(env: &Env) -> Result<Keypair> {
    let secret = env.secret("RELAYER_PRIVATE_KEY")?.to_string();
    Keypair::try_from_base58_string(secret.trim())
        .map_err(|_| Error::RustError("RELAYER_PRIVATE_KEY is not a valid base58 keypair".into()))
}

#[derive(Deserialize)]
struct RelayBody {
    transaction: String,
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    Router::new()
        .get("/relayer", |_, ctx| {
            let kp = relayer_keypair(&ctx.env)?;
            Response::from_json(&json!({ "pubkey": kp.pubkey().to_string() }))
        })
        .post_async("/spike/cosign", |mut req, ctx| async move {
            let kp = relayer_keypair(&ctx.env)?;
            let body: RelayBody = req.json().await?;
            let bytes = match B64.decode(body.transaction.as_bytes()) {
                Ok(b) => b,
                Err(e) => return Response::error(format!("base64: {e}"), 400),
            };
            let mut tx = match txcore::decode(&bytes) {
                Ok(tx) => tx,
                Err(e) => return Response::error(e, 400),
            };
            if let Err(e) = txcore::cosign_and_verify(&mut tx, &kp) {
                return Response::error(e, 400);
            }
            let signed = txcore::encode(&tx).map_err(Error::RustError)?;
            Response::from_json(&json!({
                "signature": tx.signatures[0].to_string(),
                "transaction": B64.encode(signed),
            }))
        })
        .get_async("/spike/do", |_, ctx| async move {
            let relay = ctx
                .durable_object("RELAY_CONTROLLER")?
                .id_from_name("spike-wallet")?
                .get_stub()?
                .fetch_with_str("https://do/ping")
                .await?
                .text()
                .await?;
            let global = ctx
                .durable_object("GLOBAL_THROTTLE")?
                .id_from_name("global")?
                .get_stub()?
                .fetch_with_str("https://do/ping")
                .await?
                .text()
                .await?;
            Response::from_json(&json!({ "relay_controller": relay, "global_throttle": global }))
        })
        .run(req, env)
        .await
}

#[durable_object]
pub struct RelayController {
    state: State,
    inflight_since: Cell<Option<f64>>,
}

impl DurableObject for RelayController {
    fn new(state: State, _env: Env) -> Self {
        Self { state, inflight_since: Cell::new(None) }
    }

    async fn fetch(&self, _req: Request) -> Result<Response> {
        // Spike only: proves sync in-memory state + async storage round-trip.
        let now = Date::now().as_millis() as f64;
        self.inflight_since.set(Some(now));
        let hits: u64 = self.state.storage().get("hits").await?.unwrap_or(0);
        self.state.storage().put("hits", hits + 1).await?;
        self.inflight_since.set(None);
        Response::ok(format!("hits={}", hits + 1))
    }
}

#[durable_object]
pub struct GlobalThrottle {
    state: State,
}

impl DurableObject for GlobalThrottle {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, _req: Request) -> Result<Response> {
        let hits: u64 = self.state.storage().get("hits").await?.unwrap_or(0);
        self.state.storage().put("hits", hits + 1).await?;
        Response::ok(format!("hits={}", hits + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::txcore;
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_message::{Message, VersionedMessage};
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;
    use solana_signer::Signer;
    use solana_transaction::versioned::VersionedTransaction;

    const MEMO_V2: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

    /// Builds what the frontend sends: fee payer = relayer, user partial-signed.
    fn user_signed_tx(relayer: &Pubkey, user: &Keypair) -> VersionedTransaction {
        let memo: Pubkey = MEMO_V2.parse().unwrap();
        let ix = Instruction::new_with_bytes(
            memo,
            b"gasless-poc:v1:mint:0",
            vec![AccountMeta::new_readonly(user.pubkey(), true)],
        );
        let msg = Message::new_with_blockhash(&[ix], Some(relayer), &Hash::new_from_array([7u8; 32]));
        let message = VersionedMessage::Legacy(msg);
        let mut signatures = vec![Signature::default(); 2];
        signatures[1] = user.sign_message(&message.serialize());
        VersionedTransaction { signatures, message }
    }

    #[test]
    fn wire_roundtrip_then_cosign_verifies() {
        let relayer = Keypair::new();
        let user = Keypair::new();
        let tx = user_signed_tx(&relayer.pubkey(), &user);

        // Before co-sign, full verify must fail (slot 0 is zeroed).
        assert!(tx.verify_and_hash_message().is_err());

        let bytes = txcore::encode(&tx).unwrap();
        let mut decoded = txcore::decode(&bytes).unwrap();
        assert_eq!(decoded, tx);

        txcore::cosign_and_verify(&mut decoded, &relayer).unwrap();
        assert_eq!(decoded.message.header().num_required_signatures, 2);
    }

    #[test]
    fn rejects_foreign_fee_payer() {
        let relayer = Keypair::new();
        let other = Keypair::new();
        let user = Keypair::new();
        let mut tx = user_signed_tx(&other.pubkey(), &user);
        assert!(txcore::cosign_and_verify(&mut tx, &relayer).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let relayer = Keypair::new();
        let user = Keypair::new();
        let mut bytes = txcore::encode(&user_signed_tx(&relayer.pubkey(), &user)).unwrap();
        bytes.push(0);
        assert!(txcore::decode(&bytes).is_err());
    }

    #[test]
    fn rejects_tampered_user_signature() {
        let relayer = Keypair::new();
        let user = Keypair::new();
        let mut tx = user_signed_tx(&relayer.pubkey(), &user);
        tx.signatures[1] = Signature::default();
        assert!(txcore::cosign_and_verify(&mut tx, &relayer).is_err());
    }
}
