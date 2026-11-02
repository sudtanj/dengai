//! The Steam session: login, card farming, key redemption, free licenses and PICS auto-claim.

use crate::web::Web;
use dengai::badges::{self, BadgeGame};
use dengai::config::Config;
use dengai::pics::{self, App, Owned, Package, Skip};
use dengai::purchase::{self, Claim};
use dengai::state::{State, TokenBucket};
use dengai::{now, totp};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use steam_vent::auth::{
    AuthConfirmationHandler, ConsoleAuthConfirmationHandler, DeviceConfirmationHandler, FileGuardDataStore,
    UserProvidedAuthConfirmationHandler,
};
use steam_vent::{Connection, ConnectionTrait, EResult, NetworkError, ServerList};
use steam_vent_proto::enums_clientserver::EMsg;
use steam_vent_proto::steammessages_auth_steamclient::CAuthentication_AccessToken_GenerateForApp_Request;
use steam_vent_proto::steammessages_clientserver::{
    cmsg_client_games_played, CMsgClientGamesPlayed, CMsgClientLicenseList,
};
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientPlayingSessionState, CMsgClientPurchaseResponse, CMsgClientRegisterKey, CMsgClientRequestFreeLicense,
    CMsgClientRequestFreeLicenseResponse,
};
use steam_vent_proto::steammessages_clientserver_appinfo::{
    cmsg_client_picsproduct_info_request, CMsgClientPICSAccessTokenRequest, CMsgClientPICSAccessTokenResponse,
    CMsgClientPICSChangesSinceRequest, CMsgClientPICSChangesSinceResponse, CMsgClientPICSProductInfoRequest,
    CMsgClientPICSProductInfoResponse,
};
use steam_vent_proto::steammessages_clientserver_login::CMsgClientLoggedOff;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, sleep_until, Instant};
use tracing::{debug, error, info, warn};

const RECENT: usize = 20;
const WEB_SESSION_TTL: Duration = Duration::from_secs(6 * 3600);

/// What the API sees.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Status {
    pub connected: bool,
    pub steam_id: u64,
    pub farming: Option<BadgeGame>,
    pub games_to_farm: usize,
    pub farm_paused: bool,
    pub playing_blocked: bool,
    pub claim_queue: usize,
    pub change_number: u32,
    pub recent_claims: VecDeque<ClaimLog>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimLog {
    pub id: String,
    pub result: String,
    pub at: u64,
}

pub type Shared = Arc<Mutex<Status>>;

#[derive(Debug, Serialize)]
pub struct ItemResult {
    pub id: String,
    pub ok: bool,
    pub result: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

pub enum Command {
    Redeem(Vec<String>, oneshot::Sender<Vec<ItemResult>>),
    AddLicense(Vec<String>, oneshot::Sender<Vec<ItemResult>>),
    Pause(bool),
}

enum Exit {
    Disconnected,
    LoggedInElsewhere,
}

type Res<T> = Result<T, String>;

fn net(e: NetworkError) -> String {
    format!("steam: {e}")
}

fn eresult(r: i32) -> String {
    match EResult::from_result(r) {
        Ok(()) => "OK".into(),
        Err(e) => format!("{e:?} ({r})"),
    }
}

fn mask(key: &str) -> String {
    format!("{}-*****", key.chars().take(5).collect::<String>())
}

pub struct Bot {
    cfg: Arc<Config>,
    shared: Shared,
    rx: mpsc::Receiver<Command>,
    http: reqwest::Client,
    state: State,
    state_path: PathBuf,
    bucket: TokenBucket,
}

/// Per-connection state.
struct Session {
    conn: Connection,
    steam_id: u64,
    refresh_token: String,
    web: Option<(Web, Instant)>,
    owned: Owned,
    licenses_known: bool,
    games: Vec<BadgeGame>,
    playing: Option<u32>,
    farm_next: Instant,
    scan_next: Instant,
    blocked: bool,
}

impl Bot {
    pub fn new(cfg: Arc<Config>, shared: Shared, rx: mpsc::Receiver<Command>) -> Res<Self> {
        let state_path = cfg.data_dir.join("state.json");
        let state = State::load(&state_path)?;
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux aarch64) dengai")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let bucket = TokenBucket::new(cfg.claims_per_hour, now());
        shared.lock().unwrap().claim_queue = state.queue.len();
        Ok(Bot { cfg, shared, rx, http, state, state_path, bucket })
    }

    pub async fn run(mut self) {
        let mut backoff = 30;
        loop {
            let wait = match self.session().await {
                Ok(Exit::Disconnected) => {
                    backoff = 30;
                    10
                }
                Ok(Exit::LoggedInElsewhere) => {
                    warn!("logged in elsewhere; reconnecting in 5 minutes");
                    300
                }
                Err(e) => {
                    error!("{e}; retrying in {backoff}s");
                    backoff = (backoff * 2).min(1800);
                    backoff / 2
                }
            };
            self.status(|s| {
                s.connected = false;
                s.farming = None;
            });
            sleep(Duration::from_secs(wait)).await;
        }
    }

    fn status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.shared.lock().unwrap());
    }

    fn log_claim(&self, id: String, result: String) {
        info!("{id}: {result}");
        self.status(|s| {
            s.recent_claims.push_front(ClaimLog { id, result, at: now() });
            s.recent_claims.truncate(RECENT);
        });
    }

    fn save_state(&self) {
        if let Err(e) = self.state.save(&self.state_path) {
            error!("saving {}: {e}", self.state_path.display());
        }
        let (queue, change) = (self.state.queue.len(), self.state.change_number);
        self.status(|s| {
            s.claim_queue = queue;
            s.change_number = change;
        });
    }

    async fn connect(&self) -> Res<Connection> {
        let cfg = &self.cfg;
        let servers = ServerList::discover().await.map_err(|e| format!("CM server list: {e}"))?;
        let token_path = cfg.data_dir.join("refresh_token");
        if let Ok(token) = std::fs::read_to_string(&token_path) {
            match Connection::access(&servers, &cfg.username, token.trim()).await {
                Ok(c) => return Ok(c),
                Err(e) => warn!("saved refresh token rejected ({e}), logging in with the password"),
            }
        }
        let password = cfg.password.as_deref().ok_or("no saved refresh token and no password configured")?;
        let guard = FileGuardDataStore::new(cfg.data_dir.join("guard.json"));
        let code = match &cfg.shared_secret {
            Some(s) => Some(totp::steam_code_b64(s, now())?),
            None => cfg.guard_code.clone(),
        };
        let conn = match code {
            Some(code) => {
                let input = std::io::Cursor::new(format!("{code}\n").into_bytes());
                let handler = UserProvidedAuthConfirmationHandler::new(input, tokio::io::sink());
                Connection::login(&servers, &cfg.username, password, guard, handler).await
            }
            None if std::io::stdin().is_terminal() => {
                info!("confirm the login in the Steam app, or type the Steam Guard code");
                let handler = DeviceConfirmationHandler.or(ConsoleAuthConfirmationHandler::default());
                Connection::login(&servers, &cfg.username, password, guard, handler).await
            }
            None => {
                info!("confirm the login in the Steam mobile app (or set DENGAI_SHARED_SECRET / DENGAI_GUARD_CODE)");
                Connection::login(&servers, &cfg.username, password, guard, DeviceConfirmationHandler).await
            }
        }
        .map_err(|e| format!("login failed: {e}"))?;
        if let Some(token) = conn.access_token() {
            if let Err(e) = write_private(&token_path, token) {
                error!("saving {}: {e}", token_path.display());
            }
        }
        Ok(conn)
    }

    async fn session(&mut self) -> Res<Exit> {
        let mut conn = self.connect().await?;
        conn.set_timeout(Duration::from_secs(30));
        // Subscribe before yielding so the post-logon license list isn't missed.
        let mut licenses = pin!(conn.on::<CMsgClientLicenseList>());
        let mut playing = pin!(conn.on::<CMsgClientPlayingSessionState>());
        let mut logged_off = pin!(conn.on::<CMsgClientLoggedOff>());
        let steam_id = u64::from(conn.steam_id());
        let country = self.cfg.country.clone().or_else(|| conn.ip_country_code()).unwrap_or_default();
        info!("logged in as {steam_id} (country {country})");
        let mut s = Session {
            refresh_token: conn.access_token().unwrap_or_default().to_string(),
            conn,
            steam_id,
            web: None,
            owned: Owned { country: country.to_ascii_uppercase(), ..Owned::default() },
            licenses_known: false,
            games: Vec::new(),
            playing: None,
            farm_next: Instant::now() + Duration::from_secs(5),
            scan_next: Instant::now(),
            blocked: false,
        };
        self.status(|st| {
            st.connected = true;
            st.steam_id = steam_id;
        });
        for raw in s.conn.take_unprocessed() {
            if raw.kind == EMsg::k_EMsgClientLicenseList {
                if let Ok(l) = raw.into_message::<CMsgClientLicenseList>() {
                    self.on_licenses(&mut s, l).await?;
                }
            } else if raw.kind == EMsg::k_EMsgClientPlayingSessionState {
                if let Ok(p) = raw.into_message::<CMsgClientPlayingSessionState>() {
                    self.on_playing_state(&mut s, p);
                }
            }
        }
        let mut pics_tick = tokio::time::interval(Duration::from_secs(self.cfg.pics_poll_secs));
        loop {
            tokio::select! {
                Some(cmd) = self.rx.recv() => self.command(&mut s, cmd).await?,
                Some(Ok(l)) = licenses.next() => self.on_licenses(&mut s, l).await?,
                Some(Ok(p)) = playing.next() => self.on_playing_state(&mut s, p),
                Some(Ok(o)) = logged_off.next() => {
                    warn!("logged off by Steam: {}", eresult(o.eresult()));
                    return Ok(if o.eresult() == 6 { Exit::LoggedInElsewhere } else { Exit::Disconnected });
                }
                _ = sleep_until(s.farm_next), if self.cfg.farm => self.farm(&mut s).await?,
                _ = pics_tick.tick() => self.pics(&mut s).await?,
            }
        }
    }

    async fn web<'a>(&self, s: &'a mut Session) -> Res<&'a Web> {
        if s.web.as_ref().is_none_or(|(_, at)| at.elapsed() > WEB_SESSION_TTL) {
            let req = CAuthentication_AccessToken_GenerateForApp_Request {
                refresh_token: Some(s.refresh_token.clone()),
                steamid: Some(s.steam_id),
                ..Default::default()
            };
            let resp = s.conn.service_method(req).await.map_err(net)?;
            let token = resp.access_token();
            if token.is_empty() {
                return Err("steam returned no web access token".into());
            }
            s.web = Some((Web::new(self.http.clone(), s.steam_id, token), Instant::now()));
        }
        Ok(&s.web.as_ref().expect("set above").0)
    }

    // ---- commands ----

    async fn command(&mut self, s: &mut Session, cmd: Command) -> Res<()> {
        match cmd {
            Command::Pause(paused) => {
                info!("farming {}", if paused { "paused" } else { "resumed" });
                self.status(|st| st.farm_paused = paused);
                s.farm_next = Instant::now();
            }
            Command::Redeem(keys, reply) => {
                let r = self.redeem(s, keys).await?;
                let _ = reply.send(r);
            }
            Command::AddLicense(ids, reply) => {
                let r = self.add_licenses(s, ids).await?;
                let _ = reply.send(r);
            }
        }
        Ok(())
    }

    async fn redeem(&mut self, s: &mut Session, keys: Vec<String>) -> Res<Vec<ItemResult>> {
        let mut out = Vec::new();
        let mut limited = false;
        for (i, key) in keys.iter().map(|k| k.trim()).enumerate() {
            if limited {
                out.push(ItemResult {
                    id: key.into(),
                    ok: false,
                    result: "skipped (rate limited)".into(),
                    items: vec![],
                });
                continue;
            }
            if i > 0 {
                sleep(Duration::from_secs(self.cfg.redeem_delay_secs)).await;
            }
            let req = CMsgClientRegisterKey { key: Some(key.into()), ..Default::default() };
            let resp: CMsgClientPurchaseResponse = s.conn.job(req).await.map_err(net)?;
            let detail = resp.purchase_result_details();
            let ok = resp.eresult() == 1;
            limited = detail == purchase::DETAIL_RATE_LIMITED;
            let result = if ok {
                "ok".to_string()
            } else {
                format!("{} ({})", purchase::detail_text(detail), eresult(resp.eresult()))
            };
            let items = purchase::receipt_items(resp.purchase_receipt_info());
            info!("key {}: {result} {items:?}", mask(key));
            out.push(ItemResult { id: key.into(), ok, result, items });
        }
        Ok(out)
    }

    async fn add_licenses(&mut self, s: &mut Session, ids: Vec<String>) -> Res<Vec<ItemResult>> {
        let mut out = Vec::new();
        let mut limited = false;
        for (i, raw) in ids.iter().enumerate() {
            let id = raw.trim();
            let parsed =
                id.split_once('/').and_then(|(kind, n)| Some((kind.to_ascii_lowercase(), n.parse::<u32>().ok()?)));
            let (ok, result) = match parsed {
                _ if limited => (false, "skipped (rate limited)".to_string()),
                Some((kind, n)) if kind == "sub" || kind == "s" => {
                    if i > 0 {
                        sleep(Duration::from_secs(self.cfg.redeem_delay_secs)).await;
                    }
                    let claim = self.web(s).await?.add_free_license(n).await;
                    limited = claim == Claim::RateLimited;
                    if claim == Claim::Ok {
                        self.state.done.insert(n);
                        self.state.queue.retain(|q| *q != n);
                        self.save_state();
                    }
                    (claim == Claim::Ok, claim_text(&claim))
                }
                Some((kind, n)) if kind == "a" || kind == "app" => {
                    if i > 0 {
                        sleep(Duration::from_secs(self.cfg.redeem_delay_secs)).await;
                    }
                    let req = CMsgClientRequestFreeLicense { appids: vec![n], ..Default::default() };
                    let resp: CMsgClientRequestFreeLicenseResponse = s.conn.job(req).await.map_err(net)?;
                    let r = resp.eresult() as i32;
                    limited = r == 84; // RateLimitExceeded
                    match (r, resp.granted_appids.is_empty() && resp.granted_packageids.is_empty()) {
                        (1, false) => (true, format!("ok, granted subs {:?}", resp.granted_packageids)),
                        (1, true) => (false, "nothing granted (not free, or already owned)".to_string()),
                        _ => (false, eresult(r)),
                    }
                }
                _ => (false, "invalid id, use sub/<id> or a/<id>".to_string()),
            };
            self.log_claim(id.to_string(), result.clone());
            out.push(ItemResult { id: id.into(), ok, result, items: vec![] });
        }
        Ok(out)
    }

    // ---- farming ----

    fn on_playing_state(&mut self, s: &mut Session, p: CMsgClientPlayingSessionState) {
        let blocked = p.playing_blocked();
        if blocked != s.blocked {
            info!(
                "{}",
                if blocked { "account is playing elsewhere, farming paused" } else { "playing elsewhere ended" }
            );
            s.blocked = blocked;
            if blocked {
                // Steam already stopped our session's game.
                s.playing = None;
            }
            s.farm_next = Instant::now() + Duration::from_secs(if blocked { 0 } else { 5 });
            self.status(|st| st.playing_blocked = blocked);
        }
    }

    async fn play(&self, s: &mut Session, appid: Option<u32>) -> Res<()> {
        if s.playing == appid {
            return Ok(());
        }
        let msg = CMsgClientGamesPlayed {
            games_played: appid
                .map(|id| cmsg_client_games_played::GamePlayed { game_id: Some(id as u64), ..Default::default() })
                .into_iter()
                .collect(),
            ..Default::default()
        };
        s.conn.send_with_kind(msg, EMsg::k_EMsgClientGamesPlayedWithDataBlob).await.map_err(net)?;
        s.playing = appid;
        Ok(())
    }

    async fn scan(&self, s: &mut Session) -> Res<Vec<BadgeGame>> {
        let mut games = Vec::new();
        let mut page = 1;
        loop {
            let html = self.web(s).await?.badges(page).await?;
            let (found, pages) = badges::parse_badges(&html);
            for g in found {
                if !games.iter().any(|x: &BadgeGame| x.appid == g.appid) {
                    games.push(g);
                }
            }
            if page >= pages {
                break;
            }
            page += 1;
            sleep(Duration::from_secs(1)).await;
        }
        Ok(games)
    }

    async fn farm(&self, s: &mut Session) -> Res<()> {
        let check = Duration::from_secs(self.cfg.farm_check_secs);
        s.farm_next = Instant::now() + check;
        let paused = self.shared.lock().unwrap().farm_paused;
        if paused || s.blocked {
            if !s.blocked {
                self.play(s, None).await?;
            }
            s.farm_next = Instant::now() + Duration::from_secs(24 * 3600);
            self.status(|st| st.farming = None);
            return Ok(());
        }
        let result = self.farm_step(s).await;
        if let Err(e) = &result {
            warn!("farming: {e}");
            s.web = None;
        }
        let (current, left) = (s.games.first().cloned(), s.games.len());
        if current.is_none() {
            self.play(s, None).await?;
            if result.is_ok() {
                s.farm_next = s.scan_next;
            }
        }
        self.status(|st| {
            st.farming = current;
            st.games_to_farm = left;
        });
        Ok(())
    }

    async fn farm_step(&self, s: &mut Session) -> Res<()> {
        if s.games.is_empty() {
            if Instant::now() < s.scan_next {
                return Ok(());
            }
            s.games = self.scan(s).await?;
            s.scan_next = Instant::now() + Duration::from_secs(self.cfg.farm_rescan_secs);
            info!("{} games with card drops remaining", s.games.len());
        } else if s.playing == Some(s.games[0].appid) {
            let appid = s.games[0].appid;
            let html = self.web(s).await?.gamecards(appid).await?;
            match badges::parse_drops(&html) {
                Some(0) => {
                    info!("finished farming {} ({appid})", s.games[0].name);
                    s.games.remove(0);
                    if s.games.is_empty() {
                        // Confirm with a fresh scan before idling.
                        s.scan_next = Instant::now();
                    }
                }
                Some(n) => s.games[0].drops = n,
                None => return Err(format!("no drop info on the gamecards page of {appid}")),
            }
        }
        if let Some(g) = s.games.first().cloned() {
            if s.playing != Some(g.appid) {
                info!("farming {} ({}), {} drops left", g.name, g.appid, g.drops);
                self.play(s, Some(g.appid)).await?;
            }
        }
        Ok(())
    }

    // ---- licenses and PICS ----

    async fn on_licenses(&mut self, s: &mut Session, list: CMsgClientLicenseList) -> Res<()> {
        let ids: Vec<u32> = list.licenses.iter().map(|l| l.package_id()).collect();
        let new: Vec<u32> = ids.iter().copied().filter(|id| !s.owned.packages.contains(id)).collect();
        s.owned.packages.extend(&ids);
        for chunk in new.chunks(200) {
            let (packages, _) = self.product_info(s, chunk, &[]).await?;
            s.owned.apps.extend(packages.iter().flat_map(|p| p.app_ids.iter().copied()));
        }
        if !s.licenses_known {
            info!("{} licenses, {} apps owned", s.owned.packages.len(), s.owned.apps.len());
        }
        s.licenses_known = true;
        Ok(())
    }

    /// Product info for packages and apps (with PICS access tokens).
    async fn product_info(
        &self,
        s: &Session,
        packages: &[u32],
        apps: &[u32],
    ) -> Res<(Vec<Package>, HashMap<u32, App>)> {
        let req = CMsgClientPICSAccessTokenRequest {
            packageids: packages.to_vec(),
            appids: apps.to_vec(),
            ..Default::default()
        };
        let tokens: CMsgClientPICSAccessTokenResponse = s.conn.job(req).await.map_err(net)?;
        let ptok: HashMap<u32, u64> =
            tokens.package_access_tokens.iter().map(|t| (t.packageid(), t.access_token())).collect();
        let atok: HashMap<u32, u64> = tokens.app_access_tokens.iter().map(|t| (t.appid(), t.access_token())).collect();
        let req = CMsgClientPICSProductInfoRequest {
            packages: packages
                .iter()
                .map(|&id| cmsg_client_picsproduct_info_request::PackageInfo {
                    packageid: Some(id),
                    access_token: Some(ptok.get(&id).copied().unwrap_or(0)),
                    ..Default::default()
                })
                .collect(),
            apps: apps
                .iter()
                .map(|&id| cmsg_client_picsproduct_info_request::AppInfo {
                    appid: Some(id),
                    access_token: Some(atok.get(&id).copied().unwrap_or(0)),
                    ..Default::default()
                })
                .collect(),
            meta_data_only: Some(false),
            ..Default::default()
        };
        let (mut pkgs, mut app_map) = (Vec::new(), HashMap::new());
        let mut stream = pin!(s.conn.job_multi::<_, CMsgClientPICSProductInfoResponse>(req));
        while let Some(resp) = stream.next().await {
            let resp = resp.map_err(net)?;
            pkgs.extend(resp.packages.iter().filter_map(|p| pics::parse_package_buffer(p.packageid(), p.buffer())));
            for a in &resp.apps {
                if let Some(app) = pics::parse_app_buffer(a.appid(), a.buffer()) {
                    app_map.insert(app.id, app);
                }
            }
        }
        Ok((pkgs, app_map))
    }

    async fn pics(&mut self, s: &mut Session) -> Res<()> {
        // Doubles as the connection health check.
        let req = CMsgClientPICSChangesSinceRequest {
            since_change_number: Some(self.state.change_number),
            send_app_info_changes: Some(false),
            send_package_info_changes: Some(true),
            ..Default::default()
        };
        let resp: CMsgClientPICSChangesSinceResponse = s.conn.job(req).await.map_err(net)?;
        if !self.cfg.auto_claim {
            return Ok(());
        }
        let current = resp.current_change_number();
        if current != self.state.change_number {
            if self.state.change_number == 0 || resp.force_full_update() || resp.force_full_package_update() {
                info!("PICS: starting from change {current}");
            } else if s.licenses_known {
                let ids: Vec<u32> = resp
                    .package_changes
                    .iter()
                    .map(|p| p.packageid())
                    .filter(|id| !s.owned.packages.contains(id) && !self.state.known(*id))
                    .collect();
                debug!("PICS: {} changed packages since {}", ids.len(), self.state.change_number);
                for chunk in ids.chunks(100) {
                    self.evaluate(s, chunk).await?;
                }
            } else {
                // Wait for the license list before evaluating anything.
                return Ok(());
            }
            self.state.change_number = current;
            self.save_state();
        }
        self.claim_next(s).await
    }

    async fn evaluate(&mut self, s: &mut Session, ids: &[u32]) -> Res<()> {
        let (packages, _) = self.product_info(s, ids, &[]).await?;
        let now = now();
        let candidates: Vec<Package> = packages
            .into_iter()
            .filter(|p| match pics::precheck(p, &s.owned, now) {
                Ok(()) => true,
                Err(why) => {
                    debug!("sub/{}: {why}", p.id);
                    false
                }
            })
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let mut app_ids: Vec<u32> = candidates.iter().flat_map(|p| p.app_ids.iter().copied()).collect();
        app_ids.sort_unstable();
        app_ids.dedup();
        let mut apps = HashMap::new();
        for chunk in app_ids.chunks(50) {
            apps.extend(self.product_info(s, &[], chunk).await?.1);
        }
        let skip = Skip {
            demos: self.cfg.skip_demos,
            playtests: self.cfg.skip_playtests,
            unowned_dlc: self.cfg.skip_unowned_dlc,
        };
        let mut added = false;
        for p in candidates {
            let contents: Vec<App> = p.app_ids.iter().filter_map(|id| apps.get(id).cloned()).collect();
            match pics::check_apps(&p, &contents, &s.owned, skip) {
                Ok(()) => {
                    if self.state.enqueue(p.id) {
                        info!("sub/{}: free, queued", p.id);
                        added = true;
                    }
                }
                Err(why) => debug!("sub/{}: {why}", p.id),
            }
        }
        if added {
            self.save_state();
        }
        Ok(())
    }

    /// Claims at most one queued package per call, within the token bucket.
    async fn claim_next(&mut self, s: &mut Session) -> Res<()> {
        while let Some(&id) = self.state.queue.front() {
            if !s.owned.packages.contains(&id) {
                break;
            }
            self.state.queue.pop_front();
            self.state.done.insert(id);
            self.save_state();
        }
        let Some(&id) = self.state.queue.front() else { return Ok(()) };
        if !s.licenses_known || !self.bucket.try_take(now()) {
            return Ok(());
        }
        let claim = match self.web(s).await {
            Ok(web) => web.add_free_license(id).await,
            Err(e) => Claim::Retry(e),
        };
        match &claim {
            Claim::Ok => {
                self.state.queue.pop_front();
                self.state.done.insert(id);
            }
            Claim::RateLimited => {
                self.bucket.back_off(now(), 3600);
                warn!("free licenses rate limited until {}", self.bucket.blocked_until());
            }
            Claim::Retry(_) => {
                // Move it to the back so one bad package can't block the queue.
                self.state.queue.pop_front();
                self.state.queue.push_back(id);
                s.web = None;
            }
            Claim::Rejected(_) => {
                self.state.queue.pop_front();
                self.state.rejected.insert(id);
            }
        }
        self.log_claim(format!("sub/{id}"), claim_text(&claim));
        self.save_state();
        Ok(())
    }
}

fn claim_text(c: &Claim) -> String {
    match c {
        Claim::Ok => "ok".into(),
        Claim::RateLimited => "rate limited, try again in an hour".into(),
        Claim::Retry(e) => format!("will retry: {e}"),
        Claim::Rejected(e) => format!("rejected: {e}"),
    }
}

fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}
