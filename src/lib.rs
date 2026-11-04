//! Network-free logic of dengai, kept in the library so it can be tested without Steam.

pub mod api;
pub mod badges;
pub mod config;
pub mod pics;
pub mod purchase;
pub mod state;
pub mod totp;
pub mod vdf;

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
