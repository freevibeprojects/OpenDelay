// SPDX-License-Identifier: GPL-3.0-only
//! The relay: RTMP listener(s), route lookup, the set of live sessions,
//! graceful shutdown and emergency stop.

use crate::engine::{self, Enabled, Engine, Refusal};
use crate::routes::{Route, normalize_path};
use crate::session::{self, Handover, Phase, Session};
use crate::upstream;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::rustls::ClientConfig;
use tokio_util::sync::CancellationToken;

/// Delay settings shared by every stream (SPEC §4.1, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayConfig {
    /// Rolling-buffer feature on/off ("delay mode").
    pub mode_enabled: bool,
    /// Rolling buffer length chosen by the user; the buffer kept is at
    /// least the delay (see [`DelayConfig::retain`]).
    pub buffer: Duration,
    /// Delay used by "enable delay" without an explicit duration.
    pub delay: Duration,
}

impl Default for DelayConfig {
    fn default() -> Self {
        DelayConfig { mode_enabled: true, buffer: Duration::from_secs(60), delay: Duration::from_secs(30) }
    }
}

impl DelayConfig {
    /// Buffer kept: the chosen length, or the delay when that is longer, so
    /// it shrinks back once a long delay is shortened again.
    pub fn retain(&self) -> Duration {
        if self.mode_enabled { self.buffer.max(self.delay) } else { Duration::ZERO }
    }
}

pub const MIN_BUFFER: Duration = Duration::from_secs(5);
/// Concurrent encoder connections from this computer (127.0.0.1 / ::1).
pub const MAX_LOCAL_CONNECTIONS: usize = 16;
/// Concurrent connections from other computers (LAN mode), in total and per
/// address: they can never use up the local pool.
pub const MAX_REMOTE_CONNECTIONS: usize = 16;
pub const MAX_PER_REMOTE_ADDRESS: usize = 4;
/// Time from accepting a connection until it publishes.
pub const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Encoders on this computer (other addresses, even 127.0.0.2, count as remote).
pub(crate) fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4 == std::net::Ipv4Addr::LOCALHOST,
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped() == Some(std::net::Ipv4Addr::LOCALHOST),
    }
}

#[derive(Default)]
struct Connections {
    local: usize,
    remote: usize,
    per_remote: std::collections::HashMap<IpAddr, usize>,
}

/// One admitted connection; frees its place when dropped.
struct Admission {
    shared: Arc<Shared>,
    ip: IpAddr,
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut c = self.shared.connections.lock().unwrap_or_else(|e| e.into_inner());
        if is_local(self.ip) {
            c.local -= 1;
        } else {
            c.remote -= 1;
            if let Some(n) = c.per_remote.get_mut(&self.ip) {
                *n -= 1;
                if *n == 0 {
                    c.per_remote.remove(&self.ip);
                }
            }
        }
    }
}

/// Same wording as the window and dock: `45 s`, `2 min`, `1 min 10 s`.
fn secs(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 60, s % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

pub struct RelayOptions {
    pub delay: DelayConfig,
    /// Folder for per-session logs (`None`: no log files).
    pub log_dir: Option<PathBuf>,
    /// TLS client configuration for `rtmps` upstreams (default: OS trust store).
    pub tls: Option<Arc<ClientConfig>>,
    /// Deadline from accepting a connection to its `publish` (tests shorten it).
    pub negotiate_timeout: Duration,
    /// Key that encoders on other computers must give as `?key=` in the server
    /// URL (`None`: no key needed).
    pub lan_key: Option<String>,
}

impl Default for RelayOptions {
    fn default() -> Self {
        RelayOptions {
            delay: DelayConfig::default(),
            log_dir: None,
            tls: None,
            negotiate_timeout: NEGOTIATE_TIMEOUT,
            lan_key: None,
        }
    }
}

pub(crate) struct Shared {
    routes: RwLock<Vec<Route>>,
    sessions: Mutex<Vec<Arc<Session>>>,
    /// Cancelled when the relay stops accepting connections (shutdown).
    stop: CancellationToken,
    /// Latched by emergency stop: refuse publishing until cleared.
    emergency: AtomicBool,
    next_id: AtomicU64,
    delay: Mutex<DelayConfig>,
    /// Open encoder connections, shared by all listeners.
    connections: Mutex<Connections>,
    /// Bytes held by all stream buffers together.
    buffered: Arc<AtomicU64>,
    pub negotiate_timeout: Duration,
    lan_key: RwLock<Option<String>>,
    pub tls: Arc<ClientConfig>,
    pub started: Instant,
    pub log_dir: Option<PathBuf>,
    /// Per route path: the delay its last session ended with, and when.
    kept_delays: Mutex<HashMap<String, (Duration, Instant)>>,
    /// Per route path: the buffer of a delayed session that a reconnecting
    /// encoder replaced, for the new session to continue.
    handovers: Mutex<HashMap<String, Handover>>,
    /// Bumped when a stream connects, ends, changes phase or jumps (status pushes).
    pub changed: Arc<watch::Sender<u64>>,
}

/// How long a reconnecting encoder still gets the delay its route ended with.
const KEEP_DELAY_FOR: Duration = Duration::from_secs(600);

impl Shared {
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn route_for(&self, path: &str) -> Result<Route, String> {
        self.emergency_or_stopping()?;
        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());
        match routes.iter().find(|r| normalize_path(&r.path) == path) {
            Some(r) if !r.enabled => Err(format!("The route \"{}\" is disabled in OpenDelay.", r.name)),
            Some(r) if !r.source.is_empty() => Err(format!(
                "The route \"{}\" sends the stream of another route in OpenDelay; point your encoder at that route.",
                r.name
            )),
            Some(r) => Ok(r.clone()),
            None => Err(format!("OpenDelay has no route \"/{path}\". Check the server URL in your encoder.")),
        }
    }

    /// Enabled routes that also send the stream of route `id`.
    pub fn mirrors_of(&self, id: &str) -> Vec<Route> {
        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());
        routes.iter().filter(|r| r.enabled && r.source == id && !r.key.is_empty()).cloned().collect()
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn lan_key(&self) -> Option<String> {
        self.lan_key.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn new_engine(&self, route: &Route) -> Engine {
        if !route.source.is_empty() {
            // shortcut: a mirror shares its source's payloads, so it stays out of the
            // total budget; content only it still holds is bounded by MAX_BUFFER_BYTES
            // alone. Count shared payloads once if that ever matters.
            return Engine::new(self.config().retain());
        }
        Engine::with_budget(self.config().retain(), self.buffered.clone())
    }

    /// Admit a connection from `ip`, or `None` when its pool is full.
    fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<Admission> {
        let mut c = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        if is_local(ip) {
            if c.local >= MAX_LOCAL_CONNECTIONS {
                return None;
            }
            c.local += 1;
        } else {
            let mine = c.per_remote.get(&ip).copied().unwrap_or(0);
            if c.remote >= MAX_REMOTE_CONNECTIONS || mine >= MAX_PER_REMOTE_ADDRESS {
                return None;
            }
            c.remote += 1;
            c.per_remote.insert(ip, mine + 1);
        }
        Some(Admission { shared: self.clone(), ip })
    }

    fn emergency_or_stopping(&self) -> Result<(), String> {
        if self.emergency.load(Ordering::SeqCst) {
            return Err("Emergency stop is active in OpenDelay; allow streaming again in the app first.".into());
        }
        if self.stop.is_cancelled() {
            return Err("OpenDelay is shutting down.".into());
        }
        Ok(())
    }

    fn config(&self) -> DelayConfig {
        *self.delay.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a session. Checked under the same lock that emergency stop
    /// takes, so a connection racing an emergency stop cannot slip through.
    pub fn add_session(&self, s: Arc<Session>) -> Result<(), String> {
        let mut sessions = self.lock_sessions();
        self.emergency_or_stopping()?;
        sessions.push(s);
        Ok(())
    }

    /// At `publish`: take the route for `sess`. Fails during an emergency stop
    /// or when another session already publishes on the route. Atomic, so two
    /// encoders cannot both win.
    pub fn claim_route(&self, sess: &Session) -> Result<(), String> {
        let sessions = self.lock_sessions();
        self.emergency_or_stopping()?;
        let path = normalize_path(&sess.route.path);
        if sessions.iter().any(|s| s.id != sess.id && s.is_claimed() && normalize_path(&s.route.path) == path) {
            return Err("This route is already receiving a stream from another computer.".into());
        }
        sess.claim();
        Ok(())
    }

    pub fn remove_session(&self, id: u64) {
        self.lock_sessions().retain(|s| s.id != id);
        self.changed.send_modify(|v| *v += 1);
    }

    fn on_route(&self, route: &Route) -> Vec<Arc<Session>> {
        let path = normalize_path(&route.path);
        let sessions = self.lock_sessions();
        sessions
            .iter()
            .filter(|s| normalize_path(&s.route.path) == path || s.route.source == route.id)
            .cloned()
            .collect()
    }

    /// At `connect`: another machine cannot take over a route that is publishing.
    pub fn check_other_machines(&self, route: &Route, peer: IpAddr) -> Result<(), String> {
        if self.on_route(route).iter().any(|s| s.peer.ip() != peer && s.is_claimed()) {
            return Err("This route is already receiving a stream from another computer.".into());
        }
        Ok(())
    }

    /// Whether `peer` is publishing on `route` (an encoder reconnecting finds its old session).
    pub fn has_own_stream(&self, route: &Route, peer: IpAddr) -> bool {
        self.on_route(route).iter().any(|s| s.peer.ip() == peer && s.is_claimed() && s.route.source.is_empty())
    }

    /// At `publish` with `key`: stop the session the same machine has
    /// publishing on `route` with that key, and the routes sending its stream
    /// (an encoder reconnecting), and wait (≤ 3 s) for them to end. A session
    /// publishing another key is not replaced: a connection that only knows
    /// the route cannot end the stream. Sessions still negotiating are left
    /// alone (see `claim_route`).
    pub async fn replace_route_sessions(&self, route: &Route, peer: IpAddr, key: &str) -> Result<(), String> {
        let on_route = self.on_route(route);
        if on_route.iter().any(|s| s.peer.ip() != peer && s.is_claimed()) {
            return Err("This route is already receiving a stream from another computer.".into());
        }
        let old: Vec<Arc<Session>> = on_route.into_iter().filter(|s| s.peer.ip() == peer && s.is_claimed()).collect();
        let same_key = |s: &Session| {
            s.publish_key.get().is_some_and(|k| crate::api::constant_time_eq(k.as_bytes(), key.as_bytes()))
        };
        if old.iter().any(|s| s.route.source.is_empty() && !same_key(s)) {
            return Err("This route is still sending a stream with another stream key. Stop that stream first, \
                        or wait until it has finished."
                .into());
        }
        for s in &old {
            s.log.event("Replaced by a new encoder connection on the same route");
            s.replaced.store(true, Ordering::SeqCst);
            s.token.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && self.lock_sessions().iter().any(|s| old.iter().any(|o| o.id == s.id)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for s in &old {
            s.replaced.store(false, Ordering::SeqCst); // too late to be continued
        }
        Ok(())
    }

    /// A session ending: when a reconnecting encoder replaced it with the
    /// delay on, keep its buffer and platform connection for the new session
    /// ([`Shared::take_handover`]).
    pub(crate) fn end_session(&self, sess: &Session) {
        let mut engine = sess.engine();
        if sess.replaced.load(Ordering::SeqCst) && engine.is_active() {
            let engine = std::mem::take(&mut *engine);
            let upstream = sess.kept_upstream.lock().unwrap_or_else(|e| e.into_inner()).take();
            self.lock_handovers().insert(normalize_path(&sess.route.path), Handover { engine, upstream });
        } else if sess.route.source.is_empty() {
            self.remember_delay(&sess.route, engine.target_delay());
        }
    }

    pub(crate) fn take_handover(&self, route: &Route) -> Option<Handover> {
        self.lock_handovers().remove(&normalize_path(&route.path))
    }

    fn lock_handovers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Handover>> {
        self.handovers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// When a route's encoder session ends: the delay it had on (zero: live),
    /// so an encoder reconnecting soon after (OBS crash, network blip) does
    /// not go out live while the user believes the delay is on.
    pub fn remember_delay(&self, route: &Route, delay: Duration) {
        let mut kept = self.kept_delays.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|_, (_, at)| at.elapsed() < KEEP_DELAY_FOR);
        if delay.is_zero() {
            kept.remove(&normalize_path(&route.path));
        } else {
            kept.insert(normalize_path(&route.path), (delay, Instant::now()));
        }
    }

    pub fn kept_delay(&self, route: &Route) -> Option<Duration> {
        let kept = self.kept_delays.lock().unwrap_or_else(|e| e.into_inner());
        kept.get(&normalize_path(&route.path)).filter(|(_, at)| at.elapsed() < KEEP_DELAY_FOR).map(|(d, _)| *d)
    }
}

#[derive(Clone)]
pub struct Relay {
    shared: Arc<Shared>,
}

impl Relay {
    pub fn new(routes: Vec<Route>, opts: RelayOptions) -> Self {
        Relay {
            shared: Arc::new(Shared {
                routes: RwLock::new(routes),
                sessions: Mutex::new(Vec::new()),
                stop: CancellationToken::new(),
                emergency: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
                delay: Mutex::new(opts.delay),
                connections: Mutex::new(Connections::default()),
                buffered: Arc::new(AtomicU64::new(0)),
                negotiate_timeout: opts.negotiate_timeout,
                lan_key: RwLock::new(opts.lan_key),
                tls: opts.tls.unwrap_or_else(upstream::default_tls),
                started: Instant::now(),
                log_dir: opts.log_dir,
                kept_delays: Mutex::default(),
                handovers: Mutex::default(),
                changed: Arc::new(watch::Sender::new(0)),
            }),
        }
    }

    /// Change counter: bumped on stream and delay changes, and by [`Relay::notify`].
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.shared.changed.subscribe()
    }

    pub fn notify(&self) {
        self.shared.changed.send_modify(|v| *v += 1);
    }

    /// Bind an RTMP listener and accept encoders until shutdown.
    pub async fn listen(&self, addr: SocketAddr) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let shared = self.shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shared.stop.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((tcp, peer)) => {
                            // Over the limit the connection is simply closed.
                            let Some(admission) = shared.admit(peer.ip()) else { continue };
                            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
                            let shared = shared.clone();
                            tokio::spawn(async move {
                                session::run(shared, tcp, peer, id).await;
                                drop(admission);
                            });
                        }
                        // e.g. out of file descriptors: back off briefly
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    },
                }
            }
        });
        Ok(local)
    }

    pub fn routes(&self) -> Vec<Route> {
        self.shared.routes.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the route table; takes effect on the next encoder connection.
    pub fn set_routes(&self, routes: Vec<Route>) {
        *self.shared.routes.write().unwrap_or_else(|e| e.into_inner()) = routes;
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.shared.lock_sessions().clone()
    }

    /// Disconnect every stream (encoder and upstream) at once, and refuse new
    /// publishes until [`Relay::clear_emergency`].
    pub fn emergency_stop(&self) {
        // Set under the sessions lock: `add_session`/`claim_route` either see
        // the flag or their session is in the list cancelled here.
        let sessions = {
            let list = self.shared.lock_sessions();
            self.shared.emergency.store(true, Ordering::SeqCst);
            list.clone()
        };
        for s in sessions {
            s.log.event("Emergency stop");
            s.token.cancel();
        }
    }

    /// Replace the LAN access key; streams already connected keep running.
    pub fn set_lan_key(&self, key: Option<String>) {
        *self.shared.lan_key.write().unwrap_or_else(|e| e.into_inner()) = key;
    }

    pub fn clear_emergency(&self) {
        self.shared.emergency.store(false, Ordering::SeqCst);
    }

    pub fn emergency_active(&self) -> bool {
        self.shared.emergency.load(Ordering::SeqCst)
    }

    /// Stop accepting encoders, wait up to `wait` for active streams to end,
    /// then disconnect whatever is left.
    pub async fn shutdown(&self, wait: Duration) {
        self.shared.stop.cancel();
        let deadline = Instant::now() + wait;
        while !self.sessions().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for s in self.sessions() {
            s.token.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.sessions().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn delay_config(&self) -> DelayConfig {
        self.shared.config()
    }

    fn update_config(&self, f: impl FnOnce(&mut DelayConfig)) {
        f(&mut self.shared.delay.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Sessions that are publishing (delay actions apply to all of them),
    /// including those still connecting or reconnecting to their platform:
    /// their engines keep buffering, and a platform that missed a delay
    /// change would show viewers more than the others.
    fn streaming(&self) -> Vec<Arc<Session>> {
        self.sessions().into_iter().filter(|s| s.is_claimed() && s.info().phase != Phase::Finishing).collect()
    }

    /// Run `f` on every streaming session's engine. Succeeds if any session
    /// accepted; otherwise returns the first refusal.
    fn apply<T>(&self, f: impl Fn(&mut Engine, Instant) -> Result<T, Refusal>) -> Result<Vec<T>, Refusal> {
        let (mut ok, mut refused) = (Vec::new(), None);
        for s in self.streaming() {
            let r =
                if s.token.is_cancelled() { Err(Refusal::TransportError) } else { f(&mut s.engine(), Instant::now()) };
            s.wake();
            match r {
                Ok(v) => ok.push(v),
                Err(e) => {
                    refused.get_or_insert(e);
                }
            }
        }
        match (ok.is_empty(), refused) {
            (false, _) => Ok(ok),
            (true, Some(e)) => Err(e),
            (true, None) => Err(Refusal::NoStream),
        }
    }

    /// Record an action in every publishing stream's session log.
    fn log_all(&self, text: &str) {
        for s in self.streaming() {
            s.log.event(text);
        }
    }

    fn check_delay(d: Duration) -> Result<(), Refusal> {
        if d.is_zero() || d > engine::MAX_DELAY {
            return Err(Refusal::InvalidValue("delay_range"));
        }
        Ok(())
    }

    /// Enable the delay with `delay` or the configured default.
    pub fn enable_delay(&self, delay: Option<Duration>) -> Result<String, Refusal> {
        let cfg = self.delay_config();
        if !cfg.mode_enabled {
            return Err(Refusal::DelayModeOff);
        }
        let d = delay.unwrap_or(cfg.delay);
        Self::check_delay(d)?;
        let results = self.apply(|e, now| e.enable(d, now))?;
        self.update_config(|c| c.delay = d);
        self.apply_retain();
        let how = if results.contains(&Enabled::Instant) {
            "instant switch"
        } else if results.contains(&Enabled::NextKeyframe) {
            "at the next keyframe"
        } else {
            "once that much is buffered"
        };
        self.log_all(&format!("Delay enabled: {} ({how})", secs(d)));
        Ok(if results.contains(&Enabled::Instant) {
            format!("Delay on: viewers are now {} behind.", secs(d))
        } else if results.contains(&Enabled::NextKeyframe) {
            format!("Delay on: viewers jump back {} at the next keyframe.", secs(d))
        } else if d > cfg.buffer || results.contains(&Enabled::Building { buffer_grown: true }) {
            format!("Buffer enlarged to {}; viewers stay live until it has filled, then jump back.", secs(d))
        } else {
            format!("Delay on: viewers stay live until {} is buffered, then jump back.", secs(d))
        })
    }

    /// Return to live.
    pub fn go_live(&self) -> Result<String, Refusal> {
        self.apply(|e, now| e.go_live(now))?;
        self.log_all("Delay disabled: jumping to the live edge");
        Ok("Going live: viewers jump to the live edge.".into())
    }

    pub fn toggle_delay(&self) -> Result<String, Refusal> {
        if self.streaming().iter().any(|s| s.engine().is_active()) { self.go_live() } else { self.enable_delay(None) }
    }

    /// Change the delay: applies to active streams and to the next activation.
    pub fn set_delay(&self, delay: Duration) -> Result<String, Refusal> {
        Self::check_delay(delay)?;
        let active: Vec<Arc<Session>> = self.streaming().into_iter().filter(|s| s.engine().is_active()).collect();
        if !active.is_empty() {
            self.apply(|e, now| if e.is_active() { e.set_delay(delay, now) } else { Ok(()) })?;
        }
        self.update_config(|c| c.delay = delay);
        self.apply_retain();
        self.log_all(&format!("Delay set to {}", secs(delay)));
        Ok(format!("Delay set to {}.", secs(delay)))
    }

    /// Resize the rolling buffer.
    pub fn set_buffer(&self, buffer: Duration) -> Result<String, Refusal> {
        if buffer < MIN_BUFFER || buffer > engine::MAX_DELAY {
            return Err(Refusal::InvalidValue("buffer_range"));
        }
        self.update_config(|c| c.buffer = buffer);
        self.apply_retain();
        self.log_all(&format!("Buffer set to {}", secs(buffer)));
        Ok(format!("Buffer set to {}.", secs(buffer)))
    }

    /// Resize every stream's buffer to the configuration (never below its delay).
    fn apply_retain(&self) {
        let retain = self.delay_config().retain();
        for s in self.streaming() {
            s.engine().set_retain(retain);
        }
    }

    /// Turn the rolling-buffer feature on or off as a whole.
    pub fn set_delay_mode(&self, on: bool) -> Result<String, Refusal> {
        let sessions = self.streaming();
        if !on && sessions.iter().any(|s| s.engine().is_active()) {
            return Err(Refusal::InvalidInMode("go_live_first"));
        }
        self.update_config(|c| c.mode_enabled = on);
        self.apply_retain();
        self.log_all(if on { "Delay mode on" } else { "Delay mode off" });
        Ok(if on { "Delay mode on." } else { "Delay mode off." }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_session;

    #[test]
    fn durations_read_like_the_ui() {
        let f = |s| secs(Duration::from_secs(s));
        assert_eq!(
            (f(45), f(60), f(70), f(600)),
            ("45 s".into(), "1 min".into(), "1 min 10 s".into(), "10 min".into())
        );
    }

    #[test]
    fn emergency_stop_wins_against_late_registration_and_claims() {
        let relay = Relay::new(vec![], RelayOptions { tls: Some(upstream::default_tls()), ..Default::default() });
        let negotiating = test_session(1, "t", [127, 0, 0, 1]);
        relay.shared.add_session(negotiating.clone()).unwrap();
        // A connection that passed `route_for` just before the stop...
        relay.emergency_stop();
        assert!(negotiating.token.is_cancelled(), "registered sessions are cancelled");
        // ...cannot register or claim afterwards.
        assert!(relay.shared.add_session(test_session(2, "t", [127, 0, 0, 1])).is_err());
        assert!(relay.shared.claim_route(&negotiating).is_err());
        relay.clear_emergency();
        assert!(relay.shared.claim_route(&negotiating).is_ok());
    }

    #[test]
    fn only_one_session_claims_a_route() {
        let relay = Relay::new(vec![], RelayOptions { tls: Some(upstream::default_tls()), ..Default::default() });
        let a = test_session(1, "t", [192, 168, 1, 2]);
        let b = test_session(2, "T/", [192, 168, 1, 3]);
        relay.shared.add_session(a.clone()).unwrap();
        relay.shared.add_session(b.clone()).unwrap();
        assert!(relay.shared.claim_route(&a).is_ok());
        assert!(relay.shared.claim_route(&b).is_err(), "same route, another computer");
        let other_route = test_session(3, "u", [192, 168, 1, 3]);
        relay.shared.add_session(other_route.clone()).unwrap();
        assert!(relay.shared.claim_route(&other_route).is_ok());
    }

    #[test]
    fn mirrors_share_payloads_outside_the_budget() {
        let relay = Relay::new(vec![], RelayOptions { tls: Some(upstream::default_tls()), ..Default::default() });
        let mirror = Route { source: "twitch".into(), ..Default::default() };
        let (mut own, mut copy) = (relay.shared.new_engine(&Route::default()), relay.shared.new_engine(&mirror));
        let frame = opendelay_rtmp::Message::new(opendelay_rtmp::msg_type::VIDEO, 1, 0, vec![0x17, 1, 0, 0, 0, 9]);
        own.push(frame.clone(), Instant::now());
        copy.push(frame, Instant::now());
        assert_eq!(relay.shared.buffered.load(Ordering::Relaxed), 6, "counted once");
    }

    #[test]
    fn connection_pools() {
        let relay = Relay::new(vec![], RelayOptions { tls: Some(upstream::default_tls()), ..Default::default() });
        let remote: IpAddr = "192.168.1.9".parse().unwrap();
        let held: Vec<Admission> = (0..MAX_PER_REMOTE_ADDRESS).map(|_| relay.shared.admit(remote).unwrap()).collect();
        assert!(relay.shared.admit(remote).is_none(), "per-address limit");
        assert!(relay.shared.admit("::1".parse().unwrap()).is_some(), "local pool unaffected");
        drop(held);
        assert!(relay.shared.admit(remote).is_some(), "places are returned");
    }
}
