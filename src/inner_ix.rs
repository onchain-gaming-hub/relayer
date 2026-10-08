//! Simulation-based verification of what a program does with the fee payer (spec v3 §4.3, W4b).
//! Pure functions over the JSON of a `simulateTransaction` result; unit-tested natively.
//!
//! RPC returns inner instructions in one of three shapes (`UiInstruction`): `Parsed`
//! (`{program, programId, parsed:{type, info}}`, what Agave 4.3 returns for System), `PartiallyDecoded`
//! (`{programId, accounts:[base58], data}`) or `Compiled` (`{programIdIndex, accounts:[u8], data}`).
//! All are normalized to `{program_id, pubkeys, system_op}` and checked by the fee payer's pubkey, so
//! the rules do not depend on which shape a provider / Agave version returns.

use std::collections::HashSet;

use serde_json::Value;
use solana_pubkey::Pubkey;

use crate::program_policy::SYSTEM_PROGRAM;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemOp {
    CreateAccount {
        source: Pubkey,
        new_account: Pubkey,
        owner: Pubkey,
        lamports: u64,
    },
    Transfer {
        source: Pubkey,
        destination: Pubkey,
        lamports: u64,
    },
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InnerIx {
    pub program_id: Pubkey,
    pub pubkeys: HashSet<Pubkey>,
    /// Decoded only for the System program.
    pub system: Option<SystemOp>,
}

/// The parts of a simulation result §4.3 needs. `None` fields = missing in the response.
#[derive(Clone, Debug, PartialEq)]
pub struct Simulation {
    pub err: Value,
    pub fee: Option<u64>,
    pub pre_fee_payer: Option<u64>,
    pub post_fee_payer: Option<u64>,
    pub inner: Option<Vec<InnerIx>>,
    /// Owner of each requested address after simulation (`None` = account missing / closed).
    pub owners: Option<Vec<(Pubkey, Option<Pubkey>)>>,
}

fn pk(s: &str) -> Option<Pubkey> {
    s.parse().ok()
}

/// Every string in `v` (recursively, except `data` fields) that is a valid pubkey.
fn collect_pubkeys(v: &Value, out: &mut HashSet<Pubkey>) {
    match v {
        Value::String(s) => {
            if let Some(p) = pk(s) {
                out.insert(p);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_pubkeys(x, out)),
        Value::Object(o) => {
            for (k, x) in o {
                if k != "data" {
                    collect_pubkeys(x, out);
                }
            }
        }
        _ => {}
    }
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// System instruction from raw data + its account list (Compiled / PartiallyDecoded shapes).
fn decode_system(data: &[u8], accounts: &[Pubkey]) -> SystemOp {
    let tag = data
        .get(..4)
        .map(|t| u32::from_le_bytes(t.try_into().unwrap()));
    match (tag, accounts) {
        // CreateAccount { lamports: u64, space: u64, owner: Pubkey } · accounts [from, to]
        (Some(0), [from, to, ..]) if data.len() == 52 => SystemOp::CreateAccount {
            source: *from,
            new_account: *to,
            owner: Pubkey::new_from_array(data[20..52].try_into().unwrap()),
            lamports: u64_at(data, 4).unwrap(),
        },
        // Transfer { lamports: u64 } · accounts [from, to]
        (Some(2), [from, to, ..]) if data.len() == 12 => SystemOp::Transfer {
            source: *from,
            destination: *to,
            lamports: u64_at(data, 4).unwrap(),
        },
        _ => SystemOp::Other,
    }
}

fn decode_parsed_system(parsed: &Value) -> SystemOp {
    let info = parsed.get("info");
    let s = |k: &str| {
        info.and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .and_then(pk)
    };
    let n = |k: &str| info.and_then(|i| i.get(k)).and_then(Value::as_u64);
    match parsed.get("type").and_then(Value::as_str) {
        Some("createAccount") => match (s("source"), s("newAccount"), s("owner"), n("lamports")) {
            (Some(source), Some(new_account), Some(owner), Some(lamports)) => {
                SystemOp::CreateAccount {
                    source,
                    new_account,
                    owner,
                    lamports,
                }
            }
            _ => SystemOp::Other,
        },
        Some("transfer") => match (s("source"), s("destination"), n("lamports")) {
            (Some(source), Some(destination), Some(lamports)) => SystemOp::Transfer {
                source,
                destination,
                lamports,
            },
            _ => SystemOp::Other,
        },
        _ => SystemOp::Other,
    }
}

/// Normalizes one inner instruction. `None` = unknown shape (callers reject: fail-closed).
pub fn normalize(ix: &Value, static_keys: &[Pubkey]) -> Option<InnerIx> {
    let program_id = match (ix.get("programId"), ix.get("programIdIndex")) {
        (Some(Value::String(s)), _) => pk(s)?,
        (None, Some(i)) => *static_keys.get(i.as_u64()? as usize)?,
        _ => return None,
    };
    let mut pubkeys = HashSet::new();
    let system;
    if let Some(parsed) = ix.get("parsed") {
        // Parsed: scan everything except `data`, and decode System ops from `type` / `info`.
        collect_pubkeys(parsed, &mut pubkeys);
        system = (program_id == SYSTEM_PROGRAM).then(|| decode_parsed_system(parsed));
    } else {
        let accounts: Vec<Pubkey> = match ix.get("accounts")?.as_array()?.as_slice() {
            a if a.iter().all(Value::is_string) => a
                .iter()
                .map(|x| pk(x.as_str().unwrap()))
                .collect::<Option<_>>()?,
            a if a.iter().all(Value::is_u64) => a
                .iter()
                .map(|x| static_keys.get(x.as_u64().unwrap() as usize).copied())
                .collect::<Option<_>>()?,
            _ => return None,
        };
        let data = bs58::decode(ix.get("data")?.as_str()?).into_vec().ok()?;
        pubkeys.extend(accounts.iter().copied());
        system = (program_id == SYSTEM_PROGRAM).then(|| decode_system(&data, &accounts));
    }
    pubkeys.insert(program_id);
    Some(InnerIx {
        program_id,
        pubkeys,
        system,
    })
}

/// Extracts what §4.3 needs from `result.value` of `simulateTransaction`.
/// `requested` = the addresses passed in `accounts.addresses` (same order as `value.accounts`).
pub fn parse_simulation(value: &Value, static_keys: &[Pubkey], requested: &[Pubkey]) -> Simulation {
    let inner = value
        .get("innerInstructions")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .flat_map(|g| {
                    g.get("instructions")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                })
                .map(|ix| normalize(&ix, static_keys))
                .collect::<Option<Vec<_>>>()
        });
    let owners = value
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|a| {
            if a.len() != requested.len() {
                return None;
            }
            Some(
                requested
                    .iter()
                    .zip(a)
                    .map(|(k, acc)| (*k, acc.get("owner").and_then(Value::as_str).and_then(pk)))
                    .collect(),
            )
        });
    let bal = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_array)
            .and_then(|b| b.first())
            .and_then(Value::as_u64)
    };
    Simulation {
        err: value.get("err").cloned().unwrap_or(Value::Null),
        fee: value.get("fee").and_then(Value::as_u64),
        pre_fee_payer: bal("preBalances"),
        post_fee_payer: bal("postBalances"),
        // A group with an unknown shape poisons the whole list (fail-closed).
        inner: inner.flatten(),
        owners,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    pub fee_payer: &'a Pubkey,
    pub user: &'a Pubkey,
    /// Programs registered by this project (allowed owners of rent-funded accounts).
    pub registered: &'a [Pubkey],
    pub fee_cost: u64,
    pub declared_rent: u64,
    /// Refund-slot tx (no payer slot): the fee payer may not lose more than the fee (W4b).
    pub refund_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Lamports the fee payer funded as rent (outflow − fee); 0 for refunds.
    Ok { rent_spent: u64 },
    /// The program failed in simulation (`err != null`): handled by the caller (§4.2).
    ProgramError(String),
    /// A field §4.3 needs is missing while `err == null`: 503 upstream (fail-closed).
    Incomplete(&'static str),
    /// §4.3 violated: 400 `inner_ix_rejected`.
    Rejected(String),
}

pub fn verify(sim: &Simulation, e: Expect) -> Verdict {
    if !sim.err.is_null() {
        return Verdict::ProgramError(sim.err.to_string().chars().take(200).collect());
    }
    let (Some(pre), Some(post)) = (sim.pre_fee_payer, sim.post_fee_payer) else {
        return Verdict::Incomplete("balances");
    };
    let Some(inner) = &sim.inner else {
        return Verdict::Incomplete("innerInstructions");
    };
    let Some(owners) = &sim.owners else {
        return Verdict::Incomplete("accounts");
    };
    // Rule 2: the fee payer's net outflow.
    let fee = e.fee_cost;
    if e.refund_only {
        if post.saturating_add(fee) < pre {
            return Verdict::Rejected(format!(
                "refund tx lost {} lamports (> fee {fee})",
                pre - post
            ));
        }
    } else if pre.saturating_sub(post) > fee.saturating_add(e.declared_rent) {
        return Verdict::Rejected(format!(
            "outflow {} > fee {fee} + declared rent {}",
            pre - post,
            e.declared_rent
        ));
    }
    // Rule 3: every inner ix touching the fee payer is a System create/transfer into an account
    // the registered program owns.
    let owner_of = |k: &Pubkey| owners.iter().find(|(a, _)| a == k).map(|(_, o)| *o);
    for ix in inner {
        if !ix.pubkeys.contains(e.fee_payer) {
            continue;
        }
        if ix.program_id != SYSTEM_PROGRAM {
            return Verdict::Rejected(format!("fee payer passed to program {}", ix.program_id));
        }
        let dest = match &ix.system {
            Some(SystemOp::CreateAccount {
                source,
                new_account,
                owner,
                ..
            }) if source == e.fee_payer => {
                if !e.registered.contains(owner) {
                    return Verdict::Rejected(format!(
                        "createAccount owner {owner} is not a registered program"
                    ));
                }
                new_account
            }
            Some(SystemOp::Transfer {
                source,
                destination,
                ..
            }) if source == e.fee_payer => {
                match owner_of(destination) {
                    Some(Some(o)) if e.registered.contains(&o) => {}
                    Some(_) => {
                        return Verdict::Rejected(format!(
                            "transfer to {destination} not owned by a registered program"
                        ));
                    }
                    None => {
                        return Verdict::Rejected(format!(
                            "transfer destination {destination} not in simulated accounts"
                        ));
                    }
                }
                destination
            }
            _ => {
                return Verdict::Rejected(
                    "fee payer used in a System instruction other than createAccount/transfer"
                        .into(),
                );
            }
        };
        if dest == e.fee_payer || dest == e.user {
            return Verdict::Rejected("rent sent to the fee payer or the user".into());
        }
    }
    let simulated_fee = sim.fee.unwrap_or(fee);
    Verdict::Ok {
        rent_spent: pre.saturating_sub(post).saturating_sub(simulated_fee),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Real values from the Phase 0 devnet spike (testkit/scripts/spike-v3-output-2026-10-08.json).
    const FP: &str = "Ej77KMCVLEcDTrSjXHTLHTjMZUAr5F46PFrDw6MVVe7F";
    const TESTKIT: &str = "Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW";
    const THING: &str = "GXD5K3SHLJvzAKkok3xKJsTZV3THWSmewhUL8CmvD5L";
    const USER: &str = "6BZ5x7HFiPKy7VnRJcDhsyzd15oRvUHmTeur4aCiEfrR";
    const SYS: &str = "11111111111111111111111111111111";

    fn p(s: &str) -> Pubkey {
        s.parse().unwrap()
    }

    fn create_value() -> Value {
        json!({
            "err": null, "fee": 10000, "preBalances": [1313586200u64, 0, 0], "postBalances": [1312555120u64, 0, 0],
            "innerInstructions": [{ "index": 0, "instructions": [{ "parsed": { "info": {
                "lamports": 1021080, "newAccount": THING, "owner": TESTKIT, "source": FP, "space": 73 },
                "type": "createAccount" }, "program": "system", "programId": SYS, "stackHeight": 2 }] }],
            "accounts": [{ "owner": TESTKIT, "lamports": 1021080 }]
        })
    }

    fn expect<'a>(fp: &'a Pubkey, user: &'a Pubkey, reg: &'a [Pubkey]) -> Expect<'a> {
        Expect {
            fee_payer: fp,
            user,
            registered: reg,
            fee_cost: 10_000,
            declared_rent: 1_100_000,
            refund_only: false,
        }
    }

    #[test]
    fn real_create_from_phase0_passes() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let sim = parse_simulation(&create_value(), &[], &[p(THING)]);
        assert_eq!(
            verify(&sim, expect(&fp, &user, &reg)),
            Verdict::Ok {
                rent_spent: 1_021_080
            }
        );
    }

    #[test]
    fn real_drain_from_phase0_is_rejected() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let v = json!({
            "err": null, "fee": 10000, "preBalances": [1313586200u64], "postBalances": [1308576200u64],
            "innerInstructions": [{ "index": 0, "instructions": [{ "parsed": { "info": {
                "destination": USER, "lamports": 5000000, "source": FP }, "type": "transfer" },
                "program": "system", "programId": SYS, "stackHeight": 2 }] }],
            "accounts": [{ "owner": SYS, "lamports": 5000000 }]
        });
        let mut e = expect(&fp, &user, &reg);
        e.declared_rent = 10_000_000; // even with a generous declaration, the destination fails
        assert!(
            matches!(verify(&parse_simulation(&v, &[], &[user]), e), Verdict::Rejected(m) if m.contains("not owned"))
        );
    }

    #[test]
    fn real_close_refund_passes_and_a_fake_refund_fails() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let mut e = expect(&fp, &user, &reg);
        e.refund_only = true;
        e.declared_rent = 0;
        let ok = json!({ "err": null, "fee": 10000, "preBalances": [1312555120u64], "postBalances": [1313566200u64],
                         "innerInstructions": [], "accounts": [] });
        assert_eq!(
            verify(&parse_simulation(&ok, &[], &[]), e),
            Verdict::Ok { rent_spent: 0 }
        );
        let lost = json!({ "err": null, "fee": 10000, "preBalances": [1312555120u64], "postBalances": [1312000000u64],
                           "innerInstructions": [], "accounts": [] });
        assert!(matches!(
            verify(&parse_simulation(&lost, &[], &[]), e),
            Verdict::Rejected(_)
        ));
    }

    #[test]
    fn outflow_above_declared_is_rejected() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let mut e = expect(&fp, &user, &reg);
        e.declared_rent = 1_000_000; // actual 1_021_080
        assert!(
            matches!(verify(&parse_simulation(&create_value(), &[], &[p(THING)]), e), Verdict::Rejected(m) if m.contains("outflow"))
        );
    }

    #[test]
    fn create_owned_by_another_program_or_into_the_user_is_rejected() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let mut v = create_value();
        v["innerInstructions"][0]["instructions"][0]["parsed"]["info"]["owner"] = json!(SYS);
        assert!(
            matches!(verify(&parse_simulation(&v, &[], &[p(THING)]), expect(&fp, &user, &reg)), Verdict::Rejected(m) if m.contains("owner"))
        );
        let mut v = create_value();
        v["innerInstructions"][0]["instructions"][0]["parsed"]["info"]["newAccount"] = json!(USER);
        assert!(
            matches!(verify(&parse_simulation(&v, &[], &[p(THING)]), expect(&fp, &user, &reg)), Verdict::Rejected(m) if m.contains("user"))
        );
    }

    #[test]
    fn fee_payer_forwarded_to_another_program_is_rejected_even_nested() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let mut v = create_value();
        // A token-like parsed ix with the fee payer hidden in a nested array.
        v["innerInstructions"][0]["instructions"] = json!([{ "program": "spl-token", "programId": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "parsed": { "type": "transfer", "info": { "multisigAuthority": THING, "signers": [USER, FP] } } }]);
        assert!(
            matches!(verify(&parse_simulation(&v, &[], &[p(THING)]), expect(&fp, &user, &reg)), Verdict::Rejected(m) if m.contains("passed to program"))
        );
    }

    #[test]
    fn compiled_and_partially_decoded_shapes_normalize_the_same() {
        let (fp, user, thing, prog) = (p(FP), p(USER), p(THING), p(TESTKIT));
        let mut data = vec![0u8, 0, 0, 0];
        data.extend_from_slice(&1_021_080u64.to_le_bytes());
        data.extend_from_slice(&73u64.to_le_bytes());
        data.extend_from_slice(prog.as_ref());
        let d58 = bs58::encode(&data).into_string();
        let keys = [fp, user, thing, prog, p(SYS)];
        for ix in [
            json!({ "programIdIndex": 4, "accounts": [0, 2], "data": d58 }),
            json!({ "programId": SYS, "accounts": [FP, THING], "data": d58 }),
        ] {
            let n = normalize(&ix, &keys).unwrap();
            assert_eq!(
                n.system,
                Some(SystemOp::CreateAccount {
                    source: fp,
                    new_account: thing,
                    owner: prog,
                    lamports: 1_021_080
                })
            );
            assert!(n.pubkeys.contains(&fp));
        }
        // A System transfer in compiled form.
        let mut t = vec![2u8, 0, 0, 0];
        t.extend_from_slice(&5u64.to_le_bytes());
        let n = normalize(&json!({ "programIdIndex": 4, "accounts": [0, 1], "data": bs58::encode(&t).into_string() }), &keys).unwrap();
        assert_eq!(
            n.system,
            Some(SystemOp::Transfer {
                source: fp,
                destination: user,
                lamports: 5
            })
        );
    }

    #[test]
    fn unknown_shapes_and_missing_fields_fail_closed() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let mut v = create_value();
        v["innerInstructions"][0]["instructions"] = json!([{ "weird": true }]);
        assert_eq!(
            verify(
                &parse_simulation(&v, &[], &[p(THING)]),
                expect(&fp, &user, &reg)
            ),
            Verdict::Incomplete("innerInstructions")
        );
        assert!(
            normalize(
                &json!({ "programIdIndex": 9, "accounts": [], "data": "" }),
                &[fp]
            )
            .is_none()
        );
        for k in ["preBalances", "accounts"] {
            let mut v = create_value();
            v.as_object_mut().unwrap().remove(k);
            assert!(
                matches!(
                    verify(
                        &parse_simulation(&v, &[], &[p(THING)]),
                        expect(&fp, &user, &reg)
                    ),
                    Verdict::Incomplete(_)
                ),
                "{k}"
            );
        }
        // Missing post-state of a transfer destination.
        let v = json!({ "err": null, "fee": 10000, "preBalances": [100u64], "postBalances": [90u64],
            "innerInstructions": [{ "index": 0, "instructions": [{ "program": "system", "programId": SYS,
              "parsed": { "type": "transfer", "info": { "source": FP, "destination": THING, "lamports": 0 } } }] }],
            "accounts": [] });
        assert!(
            matches!(verify(&parse_simulation(&v, &[], &[]), expect(&fp, &user, &reg)), Verdict::Rejected(m) if m.contains("not in simulated"))
        );
        // Program error is not a verification failure.
        let mut v = create_value();
        v["err"] = json!({ "InstructionError": [0, { "Custom": 6000 }] });
        assert!(matches!(
            verify(
                &parse_simulation(&v, &[], &[p(THING)]),
                expect(&fp, &user, &reg)
            ),
            Verdict::ProgramError(_)
        ));
    }

    #[test]
    fn other_system_ops_with_the_fee_payer_are_rejected() {
        let (fp, user, reg) = (p(FP), p(USER), [p(TESTKIT)]);
        let mut v = create_value();
        v["innerInstructions"][0]["instructions"] = json!([{ "program": "system", "programId": SYS,
            "parsed": { "type": "assign", "info": { "account": FP, "owner": TESTKIT } } }]);
        assert!(matches!(
            verify(
                &parse_simulation(&v, &[], &[p(THING)]),
                expect(&fp, &user, &reg)
            ),
            Verdict::Rejected(_)
        ));
    }
}
