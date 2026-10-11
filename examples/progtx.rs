//! Test-only: prints `{"tx","user","user_hex"}` for a program-action tx (relayer v3 e2e).
//! Usage: progtx <fee_payer> <blockhash> <user_seed_hex|-> <ix>...
//!   ix = p:<program>:<data_hex>:<acct,acct,...>   acct = fp:w | fp:r | u:s | u:ws | new:w | new:r | <pubkey>:w | <pubkey>:r
//!        | pda/<seed>/<program>:w  (PDA of [seed, user] — live devnet tests)
//!      | cbl:<compute unit limit> | cbp:<micro-lamports per CU>
//! Slot 0 (fee payer) is left empty; the user signs its slot. `PROGTX_V0=1` builds a v0 message
//! (no lookup tables), as `@solana/kit` does in the frontend.
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const COMPUTE_BUDGET: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let fee_payer: Pubkey = args[1].parse().expect("fee payer");
    let blockhash: Hash = args[2].parse().expect("blockhash");
    let user = match args.get(3).filter(|s| *s != "-") {
        Some(h) => Keypair::new_from_array(hex(&h[..64]).try_into().expect("32-byte seed")),
        None => Keypair::new(),
    };
    let ixs: Vec<Instruction> = args[4..]
        .iter()
        .map(|spec| {
            if let Some(n) = spec.strip_prefix("cbl:") {
                let mut d = vec![2u8];
                d.extend_from_slice(&n.parse::<u32>().unwrap().to_le_bytes());
                return Instruction::new_with_bytes(COMPUTE_BUDGET, &d, vec![]);
            }
            if let Some(n) = spec.strip_prefix("cbp:") {
                let mut d = vec![3u8];
                d.extend_from_slice(&n.parse::<u64>().unwrap().to_le_bytes());
                return Instruction::new_with_bytes(COMPUTE_BUDGET, &d, vec![]);
            }
            let mut parts = spec.splitn(4, ':');
            assert_eq!(parts.next(), Some("p"), "ix spec must start with p:");
            let program: Pubkey = parts.next().unwrap().parse().expect("program");
            let data = hex(parts.next().unwrap());
            let accounts = parts
                .next()
                .unwrap_or("")
                .split(',')
                .filter(|a| !a.is_empty())
                .map(|a| {
                    let (who, mode) = a.rsplit_once(':').expect("acct:mode");
                    let key = match who {
                        "fp" => fee_payer,
                        "u" => user.pubkey(),
                        "new" => Pubkey::new_unique(),
                        k if k.starts_with("pda/") => {
                            let mut it = k.splitn(3, '/').skip(1);
                            let seed = it.next().expect("pda seed");
                            let program: Pubkey =
                                it.next().expect("pda program").parse().expect("program");
                            Pubkey::find_program_address(
                                &[seed.as_bytes(), user.pubkey().as_ref()],
                                &program,
                            )
                            .0
                        }
                        k => k.parse().expect("pubkey"),
                    };
                    let signer = who == "fp" || mode.contains('s');
                    if mode.contains('w') {
                        AccountMeta::new(key, signer)
                    } else {
                        AccountMeta::new_readonly(key, signer)
                    }
                })
                .collect();
            Instruction::new_with_bytes(program, &data, accounts)
        })
        .collect();
    let message = if std::env::var("PROGTX_V0").as_deref() == Ok("1") {
        VersionedMessage::V0(
            solana_message::v0::Message::try_compile(&fee_payer, &ixs, &[], blockhash)
                .expect("v0 compile"),
        )
    } else {
        VersionedMessage::Legacy(Message::new_with_blockhash(
            &ixs,
            Some(&fee_payer),
            &blockhash,
        ))
    };
    let keys = message.static_account_keys();
    let n = message.header().num_required_signatures as usize;
    let mut signatures = vec![Signature::default(); n];
    if let Some(i) = keys
        .iter()
        .position(|k| *k == user.pubkey())
        .filter(|i| *i < n)
    {
        signatures[i] = user.sign_message(&message.serialize());
    }
    let tx = VersionedTransaction {
        signatures,
        message,
    };
    let user_hex: String = user
        .pubkey()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!(
        "{{\"tx\":\"{}\",\"user\":\"{}\",\"user_hex\":\"{}\"}}",
        B64.encode(wincode::serialize(&tx).unwrap()),
        user.pubkey(),
        user_hex
    );
}
