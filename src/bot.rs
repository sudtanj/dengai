//! The Steam session: login, card farming, key redemption, free licenses and PICS auto-claim.

use crate::web::Web;
use dengai::api::{ClaimLog, Command, ItemResult, Shared, Status};
use dengai::badges::{self, BadgeGame};
use dengai::config::Config;
use dengai::pics::{self, App, Owned, Package, Skip};
use dengai::purchase::{self, Claim};
use dengai::state::{self, Id, State, MAX_RETRIES};
use dengai::{now, totp};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use steam_vent::auth::{
    AuthConfirmationHandler, ConsoleAuthConfirmationHandler, DeviceConfirmationHandler, FileGuardDataStore,
    UserProvidedAuthConfirmationHandler,
};
use steam_vent::{Connection, ConnectionError, ConnectionTrait, EResult, LoginError, NetworkError, ServerList};
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
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, sleep_until, Instant};
use tracing::{debug, error, info, warn};

const RECENT: usize = 20;
const WEB_SESSION_TTL: Duration = Duration::from_secs(6 * 3600);
const CLOCK_TTL: Duration = Duration::from_secs(12 * 3600);
const RECONNECT_MIN_SECS: u64 = 30;
/// Backfill ids checked per PICS tick, so one huge list never holds up the loop.
const BACKFILL_BATCH: usize = 200;
/// Products per PICS request for apps, whose buffers are large.
const APP_CHUNK: usize = 10;

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
    /// Set by the API; a change wakes the farm loop.
    pause: watch::Receiver<bool>,
    http: reqwest::Client,
    state: State,
    state_path: PathBuf,
    /// Steam clock offset (`server - local` s) and when it was measured.
    clock: Option<(i64, Instant)>,
    /// Unix time before which a failed backfill fetch isn't retried.
    backfill_retry: u64,
    /// Backfill ids not checked yet; lost on restart, which just fetches the list again.
    backfill_pending: Vec<Id>,
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

impl Session {
    fn owns(&self, id: Id) -> bool {
        match id {
            Id::Sub(n) => self.owned.packages.contains(&n),
            Id::App(n) => self.owned.apps.contains(&n),
        }
    }
}

impl Bot {
    pub fn new(
        cfg: Arc<Config>,
        shared: Shared,
        rx: mpsc::Receiver<Command>,
        pause: watch::Receiver<bool>,
    ) -> Res<Self> {
        let state_path = cfg.data_dir.join("state.json");
        let mut state = State::load(&state_path)?;
        state.bucket.configure(cfg.claims_per_hour, now());
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux aarch64) dengai")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        shared.lock().unwrap().claim_queue = state.queue.len();
        Ok(Bot {
            cfg,
            shared,
            rx,
            pause,
            http,
            state,
            state_path,
            clock: None,
            backfill_retry: 0,
            backfill_pending: Vec::new(),
        })
    }

    /// Steam's time for Steam Guard codes: the local clock plus the offset from `QueryTime`, re-measured
    /// when older than 12 h. Falls back to the last offset, or the local clock, when the call fails.
    // ponytail: synced from connect() (start and reconnects) rather than on a 12 h timer, since codes
    // are only generated there.
    async fn steam_now(&mut self) -> u64 {
        if self.clock.is_none_or(|(_, at)| at.elapsed() > CLOCK_TTL) {
            match crate::web::query_time(&self.http).await {
                Ok(server) => {
                    let offset = totp::time_offset(server, now());
                    debug!("Steam clock offset {offset}s");
                    self.clock = Some((offset, Instant::now()));
                }
                Err(e) if self.clock.is_some() => warn!("Steam time sync failed ({e}), keeping the last offset"),
                Err(e) => warn!("Steam time sync failed ({e}), using the local clock"),
            }
        }
        totp::steam_time(now(), self.clock.map_or(0, |(o, _)| o))
    }

    pub async fn run(mut self) {
        let mut backoff = RECONNECT_MIN_SECS;
        loop {
            let wait = match self.session(&mut backoff).await {
                Ok(Exit::Disconnected) => 10,
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

    /// Blocks automatic and manual claims after Steam rate limited them, for longer on each repeat.
    fn back_off(&mut self) {
        self.state.bucket.back_off(now());
        warn!("free licenses rate limited until {}", self.state.bucket.blocked_until());
        self.save_state();
    }

    async fn connect(&mut self) -> Res<Connection> {
        let cfg = self.cfg.clone();
        if cfg.shared_secret.is_some() {
            self.steam_now().await;
        }
        let servers = ServerList::discover().await.map_err(|e| format!("CM server list: {e}"))?;
        let token_path = cfg.data_dir.join("refresh_token");
        if let Ok(token) = std::fs::read_to_string(&token_path) {
            match Connection::access(&servers, &cfg.username, token.trim()).await {
                Ok(c) => return Ok(c),
                Err(
                    e @ (ConnectionError::AccessToken(_)
                    | ConnectionError::LoginError(LoginError::InvalidCredentials | LoginError::Unknown(_))),
                ) => warn!("saved refresh token rejected ({e}), logging in with the password"),
                Err(e) => return Err(format!("connecting with the saved refresh token: {e}")),
            }
        }
        let password = cfg.password.as_deref().ok_or("no saved refresh token and no password configured")?;
        let guard = FileGuardDataStore::new(cfg.data_dir.join("guard.json"));
        let code = match &cfg.shared_secret {
            Some(s) => Some(totp::steam_code_b64(s, self.steam_now().await)?),
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

    async fn session(&mut self, backoff: &mut u64) -> Res<Exit> {
        let mut conn = self.connect().await?;
        conn.set_timeout(Duration::from_secs(30));
        // Subscribe before yielding so the post-logon license list isn't missed.
        let mut licenses = pin!(conn.on::<CMsgClientLicenseList>());
        let mut playing = pin!(conn.on::<CMsgClientPlayingSessionState>());
        let mut logged_off = pin!(conn.on::<CMsgClientLoggedOff>());
        let steam_id = u64::from(conn.steam_id());
        let country = self.cfg.country.clone().or_else(|| conn.ip_country_code()).unwrap_or_default();
        info!("logged in as {steam_id} (country {country})");
        *backoff = RECONNECT_MIN_SECS;
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
                Some(cmd) = self.rx.recv() => self.command(&mut s, cmd).await,
                Ok(()) = self.pause.changed() => {
                    info!("farming {}", if *self.pause.borrow_and_update() { "paused" } else { "resumed" });
                    s.farm_next = Instant::now();
                }
                Some(Ok(l)) = licenses.next() => self.on_licenses(&mut s, l).await?,
                Some(Ok(p)) = playing.next() => self.on_playing_state(&mut s, p),
                Some(Ok(o)) = logged_off.next() => {
                    warn!("logged off by Steam: {}", eresult(o.eresult()));
                    // 6 LoggedInElsewhere, 34 LogonSessionReplaced.
                    return Ok(if matches!(o.eresult(), 6 | 34) { Exit::LoggedInElsewhere } else { Exit::Disconnected });
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

    async fn command(&mut self, s: &mut Session, cmd: Command) {
        match cmd {
            Command::Redeem(keys, reply) => {
                let r = self.redeem(s, keys).await;
                let _ = reply.send(r);
            }
            Command::AddLicense(ids, reply) => {
                let r = self.add_licenses(s, ids).await;
                let _ = reply.send(r);
            }
        }
    }

    /// A Steam error on one key ends the run but keeps the earlier results; the rest are skipped.
    async fn redeem(&mut self, s: &mut Session, keys: Vec<String>) -> Vec<ItemResult> {
        let mut out = Vec::new();
        let mut skip: Option<String> = None;
        for (i, key) in keys.iter().map(|k| k.trim()).enumerate() {
            if let Some(why) = &skip {
                out.push(ItemResult { id: key.into(), ok: false, result: why.clone(), items: vec![] });
                continue;
            }
            if i > 0 {
                sleep(Duration::from_secs(self.cfg.redeem_delay_secs)).await;
            }
            let req = CMsgClientRegisterKey { key: Some(key.into()), ..Default::default() };
            let resp: Result<CMsgClientPurchaseResponse, _> = s.conn.job(req).await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    out.push(ItemResult { id: key.into(), ok: false, result: net(e), items: vec![] });
                    skip = Some("skipped (steam error)".to_string());
                    continue;
                }
            };
            let detail = resp.purchase_result_details();
            let ok = resp.eresult() == 1;
            if detail == purchase::DETAIL_RATE_LIMITED {
                skip = Some("skipped (rate limited)".to_string());
            }
            let result = if ok {
                "ok".to_string()
            } else {
                format!("{} ({})", purchase::detail_text(detail), eresult(resp.eresult()))
            };
            let items = purchase::receipt_items(resp.purchase_receipt_info());
            info!("key {}: {result} {items:?}", mask(key));
            out.push(ItemResult { id: key.into(), ok, result, items });
        }
        out
    }

    /// Claims through the shared token bucket. A rate limit ends the run, the rest are skipped.
    async fn add_licenses(&mut self, s: &mut Session, ids: Vec<String>) -> Vec<ItemResult> {
        let mut out = Vec::new();
        let mut limited = false;
        for (i, raw) in ids.iter().enumerate() {
            let id = raw.trim();
            let (ok, result) = match id.parse::<Id>() {
                _ if limited => (false, "skipped (rate limited)".to_string()),
                Ok(lic) => {
                    if i > 0 {
                        sleep(Duration::from_secs(self.cfg.redeem_delay_secs)).await;
                    }
                    if self.state.bucket.try_take(now()) {
                        let claim = self.claim(s, lic).await;
                        match claim {
                            Claim::Ok => {
                                self.state.bucket.reset_backoff();
                                self.state.queue.retain(|q| *q != lic);
                                self.state.done.insert(lic);
                                self.save_state();
                            }
                            Claim::RateLimited => {
                                limited = true;
                                self.back_off();
                            }
                            _ => {}
                        }
                        (claim == Claim::Ok, claim_text(&claim))
                    } else {
                        (false, "claim limit reached, try again later".to_string())
                    }
                }
                Err(e) => (false, e),
            };
            self.log_claim(id.to_string(), result.clone());
            out.push(ItemResult { id: id.into(), ok, result, items: vec![] });
        }
        out
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
            send_app_info_changes: Some(self.cfg.auto_claim && self.cfg.auto_claim_apps),
            send_package_info_changes: Some(true),
            ..Default::default()
        };
        let resp: CMsgClientPICSChangesSinceResponse = s.conn.job(req).await.map_err(net)?;
        if !self.cfg.auto_claim {
            return Ok(());
        }
        if !s.licenses_known {
            // Wait for the license list before evaluating anything.
            return Ok(());
        }
        let current = resp.current_change_number();
        if current != self.state.change_number {
            if self.state.change_number == 0 || resp.force_full_update() || resp.force_full_package_update() {
                info!("PICS: starting from change {current}");
            } else {
                let subs = resp.package_changes.iter().map(|p| Id::Sub(p.packageid()));
                let ids: Vec<Id> = subs.chain(resp.app_changes.iter().map(|a| Id::App(a.appid()))).collect();
                debug!("PICS: {} changes since {}", ids.len(), self.state.change_number);
                self.evaluate(s, &ids).await?;
            }
            self.state.change_number = current;
            self.save_state();
        }
        self.backfill(s).await?;
        self.claim_next(s).await
    }

    /// Fetches `backfill_url` on the first start (then every `backfill_interval_secs`) and checks its ids
    /// exactly like PICS changes, `BACKFILL_BATCH` per call; nothing from the list is queued without
    /// passing the filter.
    async fn backfill(&mut self, s: &mut Session) -> Res<()> {
        if self.backfill_pending.is_empty() {
            let (cfg, t) = (self.cfg.clone(), now());
            let every = cfg.backfill_interval_secs;
            let due = self.state.backfilled_at == 0 || (every > 0 && t >= self.state.backfilled_at + every);
            if cfg.backfill_url.is_empty() || !due || t < self.backfill_retry {
                return Ok(());
            }
            // reqwest errors embed the full URL, which may carry a secret in its path or query.
            let host = reqwest::Url::parse(&cfg.backfill_url).ok().and_then(|u| u.host_str().map(str::to_owned));
            let host = host.as_deref().unwrap_or("?");
            let body = match self.http.get(cfg.backfill_url.as_str()).send().await.and_then(|r| r.error_for_status()) {
                Ok(r) => r.text().await,
                Err(e) => Err(e),
            };
            self.backfill_pending = match body {
                Ok(b) => state::parse_id_list(&b),
                Err(e) => {
                    warn!("backfill {host}: {}; retrying in an hour", e.without_url());
                    self.backfill_retry = t + 3600;
                    return Ok(());
                }
            };
            info!("backfill: checking {} ids from {host}", self.backfill_pending.len());
        }
        let n = BACKFILL_BATCH.min(self.backfill_pending.len());
        let batch = self.backfill_pending[..n].to_vec();
        self.evaluate(s, &batch).await?;
        self.backfill_pending.drain(..n);
        if self.backfill_pending.is_empty() {
            self.state.backfilled_at = now();
            self.save_state();
        }
        Ok(())
    }

    fn skip(&self) -> Skip {
        Skip { demos: self.cfg.skip_demos, playtests: self.cfg.skip_playtests, unowned_dlc: self.cfg.skip_unowned_dlc }
    }

    /// Queues `id` when the filter passed. Returns 1 when it was new.
    fn verdict(&mut self, id: Id, v: Result<(), &str>) -> usize {
        match v {
            Ok(()) if self.state.enqueue(id) => {
                info!("{id}: free, queued");
                1
            }
            Ok(()) => 0,
            Err(why) => {
                debug!("{id}: {why}");
                0
            }
        }
    }

    /// Checks licenses that aren't owned or known yet with PICS product info, in batches, and queues
    /// the claimable ones. Apps are dropped unless `auto_claim_apps`.
    async fn evaluate(&mut self, s: &mut Session, ids: &[Id]) -> Res<()> {
        let fresh = |id: &&Id| !s.owns(**id) && !self.state.known(**id);
        let pick = |want_app: bool| -> Vec<u32> {
            let mut v: Vec<u32> = ids
                .iter()
                .filter(fresh)
                .filter_map(|id| match *id {
                    Id::Sub(n) if !want_app => Some(n),
                    Id::App(n) if want_app => Some(n),
                    _ => None,
                })
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let subs = pick(false);
        let apps = if self.cfg.auto_claim_apps { pick(true) } else { Vec::new() };
        let mut added = 0;
        for chunk in subs.chunks(100) {
            added += self.evaluate_subs(s, chunk).await?;
        }
        let skip = self.skip();
        for chunk in apps.chunks(APP_CHUNK) {
            for app in self.product_info(s, &[], chunk).await?.1.values() {
                added += self.verdict(Id::App(app.id), pics::check_app(app, &s.owned, skip));
            }
        }
        if added > 0 {
            self.save_state();
        }
        Ok(())
    }

    async fn evaluate_subs(&mut self, s: &mut Session, ids: &[u32]) -> Res<usize> {
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
        let mut app_ids: Vec<u32> = candidates.iter().flat_map(|p| p.app_ids.iter().copied()).collect();
        app_ids.sort_unstable();
        app_ids.dedup();
        let mut apps = HashMap::new();
        for chunk in app_ids.chunks(APP_CHUNK) {
            apps.extend(self.product_info(s, &[], chunk).await?.1);
        }
        let skip = self.skip();
        let mut added = 0;
        for p in candidates {
            let contents: Vec<App> = p.app_ids.iter().filter_map(|id| apps.get(id).cloned()).collect();
            added += self.verdict(Id::Sub(p.id), pics::check_apps(&p, &contents, &s.owned, skip));
        }
        Ok(added)
    }

    /// One free-license request: `sub/` through the store, `a/` through `CMsgClientRequestFreeLicense`.
    async fn claim(&self, s: &mut Session, id: Id) -> Claim {
        match id {
            Id::Sub(n) => match self.web(s).await {
                Ok(web) => web.add_free_license(n).await,
                Err(e) => Claim::Retry(e),
            },
            Id::App(n) => {
                let req = CMsgClientRequestFreeLicense { appids: vec![n], ..Default::default() };
                let resp: Result<CMsgClientRequestFreeLicenseResponse, _> = s.conn.job(req).await;
                match resp {
                    Ok(r) => purchase::classify_free_app(
                        r.eresult() as i32,
                        !(r.granted_appids.is_empty() && r.granted_packageids.is_empty()),
                    ),
                    Err(e) => Claim::Retry(net(e)),
                }
            }
        }
    }

    /// Claims at most one queued license per call, within the token bucket.
    async fn claim_next(&mut self, s: &mut Session) -> Res<()> {
        while let Some(&id) = self.state.queue.front() {
            if !s.owns(id) {
                break;
            }
            self.state.queue.pop_front();
            self.state.done.insert(id);
            self.save_state();
        }
        let Some(&id) = self.state.queue.front() else { return Ok(()) };
        if !self.state.bucket.try_take(now()) {
            return Ok(());
        }
        let claim = self.claim(s, id).await;
        match &claim {
            Claim::Ok => self.state.bucket.reset_backoff(),
            Claim::RateLimited => self.back_off(),
            Claim::Retry(_) => s.web = None,
            Claim::Rejected(_) => {}
        }
        if self.state.settle(id, &claim) {
            warn!("{id}: giving up after {MAX_RETRIES} failures");
        }
        self.log_claim(id.to_string(), claim_text(&claim));
        self.save_state();
        Ok(())
    }
}

fn claim_text(c: &Claim) -> String {
    match c {
        Claim::Ok => "ok".into(),
        Claim::RateLimited => "rate limited, backing off".into(),
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
