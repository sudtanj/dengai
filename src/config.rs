//! Configuration: an optional TOML file, overridden by `DENGAI_*` environment variables.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Steam account name.
    pub username: String,
    /// Password; only needed when there is no saved refresh token.
    pub password: Option<String>,
    /// Base64 `shared_secret` of the mobile authenticator, to generate Steam Guard codes.
    pub shared_secret: Option<String>,
    /// One-off Steam Guard code (email or mobile) for the first login without `shared_secret`.
    pub guard_code: Option<String>,
    /// Where the refresh token, guard data and state.json live.
    pub data_dir: PathBuf,
    /// Log level: error, warn, info, debug, trace.
    pub log: String,
    /// API listen address.
    pub api_bind: String,
    /// Bearer token for the API. The API is off when empty.
    pub api_token: String,
    /// Farm card drops.
    pub farm: bool,
    /// Seconds between drop checks of the game being played.
    pub farm_check_secs: u64,
    /// Seconds between badge page scans when nothing is left to farm.
    pub farm_rescan_secs: u64,
    /// Seconds between keys / licenses in /redeem and /addlicense.
    pub redeem_delay_secs: u64,
    /// Watch PICS and claim free packages.
    pub auto_claim: bool,
    /// Seconds between PICS change polls.
    pub pics_poll_secs: u64,
    /// Max free-package claims per hour.
    pub claims_per_hour: u32,
    pub skip_demos: bool,
    pub skip_playtests: bool,
    pub skip_unowned_dlc: bool,
    /// Country for region rules; defaults to the one Steam reports for the login IP.
    pub country: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            username: String::new(),
            password: None,
            shared_secret: None,
            guard_code: None,
            data_dir: PathBuf::from("data"),
            log: "info".into(),
            api_bind: "127.0.0.1:8737".into(),
            api_token: String::new(),
            farm: true,
            farm_check_secs: 300,
            farm_rescan_secs: 6 * 3600,
            redeem_delay_secs: 5,
            auto_claim: true,
            pics_poll_secs: 60,
            claims_per_hour: 30,
            skip_demos: false,
            skip_playtests: false,
            skip_unowned_dlc: true,
            country: None,
        }
    }
}

trait FromEnv: Sized {
    fn from_env(s: &str) -> Option<Self>;
}

impl FromEnv for String {
    fn from_env(s: &str) -> Option<Self> {
        Some(s.to_string())
    }
}

impl FromEnv for Option<String> {
    fn from_env(s: &str) -> Option<Self> {
        Some((!s.is_empty()).then(|| s.to_string()))
    }
}

impl FromEnv for PathBuf {
    fn from_env(s: &str) -> Option<Self> {
        Some(PathBuf::from(s))
    }
}

impl FromEnv for bool {
    fn from_env(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    }
}

impl FromEnv for u64 {
    fn from_env(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

impl FromEnv for u32 {
    fn from_env(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

impl Config {
    /// Reads `DENGAI_CONFIG` (default `dengai.toml`, optional), then applies `DENGAI_<FIELD>` overrides
    /// from `env` (a lookup such as `std::env::var(..).ok()`).
    pub fn load(env: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let explicit = env("DENGAI_CONFIG");
        let path = PathBuf::from(explicit.clone().unwrap_or_else(|| "dengai.toml".into()));
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if explicit.is_none() && e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        macro_rules! overrides {
            ($($field:ident),*) => {$(
                let name = format!("DENGAI_{}", stringify!($field).to_ascii_uppercase());
                if let Some(v) = env(&name) {
                    cfg.$field = FromEnv::from_env(&v).ok_or_else(|| format!("{name}: invalid value {v:?}"))?;
                }
            )*};
        }
        overrides!(
            username,
            password,
            shared_secret,
            guard_code,
            data_dir,
            log,
            api_bind,
            api_token,
            farm,
            farm_check_secs,
            farm_rescan_secs,
            redeem_delay_secs,
            auto_claim,
            pics_poll_secs,
            claims_per_hour,
            skip_demos,
            skip_playtests,
            skip_unowned_dlc,
            country
        );
        if cfg.username.is_empty() {
            return Err("DENGAI_USERNAME (or `username` in the config file) is required".into());
        }
        cfg.farm_check_secs = cfg.farm_check_secs.max(60);
        cfg.pics_poll_secs = cfg.pics_poll_secs.max(30);
        Ok(cfg)
    }
}
