//! Steam Community / Store web session built from a web access token (`steamLoginSecure` cookie).

use dengai::purchase::{self, Claim};
use reqwest::header::{CONTENT_TYPE, COOKIE};
use std::hash::{BuildHasher, Hasher};

pub struct Web {
    http: reqwest::Client,
    steam_id: u64,
    session_id: String,
    cookie: String,
}

/// 24 random hex chars (std's per-process random hasher keys are enough for a CSRF token).
fn random_hex() -> String {
    (0..2)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(dengai::now() ^ i);
            format!("{:012x}", h.finish() & 0xffff_ffff_ffff)
        })
        .collect()
}

impl Web {
    pub fn new(http: reqwest::Client, steam_id: u64, access_token: &str) -> Self {
        let session_id = random_hex();
        let cookie =
            format!("steamLoginSecure={steam_id}%7C%7C{access_token}; sessionid={session_id}; Steam_Language=english");
        Web { http, steam_id, session_id, cookie }
    }

    async fn get(&self, url: &str) -> Result<String, String> {
        let resp = self.http.get(url).header(COOKIE, &self.cookie).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status} for {url}"));
        }
        if dengai::badges::is_logged_out(&body) {
            return Err("web session is not logged in".into());
        }
        Ok(body)
    }

    pub async fn badges(&self, page: u32) -> Result<String, String> {
        self.get(&format!("https://steamcommunity.com/profiles/{}/badges?l=english&p={page}", self.steam_id)).await
    }

    pub async fn gamecards(&self, appid: u32) -> Result<String, String> {
        self.get(&format!("https://steamcommunity.com/profiles/{}/gamecards/{appid}?l=english", self.steam_id)).await
    }

    pub async fn add_free_license(&self, sub: u32) -> Claim {
        let resp = self
            .http
            .post(format!("https://store.steampowered.com/freelicense/addfreelicense/{sub}"))
            .header(COOKIE, &self.cookie)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(format!("ajax=true&sessionid={}", self.session_id))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                purchase::classify_free_license(status, &body)
            }
            Err(e) => Claim::Retry(e.to_string()),
        }
    }
}

/// Steam's clock from `ITwoFactorService/QueryTime`, for Steam Guard codes.
pub async fn query_time(http: &reqwest::Client) -> Result<u64, String> {
    let resp = http
        .post("https://api.steampowered.com/ITwoFactorService/QueryTime/v0001")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("steamid=0")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body = resp.text().await.map_err(|e| e.to_string())?;
    dengai::totp::parse_server_time(&body).ok_or_else(|| "no server_time in the reply".to_string())
}
