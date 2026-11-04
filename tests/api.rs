//! The HTTP API through its router, with a fake bot task answering the commands.

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use axum::Router;
use dengai::api::{router, Command, ItemResult, Shared, Status};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef";

fn setup() -> (Router, mpsc::Receiver<Command>, watch::Receiver<bool>) {
    let (tx, rx) = mpsc::channel(8);
    let (pause, pause_rx) = watch::channel(false);
    let shared: Shared = Arc::new(Mutex::new(Status::default()));
    (router(TOKEN.into(), shared, tx, pause), rx, pause_rx)
}

async fn post(app: Router, path: &str, token: Option<&str>, body: &str) -> (StatusCode, Value) {
    let mut req = Request::builder().method("POST").uri(path);
    if let Some(t) = token {
        req = req.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = app.oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn missing_or_wrong_token_is_unauthorized() {
    let (app, _rx, _pause) = setup();
    let body = r#"{"keys":["AAAAA-BBBBB-CCCCC"]}"#;
    for token in [None, Some("0123456789abcdeX"), Some("short")] {
        let (status, _) = post(app.clone(), "/redeem", token, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
    }
    let (status, _) = post(app.clone(), "/farm/pause", None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bad_bodies_are_rejected() {
    let (app, _rx, _pause) = setup();
    let too_many = |field: &str| json!({ field: vec!["x"; 51] }).to_string();
    for (path, body) in [
        ("/redeem", "not json".to_string()),
        ("/redeem", r#"{"keys":[]}"#.to_string()),
        ("/redeem", too_many("keys")),
        ("/addlicense", r#"{"keys":["sub/1"]}"#.to_string()),
        ("/addlicense", too_many("ids")),
    ] {
        let (status, _) = post(app.clone(), path, Some(TOKEN), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} {body:.40}");
    }
}

#[tokio::test]
async fn redeem_returns_the_bots_results() {
    let (app, mut rx, _pause) = setup();
    let bot = tokio::spawn(async move {
        let Some(Command::Redeem(keys, reply)) = rx.recv().await else { panic!("expected a Redeem command") };
        let results = keys
            .into_iter()
            .map(|id| ItemResult { id, ok: true, result: "ok".into(), items: vec!["Some Game".into()] })
            .collect();
        let _ = reply.send(results);
    });
    let (status, body) = post(app, "/redeem", Some(TOKEN), r#"{"keys":["AAAAA-BBBBB-CCCCC"]}"#).await;
    bot.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "results": [{ "id": "AAAAA-BBBBB-CCCCC", "ok": true, "result": "ok", "items": ["Some Game"] }] })
    );
}

#[tokio::test]
async fn bot_gone_is_service_unavailable() {
    let body = r#"{"keys":["AAAAA-BBBBB-CCCCC"]}"#;
    let (app, rx, _pause) = setup();
    drop(rx);
    let (status, _) = post(app, "/redeem", Some(TOKEN), body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (app, mut rx, _pause) = setup();
    let bot = tokio::spawn(async move { drop(rx.recv().await) });
    let (status, _) = post(app, "/redeem", Some(TOKEN), body).await;
    bot.await.unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "the bot dropped the reply");
}

#[tokio::test]
async fn pause_and_resume_flip_the_watch_flag() {
    let (app, mut rx, mut pause) = setup();
    let (status, _) = post(app.clone(), "/farm/pause", Some(TOKEN), "").await;
    assert_eq!((status, *pause.borrow_and_update()), (StatusCode::OK, true));
    let (status, _) = post(app, "/farm/resume", Some(TOKEN), "").await;
    assert_eq!((status, *pause.borrow_and_update()), (StatusCode::OK, false));
    assert!(rx.try_recv().is_err(), "pausing uses no command slot");
}
