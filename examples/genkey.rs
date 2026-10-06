//! Test-only: prints `<base58 keypair> <pubkey>` for a throwaway key (tests/run-e2e.sh).
use solana_keypair::Keypair;
use solana_signer::Signer;

fn main() {
    let kp = Keypair::new();
    println!("{} {}", kp.to_base58_string(), kp.pubkey());
}
