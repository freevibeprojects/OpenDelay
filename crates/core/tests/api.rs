// SPDX-License-Identifier: GPL-3.0-only
//! HTTP API and controller WebSocket: token, cross-site rejection, protocol.

use futures_util::{SinkExt, StreamExt};
use opendelay_core::{App, AppOptions};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

struct Server {
    app: Arc<App>,
    addr: SocketAddr,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("opendelay-api-{}", opendelay_core::diag::short_id()));
    let app =
        App::new(AppOptions { config_dir: Some(dir.join("config")), data_dir: Some(dir.join("data")), tls: None });
    let addr = app.start_http(0).await.unwrap();
    Server { app, addr, dir }
}

struct Resp {
    code: u16,
    head: String,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }
}

/// Minimal HTTP/1.1 client; `headers` replace the defaults (Host).
async fn http(s: &Server, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut tcp = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    let host = format!("127.0.0.1:{}", s.addr.port());
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: {host}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    tcp.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tcp.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let code = head[9..12].parse().unwrap();
    Resp { code, head: head.to_owned(), body: body.to_owned() }
}

async fn post(s: &Server, path: &str, body: &str, extra: &[(&str, &str)]) -> Resp {
    let mut h = vec![("X-OpenDelay-Token", s.app.token())];
    h.extend_from_slice(extra);
    http(s, "POST", path, &h, body).await
}

#[tokio::test]
async fn status_and_pages_are_served_with_the_token() {
    let s = start().await;
    let r = http(&s, "GET", "/api/status", &[], "").await;
    assert_eq!(r.code, 200);
    let v = r.json();
    assert_eq!((v["success"].as_bool(), v["status"]["state"].as_str()), (Some(true), Some("offline")));
    assert_eq!(v["status"]["delay"]["configuredMs"], 30_000);

    let r = http(&s, "GET", "/dock", &[("Sec-Fetch-Site", "none")], "").await;
    assert_eq!(r.code, 200);
    assert!(r.body.contains(s.app.token()), "token embedded in the dock");
    let head = r.head.to_ascii_lowercase();
    assert!(head.contains("x-frame-options: deny"), "pages cannot be framed");
    assert!(
        head.contains("content-security-policy: default-src 'self'") && head.contains("referrer-policy: no-referrer")
    );
    for page in ["/overlay", "/setup"] {
        let r = http(&s, "GET", page, &[], "").await;
        assert!(!r.body.contains(s.app.token()), "read-only page {page} carries no token");
        assert!(!r.body.contains("<script>") && !r.body.contains("<style>"), "no inline code in {page}");
    }
    assert_eq!(http(&s, "GET", "/nope", &[], "").await.code, 404);
}

#[tokio::test]
async fn state_changes_need_the_token() {
    let s = start().await;
    let body = r#"{"action":"toggleOverlay"}"#;
    assert_eq!(http(&s, "POST", "/api/action", &[], body).await.code, 403);
    assert_eq!(http(&s, "POST", "/api/action", &[("X-OpenDelay-Token", "wrong")], body).await.code, 403);
    let r = post(&s, "/api/action", body, &[]).await;
    assert_eq!(r.code, 200);
    assert_eq!(r.json()["status"]["overlayVisible"], false);
    // Other state-changing methods too.
    assert_eq!(http(&s, "DELETE", "/api/routes/x", &[], "").await.code, 403);
    assert_eq!(http(&s, "PUT", "/api/network", &[], r#"{"listenerMode":"lan"}"#).await.code, 403);
    let r = post(&s, "/api/action", &" ".repeat(70_000), &[]).await;
    assert_eq!((r.code, r.json()["code"].as_str()), (413, Some("too_large")));
}

#[tokio::test]
async fn cross_site_requests_cannot_trigger_actions() {
    let s = start().await;
    let port = s.addr.port();
    let body = r#"{"action":"emergencyStop"}"#;
    let own = format!("http://127.0.0.1:{port}");
    let evil_host = format!("evil.example:{port}");
    for (k, v) in [
        ("Origin", "https://evil.example"),
        ("Origin", "null"),
        ("Origin", "http://127.0.0.1:1"),
        ("Sec-Fetch-Site", "cross-site"),
        ("Sec-Fetch-Site", "same-site"),
        ("Host", evil_host.as_str()),
    ] {
        let r = post(&s, "/api/action", body, &[(k, v)]).await;
        assert_eq!(r.code, 403, "{k}: {v}");
        assert_eq!(r.json()["code"], "forbidden");
    }
    assert!(!s.app.relay.emergency_active(), "no cross-site request got through");
    assert_eq!(http(&s, "GET", "/dock", &[("Host", &evil_host)], "").await.code, 403, "DNS rebinding blocked");
    assert_eq!(http(&s, "GET", "/api/status", &[("Sec-Fetch-Site", "cross-site")], "").await.code, 403);

    let local = format!("localhost:{port}");
    let same_origin = [("Origin", own.as_str()), ("Sec-Fetch-Site", "same-origin"), ("Host", local.as_str())];
    assert_eq!(post(&s, "/api/action", body, &same_origin).await.code, 200);
    assert!(s.app.relay.emergency_active());
}

#[tokio::test]
async fn actions_report_refusal_codes() {
    let s = start().await;
    let r = post(&s, "/api/action", r#"{"action":"enableDelay","seconds":30}"#, &[]).await;
    let v = r.json();
    assert_eq!((r.code, v["success"].as_bool(), v["code"].as_str()), (200, Some(false), Some("no_stream")));
    assert!(!v["message"].as_str().unwrap().is_empty());
    let v = post(&s, "/api/action", r#"{"action":"setDelay","seconds":9999}"#, &[]).await.json();
    assert_eq!(v["code"], "invalid_value");
    let r = post(&s, "/api/action", r#"{"action":"setDelay","seconds":45}"#, &[]).await;
    assert_eq!(r.json()["status"]["delay"]["configuredMs"], 45_000);
    assert_eq!(s.app.settings().delay_secs, 45, "persisted");
    assert_eq!(post(&s, "/api/action", "{nope", &[]).await.code, 400);
    assert_eq!(post(&s, "/api/action", r#"{"action":"fly"}"#, &[]).await.code, 400);
}

#[tokio::test]
async fn routes_crud_and_rules() {
    let s = start().await;
    let yt = r#"{"name":"YouTube","preset":"youtube","path":"YT","upstream":"rtmp://a.rtmp.youtube.com/live2"}"#;
    let r = post(&s, "/api/routes", yt, &[]).await;
    assert_eq!(r.code, 200, "{}", r.body);
    let id = r.json()["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(r.json()["data"]["path"], "yt");
    assert_eq!(post(&s, "/api/routes", yt, &[]).await.code, 400, "duplicate path");
    let bad = r#"{"name":"X","preset":"custom","path":"x","upstream":"http://nope"}"#;
    assert_eq!(post(&s, "/api/routes", bad, &[]).await.code, 400);

    let tok = [("X-OpenDelay-Token", s.app.token())];
    let moved = yt.replace("\"YT\"", "\"yt2\"");
    let r = http(&s, "PUT", &format!("/api/routes/{id}"), &tok, &moved).await;
    assert_eq!(r.code, 200);
    assert!(r.json()["message"].as_str().unwrap().contains("next time"));
    assert_eq!(http(&s, "DELETE", "/api/routes/twitch", &tok, "").await.code, 400, "default route kept");
    assert_eq!(http(&s, "DELETE", &format!("/api/routes/{id}"), &tok, "").await.code, 200);

    let saved = std::fs::read_to_string(s.dir.join("config/settings.json")).unwrap();
    assert!(saved.contains("\"twitch\"") && !saved.contains("yt2"));
    let list = http(&s, "GET", "/api/routes", &[], "").await.json();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn multistream_keys_are_kept_but_never_returned() {
    let s = start().await;
    let tok = [("X-OpenDelay-Token", s.app.token())];
    let kick = r#"{"name":"Kick","preset":"kick","path":"kick","upstream":"rtmps://k.example:443/app","source":"twitch","key":"sk_secret"}"#;
    let r = post(&s, "/api/routes", kick, &[]).await;
    assert_eq!(r.code, 200, "{}", r.body);
    assert_eq!(r.json()["data"]["key"], "", "not echoed");
    let id = r.json()["data"]["id"].as_str().unwrap().to_owned();
    // Saving without a key (the key is never shown) keeps the stored one.
    let renamed = kick.replace("\"Kick\"", "\"Kick 2\"").replace("sk_secret", "");
    assert_eq!(http(&s, "PUT", &format!("/api/routes/{id}"), &tok, &renamed).await.code, 200);
    assert_eq!(s.app.routes()[1].key, "sk_secret");
    let list = http(&s, "GET", "/api/routes", &[], "").await;
    assert!(!list.body.contains("sk_secret"), "{}", list.body);
    assert_eq!(list.json()["data"][1]["source"], "twitch");
    let r = http(&s, "DELETE", "/api/routes/twitch", &tok, "").await;
    assert_eq!(r.code, 400, "a route other routes send from is kept");
    assert_eq!(
        post(&s, "/api/routes", &kick.replace("sk_secret", "").replace("kick\",\"up", "k2\",\"up"), &[]).await.code,
        400,
        "key needed"
    );
}

#[tokio::test]
async fn network_twitch_and_diagnostics() {
    let s = start().await;
    let tok = [("X-OpenDelay-Token", s.app.token())];
    let r = http(&s, "PUT", "/api/network", &tok, r#"{"listenerMode":"lan"}"#).await.json();
    assert_eq!(r["status"]["listener"]["restartRequired"], true);
    assert_eq!(http(&s, "PUT", "/api/twitch", &tok, r#"{"server":"rtmp://evil.example/app"}"#).await.code, 400);
    let ok = http(&s, "PUT", "/api/twitch", &tok, r#"{"server":"rtmp://fra.contribute.live-video.net/app"}"#);
    assert_eq!(ok.await.code, 200);
    assert_eq!(
        http(&s, "GET", "/api/twitch", &[], "").await.json()["data"]["server"],
        "rtmp://fra.contribute.live-video.net/app"
    );

    let logs = s.dir.join("data/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("20261014-170509-3.log"), "hello log").unwrap();
    let list = http(&s, "GET", "/api/diagnostics", &[], "").await.json();
    assert_eq!(list["data"][0]["id"], "20261014-170509-3");
    let r = http(&s, "GET", "/api/diagnostics/20261014-170509-3", &[], "").await;
    assert_eq!((r.code, r.body.as_str()), (200, "hello log"));
    assert!(r.head.to_ascii_lowercase().contains("content-disposition: attachment"));
    for evil in ["..%2Fconfig%2Fsettings.json", "..%2F..%2Fetc%2Fpasswd", "20261014-170509-3.log"] {
        assert_eq!(http(&s, "GET", &format!("/api/diagnostics/{evil}"), &[], "").await.code, 404, "{evil}");
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(s: &Server, origin: Option<&str>) -> Result<Ws, tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{}/ws", s.addr.port()).into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("Origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

async fn next_json(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(3), ws.next()).await.expect("message").unwrap().unwrap();
        if let tungstenite::Message::Text(t) = m {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

async fn request(ws: &mut Ws, msg: &str) -> Value {
    ws.send(tungstenite::Message::Text(msg.into())).await.unwrap();
    loop {
        let v = next_json(ws).await;
        if v["type"] == "response" {
            return v;
        }
    }
}

#[tokio::test]
async fn websocket_rejects_cross_site_origins() {
    let s = start().await;
    match ws_connect(&s, Some("https://evil.example")).await {
        Err(tungstenite::Error::Http(r)) => assert_eq!(r.status(), 403),
        other => panic!("cross-site WebSocket accepted: {:?}", other.map(|_| ())),
    }
    let own = format!("http://localhost:{}", s.addr.port());
    assert!(ws_connect(&s, Some(&own)).await.is_ok());
}

#[tokio::test]
async fn websocket_protocol() {
    let s = start().await;
    let mut ws = ws_connect(&s, None).await.expect("native controllers send no Origin");
    let hello = next_json(&mut ws).await;
    assert_eq!((hello["type"].as_str(), hello["protocol"].as_u64()), (Some("hello"), Some(1)));

    let v = request(&mut ws, r#"{"id":1,"action":"getStatus"}"#).await;
    assert_eq!((v["id"].as_u64(), v["success"].as_bool()), (Some(1), Some(true)));
    assert_eq!(v["status"]["state"], "offline");

    let v = request(&mut ws, "garbage").await;
    assert_eq!((v["success"].as_bool(), v["code"].as_str()), (Some(false), Some("bad_request")));
    let v = request(&mut ws, r#"{"id":"x","action":"teleport"}"#).await;
    assert_eq!((v["id"].as_str(), v["code"].as_str()), (Some("x"), Some("unknown_action")));
    let v = request(&mut ws, r#"{"id":3,"action":"setDelay"}"#).await;
    assert_eq!(v["code"], "bad_request");
    let v = request(&mut ws, r#"{"id":4,"action":"enableDelay"}"#).await;
    assert_eq!(v["code"], "no_stream");
    let v = request(&mut ws, r#"{"id":5,"action":"setDelay","seconds":20}"#).await;
    assert_eq!((v["success"].as_bool(), v["status"]["delay"]["configuredMs"].as_u64()), (Some(true), Some(20_000)));

    let v = request(&mut ws, &json!({"id": 6, "action": "subscribe"}).to_string()).await;
    assert_eq!(v["success"], true);
    s.app.act(&opendelay_core::Action::ToggleOverlay);
    let push = loop {
        let v = next_json(&mut ws).await;
        if v["type"] == "status" && v["status"]["overlayVisible"] == false {
            break v;
        }
    };
    assert_eq!(push["status"]["version"], env!("CARGO_PKG_VERSION"));

    let mut req = format!("ws://127.0.0.1:{}/ws", s.addr.port()).into_client_request().unwrap();
    req.headers_mut().insert("Sec-WebSocket-Protocol", "opendelay".parse().unwrap());
    let (_, res) = tokio_tungstenite::connect_async(req).await.expect("subprotocol accepted");
    assert_eq!(res.headers()["sec-websocket-protocol"], "opendelay");
}

#[tokio::test]
async fn web_assets_and_translations() {
    let s = start().await;
    for page in ["/dock", "/overlay", "/setup"] {
        let r = http(&s, "GET", page, &[], "").await;
        assert_eq!(r.code, 200, "{page}");
        assert!(r.body.contains("/assets/common.js"));
        let name = &page[1..];
        for asset in [format!("/assets/{name}.js"), format!("/assets/{name}.css")] {
            assert!(r.body.contains(&asset), "{page} links {asset}");
            assert_eq!(http(&s, "GET", &asset, &[], "").await.code, 200, "{asset}");
        }
    }
    let r = http(&s, "GET", "/assets/lang/en.json", &[], "").await;
    assert_eq!(r.code, 200);
    assert_eq!(r.json()["_name"], "English");
    assert_eq!(http(&s, "GET", "/assets/lang/xx.json", &[], "").await.code, 404);
    assert_eq!(http(&s, "GET", "/assets/lang/..%2Fdock.html", &[], "").await.code, 404);
    let font = http(&s, "GET", "/assets/inter-400.woff2", &[], "").await;
    assert!(font.code == 200 && font.head.to_ascii_lowercase().contains("font/woff2"));
}

#[tokio::test]
async fn update_check_is_opt_in() {
    let s = start().await;
    assert!(!s.app.settings().update_check, "off by default");
    let v = http(&s, "GET", "/api/update", &[], "").await.json();
    assert_eq!((v["success"].as_bool(), v["code"].as_str()), (Some(false), Some("update_check_off")), "{v}");
}

#[tokio::test]
async fn show_window_request_and_security_headers() {
    let s = start().await;
    assert_eq!(http(&s, "POST", "/api/window/show", &[], "").await.code, 403, "token required");
    assert!(!s.app.take_show_request());
    let r = post(&s, "/api/window/show", "", &[]).await;
    assert_eq!(r.code, 200);
    assert!(s.app.take_show_request(), "flag set for the desktop window");
    assert!(!s.app.take_show_request(), "flag consumed");
    let head = http(&s, "GET", "/api/status", &[], "").await.head.to_ascii_lowercase();
    assert!(head.contains("x-content-type-options: nosniff") && head.contains("cache-control: no-store"));
}
