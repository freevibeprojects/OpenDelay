// SPDX-License-Identifier: GPL-3.0-only
//! The relay: RTMP listener(s), route lookup, the set of live sessions,
//! graceful shutdown and emergency stop.

use crate::engine::{self, Enabled, Engine, Refusal};
use crate::routes::{Route, normalize_path};
use crate::session::{self, Phase, Session};
use crate::upstream;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_rustls::rustls::ClientConfig;
use tokio_util::sync::CancellationToken;

/// Delay settings shared by every stream (SPEC §4.1, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayConfig {
    /// Rolling-buffer feature on/off ("delay mode").
    pub mode_enabled: bool,
    /// Rolling buffer length.
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
    fn retain(&self) -> Duration {
        if self.mode_enabled { self.buffer } else { Duration::ZERO }
    }
}

pub const MIN_BUFFER: Duration = Duration::from_secs(5);

fn secs(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 && s.is_multiple_of(60) { format!("{} min", s / 60) } else { format!("{s} s") }
}

#[derive(Default)]
pub struct RelayOptions {
    pub delay: DelayConfig,
    /// Folder for per-session logs (`None`: no log files).
    pub log_dir: Option<PathBuf>,
    /// TLS client configuration for `rtmps` upstreams (default: OS trust store).
    pub tls: Option<Arc<ClientConfig>>,
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
    pub tls: Arc<ClientConfig>,
    pub started: Instant,
    pub log_dir: Option<PathBuf>,
}

impl Shared {
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn route_for(&self, path: &str) -> Result<Route, String> {
        if self.emergency.load(Ordering::SeqCst) {
            return Err("Emergency stop is active in OpenDelay; allow streaming again in the app first.".into());
        }
        if self.stop.is_cancelled() {
            return Err("OpenDelay is shutting down.".into());
        }
        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());
        match routes.iter().find(|r| normalize_path(&r.path) == path) {
            Some(r) if r.enabled => Ok(r.clone()),
            Some(r) => Err(format!("The route \"{}\" is disabled in OpenDelay.", r.name)),
            None => Err(format!("OpenDelay has no route \"/{path}\". Check the server URL in your encoder.")),
        }
    }

    pub fn new_engine(&self) -> Engine {
        Engine::new(self.config().retain())
    }

    fn config(&self) -> DelayConfig {
        *self.delay.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_session(&self, s: Arc<Session>) {
        self.lock_sessions().push(s);
    }

    pub fn remove_session(&self, id: u64) {
        self.lock_sessions().retain(|s| s.id != id);
    }

    /// Stop sessions already publishing on `path` and wait (≤ 3 s) for them to end.
    pub async fn replace_route_sessions(&self, path: &str) {
        let old: Vec<Arc<Session>> = self
            .lock_sessions()
            .iter()
            .filter(|s| normalize_path(&s.route.path) == normalize_path(path))
            .cloned()
            .collect();
        for s in &old {
            s.log.event("Replaced by a new encoder connection on the same route");
            s.token.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && self.lock_sessions().iter().any(|s| old.iter().any(|o| o.id == s.id)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
                tls: opts.tls.unwrap_or_else(upstream::default_tls),
                started: Instant::now(),
                log_dir: opts.log_dir,
            }),
        }
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
                            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
                            tokio::spawn(session::run(shared.clone(), tcp, peer, id));
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
        self.shared.emergency.store(true, Ordering::SeqCst);
        for s in self.sessions() {
            s.log.event("Emergency stop");
            s.token.cancel();
        }
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

    /// Sessions that are publishing (delay actions apply to all of them).
    fn streaming(&self) -> Vec<Arc<Session>> {
        self.sessions().into_iter().filter(|s| matches!(s.info().phase, Phase::Live | Phase::Recovering)).collect()
    }

    /// Run `f` on every streaming session's engine. Succeeds if any session
    /// accepted; otherwise returns the first refusal.
    fn apply<T>(&self, f: impl Fn(&mut Engine, Instant) -> Result<T, Refusal>) -> Result<Vec<T>, Refusal> {
        let (mut ok, mut refused) = (Vec::new(), None);
        for s in self.streaming() {
            let r = if s.token.is_cancelled() {
                Err(Refusal::TransportError)
            } else if s.info().phase == Phase::Recovering {
                Err(Refusal::UpstreamRecovering)
            } else {
                f(&mut s.engine(), Instant::now())
            };
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
            return Err(Refusal::InvalidValue("The delay must be between 1 second and 10 minutes."));
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
        if d > cfg.buffer {
            self.update_config(|c| c.buffer = d);
        }
        let how = if results.contains(&Enabled::Instant) { "instant switch" } else { "building up" };
        self.log_all(&format!("Delay enabled: {} ({how})", secs(d)));
        Ok(if results.contains(&Enabled::Instant) {
            format!("Delay on: viewers are now {} behind.", secs(d))
        } else if d > cfg.buffer || results.contains(&Enabled::Building { buffer_grown: true }) {
            format!("Buffer enlarged to {}; the delay becomes fully effective once it has filled.", secs(d))
        } else {
            format!("Delay is building up to {}.", secs(d))
        })
    }

    /// Return to live.
    pub fn go_live(&self) -> Result<String, Refusal> {
        self.apply(|e, now| e.go_live(now))?;
        self.log_all("Delay disabled: draining to the live edge");
        Ok("Going live: catching up with the live edge.".into())
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
        self.update_config(|c| {
            c.delay = delay;
            c.buffer = c.buffer.max(delay);
        });
        self.log_all(&format!("Delay set to {}", secs(delay)));
        Ok(format!("Delay set to {}.", secs(delay)))
    }

    /// Resize the rolling buffer.
    pub fn set_buffer(&self, buffer: Duration) -> Result<String, Refusal> {
        if buffer < MIN_BUFFER || buffer > engine::MAX_DELAY {
            return Err(Refusal::InvalidValue("The buffer must be between 5 seconds and 10 minutes."));
        }
        let cfg = DelayConfig { buffer, ..self.delay_config() };
        let sessions = self.streaming();
        for s in &sessions {
            s.engine().check_retain(cfg.retain())?;
        }
        for s in &sessions {
            s.engine().set_retain(cfg.retain());
        }
        self.update_config(|c| c.buffer = buffer);
        self.log_all(&format!("Buffer set to {}", secs(buffer)));
        Ok(format!("Buffer set to {}.", secs(buffer)))
    }

    /// Turn the rolling-buffer feature on or off as a whole.
    pub fn set_delay_mode(&self, on: bool) -> Result<String, Refusal> {
        let sessions = self.streaming();
        if !on && sessions.iter().any(|s| s.engine().is_active()) {
            return Err(Refusal::InvalidInMode("Go live before turning delay mode off."));
        }
        self.update_config(|c| c.mode_enabled = on);
        let retain = self.delay_config().retain();
        for s in &sessions {
            s.engine().set_retain(retain);
        }
        self.log_all(if on { "Delay mode on" } else { "Delay mode off" });
        Ok(if on { "Delay mode on." } else { "Delay mode off." }.into())
    }
}
