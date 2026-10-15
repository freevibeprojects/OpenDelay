// SPDX-License-Identifier: GPL-3.0-only
//! One encoder connection: RTMP server side towards the encoder, the publish
//! sequence with the upstream on its behalf, then the relay loop with
//! automatic upstream recovery.

use crate::conn::{self, BoxIo, MsgReader, MsgWriter, OUT_CHUNK_SIZE};
use crate::diag::SessionLog;
use crate::engine::{Engine, Poll};
use crate::health::{self, HealthAnalyzer, HealthReport};
use crate::relay::Shared;
use crate::routes::{Route, normalize_path};
use crate::upstream::{self, Upstream, UpstreamError, control_reply, status_of};
use crate::url::UpstreamUrl;
use opendelay_rtmp::amf0::Amf0;
use opendelay_rtmp::message::{split_aggregate, status_object, user_control};
use opendelay_rtmp::{Command, Message, msg_type};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::{sleep, sleep_until, timeout};
use tokio_rustls::rustls::ClientConfig;
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(30);
/// An encoder that sends nothing for this long is gone (or hostile).
const ENCODER_IDLE: Duration = Duration::from_secs(20);
/// Writing upstream that makes no progress for this long means the
/// connection is dead (the kernel would take many minutes to notice).
const UPSTREAM_STALL: Duration = Duration::from_secs(15);
pub const MAX_RECOVERY_ATTEMPTS: u32 = 8;
/// Encoder-facing stream id returned by our `createStream`.
const ENCODER_STREAM_ID: u32 = 1;

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
        {
            let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
            h.analyzer.observe(&msg, now);
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
        self.engine().push(msg, now);
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
    let (mut er, mut ew) = conn::split(io);
    let Ok(Some(connect)) = timeout(NEGOTIATE_TIMEOUT, read_connect(&mut er, &mut ew)).await else { return };
    let app = connect.object.get("app").and_then(Amf0::as_str).unwrap_or("");
    let route = match relay.route_for(&normalize_path(app)) {
        Ok(r) => r,
        Err(reason) => {
            let _ = ew.send(&error_reply(connect.transaction_id, "NetConnection.Connect.Rejected", &reason)).await;
            return;
        }
    };
    // The same machine reconnecting replaces its stale session; another
    // machine cannot take over a route that is streaming.
    if let Err(reason) = relay.replace_route_sessions(&route.path, peer.ip()).await {
        let _ = ew.send(&error_reply(connect.transaction_id, "NetConnection.Connect.Rejected", &reason)).await;
        return;
    }

    let host = UpstreamUrl::parse(&route.upstream).map(|u| u.host).unwrap_or_default();
    let mut header = vec![
        ("Session", crate::diag::short_id()),
        ("Route", format!("{} (/{})", route.name, route.path)),
        ("Upstream server", host),
        ("Encoder address", peer.to_string()),
    ];
    if route.preset == "twitch" {
        header.push(("Twitch server", route.upstream.clone()));
    }
    let log = SessionLog::create(relay.log_dir.as_deref(), id, &header);
    let preset = route.preset.clone();
    let sess = Arc::new(Session {
        id,
        route,
        peer,
        started: Instant::now(),
        started_at: SystemTime::now(),
        engine: Mutex::new(relay.new_engine()),
        notify: Notify::new(),
        token: CancellationToken::new(),
        info: Mutex::new(SessionInfo { phase: Phase::Connecting, recovery_attempt: 0, last_error: None }),
        bytes_in: AtomicU64::new(0),
        bytes_out: AtomicU64::new(0),
        log,
        meter: Mutex::new((Instant::now(), 0, 0)),
        health: Mutex::new(Health {
            analyzer: HealthAnalyzer::new(health::platform_limit(&preset), Instant::now()),
            last_check: Instant::now(),
            logged: Vec::new(),
        }),
    });
    relay.add_session(sess.clone());
    sess.log.event(format!("Encoder connected from {peer}"));
    let result = serve(&sess, &relay.tls, er, ew, connect).await;
    match result {
        Ok(()) => sess.log.event("Session ended"),
        Err(e) => sess.log.event(format!("Session ended: {e}")),
    }
    sess.token.cancel();
    sess.log.finish();
    relay.remove_session(id);
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
    tls: &Arc<ClientConfig>,
    mut er: MsgReader,
    mut ew: MsgWriter,
    connect: Command,
) -> Res<()> {
    ew.queue(&Message::window_ack_size(2_500_000));
    ew.queue(&Message::set_peer_bandwidth(2_500_000, 2));
    ew.queue_chunk_size(OUT_CHUNK_SIZE);
    ew.flush().await.map_err(io_err)?;

    let url = UpstreamUrl::parse(&sess.route.upstream).map_err(|e| e.to_string())?;
    let connect_obj = upstream::connect_object(&connect.object, &url);
    sess.log.event(format!(
        "Connecting to upstream {}:{} ({})",
        url.host,
        url.port,
        if url.tls { "rtmps" } else { "rtmp" }
    ));
    let connected = tokio::select! {
        r = Upstream::connect(&url, &connect_obj, tls) => r,
        _ = sess.token.cancelled() => return Err("stopped".into()),
    };
    let (mut up, result) = match connected {
        Ok(v) => v,
        Err(e) => {
            let (code, desc) = match &e {
                UpstreamError::Rejected(c, d) => (c.clone(), d.clone()),
                _ => ("NetConnection.Connect.Failed".into(), format!("OpenDelay could not reach {}: {e}", url.host)),
            };
            let _ = ew.send(&error_reply(connect.transaction_id, &code, &desc)).await;
            return Err(format!("upstream connect failed: {e}"));
        }
    };
    // Relay the platform's answer so the encoder sees a normal session.
    ew.send(&reply("_result", connect.transaction_id, result.object, result.args)).await.map_err(io_err)?;

    // Answer the stream setup locally until `publish` carries the stream key.
    let key = loop {
        let msg = tokio::select! {
            r = timeout(ENCODER_IDLE, er.read()) => r
                .map_err(|_| "encoder sent nothing before publishing")?
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
        ew.flush().await.map_err(io_err)?;
    };

    let published = tokio::select! {
        r = up.publish(&key) => r,
        _ = sess.token.cancelled() => return Err("stopped".into()),
    };
    match published {
        Ok(info) => {
            ew.queue(&Message::user_control(user_control::STREAM_BEGIN, &ENCODER_STREAM_ID.to_be_bytes()));
            ew.send(&Command::new("onStatus", 0.0, Amf0::Null, vec![info]).to_message(ENCODER_STREAM_ID))
                .await
                .map_err(io_err)?;
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

    let upstream_task = tokio::spawn(upstream_loop(sess.clone(), tls.clone(), up, url, connect_obj, key));
    let ended = encoder_loop(sess, &mut er, &mut ew).await;
    sess.engine().finish_input();
    sess.wake();
    if !sess.token.is_cancelled() {
        let info = sess.info();
        sess.set_info(Phase::Finishing, info.recovery_attempt, info.last_error);
    }
    let _ = upstream_task.await;
    ended
}

async fn encoder_loop(sess: &Session, er: &mut MsgReader, ew: &mut MsgWriter) -> Res<()> {
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
        sess.bytes_in.store(er.bytes_received(), Ordering::Relaxed);
        if let Some(a) = er.ack_due() {
            ew.send(&a).await.map_err(io_err)?;
        }
        match msg.type_id {
            msg_type::AUDIO | msg_type::VIDEO | msg_type::DATA_AMF0 | msg_type::DATA_AMF3 => sess.push(msg),
            msg_type::AGGREGATE => split_aggregate(&msg).into_iter().for_each(|m| sess.push(m)),
            msg_type::USER_CONTROL => {
                if let Some(r) = control_reply(&msg) {
                    ew.send(&r).await.map_err(io_err)?;
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
                        ew.send(&reply("_result", cmd.transaction_id, Amf0::Null, vec![Amf0::Null]))
                            .await
                            .map_err(io_err)?;
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
            return;
        }
        sess.log.event(format!("Upstream connection lost: {err}"));
        let Some(new) = recover(&sess, &tls, &url, &connect_obj, &key, err).await else {
            if !sess.token.is_cancelled() {
                sess.log.event("Upstream recovery failed; disconnecting the encoder");
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
    for attempt in 1..=MAX_RECOVERY_ATTEMPTS {
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
    sess.set_info(Phase::Recovering, MAX_RECOVERY_ATTEMPTS, Some(err.to_string()));
    None
}

/// Run one upstream connection until it fails (`Err`) or everything has been sent (`Ok`).
async fn pump(sess: &Session, up: &mut Upstream) -> Result<(), UpstreamError> {
    let stream_id = up.stream_id;
    let (reader, writer) = (&mut up.reader, &mut up.writer);
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    let read = async {
        loop {
            let msg = reader.read().await?.ok_or_else(|| {
                UpstreamError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed the connection",
                ))
            })?;
            if let Some(a) = reader.ack_due() {
                let _ = ctl_tx.send(a);
            }
            if let Some(r) = control_reply(&msg) {
                let _ = ctl_tx.send(r);
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

    let stalled = || UpstreamError::Timeout("sending to the platform");
    let write = async {
        let mut flushed = writer.bytes_sent();
        loop {
            while let Ok(c) = ctl_rx.try_recv() {
                writer.queue(&c);
            }
            let poll = sess.engine().poll(Instant::now());
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
                    timeout(UPSTREAM_STALL, writer.flush()).await.map_err(|_| stalled())??;
                    return Ok(());
                }
            };
            timeout(UPSTREAM_STALL, writer.flush()).await.map_err(|_| stalled())??;
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
