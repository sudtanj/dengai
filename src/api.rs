//! Small HTTP API with a bearer token, and the types it shares with the bot.

use crate::badges::BadgeGame;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// Most keys or ids in one request.
const MAX_ITEMS: usize = 50;
/// How long a request waits for the bot's answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

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
}

#[derive(Clone)]
struct Api {
    token: Arc<str>,
    shared: Shared,
    tx: mpsc::Sender<Command>,
    pause: watch::Sender<bool>,
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
    let api = Api { token: token.into(), shared, tx, pause };
    Router::new()
        .route("/status", get(status))
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
