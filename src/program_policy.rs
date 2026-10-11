//! Program actions (spec v3 W1–W5, W8, §3): parse the `program` action of a project policy and
//! check a transaction against it. Pure functions, unit-tested natively.
//!
//! Safety model: the fee payer (`keys[0]`) may appear inside an instruction only at a registered
//! payer slot (funds `init` rent, W4) or refund slot (receives a close refund, W4b) of a
//! registered program. Every top-level instruction must belong to a registered program or be a
//! bounded ComputeBudget instruction, which also blocks attaching a top-level System
//! `CreateAccount` (Kora #720). What a trusted program does with the fee payer is verified by
//! simulation (Phase 2, §4.3) or trusted through a pinned program version (W6).

use serde_json::Value;
use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

use crate::policy::{LAMPORTS_PER_SIGNATURE, MEMO_V2};

pub const COMPUTE_BUDGET: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
pub const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// Programs a project may never register directly in v3 (spec §3). Precompiles are listed
/// because their signatures are part of the base fee, which W8 assumes is 2 × 5000.
const FORBIDDEN_PROGRAMS: [Pubkey; 14] = [
    Pubkey::from_str_const("Ed25519SigVerify111111111111111111111111111"),
    Pubkey::from_str_const("KeccakSecp256k11111111111111111111111111111"),
    Pubkey::from_str_const("Secp256r1SigVerify1111111111111111111111111"),
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111"),
    Pubkey::from_str_const("Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo"),
    SYSTEM_PROGRAM,
    COMPUTE_BUDGET,
    MEMO_V2,
    Pubkey::from_str_const("BPFLoader1111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111"),
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
];
/// Loader v4: also forbidden, and its programs cannot be pinned in v3 (W6).
const LOADER_V4: Pubkey = Pubkey::from_str_const("LoaderV411111111111111111111111111111111111");

/// Hard cap regardless of project config (a tx has a 1232-byte limit anyway).
pub const MAX_INSTRUCTIONS_CAP: u8 = 8;
/// Compute unit ceiling of the runtime (per transaction).
pub const MAX_CU_CAP: u32 = 1_400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    None,
    /// Fee payer funds `init` rent here (W4).
    Payer {
        index: u8,
        max_rent_lamports: u64,
    },
    /// Fee payer receives a close refund here (W4b).
    Refund {
        index: u8,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IxRule {
    pub name: String,
    pub discriminator: Vec<u8>,
    pub slot: Slot,
    pub min_accounts: u8,
    pub max_accounts: u8,
    pub max_data_len: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SweepAccount {
    Swept { writable: bool },
    FeePayer { writable: bool },
    Fixed { pubkey: Pubkey, writable: bool },
}

/// Cron sweep template (W13, used from Phase 4). Parsed and validated now so a bad row fails
/// closed when it is written, not when the sweep first runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepRule {
    pub name: String,
    pub account_discriminator: Vec<u8>,
    pub rent_payer_offset: u32,
    pub ix_discriminator: Vec<u8>,
    pub accounts: Vec<SweepAccount>,
    pub max_sends_per_run: u32,
    pub max_simulations_per_run: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    /// ProgramData `slot` must equal this (W6).
    DeploySlot(u64),
    /// Must be verified on-chain: upgrade authority = None (W6).
    Immutable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramRule {
    pub program_id: Pubkey,
    pub pin: Option<Pin>,
    pub verify_inner_ix: bool,
    pub retryable_errors: Vec<u32>,
    pub instructions: Vec<IxRule>,
    pub sweeps: Vec<SweepRule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_instructions: u8,
    pub max_cu: u32,
    pub max_cu_price_micro_lamports: u64,
    pub skip_preflight_after_verify: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_instructions: 4,
            max_cu: 400_000,
            max_cu_price_micro_lamports: 0,
            skip_preflight_after_verify: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramPolicy {
    pub programs: Vec<ProgramRule>,
    pub limits: Limits,
}

impl ProgramPolicy {
    /// Largest declared rent of any one payer slot (for the `min_balance` rule, §3).
    pub fn max_rent_per_ix(&self) -> u64 {
        self.programs
            .iter()
            .flat_map(|p| &p.instructions)
            .filter_map(|i| match i.slot {
                Slot::Payer {
                    max_rent_lamports, ..
                } => Some(max_rent_lamports),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------- parsing (§3)

fn hex_bytes(v: Option<&Value>, what: &str, max: usize) -> Result<Vec<u8>, String> {
    let s = v
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{what}: hex string required"))?;
    // ASCII first: byte-slicing a multi-byte char would panic and take the whole registry down.
    if !s.is_ascii() || s.is_empty() || s.len() % 2 != 0 || s.len() / 2 > max {
        return Err(format!("{what}: 1..={max} bytes of hex"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("{what}: not hex")))
        .collect()
}

fn uint(v: Option<&Value>, what: &str, max: u64) -> Result<u64, String> {
    match v.and_then(Value::as_u64) {
        Some(n) if n <= max => Ok(n),
        _ => Err(format!("{what}: integer 0..={max} required")),
    }
}

fn opt_uint(v: Option<&Value>, what: &str, max: u64, default: u64) -> Result<u64, String> {
    match v {
        None | Some(Value::Null) => Ok(default),
        some => uint(some, what, max),
    }
}

fn pubkey(v: Option<&Value>, what: &str) -> Result<Pubkey, String> {
    v.and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{what}: base58 pubkey required"))
}

fn flag(v: Option<&Value>, what: &str, default: bool) -> Result<bool, String> {
    match v {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{what}: boolean required")),
    }
}

fn parse_ix(v: &Value, program: &str) -> Result<IxRule, String> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("{program}: instruction name required"))?
        .to_string();
    let w = format!("{program}.{name}");
    let discriminator = hex_bytes(v.get("discriminator"), &format!("{w}.discriminator"), 8)?;
    let min_accounts = uint(v.get("min_accounts"), &format!("{w}.min_accounts"), 64)? as u8;
    let max_accounts = uint(v.get("max_accounts"), &format!("{w}.max_accounts"), 64)? as u8;
    if min_accounts > max_accounts {
        return Err(format!("{w}: min_accounts > max_accounts"));
    }
    let max_data_len = uint(v.get("max_data_len"), &format!("{w}.max_data_len"), 1232)? as u16;
    if (max_data_len as usize) < discriminator.len() {
        return Err(format!("{w}: max_data_len shorter than the discriminator"));
    }
    let payer = v.get("payer_index").filter(|x| !x.is_null());
    let refund = v.get("refund_index").filter(|x| !x.is_null());
    let max_rent = v.get("max_rent_lamports").filter(|x| !x.is_null());
    let slot = match (payer, refund) {
        (Some(_), Some(_)) => {
            return Err(format!("{w}: payer_index and refund_index are exclusive"));
        }
        (Some(p), None) => {
            let index = uint(Some(p), &format!("{w}.payer_index"), 63)? as u8;
            let max_rent_lamports =
                uint(max_rent, &format!("{w}.max_rent_lamports"), u32::MAX as u64)?;
            if max_rent_lamports == 0 {
                return Err(format!("{w}: max_rent_lamports must be > 0"));
            }
            Slot::Payer {
                index,
                max_rent_lamports,
            }
        }
        (None, Some(r)) => {
            if max_rent.is_some() {
                return Err(format!("{w}: refund slot cannot declare max_rent_lamports"));
            }
            Slot::Refund {
                index: uint(Some(r), &format!("{w}.refund_index"), 63)? as u8,
            }
        }
        (None, None) => {
            if max_rent.is_some() {
                return Err(format!("{w}: max_rent_lamports needs payer_index"));
            }
            Slot::None
        }
    };
    if let Slot::Payer { index, .. } | Slot::Refund { index } = slot {
        if index >= min_accounts {
            return Err(format!("{w}: slot index must be < min_accounts"));
        }
        // Anchor discriminators are 8 bytes; a short prefix could match a different ix (§3).
        if discriminator.len() != 8 {
            return Err(format!(
                "{w}: slot instructions need an 8-byte discriminator"
            ));
        }
    }
    Ok(IxRule {
        name,
        discriminator,
        slot,
        min_accounts,
        max_accounts,
        max_data_len,
    })
}

fn parse_sweep(v: &Value, program: &str) -> Result<SweepRule, String> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("{program}: sweep name required"))?
        .to_string();
    let w = format!("{program}.sweeps.{name}");
    let accounts: Vec<SweepAccount> = v
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{w}.accounts: array required"))?
        .iter()
        .map(|a| {
            let writable = flag(a.get("writable"), &format!("{w}.accounts.writable"), false)?;
            match a.get("kind").and_then(Value::as_str) {
                Some("swept") => Ok(SweepAccount::Swept { writable }),
                Some("fee_payer") => Ok(SweepAccount::FeePayer { writable }),
                Some("fixed") => Ok(SweepAccount::Fixed {
                    pubkey: pubkey(a.get("pubkey"), &format!("{w}.accounts.pubkey"))?,
                    writable,
                }),
                other => Err(format!("{w}.accounts: unknown kind {other:?}")),
            }
        })
        .collect::<Result<_, String>>()?;
    let swept = accounts
        .iter()
        .filter(|a| matches!(a, SweepAccount::Swept { .. }))
        .count();
    if swept != 1 {
        return Err(format!("{w}: exactly one swept account required"));
    }
    // The rent comes back to the fee payer (the close target) and the swept account is closed:
    // both must be writable, and the fee payer appears exactly once.
    let fee_payers: Vec<_> = accounts
        .iter()
        .filter_map(|a| match a {
            SweepAccount::FeePayer { writable } => Some(*writable),
            _ => None,
        })
        .collect();
    if fee_payers != [true] {
        return Err(format!(
            "{w}: exactly one writable fee_payer account required"
        ));
    }
    if !accounts
        .iter()
        .any(|a| matches!(a, SweepAccount::Swept { writable: true }))
    {
        return Err(format!("{w}: the swept account must be writable"));
    }
    Ok(SweepRule {
        account_discriminator: hex_bytes(
            v.get("account_discriminator"),
            &format!("{w}.account_discriminator"),
            8,
        )?,
        rent_payer_offset: uint(
            v.get("rent_payer_offset"),
            &format!("{w}.rent_payer_offset"),
            10_240,
        )? as u32,
        ix_discriminator: hex_bytes(
            v.get("ix_discriminator"),
            &format!("{w}.ix_discriminator"),
            8,
        )?,
        max_sends_per_run: uint(
            v.get("max_sends_per_run"),
            &format!("{w}.max_sends_per_run"),
            100,
        )? as u32,
        max_simulations_per_run: uint(
            v.get("max_simulations_per_run"),
            &format!("{w}.max_simulations_per_run"),
            300,
        )? as u32,
        accounts,
        name,
    })
}

pub fn parse_program(v: &Value) -> Result<ProgramRule, String> {
    let program_id = pubkey(v.get("program_id"), "program.program_id")?;
    if FORBIDDEN_PROGRAMS.contains(&program_id) || program_id == LOADER_V4 {
        return Err(format!(
            "program {program_id}: not allowed as a program action in v3"
        ));
    }
    let p = program_id.to_string();
    let immutable = flag(v.get("immutable"), &format!("{p}.immutable"), false)?;
    let deploy_slot = match v.get("deploy_slot") {
        None | Some(Value::Null) => None,
        some => Some(uint(some, &format!("{p}.deploy_slot"), u64::MAX)?),
    };
    let pin = match (immutable, deploy_slot) {
        (true, Some(_)) => return Err(format!("{p}: immutable and deploy_slot are exclusive")),
        (true, None) => Some(Pin::Immutable),
        (false, Some(s)) => Some(Pin::DeploySlot(s)),
        (false, None) => None,
    };
    let verify_inner_ix = flag(
        v.get("verify_inner_ix"),
        &format!("{p}.verify_inner_ix"),
        true,
    )?;
    let retryable_errors = match v.get("retryable_errors") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|c| {
                uint(Some(c), &format!("{p}.retryable_errors"), u32::MAX as u64).map(|n| n as u32)
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(format!("{p}.retryable_errors: array required")),
    };
    let instructions: Vec<IxRule> = v
        .get("instructions")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("{p}.instructions: non-empty array required"))?
        .iter()
        .map(|i| parse_ix(i, &p))
        .collect::<Result<_, _>>()?;
    // Discriminators must never be ambiguous: no duplicate, no one a prefix of another.
    for (i, a) in instructions.iter().enumerate() {
        for b in &instructions[i + 1..] {
            if a.discriminator.starts_with(&b.discriminator)
                || b.discriminator.starts_with(&a.discriminator)
            {
                return Err(format!(
                    "{p}: discriminators of {} and {} overlap",
                    a.name, b.name
                ));
            }
        }
    }
    let has_slot = instructions.iter().any(|i| i.slot != Slot::None);
    if (has_slot || !verify_inner_ix) && pin.is_none() {
        return Err(format!(
            "{p}: payer/refund slots or verify_inner_ix=false require deploy_slot or immutable (W6)"
        ));
    }
    let sweeps = match v.get("sweeps") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| parse_sweep(s, &p))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(format!("{p}.sweeps: array required")),
    };
    // The sweep signs and pays for txs no user asked for: same W6 bar as a payer slot.
    if !sweeps.is_empty() && pin.is_none() {
        return Err(format!("{p}: sweeps require deploy_slot or immutable (W6)"));
    }
    Ok(ProgramRule {
        program_id,
        pin,
        verify_inner_ix,
        retryable_errors,
        instructions,
        sweeps,
    })
}

pub fn parse_limits(v: Option<&Value>) -> Result<Limits, String> {
    let d = Limits::default();
    let Some(v) = v.filter(|x| !x.is_null()) else {
        return Ok(d);
    };
    let max_instructions = opt_uint(
        v.get("max_instructions"),
        "limits.max_instructions",
        MAX_INSTRUCTIONS_CAP as u64,
        d.max_instructions as u64,
    )? as u8;
    if max_instructions == 0 {
        return Err("limits.max_instructions must be >= 1".into());
    }
    Ok(Limits {
        max_instructions,
        max_cu: opt_uint(
            v.get("max_cu"),
            "limits.max_cu",
            MAX_CU_CAP as u64,
            d.max_cu as u64,
        )? as u32,
        max_cu_price_micro_lamports: opt_uint(
            v.get("max_cu_price_micro_lamports"),
            "limits.max_cu_price_micro_lamports",
            10_000_000,
            0,
        )?,
        skip_preflight_after_verify: flag(
            v.get("skip_preflight_after_verify"),
            "limits.skip_preflight_after_verify",
            false,
        )?,
    })
}

// ---------------------------------------------------------------- checking (W2–W5, W8)

/// Project-independent shape of a program-action tx (W2, W5), checked before identity.
pub fn structural(tx: &VersionedTransaction) -> Result<Pubkey, &'static str> {
    match &tx.message {
        VersionedMessage::Legacy(_) => {}
        VersionedMessage::V0(m) if m.address_table_lookups.is_empty() => {}
        VersionedMessage::V0(_) => return Err("address lookup tables are not allowed"),
        _ => return Err("unsupported message version"),
    }
    tx.sanitize().map_err(|_| "transaction failed sanitize")?;
    let header = tx.message.header();
    if header.num_required_signatures != 2 || tx.signatures.len() != 2 {
        return Err("exactly 2 signatures required");
    }
    let keys = tx.message.static_account_keys();
    let n = tx.message.instructions().len();
    if n == 0 {
        return Err("no instructions");
    }
    if n > MAX_INSTRUCTIONS_CAP as usize {
        return Err("too many instructions");
    }
    // No account key may appear twice (the runtime rejects it too; the policy does not rely on it).
    for (i, k) in keys.iter().enumerate() {
        if keys[i + 1..].contains(k) {
            return Err("duplicate account key");
        }
    }
    Ok(keys[1])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramCheck {
    pub user: Pubkey,
    pub fee_cost: u64,
    pub declared_rent: u64,
    /// At least one instruction of a program with `verify_inner_ix` (W7).
    pub needs_simulation: bool,
    /// Programs whose version must be pinned before relaying (W6).
    pub pinned_programs: Vec<(Pubkey, Pin)>,
    /// Registered program of each top-level instruction (None = ComputeBudget), for W11.
    pub ix_programs: Vec<Option<Pubkey>>,
    pub has_payer_slot: bool,
}

fn compute_budget(data: &[u8], accounts: &[u8]) -> Result<(u8, u64), &'static str> {
    if !accounts.is_empty() {
        return Err("compute budget instruction with accounts");
    }
    match data {
        [2, rest @ ..] if rest.len() == 4 => {
            Ok((2, u32::from_le_bytes(rest.try_into().unwrap()) as u64))
        }
        [3, rest @ ..] if rest.len() == 8 => Ok((3, u64::from_le_bytes(rest.try_into().unwrap()))),
        _ => Err("compute budget instruction not allowed"),
    }
}

/// W11: per top-level ix index, the custom error codes its program registered as retryable
/// (empty for ComputeBudget and programs without any). Indexes match `InstructionError[i, ..]`.
pub fn retryable_by_ix(tx: &VersionedTransaction, policy: &ProgramPolicy) -> Vec<Vec<u32>> {
    let keys = tx.message.static_account_keys();
    tx.message
        .instructions()
        .iter()
        .map(|ix| {
            keys.get(ix.program_id_index as usize)
                .and_then(|id| policy.programs.iter().find(|p| p.program_id == *id))
                .map(|p| p.retryable_errors.clone())
                .unwrap_or_default()
        })
        .collect()
}

/// Checks a program-action tx against a project's policy (W2–W5) and returns its declared cost (W8).
pub fn check(
    tx: &VersionedTransaction,
    fee_payer: &Pubkey,
    policy: &ProgramPolicy,
) -> Result<ProgramCheck, &'static str> {
    let user = structural(tx)?;
    let keys = tx.message.static_account_keys();
    if keys[0] != *fee_payer {
        return Err("fee payer is not the relayer");
    }
    let ixs = tx.message.instructions();
    if ixs.len() > policy.limits.max_instructions as usize {
        return Err("too many instructions");
    }
    let (mut cu_limit, mut cu_price) = (None::<u64>, None::<u64>);
    let (mut declared_rent, mut needs_simulation) = (0u64, false);
    let (mut has_payer, mut has_refund, mut has_program) = (false, false, false);
    let mut pinned: Vec<(Pubkey, Pin)> = Vec::new();
    let mut ix_programs = Vec::with_capacity(ixs.len());

    for ix in ixs {
        let program = keys
            .get(ix.program_id_index as usize)
            .ok_or("bad program index")?;
        if *program == *fee_payer {
            return Err("fee payer cannot be a program");
        }
        if *program == COMPUTE_BUDGET {
            match compute_budget(&ix.data, &ix.accounts)? {
                (2, v) if cu_limit.is_none() => cu_limit = Some(v),
                (3, v) if cu_price.is_none() => cu_price = Some(v),
                _ => return Err("duplicate compute budget instruction"),
            }
            ix_programs.push(None);
            continue;
        }
        let rule = policy
            .programs
            .iter()
            .find(|p| p.program_id == *program)
            .ok_or("program not allowed")?;
        let ixr = rule
            .instructions
            .iter()
            .find(|i| ix.data.starts_with(&i.discriminator))
            .ok_or("instruction not allowed")?;
        let n = ix.accounts.len();
        if n < ixr.min_accounts as usize || n > ixr.max_accounts as usize {
            return Err("instruction account count not allowed");
        }
        if ix.data.len() > ixr.max_data_len as usize {
            return Err("instruction data too long");
        }
        let slot_index = match ixr.slot {
            Slot::Payer {
                index,
                max_rent_lamports,
            } => {
                has_payer = true;
                declared_rent = declared_rent
                    .checked_add(max_rent_lamports)
                    .ok_or("rent overflow")?;
                Some(index as usize)
            }
            Slot::Refund { index } => {
                has_refund = true;
                Some(index as usize)
            }
            Slot::None => None,
        };
        // W3: the fee payer (by pubkey) only at the slot, and the slot holds only the fee payer.
        for (pos, &k) in ix.accounts.iter().enumerate() {
            let key = keys.get(k as usize).ok_or("bad account index")?;
            let is_fp = key == fee_payer;
            match (Some(pos) == slot_index, is_fp) {
                (true, false) => return Err("slot must hold the relayer"),
                (false, true) => return Err("relayer not allowed in this instruction"),
                _ => {}
            }
        }
        has_program = true;
        needs_simulation |= rule.verify_inner_ix;
        if let Some(pin) = rule.pin
            && !pinned.iter().any(|(p, _)| p == program)
        {
            pinned.push((*program, pin));
        }
        ix_programs.push(Some(*program));
    }
    if !has_program {
        return Err("no program instruction");
    }
    if has_payer && has_refund {
        return Err("payer and refund slots in one transaction");
    }
    // W5: ceilings; a price needs an explicit limit so the priority fee is exact.
    let limit = cu_limit.unwrap_or(0);
    if limit > policy.limits.max_cu as u64 {
        return Err("compute unit limit above ceiling");
    }
    let price = cu_price.unwrap_or(0);
    if price > policy.limits.max_cu_price_micro_lamports {
        return Err("compute unit price above ceiling");
    }
    if price > 0 && cu_limit.is_none() {
        return Err("compute unit price needs a compute unit limit");
    }
    let priority = (limit as u128 * price as u128).div_ceil(1_000_000) as u64;
    Ok(ProgramCheck {
        user,
        fee_cost: 2 * LAMPORTS_PER_SIGNATURE + priority,
        declared_rent,
        needs_simulation,
        pinned_programs: pinned,
        ix_programs,
        has_payer_slot: has_payer,
    })
}

/// True when the tx has the v2 memo shape (W15): one instruction, program = Memo v2.
pub fn is_memo_shape(tx: &VersionedTransaction) -> bool {
    let keys = tx.message.static_account_keys();
    matches!(tx.message.instructions(), [ix] if keys.get(ix.program_id_index as usize) == Some(&MEMO_V2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_message::Message;
    use solana_signature::Signature;
    use solana_signer::Signer;

    const PROG: Pubkey = Pubkey::from_str_const("Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW");
    const CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
    const CLOSE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    const NOOP: [u8; 8] = [9, 9, 9, 9, 9, 9, 9, 9];

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn policy_json() -> Value {
        json!({
            "program_id": PROG.to_string(),
            "deploy_slot": 508764926u64,
            "retryable_errors": [6000],
            "instructions": [
                { "name": "create", "discriminator": hex(&CREATE), "payer_index": 1, "max_rent_lamports": 1_100_000,
                  "min_accounts": 4, "max_accounts": 4, "max_data_len": 8 },
                { "name": "close_thing", "discriminator": hex(&CLOSE), "refund_index": 2,
                  "min_accounts": 3, "max_accounts": 3, "max_data_len": 8 },
                { "name": "noop", "discriminator": hex(&NOOP), "min_accounts": 0, "max_accounts": 1, "max_data_len": 8 }
            ]
        })
    }

    fn policy() -> ProgramPolicy {
        ProgramPolicy {
            programs: vec![parse_program(&policy_json()).unwrap()],
            limits: Limits {
                max_cu_price_micro_lamports: 1_000,
                ..Limits::default()
            },
        }
    }

    fn tx(relayer: &Pubkey, _user: &Keypair, ixs: &[Instruction]) -> VersionedTransaction {
        let msg = Message::new_with_blockhash(ixs, Some(relayer), &Hash::new_from_array([7; 32]));
        let n = msg.header.num_required_signatures as usize;
        VersionedTransaction {
            signatures: vec![Signature::default(); n],
            message: VersionedMessage::Legacy(msg),
        }
    }

    fn create(user: &Pubkey, payer: &Pubkey) -> Instruction {
        Instruction::new_with_bytes(
            PROG,
            &CREATE,
            vec![
                AccountMeta::new_readonly(*user, true),
                AccountMeta::new(*payer, true),
                AccountMeta::new(Pubkey::new_unique(), false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            ],
        )
    }

    fn close(user: &Pubkey, dest: &Pubkey) -> Instruction {
        Instruction::new_with_bytes(
            PROG,
            &CLOSE,
            vec![
                AccountMeta::new_readonly(*user, true),
                AccountMeta::new(Pubkey::new_unique(), false),
                AccountMeta::new(*dest, false),
            ],
        )
    }

    fn noop(user: &Pubkey) -> Instruction {
        Instruction::new_with_bytes(PROG, &NOOP, vec![AccountMeta::new_readonly(*user, true)])
    }

    fn cu_limit(n: u32) -> Instruction {
        let mut d = vec![2];
        d.extend_from_slice(&n.to_le_bytes());
        Instruction::new_with_bytes(COMPUTE_BUDGET, &d, vec![])
    }

    fn cu_price(n: u64) -> Instruction {
        let mut d = vec![3];
        d.extend_from_slice(&n.to_le_bytes());
        Instruction::new_with_bytes(COMPUTE_BUDGET, &d, vec![])
    }

    #[test]
    fn payer_slot_declares_rent_and_needs_simulation() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        let c = check(&tx(&r, &u, &[create(&u.pubkey(), &r)]), &r, &policy()).unwrap();
        assert_eq!(c.user, u.pubkey());
        assert_eq!(c.fee_cost, 10_000);
        assert_eq!(c.declared_rent, 1_100_000);
        assert!(c.needs_simulation);
        assert_eq!(c.pinned_programs, vec![(PROG, Pin::DeploySlot(508764926))]);
        assert_eq!(c.ix_programs, vec![Some(PROG)]);
    }

    #[test]
    fn refund_slot_and_crank_shapes() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        let c = check(&tx(&r, &u, &[close(&u.pubkey(), &r)]), &r, &policy()).unwrap();
        assert_eq!(c.declared_rent, 0);
        // Crank: user rides as a remaining readonly signer (W12).
        assert!(check(&tx(&r, &u, &[noop(&u.pubkey())]), &r, &policy()).is_ok());
    }

    #[test]
    fn slot_must_hold_the_relayer_and_relayer_only_at_slots() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        // User puts itself in the payer slot.
        let e = check(
            &tx(&r, &u, &[create(&u.pubkey(), &u.pubkey())]),
            &r,
            &policy(),
        )
        .unwrap_err();
        assert_eq!(e, "slot must hold the relayer");
        // Refund to someone else.
        let e = check(
            &tx(&r, &u, &[close(&u.pubkey(), &Pubkey::new_unique())]),
            &r,
            &policy(),
        )
        .unwrap_err();
        assert_eq!(e, "slot must hold the relayer");
        // Relayer inside an instruction without a slot.
        let ix = Instruction::new_with_bytes(PROG, &NOOP, vec![AccountMeta::new(r, true)]);
        assert_eq!(
            check(&tx(&r, &u, &[ix, noop(&u.pubkey())]), &r, &policy()).unwrap_err(),
            "relayer not allowed in this instruction"
        );
    }

    #[test]
    fn kora_720_attachment_and_unregistered_programs_rejected() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        let create_acct = Instruction::new_with_bytes(
            SYSTEM_PROGRAM,
            &[0; 52],
            vec![
                AccountMeta::new(r, true),
                AccountMeta::new(Pubkey::new_unique(), true),
            ],
        );
        let t = tx(&r, &u, &[noop(&u.pubkey()), create_acct]);
        assert!(check(&t, &r, &policy()).is_err());
        let other = Instruction::new_with_bytes(
            Pubkey::new_unique(),
            &NOOP,
            vec![AccountMeta::new_readonly(u.pubkey(), true)],
        );
        assert_eq!(
            check(&tx(&r, &u, &[other]), &r, &policy()).unwrap_err(),
            "program not allowed"
        );
        let unknown_ix = Instruction::new_with_bytes(
            PROG,
            &[42; 8],
            vec![AccountMeta::new_readonly(u.pubkey(), true)],
        );
        assert_eq!(
            check(&tx(&r, &u, &[unknown_ix]), &r, &policy()).unwrap_err(),
            "instruction not allowed"
        );
    }

    #[test]
    fn mixing_payer_and_refund_slots_rejected() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        let t = tx(&r, &u, &[create(&u.pubkey(), &r), close(&u.pubkey(), &r)]);
        assert_eq!(
            check(&t, &r, &policy()).unwrap_err(),
            "payer and refund slots in one transaction"
        );
    }

    #[test]
    fn compute_budget_ceilings_and_priority_fee() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        let n = noop(&u.pubkey());
        let c = check(
            &tx(&r, &u, &[cu_limit(300_000), cu_price(1_000), n.clone()]),
            &r,
            &policy(),
        )
        .unwrap();
        assert_eq!(c.fee_cost, 10_000 + 300); // 300_000 × 1_000 / 1e6
        assert_eq!(c.ix_programs, vec![None, None, Some(PROG)]);
        assert_eq!(
            check(&tx(&r, &u, &[cu_limit(500_000), n.clone()]), &r, &policy()).unwrap_err(),
            "compute unit limit above ceiling"
        );
        assert_eq!(
            check(
                &tx(&r, &u, &[cu_limit(1), cu_price(2_000), n.clone()]),
                &r,
                &policy()
            )
            .unwrap_err(),
            "compute unit price above ceiling"
        );
        assert_eq!(
            check(&tx(&r, &u, &[cu_price(10), n.clone()]), &r, &policy()).unwrap_err(),
            "compute unit price needs a compute unit limit"
        );
        assert_eq!(
            check(
                &tx(&r, &u, &[cu_limit(1), cu_limit(2), n.clone()]),
                &r,
                &policy()
            )
            .unwrap_err(),
            "duplicate compute budget instruction"
        );
        let heap = Instruction::new_with_bytes(COMPUTE_BUDGET, &[1, 0, 0, 1, 0], vec![]);
        assert_eq!(
            check(&tx(&r, &u, &[heap, n.clone()]), &r, &policy()).unwrap_err(),
            "compute budget instruction not allowed"
        );
        // Only compute budget, no program instruction.
        assert_eq!(
            check(&tx(&r, &u, &[cu_limit(1)]), &r, &policy()),
            Err("exactly 2 signatures required")
        );
    }

    #[test]
    fn shape_limits() {
        let (r, u) = (Keypair::new().pubkey(), Keypair::new());
        let n = noop(&u.pubkey());
        let five = vec![n.clone(), n.clone(), n.clone(), n.clone(), n.clone()];
        assert_eq!(
            check(&tx(&r, &u, &five), &r, &policy()).unwrap_err(),
            "too many instructions"
        );
        let long = Instruction::new_with_bytes(
            PROG,
            &[NOOP.as_slice(), &[0]].concat(),
            vec![AccountMeta::new_readonly(u.pubkey(), true)],
        );
        assert_eq!(
            check(&tx(&r, &u, &[long]), &r, &policy()).unwrap_err(),
            "instruction data too long"
        );
        let other_payer = Keypair::new().pubkey();
        assert_eq!(
            check(&tx(&other_payer, &u, &[noop(&u.pubkey())]), &r, &policy()).unwrap_err(),
            "fee payer is not the relayer"
        );
        assert!(is_memo_shape(&tx(
            &r,
            &u,
            &[Instruction::new_with_bytes(
                MEMO_V2,
                b"x",
                vec![AccountMeta::new_readonly(u.pubkey(), true)]
            )]
        )));
        assert!(!is_memo_shape(&tx(&r, &u, &[n])));
    }

    #[test]
    fn parse_rejects_unsafe_or_ambiguous_programs() {
        let base = policy_json();
        let with = |f: &dyn Fn(&mut Value)| {
            let mut v = base.clone();
            f(&mut v);
            parse_program(&v)
        };
        assert!(with(&|_| {}).is_ok());
        assert!(
            with(&|v| v["deploy_slot"] = Value::Null).is_err(),
            "slots need a pin"
        );
        assert!(with(&|v| { v["deploy_slot"] = Value::Null; v["instructions"] = json!([{ "name":"noop","discriminator":hex(&NOOP),"min_accounts":0,"max_accounts":1,"max_data_len":8 }]); }).is_ok(), "no slot + verify on: pin optional");
        assert!(with(&|v| { v["deploy_slot"] = Value::Null; v["verify_inner_ix"] = json!(false); v["instructions"] = json!([{ "name":"noop","discriminator":hex(&NOOP),"min_accounts":0,"max_accounts":1,"max_data_len":8 }]); }).is_err(), "verify off needs a pin");
        assert!(
            with(&|v| v["immutable"] = json!(true)).is_err(),
            "immutable + deploy_slot"
        );
        assert!(with(&|v| v["program_id"] = json!(SYSTEM_PROGRAM.to_string())).is_err());
        assert!(with(&|v| v["program_id"] = json!(MEMO_V2.to_string())).is_err());
        assert!(
            with(&|v| v["instructions"][2]["discriminator"] = json!(hex(&CREATE[..4]))).is_err(),
            "prefix overlap"
        );
        assert!(
            with(&|v| v["instructions"][0]["discriminator"] = json!("1830c828")).is_err(),
            "slot needs 8 bytes"
        );
        // Non-ASCII must be an error, not a panic (a panic would take the whole registry down).
        assert!(with(&|v| v["instructions"][2]["discriminator"] = json!("€a")).is_err());
        assert!(
            with(&|v| v["program_id"] = json!("Ed25519SigVerify111111111111111111111111111"))
                .is_err(),
            "precompile"
        );
        assert!(with(&|v| v["instructions"][0]["max_rent_lamports"] = json!(0)).is_err());
        assert!(
            with(&|v| v["instructions"][0]["payer_index"] = json!(4)).is_err(),
            "index >= min_accounts"
        );
        assert!(
            with(&|v| v["instructions"][1]["max_rent_lamports"] = json!(1)).is_err(),
            "refund with rent"
        );
        assert!(
            with(&|v| v["instructions"][1]["payer_index"] = json!(0)).is_err(),
            "both slots"
        );
        assert!(with(&|v| v["instructions"] = json!([])).is_err());
        assert!(with(&|v| v["sweeps"] = json!([{ "name":"s","account_discriminator":"0102","rent_payer_offset":40,"ix_discriminator":"0304","accounts":[{"kind":"fee_payer","writable":true}],"max_sends_per_run":5,"max_simulations_per_run":10 }])).is_err(), "sweep needs a swept account");
        assert!(with(&|v| v["sweeps"] = json!([{ "name":"s","account_discriminator":"0102","rent_payer_offset":40,"ix_discriminator":"0304","accounts":[{"kind":"swept","writable":true},{"kind":"fee_payer","writable":true}],"max_sends_per_run":5,"max_simulations_per_run":10 }])).is_ok());
        let sweep = |accounts: Value| json!([{ "name":"s","account_discriminator":"0102","rent_payer_offset":8,"ix_discriminator":"0304","accounts":accounts,"max_sends_per_run":5,"max_simulations_per_run":10 }]);
        let err = |accounts: Value| with(&|v| v["sweeps"] = sweep(accounts.clone())).unwrap_err();
        assert!(err(json!([{"kind":"swept","writable":true}])).contains("fee_payer"));
        assert!(
            err(json!([{"kind":"swept","writable":true},{"kind":"fee_payer","writable":false}]))
                .contains("fee_payer")
        );
        assert!(err(json!([{"kind":"swept","writable":true},{"kind":"fee_payer","writable":true},{"kind":"fee_payer","writable":true}])).contains("fee_payer"));
        assert!(
            err(json!([{"kind":"swept","writable":false},{"kind":"fee_payer","writable":true}]))
                .contains("swept account must be writable")
        );
        // Unpinned program with a sweep (no payer slot, verify on): still needs a pin.
        let mut v = policy_json();
        v["sweeps"] =
            sweep(json!([{"kind":"swept","writable":true},{"kind":"fee_payer","writable":true}]));
        v.as_object_mut().unwrap().remove("deploy_slot");
        v["instructions"] = json!([{ "name": "noop", "discriminator": "0909090909090909", "min_accounts": 0, "max_accounts": 1, "max_data_len": 8 }]);
        assert!(parse_program(&v).unwrap_err().contains("sweeps require"));
    }

    #[test]
    fn limits_defaults_and_caps() {
        assert_eq!(parse_limits(None).unwrap(), Limits::default());
        assert!(parse_limits(Some(&json!({ "max_instructions": 0 }))).is_err());
        assert!(parse_limits(Some(&json!({ "max_instructions": 9 }))).is_err());
        assert!(parse_limits(Some(&json!({ "max_cu": 1_400_001 }))).is_err());
        assert_eq!(
            parse_limits(Some(&json!({ "max_cu": 200_000 })))
                .unwrap()
                .max_cu,
            200_000
        );
    }

    #[test]
    fn retryable_codes_follow_top_level_ix_index() {
        let (relayer, user) = (Pubkey::new_unique(), Keypair::new());
        let t = tx(&relayer, &user, &[cu_limit(1_000), noop(&user.pubkey())]);
        assert_eq!(retryable_by_ix(&t, &policy()), vec![vec![], vec![6000]]);
    }
}
