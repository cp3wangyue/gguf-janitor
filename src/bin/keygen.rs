//! Maintainer-only license issuer. NOT included in release zips.
//!
//! Key pair generation is a one-off (see license-private-key.txt header):
//!   python -c "from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey; ..."
//! Modes:
//!   gj-keygen --generate-keypair
//!       Print a fresh private key (seed, hex) and the matching public key.
//!   gj-keygen --private <license-private-key.txt> [--serial N]
//!       Issue one Pro license for the given serial.

use gguf_janitor::license::sign_payload;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--private") => {
            let path = args.get(1).expect("--private <file>");
            let private = std::fs::read_to_string(path).expect("private key file");
            let private = private.lines().find(|l| l.starts_with("private:")).map(|l| l[8..].trim().to_string()).unwrap_or(private.trim().to_string());
            let serial: u32 = args
                .iter()
                .position(|a| a == "--serial")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.parse().expect("serial number"))
                .unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| (d.as_secs() & 0xFFFF) as u32)
                        .unwrap_or(1)
                });
            let payload = [1u8, 1, (serial >> 24) as u8, (serial >> 16) as u8, (serial >> 8) as u8, serial as u8];
            match sign_payload(&private, &payload) {
                Some(license) => println!("{license}"),
                None => {
                    eprintln!("failed to sign (bad private key hex?)");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: gj-keygen --generate-keypair | --private <file> [--serial N]");
            std::process::exit(2);
        }
    }
}
