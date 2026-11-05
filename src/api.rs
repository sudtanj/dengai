//! Small HTTP API with a bearer token, and the types it shares with the bot.

use crate::badges::BadgeGame;
use crate::totp;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{
    header::{AUTHORIZATION, CONTENT_SECURITY_POLICY},
    HeaderMap, StatusCode,
};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// Most keys or ids in one request.
const MAX_ITEMS: usize = 50;
/// How long a request waits for the bot's answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
/// Least time between accepted `/login` attempts, so Steam isn't hammered.
const LOGIN_GAP: Duration = Duration::from_secs(10);
/// Longest username, password, guard code or shared secret accepted.
const MAX_FIELD: usize = 256;
/// The page loads nothing from elsewhere and only talks to its own origin.
const SETUP_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Where the Steam login stands, for the setup page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    /// Starting, or reconnecting with the saved refresh token or the configured password.
    #[default]
    Connecting,
    NeedsLogin,
    /// Steam accepts a code now (approving in the app still works too).
    WaitingForCode,
    WaitingForAppApproval,
    LoggedIn,
}

/// A login from the setup page. The password is never stored or logged.
#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    pub guard_code: Option<String>,
    pub shared_secret: Option<String>,
}

/// What the API sees.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Status {
    pub login: LoginState,
    pub username: String,
    pub login_error: Option<String>,
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
    Login(LoginRequest),
    LoginCode(String),
}

#[derive(Clone)]
struct Api {
    token: Arc<str>,
    shared: Shared,
    tx: mpsc::Sender<Command>,
    pause: watch::Sender<bool>,
    last_login: Arc<Mutex<Option<Instant>>>,
}

type Reply = (StatusCode, Json<Value>);

fn error(status: StatusCode, msg: &str) -> Reply {
    (status, Json(json!({ "error": msg })))
}

/// Constant-time comparison of the bearer token.
fn authorized(api: &Api, headers: &HeaderMap) -> bool {
    let Some(given) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let (a, b) = (given.as_bytes(), api.token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn router(token: String, shared: Shared, tx: mpsc::Sender<Command>, pause: watch::Sender<bool>) -> Router {
    let api = Api { token: token.into(), shared, tx, pause, last_login: Arc::default() };
    Router::new()
        .route("/", get(index))
        .route("/status", get(status))
        .route("/login", post(login))
        .route("/login/code", post(login_code))
        .route("/redeem", post(redeem))
        .route("/addlicense", post(addlicense))
        .route("/farm/pause", post(pause_farm))
        .route("/farm/resume", post(resume_farm))
        .with_state(api)
}

pub async fn serve(
    bind: String,
    token: String,
    shared: Shared,
    tx: mpsc::Sender<Command>,
    pause: watch::Sender<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("API listening on http://{bind}");
    axum::serve(listener, router(token, shared, tx, pause)).await
}

async fn status(State(api): State<Api>, headers: HeaderMap) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let status = api.shared.lock().unwrap().clone();
    (StatusCode::OK, Json(serde_json::to_value(status).unwrap_or_default()))
}

#[derive(Deserialize)]
struct Keys {
    keys: Vec<String>,
}

#[derive(Deserialize)]
struct Ids {
    ids: Vec<String>,
}

async fn run(api: &Api, make: impl FnOnce(oneshot::Sender<Vec<ItemResult>>) -> Command) -> Reply {
    let (tx, rx) = oneshot::channel();
    if api.tx.send(make(tx)).await.is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "bot stopped");
    }
    match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
        Ok(Ok(results)) => (StatusCode::OK, Json(json!({ "results": results }))),
        Ok(Err(_)) => error(StatusCode::SERVICE_UNAVAILABLE, "steam connection lost, try again"),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "timed out waiting for steam, check /status"),
    }
}

async fn redeem(State(api): State<Api>, headers: HeaderMap, body: Bytes) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match serde_json::from_slice::<Keys>(&body) {
        Ok(k) if (1..=MAX_ITEMS).contains(&k.keys.len()) => run(&api, |tx| Command::Redeem(k.keys, tx)).await,
        _ => error(StatusCode::BAD_REQUEST, "expected {\"keys\": [\"AAAAA-BBBBB-CCCCC\", ...]} with 1 to 50 keys"),
    }
}

async fn addlicense(State(api): State<Api>, headers: HeaderMap, body: Bytes) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match serde_json::from_slice::<Ids>(&body) {
        Ok(i) if (1..=MAX_ITEMS).contains(&i.ids.len()) => run(&api, |tx| Command::AddLicense(i.ids, tx)).await,
        _ => error(StatusCode::BAD_REQUEST, "expected {\"ids\": [\"sub/123\", \"a/456\", ...]} with 1 to 50 ids"),
    }
}

async fn set_paused(api: Api, headers: HeaderMap, paused: bool) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    api.shared.lock().unwrap().farm_paused = paused;
    // Wakes the farm loop; a watch value can't be lost the way a full command queue's message can.
    api.pause.send_replace(paused);
    (StatusCode::OK, Json(json!({ "farm_paused": paused })))
}

async fn pause_farm(State(api): State<Api>, headers: HeaderMap) -> Reply {
    set_paused(api, headers, true).await
}

async fn resume_farm(State(api): State<Api>, headers: HeaderMap) -> Reply {
    set_paused(api, headers, false).await
}

async fn index() -> impl IntoResponse {
    ([(CONTENT_SECURITY_POLICY, SETUP_CSP)], Html(include_str!("setup.html")))
}

/// Trims the fields and checks them; `None` when the request can't be a login.
fn clean_login(mut r: LoginRequest) -> Option<LoginRequest> {
    let blank_is_none = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    r.username = r.username.trim().to_string();
    r.guard_code = blank_is_none(r.guard_code);
    r.shared_secret = blank_is_none(r.shared_secret);
    let fits = |s: &str| (1..=MAX_FIELD).contains(&s.len());
    let ok = fits(&r.username)
        && fits(&r.password)
        && r.guard_code.as_deref().is_none_or(|c| c.len() <= MAX_FIELD)
        && r.shared_secret.as_deref().is_none_or(|s| s.len() <= MAX_FIELD && totp::steam_code_b64(s, 0).is_ok());
    ok.then_some(r)
}

async fn login(State(api): State<Api>, headers: HeaderMap, body: Bytes) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let Some(req) = serde_json::from_slice(&body).ok().and_then(clean_login) else {
        return error(
            StatusCode::BAD_REQUEST,
            "expected {\"username\", \"password\", \"guard_code\"?, \"shared_secret\"?}",
        );
    };
    {
        // The state is the lock against concurrent attempts: only the bot moves it back to needs_login.
        let mut status = api.shared.lock().unwrap();
        if status.login != LoginState::NeedsLogin {
            return error(StatusCode::CONFLICT, "not waiting for a login, check /status");
        }
        let mut last = api.last_login.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < LOGIN_GAP) {
            return error(StatusCode::TOO_MANY_REQUESTS, "wait 10 seconds between login attempts");
        }
        *last = Some(Instant::now());
        status.login = LoginState::WaitingForAppApproval;
        status.login_error = None;
    }
    if api.tx.try_send(Command::Login(req)).is_err() {
        api.shared.lock().unwrap().login = LoginState::NeedsLogin;
        return error(StatusCode::SERVICE_UNAVAILABLE, "bot busy or stopped, try again");
    }
    (StatusCode::ACCEPTED, Json(json!({ "login": LoginState::WaitingForAppApproval })))
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

async fn login_code(State(api): State<Api>, headers: HeaderMap, body: Bytes) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let code = match serde_json::from_slice::<Code>(&body) {
        Ok(c)
            if (1..=16).contains(&c.code.trim().len()) && c.code.trim().chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            c.code.trim().to_string()
        }
        _ => return error(StatusCode::BAD_REQUEST, "expected {\"code\": \"ABCDE\"}"),
    };
    if api.shared.lock().unwrap().login != LoginState::WaitingForCode {
        return error(StatusCode::CONFLICT, "steam is not asking for a code, check /status");
    }
    if api.tx.try_send(Command::LoginCode(code)).is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "bot busy or stopped, try again");
    }
    (StatusCode::ACCEPTED, Json(json!({ "login": LoginState::WaitingForCode })))
}
