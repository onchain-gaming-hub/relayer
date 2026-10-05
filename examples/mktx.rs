//! Prints a base64 wire tx the way the frontend will send it:
//! fee payer = relayer (slot 0 empty), Memo v2 ix signed by a fresh user key.
//! Usage: cargo run --example mktx -- <relayer_pubkey> <recent_blockhash>
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let relayer: Pubkey = args[1].parse().expect("relayer pubkey");
    let blockhash: Hash = args[2].parse().expect("blockhash");
    let user = Keypair::new();
    let memo: Pubkey = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr".parse().unwrap();
    let ix = Instruction::new_with_bytes(
        memo,
        b"gasless-poc:v1:mint:spike",
        vec![AccountMeta::new_readonly(user.pubkey(), true)],
    );
    let message = VersionedMessage::Legacy(Message::new_with_blockhash(&[ix], Some(&relayer), &blockhash));
    let mut signatures = vec![Signature::default(); 2];
    signatures[1] = user.sign_message(&message.serialize());
    let tx = VersionedTransaction { signatures, message };
    println!("{}", B64.encode(wincode::serialize(&tx).unwrap()));
}
