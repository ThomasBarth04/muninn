//! ADR 0011: the password hash and the authenticator (TOTP, RFC 6238).

use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use crate::percent_encode;

/// 10 to 256 characters, nothing else required (spec 001 §6). The ceiling
/// keeps one login from costing more than a hash of 256 characters.
pub fn valid_password(password: &str) -> bool {
    (10..=256).contains(&password.chars().count())
}

/// Argon2id with the crate's defaults, as a PHC string. About 20 ms of CPU,
/// so it runs off the async threads.
pub async fn hash_password(password: String) -> String {
    tokio::task::spawn_blocking(move || hash_now(&password))
        .await
        .expect("hashing thread")
}

fn hash_now(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .expect("argon2 with a random salt")
        .to_string()
}

/// Checks `password` against `hash` — or, with no hash, against a dummy, so an
/// unknown account costs the same time as a known one (spec 001 §7).
pub async fn check_password(password: String, hash: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        static DUMMY: OnceLock<String> = OnceLock::new();
        let dummy = DUMMY.get_or_init(|| hash_now("nobody's password, only for timing"));
        let matches = PasswordHash::new(hash.as_deref().unwrap_or(dummy)).is_ok_and(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        });
        matches && hash.is_some()
    })
    .await
    .unwrap_or(false)
}

/// TOTP steps are 30 seconds.
const STEP: i64 = 30;

/// 160 bits, the size RFC 4226 recommends for HMAC-SHA1.
pub fn new_secret() -> Vec<u8> {
    rand::random::<[u8; 20]>().to_vec()
}

/// The six-digit code for one 30-second step (RFC 6238 over RFC 4226).
pub fn code_at(secret: &[u8], step: i64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let at = (h[19] & 0x0f) as usize;
    let n = u32::from_be_bytes([h[at] & 0x7f, h[at + 1], h[at + 2], h[at + 3]]);
    n % 1_000_000
}

/// The step `code` is for, if it is the current one or a neighbour (clocks
/// drift) and later than the last code accepted — a code works once.
pub fn verify_code(secret: &[u8], code: &str, now: i64, last_step: Option<i64>) -> Option<i64> {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let code: u32 = code.parse().ok()?;
    let current = now.div_euclid(STEP);
    (current - 1..=current + 1)
        .filter(|&step| last_step.is_none_or(|last| step > last))
        .find(|&step| code_at(secret, step) == code)
}

/// RFC 4648 base32 without padding: how authenticator apps take a secret.
pub fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    for chunk in bytes.chunks(5) {
        let mut block = [0u8; 8];
        block[3..3 + chunk.len()].copy_from_slice(chunk);
        let n = u64::from_be_bytes(block);
        for i in 0..(chunk.len() * 8).div_ceil(5) {
            out.push(ALPHABET[(n >> (35 - 5 * i) & 31) as usize] as char);
        }
    }
    out
}

/// The Key URI format authenticator apps scan.
pub fn otpauth_uri(email: &str, secret: &[u8]) -> String {
    format!(
        "otpauth://totp/Muninn:{}?secret={}&issuer=Muninn",
        percent_encode(email),
        base32(secret)
    )
}

pub fn qr_svg(uri: &str) -> String {
    qrcode::QrCode::new(uri.as_bytes())
        .expect("an otpauth URI fits a QR code")
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1, cut to six digits.
    #[test]
    fn rfc_6238_vectors() {
        let secret = b"12345678901234567890";
        for (time, eight_digits) in [
            (59, 94287082),
            (1111111109, 7081804),
            (1234567890, 89005924),
            (2000000000, 69279037),
        ] {
            assert_eq!(
                code_at(secret, time / 30),
                eight_digits % 1_000_000,
                "{time}"
            );
        }
        assert_eq!(base32(secret), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn a_code_works_once_and_only_near_now() {
        let secret = b"12345678901234567890";
        let now = 1111111109;
        let code = |step: i64| format!("{:06}", code_at(secret, step));
        let current = now / 30;
        assert_eq!(
            verify_code(secret, &code(current), now, None),
            Some(current)
        );
        assert_eq!(
            verify_code(secret, &code(current - 1), now, None),
            Some(current - 1)
        );
        assert_eq!(
            verify_code(secret, &code(current + 1), now, None),
            Some(current + 1)
        );
        assert_eq!(verify_code(secret, &code(current - 2), now, None), None);
        assert_eq!(verify_code(secret, &code(current + 2), now, None), None);
        // Replayed, or older than the last one accepted.
        assert_eq!(
            verify_code(secret, &code(current), now, Some(current)),
            None
        );
        assert_eq!(
            verify_code(secret, &code(current + 1), now, Some(current)),
            Some(current + 1)
        );
        for bad in ["", "12345", "1234567", "12 456", "abcdef", "-12345"] {
            assert_eq!(verify_code(secret, bad, now, None), None, "{bad}");
        }
        assert_eq!(
            verify_code(secret, &format!(" {} ", code(current)), now, None),
            Some(current)
        );
    }

    #[test]
    fn password_rules() {
        assert!(!valid_password("123456789"));
        assert!(valid_password("1234567890"));
        assert!(valid_password(&"æ".repeat(256)));
        assert!(!valid_password(&"x".repeat(257)));
    }

    #[tokio::test]
    async fn hashes_verify() {
        let hash = hash_password("correct horse battery".into()).await;
        assert!(hash.starts_with("$argon2id$"));
        assert!(check_password("correct horse battery".into(), Some(hash.clone())).await);
        assert!(!check_password("wrong horse battery".into(), Some(hash)).await);
        assert!(!check_password("correct horse battery".into(), None).await);
        assert!(!check_password("x".into(), Some("not a hash".into())).await);
    }
}
