//! Small HTTP API with a bearer token.

use crate::bot::{Command, ItemResult, Shared};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
struct Api {
    token: Arc<str>,
    shared: Shared,
    tx: mpsc::Sender<Command>,
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

pub async fn serve(bind: String, token: String, shared: Shared, tx: mpsc::Sender<Command>) -> std::io::Result<()> {
    let api = Api { token: token.into(), shared, tx };
    let app = Router::new()
        .route("/status", get(status))
        .route("/redeem", post(redeem))
        .route("/addlicense", post(addlicense))
        .route("/farm/pause", post(pause))
        .route("/farm/resume", post(resume))
        .with_state(api);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("API listening on http://{bind}");
    axum::serve(listener, app).await
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
    match rx.await {
        Ok(results) => (StatusCode::OK, Json(json!({ "results": results }))),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "steam connection lost, try again"),
    }
}

async fn redeem(State(api): State<Api>, headers: HeaderMap, body: Bytes) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match serde_json::from_slice::<Keys>(&body) {
        Ok(k) if !k.keys.is_empty() => run(&api, |tx| Command::Redeem(k.keys, tx)).await,
        _ => error(StatusCode::BAD_REQUEST, "expected {\"keys\": [\"AAAAA-BBBBB-CCCCC\", ...]}"),
    }
}

async fn addlicense(State(api): State<Api>, headers: HeaderMap, body: Bytes) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match serde_json::from_slice::<Ids>(&body) {
        Ok(i) if !i.ids.is_empty() => run(&api, |tx| Command::AddLicense(i.ids, tx)).await,
        _ => error(StatusCode::BAD_REQUEST, "expected {\"ids\": [\"sub/123\", \"a/456\", ...]}"),
    }
}

async fn set_paused(api: Api, headers: HeaderMap, paused: bool) -> Reply {
    if !authorized(&api, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    api.shared.lock().unwrap().farm_paused = paused;
    // Wakes the farm loop when connected; the flag above applies either way.
    let _ = api.tx.try_send(Command::Pause(paused));
    (StatusCode::OK, Json(json!({ "farm_paused": paused })))
}

async fn pause(State(api): State<Api>, headers: HeaderMap) -> Reply {
    set_paused(api, headers, true).await
}

async fn resume(State(api): State<Api>, headers: HeaderMap) -> Reply {
    set_paused(api, headers, false).await
}
