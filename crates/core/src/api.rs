// SPDX-License-Identifier: GPL-3.0-only
//! Local HTTP server (SPEC §8.3, §8.4): OBS dock, overlay and setup pages, the
//! JSON API and the controller WebSocket. It listens on loopback only.
//!
//! Every request must name a loopback `Host` (blocks DNS rebinding). A present
//! `Origin` must be this server's own origin and a present `Sec-Fetch-Site`
//! must be `same-origin` or `none`, which rejects cross-site browser requests
//! including WebSocket upgrades. State-changing methods also need the per-run
//! token in `X-OpenDelay-Token`; the token is only embedded in our own pages.

use crate::app::{Action, App, Reply};
use crate::routes::{PRESETS, Problem, Route};
use crate::settings::ListenerMode;
use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub const TOKEN_HEADER: &str = "x-opendelay-token";
pub const WS_PROTOCOL_VERSION: u32 = 1;
const MAX_BODY: usize = 64 * 1024;
/// WebSocket messages up to this size are read and refused with an error
/// reply; larger ones close the connection.
const WS_MAX_READ: usize = 1024 * 1024;
const ACTIONS: &[&str] = &[
    "enableDelay",
    "goLive",
    "disableDelay",
    "toggleDelay",
    "setDelay",
    "setBuffer",
    "setDelayMode",
    "toggleOverlay",
    "setOverlayVisible",
    "emergencyStop",
    "clearEmergencyStop",
];

include!(concat!(env!("OUT_DIR"), "/web_langs.rs"));

const DOCK: &str = include_str!("../web/dock.html");
const OVERLAY: &str = include_str!("../web/overlay.html");
const SETUP: &str = include_str!("../web/setup.html");

type AppState = State<Arc<App>>;

pub(crate) async fn serve(app: Arc<App>, addr: SocketAddr) -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(app)).await;
    });
    Ok(local)
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/dock") }))
        // Only the dock sends commands; the read-only pages get no token.
        .route("/dock", get(|State(app): AppState| async move { page(DOCK.replace("{{TOKEN}}", app.token())) }))
        .route("/overlay", get(|| async { page(OVERLAY.into()) }))
        .route("/setup", get(|| async { page(SETUP.into()) }))
        .route("/assets/{file}", get(asset))
        .route("/assets/lang/{file}", get(lang))
        .route("/api/status", get(|State(app): AppState| async move { Json(app.reply(Ok(String::new()))) }))
        .route("/api/action", post(action))
        .route("/api/routes", get(routes_list).post(route_create))
        .route("/api/routes/{id}", put(route_update).delete(route_delete))
        .route(
            "/api/presets",
            get(|State(app): AppState| async move { respond(&app, Ok(String::new()), json!(PRESETS)) }),
        )
        .route("/api/network", get(network).put(network_set))
        .route("/api/twitch", get(twitch).put(twitch_set))
        .route("/api/twitch/servers", get(twitch_servers))
        .route("/api/update", get(update_check))
        .route("/api/diagnostics", get(diagnostics))
        .route(
            "/api/window/show",
            post(|State(app): AppState| async move {
                app.request_show();
                respond(&app, Ok(String::new()), Value::Null)
            }),
        )
        .route("/api/diagnostics/{id}", get(diagnostic))
        .route("/ws", get(ws))
        .fallback(|| async { json_error(StatusCode::NOT_FOUND, "not_found", "No such endpoint.") })
        .method_not_allowed_fallback(|| async {
            json_error(StatusCode::METHOD_NOT_ALLOWED, "bad_request", "This endpoint does not take that method.")
        })
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(app)
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The request policy described in the module documentation.
pub fn check(app: &App, method: &Method, h: &HeaderMap) -> Result<(), &'static str> {
    let port = app.http_port();
    let hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).ok_or("Missing Host header.")?;
    if !hosts.iter().any(|x| x.eq_ignore_ascii_case(host)) {
        return Err("Requests must address this computer (127.0.0.1 or localhost).");
    }
    if let Some(origin) = h.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if !hosts.iter().any(|x| origin.eq_ignore_ascii_case(&format!("http://{x}"))) {
            return Err("Cross-site requests are not allowed.");
        }
    }
    if let Some(site) = h.get("sec-fetch-site")
        && !matches!(site.to_str(), Ok("same-origin" | "none"))
    {
        return Err("Cross-site requests are not allowed.");
    }
    if !matches!(*method, Method::GET | Method::HEAD) {
        let token = h.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
        if !constant_time_eq(token.as_bytes(), app.token().as_bytes()) {
            return Err("Missing or invalid API token.");
        }
    }
    Ok(())
}

async fn guard(State(app): AppState, req: Request, next: Next) -> Response {
    match check(&app, req.method(), req.headers()) {
        Ok(()) => {
            let mut res = next.run(req).await;
            if res.status() == StatusCode::PAYLOAD_TOO_LARGE {
                let message = format!("The request body is larger than {} KiB.", MAX_BODY / 1024);
                res = (res.status(), Json(json!({ "success": false, "code": "too_large", "message": message })))
                    .into_response();
            }
            let h = res.headers_mut();
            h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
            h.entry(header::CACHE_CONTROL).or_insert(header::HeaderValue::from_static("no-store"));
            res
        }
        Err(why) => (StatusCode::FORBIDDEN, Json(json!({ "success": false, "code": "forbidden", "message": why })))
            .into_response(),
    }
}

fn page(html: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            // No inline scripts or styles: everything comes from /assets.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

async fn asset(Path(file): Path<String>) -> Response {
    let (ctype, body): (&str, &'static [u8]) = match file.as_str() {
        "style.css" => ("text/css; charset=utf-8", include_bytes!("../web/style.css")),
        "common.js" => ("text/javascript; charset=utf-8", include_bytes!("../web/common.js")),
        "dock.css" => ("text/css; charset=utf-8", include_bytes!("../web/dock.css")),
        "dock.js" => ("text/javascript; charset=utf-8", include_bytes!("../web/dock.js")),
        "overlay.css" => ("text/css; charset=utf-8", include_bytes!("../web/overlay.css")),
        "overlay.js" => ("text/javascript; charset=utf-8", include_bytes!("../web/overlay.js")),
        "setup.css" => ("text/css; charset=utf-8", include_bytes!("../web/setup.css")),
        "setup.js" => ("text/javascript; charset=utf-8", include_bytes!("../web/setup.js")),
        "inter-400.woff2" => ("font/woff2", include_bytes!("../../../assets/fonts/web/Inter-Regular.woff2")),
        "inter-600.woff2" => ("font/woff2", include_bytes!("../../../assets/fonts/web/Inter-SemiBold.woff2")),
        "inter-700.woff2" => ("font/woff2", include_bytes!("../../../assets/fonts/web/Inter-Bold.woff2")),
        _ => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn lang(Path(file): Path<String>) -> Response {
    let code = file.strip_suffix(".json").unwrap_or("");
    match WEB_LANGS.iter().find(|(c, _)| *c == code) {
        Some((_, body)) => ([(header::CONTENT_TYPE, "application/json; charset=utf-8")], *body).into_response(),
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// Envelope: success flag, message, code, fresh status, optional `data`.
fn respond(app: &App, result: Result<String, Problem>, data: Value) -> Response {
    let ok = result.is_ok();
    let mut v = serde_json::to_value(app.reply(result.map_err(|p| (p.message, p.code)))).unwrap_or_default();
    if !data.is_null() {
        v["data"] = data;
    }
    let status = match v["code"].as_str() {
        _ if ok => StatusCode::OK,
        Some("not_found") => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, Json(v)).into_response()
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Problem> {
    serde_json::from_slice(body).map_err(|e| Problem::new("bad_request", format!("Invalid request body: {e}")))
}

/// The WebSocket actions over HTTP: same replies, codes and `id` echo.
async fn action(State(app): AppState, body: Bytes) -> Response {
    let mut v = handle_ws_text(&app, &String::from_utf8_lossy(&body), &mut false);
    let bad = matches!(v["code"].as_str(), Some("bad_request" | "unknown_action"));
    if let Some(o) = v.as_object_mut() {
        o.remove("type");
        if o.get("id").is_some_and(Value::is_null) {
            o.remove("id");
        }
    }
    (if bad { StatusCode::BAD_REQUEST } else { StatusCode::OK }, Json(v)).into_response()
}

/// Error envelope for requests that never reach an endpoint.
fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "success": false, "code": code, "message": message }))).into_response()
}

/// Routes as the API shows them: stored stream keys never leave the app.
fn without_key(mut r: Route) -> Route {
    r.key.clear();
    r
}

async fn routes_list(State(app): AppState) -> Response {
    respond(&app, Ok(String::new()), json!(app.routes().into_iter().map(without_key).collect::<Vec<_>>()))
}

async fn route_create(State(app): AppState, body: Bytes) -> Response {
    match parse::<Route>(&body).and_then(|r| app.add_route(r)) {
        Ok(r) => {
            let msg = if waits_for_encoder(&app, &r) {
                "Route added. It starts sending the next time your encoder connects."
            } else {
                "Route added."
            };
            respond(&app, Ok(msg.into()), json!(without_key(r)))
        }
        Err(e) => respond(&app, Err(e), Value::Null),
    }
}

/// An enabled route sending a stream that is already live: routes join a
/// stream only when its encoder connects.
fn waits_for_encoder(app: &App, r: &Route) -> bool {
    r.enabled && !r.source.is_empty() && app.status().streams.iter().any(|s| s.route_id == r.source)
}

async fn route_update(State(app): AppState, Path(id): Path<String>, body: Bytes) -> Response {
    let before = app.routes().into_iter().find(|r| r.id == id);
    match parse::<Route>(&body).and_then(|r| app.update_route(&id, r)) {
        Ok(r) => {
            let moved = before.is_some_and(|b| b.path != r.path);
            let msg = if moved {
                "Route saved. The new path takes effect the next time your encoder connects."
            } else if waits_for_encoder(&app, &r) {
                "Route saved. It starts sending the next time your encoder connects."
            } else {
                "Route saved."
            };
            respond(&app, Ok(msg.into()), json!(without_key(r)))
        }
        Err(e) => respond(&app, Err(e), Value::Null),
    }
}

async fn route_delete(State(app): AppState, Path(id): Path<String>) -> Response {
    respond(&app, app.delete_route(&id).map(|_| "Route deleted.".into()), Value::Null)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkBody {
    listener_mode: ListenerMode,
}

async fn network(State(app): AppState) -> Response {
    respond(&app, Ok(String::new()), Value::Null)
}

async fn network_set(State(app): AppState, body: Bytes) -> Response {
    let r = parse::<NetworkBody>(&body).and_then(|b| app.set_listener_mode(b.listener_mode));
    respond(&app, r, Value::Null)
}

#[derive(Deserialize)]
struct TwitchBody {
    server: String,
}

async fn twitch(State(app): AppState) -> Response {
    let s = app.settings();
    respond(&app, Ok(String::new()), json!({ "server": s.twitch_server, "auto": crate::routes::TWITCH_AUTO }))
}

async fn twitch_set(State(app): AppState, body: Bytes) -> Response {
    let r = parse::<TwitchBody>(&body).and_then(|b| app.set_twitch_server(&b.server));
    respond(&app, r.map(|_| "Twitch server saved.".into()), Value::Null)
}

async fn twitch_servers(State(app): AppState) -> Response {
    let r = tokio::task::spawn_blocking(|| crate::net::twitch_ingests(false)).await;
    match r.map_err(|e| e.to_string()).and_then(|r| r) {
        Ok(list) => respond(&app, Ok(String::new()), json!(list)),
        Err(e) => respond(&app, Err(Problem::new("network", e)), Value::Null),
    }
}

async fn update_check(State(app): AppState) -> Response {
    // Opt-in (SPEC §11): no request to the feed unless the user turned it on.
    if !app.settings().update_check {
        let p = Problem::new("update_check_off", "Update checks are turned off in OpenDelay's settings.");
        return respond(&app, Err(p), Value::Null);
    }
    // Cached for 10 minutes: callers cannot use up the feed's rate limit.
    let checker = app.clone();
    let r = tokio::task::spawn_blocking(move || checker.check_update(false)).await;
    match r.map_err(|e| e.to_string()).and_then(|r| r) {
        Ok(info) => respond(&app, Ok(String::new()), json!({ "available": info.is_some(), "update": info })),
        Err(e) => respond(&app, Err(Problem::new("network", e)), Value::Null),
    }
}

async fn diagnostics(State(app): AppState) -> Response {
    respond(&app, Ok(String::new()), json!(app.diagnostics()))
}

async fn diagnostic(State(app): AppState, Path(id): Path<String>) -> Response {
    match app.diagnostic(&id) {
        Some(text) => (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"opendelay-{id}.log\"")),
            ],
            text,
        )
            .into_response(),
        None => json_error(StatusCode::NOT_FOUND, "not_found", "No such log."),
    }
}

// ---- controller WebSocket (docs/controller-protocol.md) ----

async fn ws(State(app): AppState, upgrade: WebSocketUpgrade) -> Response {
    // Clients that insist on a subprotocol get ours; browsers fail the
    // connection when a requested one is not echoed.
    upgrade
        .protocols(["opendelay"])
        .max_message_size(WS_MAX_READ)
        .max_frame_size(WS_MAX_READ)
        .on_upgrade(move |socket| ws_session(app, socket))
}

fn error_reply(app: &App, id: Value, code: &'static str, message: String) -> Value {
    let reply = app.reply(Err((message, code)));
    tag(reply, id)
}

fn tag(reply: Reply, id: Value) -> Value {
    let mut v = serde_json::to_value(reply).unwrap_or_default();
    v["type"] = json!("response");
    v["id"] = id;
    v
}

/// One request → one response; never fails the connection.
pub fn handle_ws_text(app: &App, text: &str, subscribed: &mut bool) -> Value {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return error_reply(app, Value::Null, "bad_request", "Malformed JSON.".into());
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let Some(name) = v.get("action").and_then(Value::as_str) else {
        return error_reply(app, id, "bad_request", "Missing \"action\".".into());
    };
    let reply = match name {
        "getStatus" => app.reply(Ok(String::new())),
        "subscribe" => {
            *subscribed = true;
            app.reply(Ok("Subscribed to status updates.".into()))
        }
        "unsubscribe" => {
            *subscribed = false;
            app.reply(Ok("Unsubscribed.".into()))
        }
        _ => match serde_json::from_value::<Action>(v.clone()) {
            Ok(a) => app.act(&a),
            Err(e) if ACTIONS.contains(&name) => {
                return error_reply(app, id, "bad_request", format!("Invalid parameters: {e}"));
            }
            Err(_) => return error_reply(app, id, "unknown_action", format!("Unknown action \"{name}\".")),
        },
    };
    tag(reply, id)
}

async fn send(socket: &mut WebSocket, v: &Value) -> Result<(), axum::Error> {
    socket.send(WsMessage::Text(v.to_string().into())).await
}

async fn ws_session(app: Arc<App>, mut socket: WebSocket) {
    let hello = json!({
        "type": "hello",
        "app": "OpenDelay",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": WS_PROTOCOL_VERSION,
    });
    if send(&mut socket, &hello).await.is_err() {
        return;
    }
    let mut subscribed = false;
    let mut changes = app.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let out = tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(WsMessage::Text(t))) if t.len() > MAX_BODY => {
                    let message = format!("Messages must be at most {} KiB.", MAX_BODY / 1024);
                    error_reply(&app, Value::Null, "too_large", message)
                }
                Some(Ok(WsMessage::Text(t))) => handle_ws_text(&app, t.as_str(), &mut subscribed),
                Some(Ok(WsMessage::Binary(_))) => {
                    error_reply(&app, Value::Null, "bad_request", "Send JSON as text messages.".into())
                }
                Some(Ok(_)) => continue,
                _ => break,
            },
            _ = tick.tick(), if subscribed => json!({ "type": "status", "status": app.status() }),
            r = changes.changed(), if subscribed => {
                if r.is_err() {
                    break;
                }
                json!({ "type": "status", "status": app.status() })
            }
        };
        if send(&mut socket, &out).await.is_err() {
            break;
        }
    }
}
