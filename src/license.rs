//! Offline license handling.
//!
//! Free tier: unlimited scanning/reporting; dedupe & archive actions limited
//! to files <= 1 GiB each. A license key unlocks unlimited actions.
//!
//! Keys are HMAC-SHA256 signed (not cryptographically unforgeable — this is
//! an honest indie-tool gate, not DRM against a determined attacker) and are
//! verified fully offline. Format: GJ-XXXX-XXXX-XXXX-XXXX (Crockford base32).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The license secret. Rotating this invalidates all previous keys.
const SECRET: &[u8] = b"gguf-janitor-v1-license-secret-7c4f2a91";

/// Free tier: actions allowed on files up to this size.
pub const FREE_FILE_LIMIT: u64 = 1024 * 1024 * 1024; // 1 GiB

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

const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"; // Crockford

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut bits: u32 = 0;
    let mut acc: u32 = 0;
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 0x1F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 0x1F) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for ch in s.chars() {
        let v = match ch.to_ascii_uppercase() {
            'O' => 0,
            'I' | 'L' => 1,
            c => {
                let idx = ALPHABET.iter().position(|&a| a as char == c)?;
                idx as u32
            }
        };
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn normalize_key(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    let body = cleaned.strip_prefix("GJ")?;
    if body.len() != 16 {
        return None;
    }
    Some(body.to_uppercase())
}

/// Generate a Pro key (used by the maintainer-only keygen binary).
pub fn generate_pro_key() -> String {
    // payload: [flags=1, reserved=0, serial=u32 from time] + 4 mac bytes = 10 bytes
    let serial = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let payload = [1u8, 0u8, (serial >> 24) as u8, (serial >> 16) as u8, (serial >> 8) as u8, serial as u8];
    let mac = hmac_sha256(SECRET, &payload);
    let mut bytes = payload.to_vec();
    bytes.extend_from_slice(&mac[..4]);
    let body = base32_encode(&bytes);
    debug_assert_eq!(body.len(), 16);
    format!("GJ-{}-{}-{}-{}", &body[0..4], &body[4..8], &body[8..12], &body[12..16])
}

/// Verify a key; returns Pro on success.
pub fn verify_key(raw: &str) -> Option<LicenseState> {
    let body = normalize_key(raw)?;
    let bytes = base32_decode(&body)?;
    if bytes.len() != 10 {
        return None;
    }
    let (payload, mac) = bytes.split_at(6);
    let expect = hmac_sha256(SECRET, payload);
    // constant-time-ish compare
    let mut diff = 0u8;
    for i in 0..4 {
        diff |= expect[i] ^ mac[i];
    }
    if diff != 0 {
        return None;
    }
    if payload[0] & 1 == 1 {
        Some(LicenseState::Pro)
    } else {
        Some(LicenseState::Free)
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
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

/// Persist a license key; returns verification result.
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
        None => Err("invalid license key".to_string()),
    }
}

/// Whether a dedupe/archive action on a file of `size` bytes is allowed.
pub fn action_allowed(state: LicenseState, size: u64) -> bool {
    state.is_pro() || size <= FREE_FILE_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_verify_roundtrip() {
        let key = generate_pro_key();
        assert_eq!(verify_key(&key), Some(LicenseState::Pro));
        // lowercased, spacing variants accepted
        let body = key.strip_prefix("GJ-").unwrap().replace('-', "").to_lowercase();
        assert_eq!(body.len(), 16);
        let mangled = format!("gj-{}-{}-{}-{}", &body[0..4], &body[4..8], &body[8..12], &body[12..16]);
        assert_eq!(verify_key(&mangled), Some(LicenseState::Pro));
    }

    #[test]
    fn rejects_tampered_keys() {
        let key = generate_pro_key();
        let mut chars: Vec<char> = key.chars().collect();
        // flip one body char to another valid base32 char
        let last = chars.len() - 2;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert_ne!(verify_key(&tampered), Some(LicenseState::Pro));
        assert_eq!(verify_key("GJ-AAAA-BBBB-CCCC-DDDD"), None);
        assert_eq!(verify_key(""), None);
        assert_eq!(verify_key("totally-not-a-key"), None);
    }

    #[test]
    fn free_tier_limits() {
        assert!(action_allowed(LicenseState::Free, FREE_FILE_LIMIT));
        assert!(!action_allowed(LicenseState::Free, FREE_FILE_LIMIT + 1));
        assert!(action_allowed(LicenseState::Pro, u64::MAX));
    }

    #[test]
    fn base32_roundtrip() {
        for sample in [&b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09"[..], b"hello", b"\xff\xff\xff"] {
            let enc = base32_encode(sample);
            let dec = base32_decode(&enc).unwrap();
            assert_eq!(dec, sample, "{enc}");
        }
    }
}
