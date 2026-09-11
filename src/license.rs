//! Offline license handling.
//!
//! Free tier: unlimited scanning/reporting; dedupe & archive actions limited
//! to files <= 1 GiB each. A license unlocks unlimited actions.
//!
//! Licenses are Ed25519-signed and verified against a public key embedded in
//! the binary. The signing key is NOT in this repository, so licenses cannot
//! be forged from the published source. A license is a pasteable block:
//!
//!   GJKEY-<base64 of payload[6] || signature[64]>
//!
//! (70 bytes payload+signature, ~96 base64 characters, whitespace-tolerant.)

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Ed25519 public key (hex). The matching private key is kept by the
/// maintainer outside this repository (license-private-key.txt, gitignored).
pub const LICENSE_PUBLIC_KEY: [u8; 32] = LICENSE_PUBLIC_KEY_BYTES;

include!("license_pubkey.inc");

/// Free tier: actions allowed on files up to this size.
pub const FREE_FILE_LIMIT: u64 = 1024 * 1024 * 1024; // 1 GiB

const KEY_PREFIX: &str = "GJKEY-";
/// payload: version u8, flags u8, serial u32 big-endian
const PAYLOAD_LEN: usize = 6;
const SIG_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseState {
    Free,
    Pro,
}

impl LicenseState {
    pub fn is_pro(self) -> bool {
        self == LicenseState::Pro
    }

    pub fn label(self) -> &'static str {
        match self {
            LicenseState::Free => "Free",
            LicenseState::Pro => "Pro",
        }
    }
}

/// Sign a payload (maintainer-only keygen binary; private key from file).
pub fn sign_payload(private_key_hex: &str, payload: &[u8; PAYLOAD_LEN]) -> Option<String> {
    use ed25519_dalek::{Signer, SigningKey};
    if private_key_hex.trim().len() != 64 {
        return None;
    }
    let mut seed = [0u8; 32];
    for i in 0..32 {
        seed[i] = u8::from_str_radix(&private_key_hex.trim()[i * 2..i * 2 + 2], 16).ok()?;
    }
    let signing = SigningKey::from_bytes(&seed);
    let sig = signing.sign(payload).to_bytes();
    let mut blob = Vec::with_capacity(PAYLOAD_LEN + SIG_LEN);
    blob.extend_from_slice(payload);
    blob.extend_from_slice(&sig);
    Some(format!("{KEY_PREFIX}{}", base64_std(&blob)))
}

/// Verify a license block; returns Pro on success.
pub fn verify_key(raw: &str) -> Option<LicenseState> {
    verify_with_pubkey(raw, &LICENSE_PUBLIC_KEY)
}

fn verify_with_pubkey(raw: &str, public_key: &[u8; 32]) -> Option<LicenseState> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let compact: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("");
    let body = compact.strip_prefix(KEY_PREFIX).or(if compact.starts_with("GJ") {
        Some(&compact[..])
    } else {
        None
    })?;
    let blob = base64_decode(body)?;
    if blob.len() != PAYLOAD_LEN + SIG_LEN {
        return None;
    }
    let (payload, sig_bytes) = blob.split_at(PAYLOAD_LEN);
    if payload[0] != 1 {
        return None; // version
    }
    if payload[1] & 1 != 1 {
        return Some(LicenseState::Free);
    }
    let vk = VerifyingKey::from_bytes(public_key).ok()?;
    let mut sig_arr = [0u8; SIG_LEN];
    sig_arr.copy_from_slice(sig_bytes);
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(payload, &sig).ok()?;
    Some(LicenseState::Pro)
}

fn base64_std(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(CHARS[(n >> 18) as usize & 63] as char);
        out.push(CHARS[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { CHARS[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { CHARS[n as usize & 63] as char } else { '=' });
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let rev = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    for chunk in cleaned.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= rev(c)? << (18 - 6 * i);
        }
        match chunk.len() {
            4 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            3 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8]),
            2 => out.push((n >> 16) as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// Where the license file lives: %APPDATA%\GGUFJanitor\license.key
pub fn license_file_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    Some(base.join("GGUFJanitor").join("license.key"))
}

/// Load and verify the stored license, if any.
pub fn current_state() -> LicenseState {
    if let Some(p) = license_file_path() {
        if let Ok(txt) = std::fs::read_to_string(&p) {
            if verify_key(txt.trim()) == Some(LicenseState::Pro) {
                return LicenseState::Pro;
            }
        }
    }
    // Also accept GGJ_LICENSE env (useful for CI/testing).
    if let Ok(k) = std::env::var("GGJ_LICENSE") {
        if verify_key(k.trim()) == Some(LicenseState::Pro) {
            return LicenseState::Pro;
        }
    }
    LicenseState::Free
}

/// Persist a license block; returns verification result.
pub fn activate(raw: &str) -> Result<LicenseState, String> {
    match verify_key(raw) {
        Some(state) => {
            if let Some(p) = license_file_path() {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                std::fs::write(&p, raw.trim()).map_err(|e| e.to_string())?;
            }
            Ok(state)
        }
        None => Err("invalid license".to_string()),
    }
}

/// Whether a dedupe/archive action on a file of `size` bytes is allowed.
pub fn action_allowed(state: LicenseState, size: u64) -> bool {
    state.is_pro() || size <= FREE_FILE_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    const TEST_SEED_HEX: &str = "9f0c1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7";

    fn test_pubkey() -> [u8; 32] {
        use ed25519_dalek::SigningKey;
        let mut seed = [0u8; 32];
        for i in 0..32 {
            seed[i] = u8::from_str_radix(&TEST_SEED_HEX[i * 2..i * 2 + 2], 16).unwrap();
        }
        SigningKey::from_bytes(&seed).verifying_key().to_bytes()
    }

    #[test]
    fn sign_verify_roundtrip() {
        let payload = [1u8, 1, 0x12, 0x34, 0x56, 0x78];
        let key = sign_payload(TEST_SEED_HEX, &payload).unwrap();
        assert!(key.starts_with("GJKEY-"));
        assert_eq!(verify_with_pubkey(&key, &test_pubkey()), Some(LicenseState::Pro));
        // whitespace / line-wrap tolerant (as it would arrive by email)
        let wrapped = key
            .chars()
            .enumerate()
            .flat_map(|(i, c)| {
                let mut v = vec![c];
                if i % 24 == 23 {
                    v.push('\n');
                }
                v.into_iter()
            })
            .collect::<String>();
        assert_eq!(verify_with_pubkey(&wrapped, &test_pubkey()), Some(LicenseState::Pro));
    }

    #[test]
    fn rejects_tampered_and_foreign() {
        let payload = [1u8, 1, 0xAA, 0xBB, 0xCC, 0xDD];
        let key = sign_payload(TEST_SEED_HEX, &payload).unwrap();
        // flip a payload char inside the base64 body
        let mut chars: Vec<char> = key.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert_ne!(verify_with_pubkey(&tampered, &test_pubkey()), Some(LicenseState::Pro));
        assert_eq!(verify_key(""), None);
        assert_eq!(verify_key("GJKEY-notbase64!!"), None);
        assert_eq!(verify_key("totally-unrelated"), None);
    }

    #[test]
    fn wrong_signing_key_is_rejected() {
        // A key signed with a different private key must not verify.
        let payload = [1u8, 1, 1, 2, 3, 4];
        let mut seed_hex = TEST_SEED_HEX.to_string();
        seed_hex.replace_range(0..1, "0");
        let key = sign_payload(&seed_hex, &payload).unwrap();
        assert_eq!(verify_with_pubkey(&key, &test_pubkey()), None);
    }

    #[test]
    fn base64_roundtrip() {
        for sample in [
            &b"\x01\x01\x12\x34\x56\x78"[..],
            b"x",
            b"ab",
            b"abc",
            &[0xffu8; 70][..],
        ] {
            let enc = base64_std(sample);
            assert_eq!(base64_decode(&enc).unwrap(), sample, "{enc}");
        }
    }

    #[test]
    fn free_tier_limits() {
        assert!(action_allowed(LicenseState::Free, FREE_FILE_LIMIT));
        assert!(!action_allowed(LicenseState::Free, FREE_FILE_LIMIT + 1));
        assert!(action_allowed(LicenseState::Pro, u64::MAX));
    }
}
