//! TOTP from RFC 6238: HMAC-SHA-1, 6 digits, 30 s steps, one step of tolerance each way (docs/SECURITY.md).

use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;

pub const STEP_SECONDS: u64 = 30;
pub const DIGITS: u32 = 6;
pub const SECRET_BYTES: usize = 20;

/// The HOTP value of `counter` with `digits` digits (RFC 4226, section 5.3).
pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = usize::from(hash[19] & 0x0f);
    let value = u32::from_be_bytes([
        hash[offset] & 0x7f,
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ]);
    value % 10u32.pow(digits)
}

pub fn step_of(unix_seconds: u64) -> u64 {
    unix_seconds / STEP_SECONDS
}

pub fn format_code(value: u32) -> String {
    format!("{value:0width$}", width = DIGITS as usize)
}

/// The step the code belongs to, if it matches `now_step` or a neighbour and is newer than the last accepted one.
/// A used code, and every code before it, is refused (no replay).
pub fn verify(secret: &[u8], code: &str, now_step: u64, last_used: Option<u64>) -> Option<u64> {
    let code = code.trim();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut found = None;
    for step in [now_step.saturating_sub(1), now_step, now_step + 1] {
        let expected = format_code(hotp(secret, step, DIGITS));
        let matches: bool = expected.as_bytes().ct_eq(code.as_bytes()).into();
        if matches && last_used.is_none_or(|last| step > last) {
            found = Some(step);
        }
    }
    found
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding, as authenticator apps expect the secret.
pub fn base32(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

/// The address an authenticator app reads from the QR code.
pub fn otpauth_uri(secret: &[u8], account: &str) -> String {
    format!(
        "otpauth://totp/RemoteConnectionSystem:{account}?secret={}&issuer=RemoteConnectionSystem&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        base32(secret)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc6238_sha1_vectors() {
        // RFC 6238, appendix B, SHA-1 column, 8 digits.
        let cases = [
            (59u64, 94_287_082u32),
            (1_111_111_109, 7_081_804),
            (1_111_111_111, 14_050_471),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
            (20_000_000_000, 65_353_130),
        ];
        for (time, code) in cases {
            assert_eq!(hotp(RFC_SECRET, step_of(time), 8), code, "T={time}");
            assert_eq!(
                hotp(RFC_SECRET, step_of(time), 6),
                code % 1_000_000,
                "T={time}"
            );
        }
    }

    #[test]
    fn neighbours_are_accepted_and_replays_refused() {
        let now = step_of(1_234_567_890);
        let previous = format_code(hotp(RFC_SECRET, now - 1, DIGITS));
        let next = format_code(hotp(RFC_SECRET, now + 1, DIGITS));
        let far = format_code(hotp(RFC_SECRET, now + 2, DIGITS));
        assert_eq!(verify(RFC_SECRET, &previous, now, None), Some(now - 1));
        assert_eq!(verify(RFC_SECRET, &next, now, None), Some(now + 1));
        assert_eq!(verify(RFC_SECRET, &far, now, None), None);
        // Once the current code is used, it and the previous one are refused.
        let current = format_code(hotp(RFC_SECRET, now, DIGITS));
        assert_eq!(verify(RFC_SECRET, &current, now, Some(now)), None);
        assert_eq!(verify(RFC_SECRET, &previous, now, Some(now)), None);
        assert_eq!(verify(RFC_SECRET, &next, now, Some(now)), Some(now + 1));
    }

    #[test]
    fn malformed_codes_are_refused() {
        for code in ["", "12345", "1234567", "12345a", "١٢٣٤٥٦"] {
            assert_eq!(verify(RFC_SECRET, code, 1000, None), None, "{code:?}");
        }
    }

    #[test]
    fn base32_matches_rfc4648() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(RFC_SECRET), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    }
}
