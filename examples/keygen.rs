//! Creates the devnet relayer keypair in `.dev.vars` (gitignored). Prints only the pubkey.
//! Refuses to overwrite an existing `.dev.vars`.
use std::{fs::OpenOptions, io::Write};

use solana_keypair::Keypair;
use solana_signer::Signer;

fn main() {
    let kp = Keypair::new();
    let mut file = match OpenOptions::new().write(true).create_new(true).open(".dev.vars") {
        Ok(f) => f,
        Err(e) => {
            eprintln!(".dev.vars not created: {e}");
            std::process::exit(1);
        }
    };
    writeln!(file, "RELAYER_PRIVATE_KEY={}", kp.to_base58_string()).expect("write .dev.vars");
    println!("{}", kp.pubkey());
}
