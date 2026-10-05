//! Prints `{"tx","user","user_hex"}`: a base64 wire tx shaped like the frontend's
//! (fee payer = relayer, slot 0 empty, Memo v2 ix signed by the user).
//! Usage: cargo run --example mktx -- <relayer_pubkey> <blockhash> [user_seed_hex] [tamper|foreign_payer|v0]
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut relayer: Pubkey = args[1].parse().expect("relayer pubkey");
    let blockhash: Hash = args[2].parse().expect("blockhash");
    let user = match args.get(3).filter(|s| !s.is_empty() && *s != "-") {
        Some(hex) => {
            let mut seed = [0u8; 32];
            for (i, b) in seed.iter_mut().enumerate() {
                *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("seed hex");
            }
            Keypair::new_from_array(seed)
        }
        None => Keypair::new(),
    };
    let mode = args.get(4).map(String::as_str).unwrap_or("");
    if mode == "foreign_payer" {
        relayer = Keypair::new().pubkey();
    }
    let memo: Pubkey = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
        .parse()
        .unwrap();
    let data = format!("gasless-poc:v1:mint:{}", std::process::id());
    let ix = Instruction::new_with_bytes(
        memo,
        data.as_bytes(),
        vec![AccountMeta::new_readonly(user.pubkey(), true)],
    );
    let message = if mode == "v0" {
        VersionedMessage::V0(v0::Message::try_compile(&relayer, &[ix], &[], blockhash).unwrap())
    } else {
        VersionedMessage::Legacy(Message::new_with_blockhash(
            &[ix],
            Some(&relayer),
            &blockhash,
        ))
    };
    let mut signatures = vec![Signature::default(); 2];
    signatures[1] = user.sign_message(&message.serialize());
    if mode == "tamper" {
        signatures[1] = Signature::from([1u8; 64]);
    }
    let tx = VersionedTransaction {
        signatures,
        message,
    };
    let hex: String = user
        .pubkey()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!(
        "{{\"tx\":\"{}\",\"user\":\"{}\",\"user_hex\":\"{}\"}}",
        B64.encode(wincode::serialize(&tx).unwrap()),
        user.pubkey(),
        hex
    );
}
