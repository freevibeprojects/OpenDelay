// SPDX-License-Identifier: GPL-3.0-only
//! One encoder connection: RTMP server side towards the encoder, the publish
//! sequence with the upstream on its behalf, then the relay loop with
//! automatic upstream recovery.

use crate::conn::{self, BoxIo, MsgReader, MsgWriter, OUT_CHUNK_SIZE};
use crate::diag::SessionLog;
use crate::engine::{Engine, Parsed, Poll};
use crate::health::{HealthAnalyzer, HealthReport};
use crate::relay::Shared;
use crate::routes::{Route, normalize_path};
use crate::upstream::{self, Upstream, UpstreamError, control_reply, status_of};
use crate::url::UpstreamUrl;
use opendelay_rtmp::amf0::Amf0;
use opendelay_rtmp::message::{split_aggregate, status_object, user_control};
use opendelay_rtmp::{Command, Message, msg_type};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::{sleep, sleep_until, timeout, timeout_at};
use tokio_rustls::rustls::ClientConfig;
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest message accepted before `publish`: real `connect`/`publish`
/// commands are a few hundred bytes, and anything larger costs memory before
/// the peer has proven anything (AMF0 decoding expands its input).
const PRE_PUBLISH_MAX_MESSAGE: u32 = 64 * 1024;
/// An encoder that sends nothing for this long is gone (or hostile).
const ENCODER_IDLE: Duration = Duration::from_secs(20);
/// Writing upstream that makes no progress for this long means the
/// connection is dead (the kernel would take many minutes to notice).
const UPSTREAM_STALL: Duration = Duration::from_secs(15);
pub const MAX_RECOVERY_ATTEMPTS: u32 = 8;
/// Encoder-facing stream id returned by our `createStream`.
const ENCODER_STREAM_ID: u32 = 1;

/// A live platform connection a replaced session leaves for its successor.
pub(crate) struct KeptUpstream {
    up: Upstream,
    /// Stream key it publishes with.
    key: String,
    /// Route server URL it was opened for.
    upstream_url: String,
}

/// What a replaced delayed session hands to the new connection on its route.
pub(crate) struct Handover {
    pub(crate) engine: Engine,
    pub(crate) upstream: Option<KeptUpstream>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Publish negotiation with the upstream in progress.
    Connecting,
    Live,
    /// Upstream lost; reconnecting.
    Recovering,
    /// Encoder stopped; sending what is still buffered.
    Finishing,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub phase: Phase,
    pub recovery_attempt: u32,
    pub last_error: Option<String>,
}

pub struct Session {
    pub id: u64,
    pub route: Route,
    pub peer: SocketAddr,
    pub started: Instant,
    pub started_at: SystemTime,
    pub engine: Mutex<Engine>,
    notify: Notify,
    pub token: CancellationToken,
    info: Mutex<SessionInfo>,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub log: SessionLog,
    /// (sample time, bytes in at that time, last computed kbit/s)
    meter: Mutex<(Instant, u64, u64)>,
    health: Mutex<Health>,
    /// Set (under the relay's session lock) when this session takes its route at `publish`.
    claimed: AtomicBool,
    /// Set when a new encoder connection from the same machine replaces this session.
    pub(crate) replaced: AtomicBool,
    /// Set when replaced: the platform connection, still publishing.
    pub(crate) kept_upstream: Mutex<Option<KeptUpstream>>,
    /// Stream key from the encoder's `publish`.
    pub(crate) publish_key: std::sync::OnceLock<String>,
}

struct Health {
    analyzer: HealthAnalyzer,
    last_check: Instant,
    /// Finding codes already written to the session log.
    logged: Vec<&'static str>,
}

impl Session {
    pub fn engine(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake the upstream writer (new input or a state change).
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    pub fn info(&self) -> SessionInfo {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_info(&self, phase: Phase, attempt: u32, error: Option<String>) {
        let last_error = error.map(|e| self.log.redact(&e));
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) =
            SessionInfo { phase, recovery_attempt: attempt, last_error };
    }

    pub fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::SeqCst)
    }

    pub(crate) fn claim(&self) {
        self.claimed.store(true, Ordering::SeqCst);
    }

    /// Incoming bitrate, averaged over at least one second.
    pub fn bitrate_kbps(&self) -> u64 {
        let mut m = self.meter.lock().unwrap_or_else(|e| e.into_inner());
        let (now, bytes) = (Instant::now(), self.bytes_in.load(Ordering::Relaxed));
        let dt = now.duration_since(m.0).as_secs_f64();
        if dt >= 1.0 {
            *m = (now, bytes, ((bytes - m.1) as f64 * 8.0 / dt / 1000.0) as u64);
        }
        m.2
    }

    pub fn health(&self) -> HealthReport {
        self.health.lock().unwrap_or_else(|e| e.into_inner()).analyzer.report(Instant::now())
    }

    fn push(&self, msg: Message) {
        let now = Instant::now();
        let parsed = Parsed::of(&msg); // once for both the health checks and the engine
        {
            let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
            h.analyzer.observe_parsed(&msg, &parsed, now);
            if now.duration_since(h.last_check) >= Duration::from_secs(5) {
                h.last_check = now;
                for f in h.analyzer.report(now).findings {
                    if !h.logged.contains(&f.code) {
                        h.logged.push(f.code);
                        self.log.event(format!("Health ({:?}): {}", f.severity, f.message));
                    }
                }
            }
        }
        self.engine().push_parsed(msg, &parsed, now);
        self.wake();
    }
}

fn reply(name: &str, tx: f64, object: Amf0, args: Vec<Amf0>) -> Message {
    Command::new(name, tx, object, args).to_message(0)
}

fn error_reply(tx: f64, code: &str, description: &str) -> Message {
    reply("_error", tx, Amf0::Null, vec![status_object("error", code, description)])
}

fn is_command(m: &Message) -> bool {
    matches!(m.type_id, msg_type::COMMAND_AMF0 | msg_type::COMMAND_AMF3)
}

type Res<T> = Result<T, String>;

fn io_err(e: std::io::Error) -> String {
    format!("encoder connection error: {e}")
}

/// Accepted TCP connection → full session lifetime.
pub(crate) async fn run(relay: Arc<Shared>, tcp: TcpStream, peer: SocketAddr, id: u64) {
    let _ = tcp.set_nodelay(true);
    let mut io: BoxIo = Box::new(tcp);
    let uptime = relay.started.elapsed().as_millis() as u32;
    if !matches!(timeout(HANDSHAKE_TIMEOUT, conn::server_handshake(&mut io, uptime)).await, Ok(Ok(()))) {
        return;
    }
    // One deadline covers everything from accepting to `publish`.
    let deadline = tokio::time::Instant::now() + relay.negotiate_timeout;
    let (mut er, mut ew) = conn::split(io);
    er.set_max_message_size(PRE_PUBLISH_MAX_MESSAGE);
    let Ok(Some(connect)) = timeout_at(deadline, read_connect(&mut er, &mut ew)).await else { return };
    // Checked before anything else (route lookup, session log): strangers on
    // the network get no further than this.
    if let Some(key) = relay.lan_key()
        && !crate::relay::is_local(peer.ip())
        && !has_lan_key(&connect.object, &key)
    {
        let reason = "This computer needs OpenDelay's network access key: copy the server URL for other computers \
                      from the Setup page in OpenDelay.";
        let _ = ew.send(&error_reply(connect.transaction_id, "NetConnection.Connect.Rejected", reason)).await;
        return;
    }
    let app = connect.object.get("app").and_then(Amf0::as_str).unwrap_or("");
    let route = match relay.route_for(&normalize_path(app)) {
        Ok(r) => r,
        Err(reason) => {
            let _ = ew.send(&error_reply(connect.transaction_id, "NetConnection.Connect.Rejected", &reason)).await;
            return;
        }
    };
    // Another machine cannot take over a route that is streaming. The same
    // machine reconnecting replaces its session only at `publish`, with the
    // same stream key (see `serve`).
    if let Err(reason) = relay.check_other_machines(&route, peer.ip()) {
        let _ = ew.send(&error_reply(connect.transaction_id, "NetConnection.Connect.Rejected", &reason)).await;
        return;
    }

    let sess = new_session(&relay, id, route, peer);
    if let Err(reason) = relay.add_session(sess.clone()) {
        let _ = ew.send(&error_reply(connect.transaction_id, "NetConnection.Connect.Rejected", &reason)).await;
        sess.log.event(format!("Refused: {reason}"));
        sess.log.finish();
        return;
    }
    sess.log.event(format!("Encoder connected from {peer}"));
    let result = serve(&sess, &relay, er, ew, connect, deadline).await;
    match result {
        Ok(()) => sess.log.event("Session ended"),
        Err(e) => sess.log.event(format!("Session ended: {e}")),
    }
    sess.token.cancel();
    sess.log.finish();
    relay.end_session(&sess);
    relay.remove_session(id);
}

fn new_session(relay: &Shared, id: u64, route: Route, peer: SocketAddr) -> Arc<Session> {
    let host = UpstreamUrl::parse(&route.upstream).map(|u| u.host).unwrap_or_default();
    let mut header = vec![
        ("Session", crate::diag::short_id()),
        ("Route", format!("{} (/{})", route.name, route.path)),
        ("Upstream server", host),
        ("Encoder address", peer.to_string()),
    ];
    if !route.source.is_empty() {
        header.push(("Sends the stream of route", route.source.clone()));
    }
    if route.preset == "twitch" {
        header.push(("Twitch server", route.upstream.clone()));
    }
    let log = SessionLog::create(relay.log_dir.as_deref(), id, &header);
    let preset = route.preset.clone();
    let engine = Mutex::new(relay.new_engine(&route));
    Arc::new(Session {
        id,
        route,
        peer,
        started: Instant::now(),
        started_at: SystemTime::now(),
        engine,
        notify: Notify::new(),
        token: CancellationToken::new(),
        info: Mutex::new(SessionInfo { phase: Phase::Connecting, recovery_attempt: 0, last_error: None }),
        bytes_in: AtomicU64::new(0),
        bytes_out: AtomicU64::new(0),
        log,
        meter: Mutex::new((Instant::now(), 0, 0)),
        health: Mutex::new(Health {
            analyzer: HealthAnalyzer::new(&preset, Instant::now()),
            last_check: Instant::now(),
            logged: Vec::new(),
        }),
        claimed: AtomicBool::new(false),
        replaced: AtomicBool::new(false),
        kept_upstream: Mutex::new(None),
        publish_key: std::sync::OnceLock::new(),
    })
}

/// Continue a replaced session's buffer in `sess`. Returns its platform
/// connection when it can be kept: same server URL and, when `key` is
/// given, the same stream key; otherwise the buffer restarts on a new one.
fn take_over(sess: &Session, h: Handover, key: Option<&str>) -> Option<KeptUpstream> {
    let kept = h.upstream.filter(|k| k.upstream_url == sess.route.upstream && key.is_none_or(|key| key == k.key));
    let mut e = sess.engine();
    *e = h.engine;
    let unsent = e.continue_with_new_encoder();
    if kept.is_none() {
        e.restart_on_new_upstream();
    }
    drop(e);
    sess.log.event(format!(
        "Continuing the previous connection's delayed stream: {:.1} s not sent yet go out first{}",
        unsent.as_secs_f64(),
        if kept.is_some() { ", on the same platform connection" } else { "" }
    ));
    kept
}

/// Sessions for the routes that also send `sess`'s stream (multistream),
/// each connecting to its platform in the background.
fn start_mirrors(sess: &Session, relay: &Arc<Shared>, encoder_connect: &Amf0) -> Vec<Arc<Session>> {
    let mut started = Vec::new();
    for route in relay.mirrors_of(&sess.route.id) {
        let m = new_session(relay, relay.next_id(), route, sess.peer);
        m.log.set_secret(&m.route.key);
        let mut kept = None;
        if let Some(h) = relay.take_handover(&m.route) {
            kept = take_over(&m, h, Some(&m.route.key));
        } else {
            // No platform less delayed than the others: start as far behind as the source.
            let source_delay = sess.engine().target_delay();
            m.engine().start_delayed(source_delay);
        }
        m.claim();
        if let Err(reason) = relay.add_session(m.clone()) {
            m.log.event(format!("Refused: {reason}"));
            m.log.finish();
            continue;
        }
        sess.log.event(format!("Also sending to route \"{}\"", m.route.name));
        tokio::spawn(run_mirror(m.clone(), relay.clone(), encoder_connect.clone(), kept));
        started.push(m);
    }
    started
}

/// A route sending another route's stream: its own platform connection and
/// stream key, fed by that route's encoder until the encoder stops.
async fn run_mirror(sess: Arc<Session>, relay: Arc<Shared>, encoder_connect: Amf0, kept: Option<KeptUpstream>) {
    let key = sess.route.key.clone();
    match UpstreamUrl::parse(&sess.route.upstream) {
        Ok(url) => {
            let connect_obj = upstream::connect_object(&encoder_connect, &url);
            let opened = if let Some(k) = kept {
                Some(Ok(k.up))
            } else {
                sess.log.event(format!("Connecting to upstream {}:{}", url.host, url.port));
                tokio::select! {
                    r = Upstream::open(&url, &connect_obj, &key, &relay.tls) => Some(r),
                    _ = sess.token.cancelled() => None,
                }
            };
            let up = match opened {
                Some(Ok(up)) => Some(up),
                Some(Err(e)) => {
                    sess.log.event(format!("Upstream publish failed: {e}"));
                    recover(&sess, &relay.tls, &url, &connect_obj, &key, e).await
                }
                None => None,
            };
            if let Some(up) = up {
                sess.set_info(Phase::Live, 0, None);
                sess.log.event("Publishing; relay is live");
                upstream_loop(sess.clone(), relay.tls.clone(), up, url, connect_obj, key).await;
            }
        }
        Err(e) => sess.log.event(format!("Invalid server URL: {e}")),
    }
    sess.log.event("Session ended");
    sess.token.cancel();
    sess.log.finish();
    relay.end_session(&sess);
    relay.remove_session(sess.id);
}

/// Whether the `connect` object's `app` or `tcUrl` carries `?key=<key>`
/// (encoders put the server URL's query in one or the other).
fn has_lan_key(connect: &Amf0, key: &str) -> bool {
    ["app", "tcUrl"].iter().filter_map(|k| connect.get(k).and_then(Amf0::as_str)).any(|url| {
        url.split_once('?').is_some_and(|(_, query)| {
            query.split('&').any(|p| {
                p.strip_prefix("key=").is_some_and(|v| crate::api::constant_time_eq(v.as_bytes(), key.as_bytes()))
            })
        })
    })
}

/// Read until the encoder's `connect` command.
async fn read_connect(er: &mut MsgReader, ew: &mut MsgWriter) -> Option<Command> {
    loop {
        let msg = er.read().await.ok()??;
        if let Some(r) = control_reply(&msg) {
            ew.send(&r).await.ok()?;
        }
        if is_command(&msg) {
            let cmd = Command::parse(&msg).ok()?;
            if cmd.name == "connect" {
                return Some(cmd);
            }
        }
    }
}

async fn serve(
    sess: &Arc<Session>,
    relay: &Arc<Shared>,
    mut er: MsgReader,
    mut ew: MsgWriter,
    connect: Command,
    deadline: tokio::time::Instant,
) -> Res<()> {
    let tls = &relay.tls;
    ew.queue(&Message::window_ack_size(2_500_000));
    ew.queue(&Message::set_peer_bandwidth(2_500_000, 2));
    ew.queue_chunk_size(OUT_CHUNK_SIZE);
    ew.flush().await.map_err(io_err)?;

    let url = UpstreamUrl::parse(&sess.route.upstream).map_err(|e| e.to_string())?;
    let connect_obj = upstream::connect_object(&connect.object, &url);
    let mut fresh = None;
    let answer = if relay.has_own_stream(&sess.route, sess.peer.ip()) {
        // Probably this machine's encoder reconnecting: its platform
        // connection may go on, so answer locally and decide at `publish`.
        let props = Amf0::Object(vec![
            ("fmsVer".into(), Amf0::str("FMS/3,0,1,123")),
            ("capabilities".into(), Amf0::Number(31.0)),
        ]);
        let info = status_object("status", "NetConnection.Connect.Success", "Connection succeeded.");
        reply("_result", connect.transaction_id, props, vec![info])
    } else {
        match connect_upstream(sess, &url, &connect_obj, tls).await {
            Ok((up, result)) => {
                fresh = Some(up);
                // Relay the platform's answer so the encoder sees a normal session.
                reply("_result", connect.transaction_id, result.object, result.args)
            }
            Err(e) => {
                let (code, desc) = match &e {
                    UpstreamError::Rejected(c, d) => (c.clone(), d.clone()),
                    _ => {
                        ("NetConnection.Connect.Failed".into(), format!("OpenDelay could not reach {}: {e}", url.host))
                    }
                };
                let _ = ew.send(&error_reply(connect.transaction_id, &code, &desc)).await;
                return Err(format!("upstream connect failed: {e}"));
            }
        }
    };
    // Writes before `publish` share its deadline too (a peer that never reads
    // would otherwise stall them).
    let late = |_| "encoder did not publish in time".to_owned();
    timeout_at(deadline, ew.send(&answer)).await.map_err(late)?.map_err(io_err)?;

    // Answer the stream setup locally until `publish` carries the stream key.
    let key = loop {
        let msg = tokio::select! {
            r = timeout_at(deadline, er.read()) => r
                .map_err(late)?
                .map_err(io_err)?
                .ok_or("encoder disconnected before publishing")?,
            _ = sess.token.cancelled() => return Err("stopped".into()),
        };
        if let Some(a) = er.ack_due() {
            ew.queue(&a);
        }
        if let Some(r) = control_reply(&msg) {
            ew.queue(&r);
        }
        if is_command(&msg) {
            let cmd = Command::parse(&msg).map_err(|e| e.to_string())?;
            let tx = cmd.transaction_id;
            match cmd.name.as_str() {
                "releaseStream" => ew.queue(&reply("_result", tx, Amf0::Null, vec![Amf0::Undefined])),
                "FCPublish" => {
                    let info = status_object("status", "NetStream.Publish.Start", cmd.arg_str(0).unwrap_or(""));
                    ew.queue(&reply("onFCPublish", 0.0, Amf0::Null, vec![info]));
                }
                "createStream" => {
                    ew.queue(&reply("_result", tx, Amf0::Null, vec![Amf0::Number(ENCODER_STREAM_ID as f64)]))
                }
                "publish" => {
                    let key = cmd.arg_str(0).unwrap_or("").to_owned();
                    // Platforms may echo the key in error descriptions.
                    sess.log.set_secret(&key);
                    break key;
                }
                "FCUnpublish" | "deleteStream" | "closeStream" => return Err("encoder ended before publishing".into()),
                _ if tx > 0.0 => ew.queue(&reply("_result", tx, Amf0::Null, vec![Amf0::Null])),
                _ => {}
            }
        }
        timeout_at(deadline, ew.flush()).await.map_err(late)?.map_err(io_err)?;
    };

    // Replace and take the route only now: a peer that never publishes (or
    // publishes another key) cannot end this machine's stream, and an
    // emergency stop pressed meanwhile still wins.
    let denied = |reason: &str| {
        let status = status_object("error", "NetStream.Publish.Denied", reason);
        Command::new("onStatus", 0.0, Amf0::Null, vec![status]).to_message(ENCODER_STREAM_ID)
    };
    let _ = sess.publish_key.set(key.clone());
    if let Err(reason) = relay.replace_route_sessions(&sess.route, sess.peer.ip(), &key).await {
        let _ = ew.send(&denied(&reason)).await;
        return Err(reason);
    }
    let mut kept = None;
    if let Some(h) = relay.take_handover(&sess.route) {
        kept = take_over(sess, h, Some(&key));
    } else if let Some(d) = relay.kept_delay(&sess.route) {
        sess.engine().start_delayed(d);
        sess.log
            .event(format!("Delay kept from the previous connection: viewers see this stream {} s later", d.as_secs()));
    }
    if let Err(reason) = relay.claim_route(sess) {
        let _ = ew.send(&denied(&reason)).await;
        return Err(reason);
    }
    let (up, published) = match kept {
        Some(k) => {
            sess.log.event("Continuing on the previous platform connection");
            (k.up, Ok(status_object("status", "NetStream.Publish.Start", "Publishing.")))
        }
        None => {
            let mut up = match fresh.take() {
                Some(up) => up,
                None => match connect_upstream(sess, &url, &connect_obj, tls).await {
                    Ok((up, _)) => up,
                    Err(e) => {
                        let desc = format!("OpenDelay could not reach {}: {e}", url.host);
                        let status = status_object("error", "NetStream.Publish.Failed", &desc);
                        let msg = Command::new("onStatus", 0.0, Amf0::Null, vec![status]).to_message(ENCODER_STREAM_ID);
                        let _ = ew.send(&msg).await;
                        return Err(format!("upstream connect failed: {e}"));
                    }
                },
            };
            let published = tokio::select! {
                r = up.publish(&key) => r,
                _ = sess.token.cancelled() => return Err("stopped".into()),
            };
            (up, published)
        }
    };
    match published {
        Ok(info) => {
            ew.queue(&Message::user_control(user_control::STREAM_BEGIN, &ENCODER_STREAM_ID.to_be_bytes()));
            send_to_encoder(
                &mut ew,
                &Command::new("onStatus", 0.0, Amf0::Null, vec![info]).to_message(ENCODER_STREAM_ID),
            )
            .await?;
        }
        Err(e) => {
            let (code, desc) = match &e {
                UpstreamError::Rejected(c, d) => (c.clone(), d.clone()),
                _ => ("NetStream.Publish.Failed".into(), format!("OpenDelay could not publish upstream: {e}")),
            };
            let status = status_object("error", &code, &desc);
            let _ =
                ew.send(&Command::new("onStatus", 0.0, Amf0::Null, vec![status]).to_message(ENCODER_STREAM_ID)).await;
            return Err(format!("upstream publish failed: {e}"));
        }
    }
    sess.set_info(Phase::Live, 0, None);
    sess.log.event("Publishing; relay is live");
    er.set_max_message_size(u32::MAX); // media: keyframes can be several MiB

    let mirrors = start_mirrors(sess, relay, &connect.object);
    let upstream_task = tokio::spawn(upstream_loop(sess.clone(), tls.clone(), up, url, connect_obj, key));
    let ended = encoder_loop(sess, &mirrors, &mut er, &mut ew).await;
    for s in std::iter::once(sess).chain(&mirrors) {
        // Stopped, or still without a platform connection: nothing to finish.
        if sess.token.is_cancelled() || (s.id != sess.id && s.info().phase != Phase::Live) {
            s.token.cancel();
            continue;
        }
        s.engine().finish_input();
        s.wake();
        let info = s.info();
        s.set_info(Phase::Finishing, info.recovery_attempt, info.last_error);
    }
    let _ = upstream_task.await;
    ended
}

/// TCP (+TLS), handshake and `connect` to the route's platform, unless the session is stopped.
async fn connect_upstream(
    sess: &Session,
    url: &UpstreamUrl,
    connect_obj: &Amf0,
    tls: &Arc<ClientConfig>,
) -> Result<(Upstream, Command), UpstreamError> {
    sess.log.event(format!(
        "Connecting to upstream {}:{} ({})",
        url.host,
        url.port,
        if url.tls { "rtmps" } else { "rtmp" }
    ));
    tokio::select! {
        r = Upstream::connect(url, connect_obj, tls) => r,
        _ = sess.token.cancelled() => Err(UpstreamError::Protocol("stopped".into())),
    }
}

/// Write to a publishing encoder; one that stops reading is dropped instead
/// of stalling its session forever.
async fn send_to_encoder(ew: &mut MsgWriter, msg: &Message) -> Res<()> {
    match timeout(ENCODER_IDLE, ew.send(msg)).await {
        Ok(r) => r.map_err(io_err),
        Err(_) => Err(format!("encoder read nothing for {} s", ENCODER_IDLE.as_secs())),
    }
}

async fn encoder_loop(sess: &Session, mirrors: &[Arc<Session>], er: &mut MsgReader, ew: &mut MsgWriter) -> Res<()> {
    let push = |m: Message| {
        for x in mirrors.iter().filter(|x| !x.token.is_cancelled()) {
            x.push(m.clone()); // payloads are shared, not copied
        }
        sess.push(m);
    };
    loop {
        let msg = tokio::select! {
            r = timeout(ENCODER_IDLE, er.read()) => match r {
                Ok(Ok(Some(m))) => m,
                Ok(Ok(None)) => {
                    sess.log.event("Encoder disconnected");
                    return Ok(());
                }
                Ok(Err(e)) => return Err(io_err(e)),
                Err(_) => return Err(format!("encoder sent nothing for {} s", ENCODER_IDLE.as_secs())),
            },
            _ = sess.token.cancelled() => return Err("stopped".into()),
        };
        for s in std::iter::once(sess).chain(mirrors.iter().map(|m| &**m)) {
            s.bytes_in.store(er.bytes_received(), Ordering::Relaxed);
        }
        if let Some(a) = er.ack_due() {
            send_to_encoder(ew, &a).await?;
        }
        match msg.type_id {
            msg_type::AUDIO | msg_type::VIDEO | msg_type::DATA_AMF0 | msg_type::DATA_AMF3 => push(msg),
            // Only media from inside an aggregate: control or command types would
            // otherwise reach the upstream verbatim.
            msg_type::AGGREGATE => split_aggregate(&msg).into_iter().filter(Message::is_media).for_each(push),
            msg_type::USER_CONTROL => {
                if let Some(r) = control_reply(&msg) {
                    send_to_encoder(ew, &r).await?;
                }
            }
            msg_type::COMMAND_AMF0 | msg_type::COMMAND_AMF3 => {
                let Ok(cmd) = Command::parse(&msg) else { continue };
                match cmd.name.as_str() {
                    "FCUnpublish" | "deleteStream" | "closeStream" => {
                        sess.log.event("Encoder stopped publishing");
                        return Ok(());
                    }
                    _ if cmd.transaction_id > 0.0 => {
                        send_to_encoder(ew, &reply("_result", cmd.transaction_id, Amf0::Null, vec![Amf0::Null]))
                            .await?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Owns the upstream connection: pumps engine output, recovers on failure.
async fn upstream_loop(
    sess: Arc<Session>,
    tls: Arc<ClientConfig>,
    mut up: Upstream,
    url: UpstreamUrl,
    connect_obj: Amf0,
    key: String,
) {
    loop {
        let err = match pump(&sess, &mut up).await {
            Ok(()) => {
                up.close(&key).await;
                sess.log.event("All buffered media sent; upstream closed");
                return;
            }
            Err(e) => e,
        };
        if sess.token.is_cancelled() {
            if sess.replaced.load(Ordering::SeqCst) {
                // The encoder reconnected: its new session continues on this connection.
                let upstream_url = sess.route.upstream.clone();
                *sess.kept_upstream.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(KeptUpstream { up, key, upstream_url });
            }
            return;
        }
        sess.log.event(format!("Upstream connection lost: {err}"));
        // Close it first: a stalled socket stays open otherwise, and platforms
        // that allow one connection per stream key refuse the new one.
        drop(up);
        let Some(new) = recover(&sess, &tls, &url, &connect_obj, &key, err).await else {
            if !sess.token.is_cancelled() {
                sess.log.event(if sess.route.source.is_empty() {
                    "Upstream recovery failed; disconnecting the encoder"
                } else {
                    "Upstream recovery failed; this route stops"
                });
                sess.token.cancel();
            }
            return;
        };
        up = new;
        sess.engine().upstream_reset();
        sess.log.event("Upstream reconnected; resuming from a keyframe");
    }
}

async fn recover(
    sess: &Session,
    tls: &Arc<ClientConfig>,
    url: &UpstreamUrl,
    connect_obj: &Amf0,
    key: &str,
    mut err: UpstreamError,
) -> Option<Upstream> {
    let resume_phase = sess.info().phase;
    let resume_phase = if resume_phase == Phase::Finishing { Phase::Finishing } else { Phase::Live };
    // A route sending another route's stream keeps trying while that stream
    // lasts; giving up on the encoder's own route disconnects the encoder,
    // which then reconnects by itself.
    let attempts =
        if sess.route.source.is_empty() || resume_phase == Phase::Finishing { MAX_RECOVERY_ATTEMPTS } else { u32::MAX };
    for attempt in 1..=attempts {
        sess.set_info(Phase::Recovering, attempt, Some(err.to_string()));
        let backoff = Duration::from_millis(500 << (attempt - 1).min(5)).min(Duration::from_secs(15));
        let opened = tokio::select! {
            r = async { sleep(backoff).await; Upstream::open(url, connect_obj, key, tls).await } => r,
            _ = sess.token.cancelled() => return None,
        };
        match opened {
            Ok(up) => {
                sess.set_info(resume_phase, attempt, None);
                return Some(up);
            }
            Err(e) => {
                sess.log.event(format!("Reconnect attempt {attempt} failed: {e}"));
                err = e;
            }
        }
    }
    sess.set_info(Phase::Recovering, attempts, Some(err.to_string()));
    None
}

/// Flush to the platform. Fails only when nothing at all goes out for
/// [`UPSTREAM_STALL`]: a slow link is not a dead one.
async fn flush_upstream(writer: &mut MsgWriter) -> Result<(), UpstreamError> {
    loop {
        let before = writer.bytes_sent();
        match timeout(UPSTREAM_STALL, writer.flush()).await {
            Ok(r) => return Ok(r?),
            // The flush is cancel-safe: carry on where it stopped.
            Err(_) if writer.bytes_sent() > before => continue,
            Err(_) => return Err(UpstreamError::Timeout("sending to the platform")),
        }
    }
}

/// Run one upstream connection until it fails (`Err`) or everything has been sent (`Ok`).
async fn pump(sess: &Session, up: &mut Upstream) -> Result<(), UpstreamError> {
    let stream_id = up.stream_id;
    let (reader, writer) = (&mut up.reader, &mut up.writer);
    // Bounded: a server that floods pings while not reading cannot grow it;
    // dropped pongs and acknowledgements are harmless.
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::channel::<Message>(64);

    let read = async {
        loop {
            let msg = reader.read().await?.ok_or_else(|| {
                UpstreamError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed the connection",
                ))
            })?;
            if let Some(a) = reader.ack_due() {
                let _ = ctl_tx.try_send(a);
            }
            if let Some(r) = control_reply(&msg) {
                let _ = ctl_tx.try_send(r);
            }
            if is_command(&msg)
                && let Ok(cmd) = Command::parse(&msg)
            {
                let (level, code, desc) = status_of(&cmd);
                if code == "NetConnection.Connect.ReconnectRequest" {
                    return Err(UpstreamError::Protocol("server requested a reconnect".into()));
                }
                if cmd.name == "onStatus" && level == "error" {
                    return Err(UpstreamError::Rejected(code, desc));
                }
            }
        }
    };

    let write = async {
        let mut flushed = writer.bytes_sent();
        loop {
            while let Ok(c) = ctl_rx.try_recv() {
                writer.queue(&c);
            }
            let (poll, skipped) = {
                let mut e = sess.engine();
                (e.poll(Instant::now()), e.take_skipped())
            };
            if !skipped.is_zero() {
                sess.log.event(format!(
                    "Upload slower than the stream: skipped {:.1} s to keep viewers at the delay",
                    skipped.as_secs_f64()
                ));
            }
            let wait = match poll {
                Poll::Send(mut m) => {
                    m.stream_id = stream_id;
                    writer.queue(&m);
                    if !writer.should_flush() {
                        continue;
                    }
                    None
                }
                Poll::ResetHeaders => {
                    writer.reset_headers();
                    continue;
                }
                Poll::Wait(t) => Some(Some(t)),
                Poll::Idle => Some(None),
                Poll::Finished => {
                    flush_upstream(writer).await?;
                    return Ok(());
                }
            };
            flush_upstream(writer).await?;
            sess.bytes_out.fetch_add(writer.bytes_sent() - flushed, Ordering::Relaxed);
            flushed = writer.bytes_sent();
            if let Some(deadline) = wait {
                let sleep = async {
                    match deadline {
                        Some(t) => sleep_until(t.into()).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = sleep => {}
                    _ = sess.notify.notified() => {}
                    Some(c) = ctl_rx.recv() => writer.queue(&c),
                }
            }
        }
    };

    tokio::select! {
        r = read => r,
        r = write => r,
        _ = sess.token.cancelled() => Err(UpstreamError::Protocol("stopped".into())),
    }
}

#[cfg(test)]
pub(crate) fn test_session(id: u64, path: &str, ip: [u8; 4]) -> Arc<Session> {
    Arc::new(Session {
        id,
        route: Route { id: path.into(), name: path.into(), path: path.into(), ..Default::default() },
        peer: SocketAddr::from((ip, 1000)),
        started: Instant::now(),
        started_at: SystemTime::now(),
        engine: Mutex::new(Engine::default()),
        notify: Notify::new(),
        token: CancellationToken::new(),
        info: Mutex::new(SessionInfo { phase: Phase::Connecting, recovery_attempt: 0, last_error: None }),
        bytes_in: AtomicU64::new(0),
        bytes_out: AtomicU64::new(0),
        log: SessionLog::create(None, id, &[]),
        meter: Mutex::new((Instant::now(), 0, 0)),
        health: Mutex::new(Health {
            analyzer: HealthAnalyzer::new("", Instant::now()),
            last_check: Instant::now(),
            logged: Vec::new(),
        }),
        claimed: AtomicBool::new(false),
        replaced: AtomicBool::new(false),
        kept_upstream: Mutex::new(None),
        publish_key: std::sync::OnceLock::new(),
    })
}
