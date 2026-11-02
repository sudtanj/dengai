//! Steam Guard mobile authenticator codes (HOTP/RFC 4226 over 30 s steps, Steam's 5-char alphabet).

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

const ALPHABET: &[u8; 26] = b"23456789BCDFGHJKMNPQRTVWXY";

/// RFC 4226 dynamic truncation of HMAC-SHA1(secret, counter), a 31-bit number.
pub fn hotp_value(secret: &[u8], counter: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC takes keys of any length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    u32::from_be_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]) & 0x7fff_ffff
}

/// The Steam Guard code for raw secret bytes at a Unix time.
pub fn steam_code(secret: &[u8], unix_time: u64) -> String {
    let mut v = hotp_value(secret, unix_time / 30);
    (0..5)
        .map(|_| {
            let c = ALPHABET[(v % 26) as usize] as char;
            v /= 26;
            c
        })
        .collect()
}

/// The Steam Guard code for a base64 `shared_secret` (as found in a maFile).
pub fn steam_code_b64(shared_secret: &str, unix_time: u64) -> Result<String, String> {
    let secret = base64::engine::general_purpose::STANDARD
        .decode(shared_secret.trim())
        .map_err(|e| format!("shared_secret is not valid base64: {e}"))?;
    Ok(steam_code(&secret, unix_time))
}

/// Offset to add to the local clock to get Steam's: `server - local` seconds.
pub fn time_offset(server: u64, local: u64) -> i64 {
    server as i64 - local as i64
}

/// The local Unix time corrected by a `time_offset`.
pub fn steam_time(local: u64, offset: i64) -> u64 {
    local.saturating_add_signed(offset)
}

/// `server_time` from an `ITwoFactorService/QueryTime` reply (Steam sends it as a string).
pub fn parse_server_time(body: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let t = &v["response"]["server_time"];
    t.as_u64().or_else(|| t.as_str()?.parse().ok())
}
