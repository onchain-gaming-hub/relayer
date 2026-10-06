//! Stateless transaction policy (spec §4.4 step 3). Pure functions, unit-tested natively.
//!
//! The only transaction shape the relayer will pay for:
//! keys = [relayer (fee payer), user (signer), Memo v2], 2 signatures, 1 Memo ix over [user].
//! Anything else — including message versions other than Legacy/V0 — is rejected.

use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

pub const MEMO_V2: Pubkey = Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// Hard upper bound for any project's memo limit (v1 `MAX_MEMO_BYTES`).
pub const MAX_MEMO_BYTES_CAP: usize = 128;
/// Base fee per signature (lamports); the policy forbids priority fees (spec v2 V7).
pub const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

/// The parts of a structurally valid transaction that the per-project check needs.
#[derive(Debug, PartialEq, Eq)]
pub struct Shape {
    pub fee_payer: Pubkey,
    pub user: Pubkey,
    pub memo: Vec<u8>,
}

/// Per-project memo rule (spec v2 V11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoRule {
    pub max_bytes: usize,
    pub prefix: Option<String>,
}

/// Project-independent checks (spec v2 §4.4 step 2): run before the idToken is verified.
pub fn structural(tx: &VersionedTransaction) -> Result<Shape, &'static str> {
    match &tx.message {
        VersionedMessage::Legacy(_) => {}
        // V0 is allowed only without address lookup tables.
        VersionedMessage::V0(m) if m.address_table_lookups.is_empty() => {}
        VersionedMessage::V0(_) => return Err("address lookup tables are not allowed"),
        // V1 carries a priority fee in its config; anything newer is unknown.
        _ => return Err("unsupported message version"),
    }
    tx.sanitize().map_err(|_| "transaction failed sanitize")?;

    let header = tx.message.header();
    if header.num_required_signatures != 2 || tx.signatures.len() != 2 {
        return Err("exactly 2 signatures required");
    }
    // Already implied by sanitize (readonly_signed < required_signatures = 2); kept as
    // defense-in-depth so this policy does not silently depend on sanitize's internals.
    if header.num_readonly_signed_accounts > 1 {
        return Err("invalid readonly signer count");
    }
    if header.num_readonly_unsigned_accounts != 1 {
        return Err("invalid readonly unsigned count");
    }

    let keys = tx.message.static_account_keys();
    let [fee_payer, user, program] = keys else {
        return Err("exactly 3 account keys required");
    };
    if *program != MEMO_V2 {
        return Err("program not allowed");
    }
    if user == fee_payer || *user == MEMO_V2 {
        return Err("invalid user key");
    }

    let [ix] = tx.message.instructions() else {
        return Err("exactly 1 instruction required");
    };
    if ix.program_id_index != 2 {
        return Err("program not allowed");
    }
    if ix.accounts.as_slice() != [1] {
        return Err("memo accounts must be [user]");
    }
    if ix.data.len() > MAX_MEMO_BYTES_CAP {
        return Err("memo too long");
    }
    if std::str::from_utf8(&ix.data).is_err() {
        return Err("memo is not utf-8");
    }
    Ok(Shape {
        fee_payer: *fee_payer,
        user: *user,
        memo: ix.data.clone(),
    })
}

/// Project-dependent checks (spec v2 §4.4 step 6). Returns the fee the relayer pays (V7).
pub fn for_project(
    shape: &Shape,
    fee_payer: &Pubkey,
    rule: &MemoRule,
) -> Result<u64, &'static str> {
    if shape.fee_payer != *fee_payer {
        return Err("fee payer is not the relayer");
    }
    if shape.memo.len() > rule.max_bytes {
        return Err("memo too long");
    }
    if let Some(prefix) = &rule.prefix
        && !shape.memo.starts_with(prefix.as_bytes())
    {
        return Err("memo prefix not allowed");
    }
    // 2 signatures (structural), no priority fee, no account creation.
    Ok(2 * LAMPORTS_PER_SIGNATURE)
}

/// Both stages with a plain memo limit; returns the user. Used by the unit tests.
#[cfg(test)]
pub fn validate(
    tx: &VersionedTransaction,
    relayer: &Pubkey,
    max_memo_bytes: usize,
) -> Result<Pubkey, &'static str> {
    let shape = structural(tx)?;
    let rule = MemoRule {
        max_bytes: max_memo_bytes,
        prefix: None,
    };
    for_project(&shape, relayer, &rule)?;
    Ok(shape.user)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_message::{Message, MessageHeader, compiled_instruction::CompiledInstruction, v0};
    use solana_signature::Signature;
    use solana_signer::Signer;

    pub fn memo_ix(user: &Pubkey, data: &[u8]) -> Instruction {
        Instruction::new_with_bytes(MEMO_V2, data, vec![AccountMeta::new_readonly(*user, true)])
    }

    pub fn signed(message: VersionedMessage, user: &Keypair) -> VersionedTransaction {
        let n = message.header().num_required_signatures as usize;
        let mut signatures = vec![Signature::default(); n];
        if let Some(i) = message
            .static_account_keys()
            .iter()
            .position(|k| *k == user.pubkey())
            && i < n
        {
            signatures[i] = user.sign_message(&message.serialize());
        }
        VersionedTransaction {
            signatures,
            message,
        }
    }

    pub fn good_tx(relayer: &Pubkey, user: &Keypair) -> VersionedTransaction {
        let msg = Message::new_with_blockhash(
            &[memo_ix(&user.pubkey(), b"gasless-poc:v1:mint:1")],
            Some(relayer),
            &Hash::new_from_array([7; 32]),
        );
        signed(VersionedMessage::Legacy(msg), user)
    }

    fn setup() -> (Pubkey, Keypair) {
        (Keypair::new().pubkey(), Keypair::new())
    }

    #[test]
    fn accepts_legacy_memo() {
        let (relayer, user) = setup();
        assert_eq!(
            validate(&good_tx(&relayer, &user), &relayer, 128),
            Ok(user.pubkey())
        );
    }

    #[test]
    fn accepts_v0_without_lookups_and_writable_user() {
        let (relayer, user) = setup();
        let ix =
            Instruction::new_with_bytes(MEMO_V2, b"m", vec![AccountMeta::new(user.pubkey(), true)]);
        let msg =
            v0::Message::try_compile(&relayer, &[ix], &[], Hash::new_from_array([7; 32])).unwrap();
        let tx = signed(VersionedMessage::V0(msg), &user);
        assert_eq!(validate(&tx, &relayer, 128), Ok(user.pubkey()));
    }

    #[test]
    fn rejects_v0_with_lookups() {
        let (relayer, user) = setup();
        let mut msg = v0::Message::try_compile(
            &relayer,
            &[memo_ix(&user.pubkey(), b"m")],
            &[],
            Hash::new_from_array([7; 32]),
        )
        .unwrap();
        msg.address_table_lookups
            .push(solana_message::v0::MessageAddressTableLookup {
                account_key: Keypair::new().pubkey(),
                writable_indexes: vec![],
                readonly_indexes: vec![0],
            });
        let tx = signed(VersionedMessage::V0(msg), &user);
        assert_eq!(
            validate(&tx, &relayer, 128),
            Err("address lookup tables are not allowed")
        );
    }

    #[test]
    fn rejects_foreign_fee_payer() {
        let (relayer, user) = setup();
        let tx = good_tx(&Keypair::new().pubkey(), &user);
        assert_eq!(
            validate(&tx, &relayer, 128),
            Err("fee payer is not the relayer")
        );
    }

    #[test]
    fn rejects_third_signer() {
        let (relayer, user) = setup();
        let extra = Keypair::new();
        let ix = Instruction::new_with_bytes(
            MEMO_V2,
            b"m",
            vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(extra.pubkey(), true),
            ],
        );
        let msg =
            Message::new_with_blockhash(&[ix], Some(&relayer), &Hash::new_from_array([7; 32]));
        let tx = signed(VersionedMessage::Legacy(msg), &user);
        assert_eq!(
            validate(&tx, &relayer, 128),
            Err("exactly 2 signatures required")
        );
    }

    #[test]
    fn rejects_other_program() {
        let (relayer, user) = setup();
        let other = Keypair::new().pubkey();
        let ix = Instruction::new_with_bytes(
            other,
            b"m",
            vec![AccountMeta::new_readonly(user.pubkey(), true)],
        );
        let msg =
            Message::new_with_blockhash(&[ix], Some(&relayer), &Hash::new_from_array([7; 32]));
        assert_eq!(
            validate(&signed(VersionedMessage::Legacy(msg), &user), &relayer, 128),
            Err("program not allowed")
        );
    }

    #[test]
    fn rejects_two_instructions() {
        let (relayer, user) = setup();
        let ixs = [memo_ix(&user.pubkey(), b"a"), memo_ix(&user.pubkey(), b"b")];
        let msg = Message::new_with_blockhash(&ixs, Some(&relayer), &Hash::new_from_array([7; 32]));
        assert_eq!(
            validate(&signed(VersionedMessage::Legacy(msg), &user), &relayer, 128),
            Err("exactly 1 instruction required")
        );
    }

    #[test]
    fn rejects_relayer_inside_instruction() {
        let (relayer, user) = setup();
        let ix = Instruction::new_with_bytes(
            MEMO_V2,
            b"m",
            vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(relayer, false),
            ],
        );
        let msg =
            Message::new_with_blockhash(&[ix], Some(&relayer), &Hash::new_from_array([7; 32]));
        assert_eq!(
            validate(&signed(VersionedMessage::Legacy(msg), &user), &relayer, 128),
            Err("memo accounts must be [user]")
        );
    }

    #[test]
    fn rejects_extra_account_key() {
        let (relayer, user) = setup();
        let ix = Instruction::new_with_bytes(
            MEMO_V2,
            b"m",
            vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(Keypair::new().pubkey(), false),
            ],
        );
        let msg =
            Message::new_with_blockhash(&[ix], Some(&relayer), &Hash::new_from_array([7; 32]));
        assert!(validate(&signed(VersionedMessage::Legacy(msg), &user), &relayer, 128).is_err());
    }

    #[test]
    fn rejects_long_and_non_utf8_memo() {
        let (relayer, user) = setup();
        let long = vec![b'a'; 129];
        let msg = Message::new_with_blockhash(
            &[memo_ix(&user.pubkey(), &long)],
            Some(&relayer),
            &Hash::new_from_array([7; 32]),
        );
        assert_eq!(
            validate(&signed(VersionedMessage::Legacy(msg), &user), &relayer, 128),
            Err("memo too long")
        );
        let msg = Message::new_with_blockhash(
            &[memo_ix(&user.pubkey(), &[0xff, 0xfe])],
            Some(&relayer),
            &Hash::new_from_array([7; 32]),
        );
        assert_eq!(
            validate(&signed(VersionedMessage::Legacy(msg), &user), &relayer, 128),
            Err("memo is not utf-8")
        );
    }

    #[test]
    fn rejects_bad_header_flags() {
        let (relayer, user) = setup();
        let mut tx = good_tx(&relayer, &user);
        if let VersionedMessage::Legacy(m) = &mut tx.message {
            m.header = MessageHeader {
                num_readonly_unsigned_accounts: 0,
                ..m.header
            };
        }
        assert_eq!(
            validate(&tx, &relayer, 128),
            Err("invalid readonly unsigned count")
        );
    }

    #[test]
    fn rejects_message_v1() {
        let (relayer, user) = setup();
        let msg = solana_message::v1::Message::try_compile_with_config(
            &relayer,
            &[memo_ix(&user.pubkey(), b"m")],
            Hash::new_from_array([7; 32]),
            solana_message::v1::TransactionConfig {
                priority_fee: Some(1_000_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        let tx = signed(VersionedMessage::V1(msg), &user);
        assert_eq!(
            validate(&tx, &relayer, 128),
            Err("unsupported message version")
        );
    }

    #[test]
    fn rejects_two_readonly_signers() {
        let (relayer, user) = setup();
        let mut tx = good_tx(&relayer, &user);
        if let VersionedMessage::Legacy(m) = &mut tx.message {
            m.header = MessageHeader {
                num_readonly_signed_accounts: 2,
                ..m.header
            };
        }
        assert!(validate(&tx, &relayer, 128).is_err());
    }

    #[test]
    fn rejects_memo_with_no_accounts() {
        let (relayer, user) = setup();
        let mut tx = good_tx(&relayer, &user);
        if let VersionedMessage::Legacy(m) = &mut tx.message {
            m.instructions = vec![CompiledInstruction::new_from_raw_parts(
                2,
                b"m".to_vec(),
                vec![],
            )];
        }
        assert_eq!(
            validate(&tx, &relayer, 128),
            Err("memo accounts must be [user]")
        );
    }

    #[test]
    fn project_rule_prefix_limit_and_cost() {
        let (relayer, user) = setup();
        let shape = structural(&good_tx(&relayer, &user)).unwrap();
        let rule = |max_bytes, prefix: Option<&str>| MemoRule {
            max_bytes,
            prefix: prefix.map(str::to_string),
        };
        assert_eq!(for_project(&shape, &relayer, &rule(128, None)), Ok(10_000));
        assert_eq!(
            for_project(&shape, &relayer, &rule(128, Some("gasless-poc:"))),
            Ok(10_000)
        );
        assert_eq!(
            for_project(&shape, &relayer, &rule(128, Some("other:"))),
            Err("memo prefix not allowed")
        );
        assert_eq!(
            for_project(&shape, &relayer, &rule(4, None)),
            Err("memo too long")
        );
        let other = Keypair::new().pubkey();
        assert_eq!(
            for_project(&shape, &other, &rule(128, None)),
            Err("fee payer is not the relayer")
        );
    }

    #[test]
    fn structural_rejects_memo_over_cap_before_any_project() {
        let (relayer, user) = setup();
        let msg = Message::new_with_blockhash(
            &[memo_ix(&user.pubkey(), &[b'a'; MAX_MEMO_BYTES_CAP + 1])],
            Some(&relayer),
            &Hash::new_from_array([7; 32]),
        );
        let tx = signed(VersionedMessage::Legacy(msg), &user);
        assert_eq!(structural(&tx), Err("memo too long"));
    }
}
