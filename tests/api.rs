//! The HTTP API through its router, with a fake bot task answering the commands.

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use axum::Router;
use dengai::api::{router, Command, ItemResult, LoginState, Shared, Status};
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
        ("/queue", r#"{"ids":[]}"#.to_string()),
        ("/queue", too_many("ids")),
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
async fn queue_hands_the_ids_to_the_bot() {
    let (app, mut rx, _pause) = setup();
    let bot = tokio::spawn(async move {
        let Some(Command::Queue(ids, reply)) = rx.recv().await else { panic!("expected a Queue command") };
        assert_eq!(ids, ["sub/1", "a/2"]);
        let results = ids.into_iter().map(|id| ItemResult { id, ok: true, result: "queued".into(), items: vec![] });
        let _ = reply.send(results.collect());
    });
    let (status, body) = post(app, "/queue", Some(TOKEN), r#"{"ids":["sub/1","a/2"]}"#).await;
    bot.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][1], json!({ "id": "a/2", "ok": true, "result": "queued" }));
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

async fn get(app: Router, path: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().uri(path);
    if let Some(t) = token {
        req = req.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    (status, to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec())
}

fn setup_login(state: LoginState) -> (Router, mpsc::Receiver<Command>, Shared) {
    let (tx, rx) = mpsc::channel(8);
    let (pause, _) = watch::channel(false);
    let shared: Shared = Arc::new(Mutex::new(Status { login: state, ..Status::default() }));
    (router(TOKEN.into(), shared.clone(), tx, pause), rx, shared)
}

#[tokio::test]
async fn setup_page_needs_no_token_but_status_does() {
    let (app, _rx, _pause) = setup();
    let (status, page) = get(app.clone(), "/", None).await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8(page).unwrap();
    assert!(page.contains("<form") && page.contains("id=\"claims-table\""));
    assert_eq!(get(app.clone(), "/status", None).await.0, StatusCode::UNAUTHORIZED);
    let (status, body) = get(app, "/status", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["login"], "connecting");
}

#[tokio::test]
async fn login_endpoints_need_the_token_and_a_body() {
    let (app, _rx, _shared) = setup_login(LoginState::NeedsLogin);
    for path in ["/login", "/login/code"] {
        assert_eq!(post(app.clone(), path, None, "{}").await.0, StatusCode::UNAUTHORIZED, "{path}");
    }
    for body in
        ["", "{}", r#"{"username":"bob","password":""}"#, r#"{"username":"bob","password":"x","shared_secret":"!!"}"#]
    {
        let (status, _) = post(app.clone(), "/login", Some(TOKEN), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    for body in ["", r#"{"code":""}"#, r#"{"code":"AB CD"}"#] {
        let (status, _) = post(app.clone(), "/login/code", Some(TOKEN), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn login_starts_once_and_is_rate_limited() {
    let (app, mut rx, shared) = setup_login(LoginState::NeedsLogin);
    let body = r#"{"username":"bob","password":"hunter2","guard_code":" "}"#;
    let (status, _) = post(app.clone(), "/login", Some(TOKEN), body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let Some(Command::Login(req)) = rx.recv().await else { panic!("expected a Login command") };
    assert_eq!((req.username.as_str(), req.guard_code), ("bob", None));
    assert_eq!(shared.lock().unwrap().login, LoginState::WaitingForAppApproval);

    let (status, _) = post(app.clone(), "/login", Some(TOKEN), body).await;
    assert_eq!(status, StatusCode::CONFLICT, "one attempt at a time");
    shared.lock().unwrap().login = LoginState::NeedsLogin;
    let (status, _) = post(app, "/login", Some(TOKEN), body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn login_code_only_while_steam_asks() {
    let (app, mut rx, shared) = setup_login(LoginState::WaitingForAppApproval);
    let (status, _) = post(app.clone(), "/login/code", Some(TOKEN), r#"{"code":"AB12C"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    shared.lock().unwrap().login = LoginState::WaitingForCode;
    let (status, _) = post(app, "/login/code", Some(TOKEN), r#"{"code":" AB12C "}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(matches!(rx.recv().await, Some(Command::LoginCode(c)) if c == "AB12C"));
}

#[tokio::test]
async fn mcp_speaks_json_rpc_behind_the_token() {
    let (app, _rx, _pause) = setup();
    let rpc = |body: Value| {
        let app = app.clone();
        async move { post(app, "/mcp", Some(TOKEN), &body.to_string()).await }
    };

    let (status, body) = post(app.clone(), "/mcp", None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error": "unauthorized" }));

    let (status, body) = rpc(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-03-26" } }))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], 1);
    assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(body["result"]["serverInfo"]["name"], "dengai");

    let (_, body) = rpc(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
    let names: Vec<&str> =
        body["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["status", "queue", "add_license", "redeem", "farm_pause", "farm_resume", "check"]);

    let (_, body) =
        rpc(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "status" } })).await;
    assert_eq!(body["result"]["isError"], false);
    let text: Value = serde_json::from_str(body["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["farm_paused"], false);

    let (_, body) = rpc(json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" })).await;
    assert_eq!(body["error"]["code"], -32601);

    let (_, body) = rpc(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "queue", "arguments": { "ids": [] } } }))
    .await;
    assert_eq!(body["error"]["code"], -32602);
}

#[tokio::test]
async fn mcp_notifications_get_202_and_get_is_not_allowed() {
    let (app, _rx, _pause) = setup();
    let req = |method: &str, body: &'static str| {
        Request::builder()
            .method(method)
            .uri("/mcp")
            .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(body))
            .unwrap()
    };
    let resp =
        app.clone().oneshot(req("POST", r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(to_bytes(resp.into_body(), usize::MAX).await.unwrap().is_empty());
    let resp = app.oneshot(req("GET", "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
