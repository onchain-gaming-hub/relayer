//! Program version pinning (spec v3 W6): BPF Upgradeable Loader account layouts.
//! Pure functions; the fetching and 60 s cache live in `lib.rs`.
//! Layout (verified on devnet, Phase 0): Program account = u32 tag 2 + ProgramData address (4..36);
//! ProgramData = u32 tag 3 + slot u64 LE (4..12) + Option<Pubkey> upgrade authority (tag at 12).

use solana_pubkey::Pubkey;

use crate::program_policy::Pin;

pub const BPF_UPGRADEABLE_LOADER: Pubkey =
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");

fn tag(data: &[u8]) -> Option<u32> {
    data.get(..4)
        .map(|t| u32::from_le_bytes(t.try_into().unwrap()))
}

/// Address of the ProgramData account, from the Program account.
pub fn programdata_address(owner: &Pubkey, data: &[u8]) -> Result<Pubkey, String> {
    if *owner != BPF_UPGRADEABLE_LOADER {
        return Err(format!(
            "program owner {owner} is not the upgradeable loader"
        ));
    }
    if tag(data) != Some(2) || data.len() < 36 {
        return Err("not an upgradeable Program account".into());
    }
    Ok(Pubkey::new_from_array(data[4..36].try_into().unwrap()))
}

/// (deploy slot, upgrade authority) from a ProgramData account (only the first 45 bytes needed).
pub fn programdata_state(owner: &Pubkey, data: &[u8]) -> Result<(u64, Option<Pubkey>), String> {
    if *owner != BPF_UPGRADEABLE_LOADER {
        return Err(format!(
            "programdata owner {owner} is not the upgradeable loader"
        ));
    }
    if tag(data) != Some(3) || data.len() < 13 {
        return Err("not a ProgramData account".into());
    }
    let slot = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let authority = match data[12] {
        0 => None,
        1 if data.len() >= 45 => Some(Pubkey::new_from_array(data[13..45].try_into().unwrap())),
        _ => return Err("bad upgrade authority option".into()),
    };
    Ok((slot, authority))
}

/// Does the on-chain state match the registered pin?
pub fn matches(pin: Pin, slot: u64, authority: Option<Pubkey>) -> bool {
    match pin {
        Pin::DeploySlot(s) => s == slot,
        Pin::Immutable => authority.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD as B64};

    #[test]
    fn real_devnet_layout_from_phase0() {
        // testkit: ProgramData Bx4Qou… slot 508764926 (0x1E5322FE), authority Ej77KM…
        let pd: Pubkey = "Bx4QouRTySJqMCPDhBStWY5EPzvkk5iFQrkvkU9Nq6YU"
            .parse()
            .unwrap();
        let mut prog = vec![2, 0, 0, 0];
        prog.extend_from_slice(pd.as_ref());
        assert_eq!(programdata_address(&BPF_UPGRADEABLE_LOADER, &prog), Ok(pd));
        // First 16 bytes of the real ProgramData data reported by the reviewer: "AwAAAP4iUx4AAAAAAc…"
        let head = B64.decode("AwAAAP4iUx4AAAAAAQ==").unwrap();
        assert_eq!(
            &head[..12],
            &[3, 0, 0, 0, 0xFE, 0x22, 0x53, 0x1E, 0, 0, 0, 0]
        );
        let auth: Pubkey = "Ej77KMCVLEcDTrSjXHTLHTjMZUAr5F46PFrDw6MVVe7F"
            .parse()
            .unwrap();
        let mut data = head[..12].to_vec();
        data.push(1);
        data.extend_from_slice(auth.as_ref());
        assert_eq!(
            programdata_state(&BPF_UPGRADEABLE_LOADER, &data),
            Ok((508764926, Some(auth)))
        );
        assert!(matches(Pin::DeploySlot(508764926), 508764926, Some(auth)));
        assert!(!matches(Pin::DeploySlot(508764927), 508764926, Some(auth)));
        assert!(!matches(Pin::Immutable, 508764926, Some(auth)));
        assert!(matches(Pin::Immutable, 1, None));
    }

    #[test]
    fn wrong_owner_or_tag_fails() {
        let other = Pubkey::new_unique();
        assert!(programdata_address(&other, &[2, 0, 0, 0]).is_err());
        assert!(programdata_address(&BPF_UPGRADEABLE_LOADER, &[3; 36]).is_err());
        assert!(
            programdata_state(
                &BPF_UPGRADEABLE_LOADER,
                &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
            )
            .is_err()
        );
        assert!(programdata_state(&other, &[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        assert_eq!(
            programdata_state(
                &BPF_UPGRADEABLE_LOADER,
                &[3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0]
            ),
            Ok((7, None))
        );
        assert!(
            programdata_state(
                &BPF_UPGRADEABLE_LOADER,
                &[3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 2]
            )
            .is_err()
        );
    }
}
