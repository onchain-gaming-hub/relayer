//! Cron sweep (spec v3 W13, §5): what the Worker hands a `SweepController`, and the pure parts
//! of a run (tx template, cursor order, the "rent must come back" check). I/O lives in
//! `sweep_controller.rs`.

use serde::{Deserialize, Serialize};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use crate::program_policy::{SweepAccount, SweepRule};

/// A sweep tx carries one signature (the fee payer's) and no ComputeBudget ix.
pub const SWEEP_FEE_LAMPORTS: u64 = 5_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepAcct {
    /// `swept` | `fee_payer` | `fixed`
    pub kind: String,
    #[serde(default)]
    pub pubkey: Option<String>,
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepSpec {
    pub program_id: String,
    pub name: String,
    /// memcmp bytes at offset 0, base58 (the RPC's memcmp encoding).
    pub account_discriminator: String,
    pub rent_payer_offset: u64,
    pub ix_discriminator: Vec<u8>,
    pub accounts: Vec<SweepAcct>,
    pub max_sends_per_run: u32,
    pub max_simulations_per_run: u32,
}

/// Everything one run needs, resolved by the Worker from the registry snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SweepJob {
    pub aud: String,
    pub slug: String,
    pub cluster: String,
    pub fee_payer: String,
    pub project_rpm: u32,
    pub project_daily_budget: u64,
    pub min_balance: u64,
    /// Programs registered by the project (allowed owners in the §4.3 rule 3 check).
    pub registered: Vec<String>,
    pub sweeps: Vec<SweepSpec>,
}

impl SweepSpec {
    pub fn from_rule(program_id: &Pubkey, rule: &SweepRule) -> Self {
        let acct = |kind: &str, pubkey: Option<String>, writable: bool| SweepAcct {
            kind: kind.into(),
            pubkey,
            writable,
        };
        Self {
            program_id: program_id.to_string(),
            name: rule.name.clone(),
            account_discriminator: bs58::encode(&rule.account_discriminator).into_string(),
            rent_payer_offset: rule.rent_payer_offset as u64,
            ix_discriminator: rule.ix_discriminator.clone(),
            accounts: rule
                .accounts
                .iter()
                .map(|a| match a {
                    SweepAccount::Swept { writable } => acct("swept", None, *writable),
                    SweepAccount::FeePayer { writable } => acct("fee_payer", None, *writable),
                    SweepAccount::Fixed { pubkey, writable } => {
                        acct("fixed", Some(pubkey.to_string()), *writable)
                    }
                })
                .collect(),
            max_sends_per_run: rule.max_sends_per_run,
            max_simulations_per_run: rule.max_simulations_per_run,
        }
    }

    /// Cursor key in the controller's storage (one per program + sweep).
    pub fn cursor_key(&self) -> String {
        format!("cursor:{}:{}", self.program_id, self.name)
    }
}

/// The close tx from the template, signed by the fee payer (its only signer).
pub fn build_tx(
    spec: &SweepSpec,
    swept: &Pubkey,
    fee_payer: &Keypair,
    blockhash: &Hash,
) -> Result<VersionedTransaction, String> {
    let program: Pubkey = spec.program_id.parse().map_err(|_| "bad program id")?;
    let payer = fee_payer.pubkey();
    let metas = spec
        .accounts
        .iter()
        .map(|a| {
            let (key, signer) = match a.kind.as_str() {
                "swept" => (*swept, false),
                "fee_payer" => (payer, true),
                "fixed" => (
                    a.pubkey
                        .as_deref()
                        .and_then(|p| p.parse().ok())
                        .ok_or("fixed account without pubkey")?,
                    false,
                ),
                _ => return Err("unknown sweep account kind"),
            };
            Ok(if a.writable {
                AccountMeta::new(key, signer)
            } else {
                AccountMeta::new_readonly(key, signer)
            })
        })
        .collect::<Result<Vec<_>, &str>>()?;
    let ix = Instruction::new_with_bytes(program, &spec.ix_discriminator, metas);
    let msg = Message::new_with_blockhash(&[ix], Some(&payer), blockhash);
    if msg.header.num_required_signatures != 1 {
        // A fixed account can never become a signer; this guards the template invariant.
        return Err("sweep tx must have exactly one signer".into());
    }
    let message = VersionedMessage::Legacy(msg);
    let signature = fee_payer.sign_message(&message.serialize());
    Ok(VersionedTransaction {
        signatures: vec![signature],
        message,
    })
}

/// Sorted addresses and where this run starts: the first address after `cursor`. `None` cursor
/// (or one past the end) starts from the beginning.
pub fn order_from_cursor(addrs: &mut Vec<String>, cursor: Option<&str>) -> usize {
    addrs.sort();
    addrs.dedup();
    match cursor {
        Some(c) => addrs
            .iter()
            .position(|a| a.as_str() > c)
            .unwrap_or(addrs.len()),
        None => 0,
    }
}

/// §5 step 4: closing must bring rent back — `post − pre ≥ minimum_rent(0) − fee`.
pub fn rent_came_back(pre: u64, post: u64, fee: u64, min_rent0: u64) -> bool {
    post as i128 - pre as i128 >= min_rent0 as i128 - fee as i128
}

/// Lamports the close returned (for the audit log): `post − pre + fee`, floored at 0.
pub fn rent_returned(pre: u64, post: u64, fee: u64) -> u64 {
    (post as i128 - pre as i128 + fee as i128).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SweepSpec {
        SweepSpec {
            program_id: "Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW".into(),
            name: "close_room".into(),
            account_discriminator: bs58::encode([0x9c, 0xc7, 0x43, 0x1b, 0xde, 0x17, 0xb9, 0x5e])
                .into_string(),
            rent_payer_offset: 8,
            ix_discriminator: vec![0xf1, 0xed, 0x4a, 0xfd, 0xdf, 0xe9, 0x36, 0xdc],
            accounts: vec![
                SweepAcct {
                    kind: "swept".into(),
                    pubkey: None,
                    writable: true,
                },
                SweepAcct {
                    kind: "fee_payer".into(),
                    pubkey: None,
                    writable: true,
                },
            ],
            max_sends_per_run: 20,
            max_simulations_per_run: 60,
        }
    }

    #[test]
    fn builds_a_one_signer_close_tx_that_verifies() {
        let kp = Keypair::new();
        let swept = Pubkey::new_unique();
        let tx = build_tx(&spec(), &swept, &kp, &Hash::new_from_array([3; 32])).unwrap();
        assert_eq!(tx.signatures.len(), 1);
        assert!(tx.verify_and_hash_message().is_ok());
        let keys = tx.message.static_account_keys();
        assert_eq!(keys[0], kp.pubkey(), "fee payer first");
        let ix = &tx.message.instructions()[0];
        assert_eq!(ix.data, spec().ix_discriminator);
        // Template order: [swept, fee_payer].
        let order: Vec<Pubkey> = ix.accounts.iter().map(|i| keys[*i as usize]).collect();
        assert_eq!(order, vec![swept, kp.pubkey()]);
        assert_eq!(
            crate::writable_unsigned_keys(&tx),
            vec![swept],
            "swept is the only writable non-signer"
        );
    }

    #[test]
    fn fixed_accounts_and_bad_templates() {
        let kp = Keypair::new();
        let mut s = spec();
        let fixed = Pubkey::new_unique();
        s.accounts.push(SweepAcct {
            kind: "fixed".into(),
            pubkey: Some(fixed.to_string()),
            writable: false,
        });
        let tx = build_tx(&s, &Pubkey::new_unique(), &kp, &Hash::default()).unwrap();
        assert!(tx.message.static_account_keys().contains(&fixed));
        s.accounts[2].pubkey = None;
        assert!(build_tx(&s, &Pubkey::new_unique(), &kp, &Hash::default()).is_err());
    }

    #[test]
    fn cursor_resumes_after_the_last_examined_address() {
        let mut a = vec!["c".to_string(), "a".into(), "b".into(), "a".into()];
        assert_eq!(order_from_cursor(&mut a, None), 0);
        assert_eq!(a, ["a", "b", "c"]);
        assert_eq!(order_from_cursor(&mut a, Some("a")), 1);
        // A cursor whose account was closed meanwhile still resumes at the next one.
        assert_eq!(order_from_cursor(&mut a, Some("aa")), 1);
        assert_eq!(order_from_cursor(&mut a, Some("c")), 3, "past the end");
    }

    #[test]
    fn rent_must_come_back() {
        // Phase 0 spike: closing a 73-byte account returned rent − fee.
        let (pre, rent, fee, min0) = (5_000_000_000u64, 1_021_080, 5_000, 890_880);
        assert!(rent_came_back(pre, pre + rent - fee, fee, min0));
        assert_eq!(rent_returned(pre, pre + rent - fee, fee), rent);
        assert!(
            !rent_came_back(pre, pre - fee, fee, min0),
            "nothing came back"
        );
        assert!(
            !rent_came_back(pre, pre + 100_000, fee, min0),
            "less than the minimum rent"
        );
        assert_eq!(rent_returned(pre, pre - 2 * fee, fee), 0);
    }
}
