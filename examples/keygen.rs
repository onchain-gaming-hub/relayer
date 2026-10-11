//! Creates a devnet fee payer for project `<SLUG>`: appends `FEE_PAYER_<SLUG>=<base58>` to
//! `.dev.vars` (gitignored, mode 0600) and prints only the pubkey. Refuses if the name exists.
//! Usage: cargo run -q --example keygen -- <SLUG>
//! Never use this for mainnet keys on a shared machine (spec v2 §10).
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

use solana_keypair::Keypair;
use solana_signer::Signer;

fn main() {
    let slug = std::env::args().nth(1).unwrap_or_default();
    let valid = (1..=32).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    if !valid {
        eprintln!("usage: keygen <SLUG>  (SLUG = [A-Z0-9_]{{1,32}})");
        std::process::exit(2);
    }
    let name = format!("FEE_PAYER_{slug}");
    let existing = fs::read_to_string(".dev.vars").unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.split('=').next().map(str::trim) == Some(name.as_str()))
    {
        eprintln!("{name} already exists in .dev.vars");
        std::process::exit(1);
    }
    let kp = Keypair::new();
    let mut opts = OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(".dev.vars").expect("open .dev.vars");
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file).expect("write .dev.vars");
    }
    writeln!(file, "{name}={}", kp.to_base58_string()).expect("write .dev.vars");
    println!("{}", kp.pubkey());
}
