// SPDX-License-Identifier: GPL-3.0-only
//! Application state shared by every control surface (desktop UI, hotkeys,
//! tray, HTTP API, controller WebSocket): settings, the relay, the actions of
//! SPEC §4.4 and the status snapshot.

use crate::diag;
use crate::engine::{DelayState, EngineStatus, MAX_DELAY, Refusal};
use crate::relay::{DelayConfig, MIN_BUFFER, Relay, RelayOptions};
use crate::routes::{self, DEFAULT_ROUTE_ID, Route, normalize_path};
use crate::session::{Phase, Session};
use crate::settings::{self, ListenerMode, Settings};
use serde::{Deserialize, Serialize};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_rustls::rustls::ClientConfig;

pub const RTMP_PORT: u16 = 1935;
pub const HTTP_PORT: u16 = 3000;
/// How long a quitting app waits for streams to end (SPEC §3.4).
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

/// An action from any control surface (SPEC §4.4). JSON form:
/// `{"action": "enableDelay", "seconds": 30}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    EnableDelay {
        #[serde(default)]
        seconds: Option<f64>,
    },
    #[serde(alias = "disableDelay")]
    GoLive,
    ToggleDelay,
    SetDelay {
        seconds: f64,
    },
    SetBuffer {
        seconds: f64,
    },
    SetDelayMode {
        enabled: bool,
    },
    ToggleOverlay,
    SetOverlayVisible {
        visible: bool,
    },
    EmergencyStop,
    /// Allow publishing again after an emergency stop.
    ClearEmergencyStop,
}

/// Result of an action or API call, with the fresh status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    pub success: bool,
    pub message: String,
    /// Refusal reason code when `success` is false.
    pub code: Option<&'static str>,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelayStatus {
    pub mode_enabled: bool,
    /// `live`, `buffering`, `delayed` or `draining`.
    pub state: &'static str,
    pub current_ms: u64,
    pub target_ms: u64,
    /// Delay used by the next "enable".
    pub configured_ms: u64,
    pub buffer_fill_ms: u64,
    pub buffer_target_ms: u64,
    pub ready: bool,
    pub max_delay_ms: u64,
    pub progress: Option<f32>,
    pub degraded: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStatus {
    pub id: u64,
    pub route_id: String,
    pub route_name: String,
    pub route_path: String,
    /// `connecting`, `live`, `recovering` or `finishing`.
    pub phase: &'static str,
    pub uptime_ms: u64,
    pub bitrate_kbps: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub recovery_attempt: u32,
    pub last_error: Option<String>,
    pub delay: DelayStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerStatus {
    /// Mode in effect (changes need a restart).
    pub mode: ListenerMode,
    pub configured_mode: ListenerMode,
    pub restart_required: bool,
    pub port: u16,
    pub lan_addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub version: &'static str,
    /// Overall state: `offline`, `connecting`, `live`, `buffering`, `delayed`,
    /// `draining` or `recovering` (of the first publishing stream).
    pub state: &'static str,
    pub connected: bool,
    pub uptime_ms: u64,
    pub bitrate_kbps: u64,
    pub delay: DelayStatus,
    pub streams: Vec<StreamStatus>,
    pub overlay_visible: bool,
    pub overlay_color: String,
    pub emergency_stop: bool,
    pub listener: ListenerStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogInfo {
    pub id: String,
    pub size: u64,
}

pub struct AppOptions {
    /// Where `settings.json` lives (`None`: in-memory only).
    pub config_dir: Option<PathBuf>,
    /// App data directory; logs go to `<data_dir>/logs` (`None`: no logs).
    pub data_dir: Option<PathBuf>,
    /// TLS configuration override for `rtmps` upstreams (tests).
    pub tls: Option<Arc<ClientConfig>>,
}

impl Default for AppOptions {
    fn default() -> Self {
        AppOptions { config_dir: settings::config_dir(), data_dir: settings::data_dir(), tls: None }
    }
}

pub struct App {
    pub relay: Relay,
    settings: Mutex<Settings>,
    settings_path: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    overlay_visible: AtomicBool,
    token: String,
    active_listener: ListenerMode,
    rtmp_port: AtomicU16,
    http_port: AtomicU16,
    changed: watch::Sender<u64>,
}

fn delay_config(s: &Settings) -> DelayConfig {
    DelayConfig {
        mode_enabled: s.delay_mode_enabled,
        buffer: Duration::from_secs(s.default_buffer_secs as u64),
        delay: Duration::from_secs(s.delay_secs as u64),
    }
}

/// Routes as the relay uses them: Twitch routes follow the chosen Twitch server.
fn effective_routes(s: &Settings) -> Vec<Route> {
    s.routes
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if r.preset == "twitch" {
                r.upstream = s.twitch_server.clone();
            }
            r
        })
        .collect()
}

fn random_token() -> String {
    let mut b = [0u8; 24];
    getrandom::getrandom(&mut b).expect("operating system random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn seconds(s: f64, min: Duration, max: Duration) -> Result<Duration, Refusal> {
    let d = Duration::try_from_secs_f64(s).map_err(|_| Refusal::InvalidValue("Invalid number of seconds."))?;
    if d < min || d > max {
        return Err(Refusal::InvalidValue("The value is out of range."));
    }
    Ok(d)
}

fn state_name(s: DelayState) -> &'static str {
    match s {
        DelayState::Live => "live",
        DelayState::Buffering => "buffering",
        DelayState::Delayed => "delayed",
        DelayState::Draining => "draining",
    }
}

fn delay_status(e: &EngineStatus, cfg: &DelayConfig) -> DelayStatus {
    DelayStatus {
        mode_enabled: cfg.mode_enabled,
        state: state_name(e.state),
        current_ms: e.current_delay_ms,
        target_ms: e.target_delay_ms,
        configured_ms: cfg.delay.as_millis() as u64,
        buffer_fill_ms: e.buffer_fill_ms,
        buffer_target_ms: e.buffer_target_ms,
        ready: e.ready,
        max_delay_ms: e.max_delay_ms,
        progress: e.progress,
        degraded: e.degraded.clone(),
    }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Connecting => "connecting",
        Phase::Live => "live",
        Phase::Recovering => "recovering",
        Phase::Finishing => "finishing",
    }
}

impl App {
    pub fn new(opts: AppOptions) -> Arc<App> {
        let settings_path = opts.config_dir.map(|d| d.join("settings.json"));
        let s = settings_path.as_deref().map(Settings::load).unwrap_or_default();
        let log_dir = opts.data_dir.map(|d| d.join("logs"));
        let relay = Relay::new(
            effective_routes(&s),
            RelayOptions { delay: delay_config(&s), log_dir: log_dir.clone(), tls: opts.tls },
        );
        Arc::new(App {
            relay,
            active_listener: s.listener_mode,
            settings: Mutex::new(s),
            settings_path,
            log_dir,
            overlay_visible: AtomicBool::new(true),
            token: random_token(),
            rtmp_port: AtomicU16::new(RTMP_PORT),
            http_port: AtomicU16::new(HTTP_PORT),
            changed: watch::Sender::new(0),
        })
    }

    /// Per-run API token for state-changing HTTP requests.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn http_port(&self) -> u16 {
        self.http_port.load(Ordering::Relaxed)
    }

    pub fn rtmp_port(&self) -> u16 {
        self.rtmp_port.load(Ordering::Relaxed)
    }

    /// Change counter, bumped after every action or settings change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify(&self) {
        self.changed.send_modify(|v| *v += 1);
    }

    /// Bind the RTMP listener(s) for the listener mode in effect.
    pub async fn start_rtmp(&self, port: u16) -> io::Result<Vec<SocketAddr>> {
        let mut bound = Vec::new();
        let primary: IpAddr = match self.active_listener {
            ListenerMode::Local => Ipv4Addr::LOCALHOST.into(),
            ListenerMode::Lan => Ipv4Addr::UNSPECIFIED.into(),
        };
        let first = self.relay.listen(SocketAddr::new(primary, port)).await?;
        self.rtmp_port.store(first.port(), Ordering::Relaxed);
        bound.push(first);
        if self.active_listener == ListenerMode::Local {
            // Encoders configured with "localhost" may resolve to ::1.
            if let Ok(a) = self.relay.listen(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), first.port())).await {
                bound.push(a);
            }
        }
        Ok(bound)
    }

    /// Serve the HTTP API, OBS pages and controller WebSocket on loopback.
    pub async fn start_http(self: &Arc<Self>, port: u16) -> io::Result<SocketAddr> {
        let addr = crate::api::serve(self.clone(), SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)).await?;
        self.http_port.store(addr.port(), Ordering::Relaxed);
        Ok(addr)
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Edit settings atomically; persists and notifies on success.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings) -> Result<(), String>) -> Result<(), String> {
        let mut guard = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        f(&mut next)?;
        if let Some(path) = &self.settings_path {
            next.save(path).map_err(|e| format!("Could not save settings: {e}"))?;
        }
        self.relay.set_routes(effective_routes(&next));
        *guard = next;
        drop(guard);
        self.notify();
        Ok(())
    }

    /// Perform an action (SPEC §4.4).
    pub fn act(&self, action: &Action) -> Reply {
        let relay = &self.relay;
        let result = match *action {
            Action::EnableDelay { seconds: s } => match s.map(|s| seconds(s, Duration::from_secs(1), MAX_DELAY)) {
                Some(Err(e)) => Err(e),
                Some(Ok(d)) => relay.enable_delay(Some(d)),
                None => relay.enable_delay(None),
            },
            Action::GoLive => relay.go_live(),
            Action::ToggleDelay => relay.toggle_delay(),
            Action::SetDelay { seconds: s } => {
                seconds(s, Duration::from_secs(1), MAX_DELAY).and_then(|d| relay.set_delay(d))
            }
            Action::SetBuffer { seconds: s } => seconds(s, MIN_BUFFER, MAX_DELAY).and_then(|d| relay.set_buffer(d)),
            Action::SetDelayMode { enabled } => relay.set_delay_mode(enabled),
            Action::ToggleOverlay => {
                let v = !self.overlay_visible.fetch_xor(true, Ordering::SeqCst);
                Ok(if v { "Overlay shown." } else { "Overlay hidden." }.into())
            }
            Action::SetOverlayVisible { visible } => {
                self.overlay_visible.store(visible, Ordering::SeqCst);
                Ok(if visible { "Overlay shown." } else { "Overlay hidden." }.into())
            }
            Action::EmergencyStop => {
                relay.emergency_stop();
                Ok("Emergency stop: every stream was disconnected. Publishing stays blocked until you allow it again."
                    .into())
            }
            Action::ClearEmergencyStop => {
                relay.clear_emergency();
                Ok("Streaming allowed again.".into())
            }
        };
        if result.is_ok() {
            self.persist_delay_config();
        }
        self.notify();
        self.reply(result.map_err(|r| (r.to_string(), r.code())))
    }

    /// Save delay settings changed by an action (SPEC §4.6).
    fn persist_delay_config(&self) {
        let cfg = self.relay.delay_config();
        let s = self.settings();
        let (buffer, delay) = (cfg.buffer.as_secs() as u32, cfg.delay.as_secs() as u32);
        if (s.delay_mode_enabled, s.default_buffer_secs, s.delay_secs) != (cfg.mode_enabled, buffer, delay) {
            let _ = self.update_settings(|s| {
                s.delay_mode_enabled = cfg.mode_enabled;
                s.default_buffer_secs = buffer;
                s.delay_secs = delay;
                Ok(())
            });
        }
    }

    pub fn reply(&self, result: Result<String, (String, &'static str)>) -> Reply {
        let (success, message, code) = match result {
            Ok(m) => (true, m, None),
            Err((m, c)) => (false, m, Some(c)),
        };
        Reply { success, message, code, status: self.status() }
    }

    pub fn status(&self) -> Status {
        let now = Instant::now();
        let cfg = self.relay.delay_config();
        let settings = self.settings();
        let sessions = self.relay.sessions();
        let streams: Vec<StreamStatus> = sessions.iter().map(|s| self.stream_status(s, now, &cfg)).collect();
        let primary = streams.iter().find(|s| matches!(s.phase, "live" | "recovering")).or(streams.first());
        let state = match primary {
            None => "offline",
            Some(p) if p.phase == "live" || p.phase == "finishing" => p.delay.state,
            Some(p) => p.phase,
        };
        let idle_delay = DelayStatus {
            mode_enabled: cfg.mode_enabled,
            state: "live",
            current_ms: 0,
            target_ms: 0,
            configured_ms: cfg.delay.as_millis() as u64,
            buffer_fill_ms: 0,
            buffer_target_ms: if cfg.mode_enabled { cfg.buffer.as_millis() as u64 } else { 0 },
            ready: false,
            max_delay_ms: 0,
            progress: None,
            degraded: None,
        };
        Status {
            version: env!("CARGO_PKG_VERSION"),
            state,
            connected: primary.is_some(),
            uptime_ms: primary.map_or(0, |p| p.uptime_ms),
            bitrate_kbps: primary.map_or(0, |p| p.bitrate_kbps),
            delay: primary.map_or(idle_delay, |p| p.delay.clone()),
            streams,
            overlay_visible: self.overlay_visible.load(Ordering::SeqCst),
            overlay_color: settings.overlay_color.clone(),
            emergency_stop: self.relay.emergency_active(),
            listener: ListenerStatus {
                mode: self.active_listener,
                configured_mode: settings.listener_mode,
                restart_required: settings.listener_mode != self.active_listener,
                port: self.rtmp_port(),
                lan_addresses: lan_addresses(),
            },
        }
    }

    fn stream_status(&self, s: &Session, now: Instant, cfg: &DelayConfig) -> StreamStatus {
        let info = s.info();
        StreamStatus {
            id: s.id,
            route_id: s.route.id.clone(),
            route_name: s.route.name.clone(),
            route_path: s.route.path.clone(),
            phase: phase_name(info.phase),
            uptime_ms: now.duration_since(s.started).as_millis() as u64,
            bitrate_kbps: s.bitrate_kbps(),
            bytes_in: s.bytes_in.load(Ordering::Relaxed),
            bytes_out: s.bytes_out.load(Ordering::Relaxed),
            recovery_attempt: info.recovery_attempt,
            last_error: info.last_error,
            delay: delay_status(&s.engine().status(now, cfg.delay), cfg),
        }
    }

    // ---- routes (SPEC §6) ----

    pub fn routes(&self) -> Vec<Route> {
        self.settings().routes
    }

    fn streaming_route(&self, id: &str) -> bool {
        self.relay.sessions().iter().any(|s| s.route.id == id)
    }

    pub fn add_route(&self, mut route: Route) -> Result<Route, String> {
        route.path = normalize_path(&route.path);
        route.id = format!("r{}", diag::short_id());
        self.save_route(None, route)
    }

    pub fn update_route(&self, id: &str, mut route: Route) -> Result<Route, String> {
        if self.streaming_route(id) {
            return Err("This route is streaming. Stop the stream in your encoder before changing it.".into());
        }
        route.id = id.to_owned();
        route.path = normalize_path(&route.path);
        self.save_route(Some(id), route)
    }

    fn save_route(&self, existing: Option<&str>, route: Route) -> Result<Route, String> {
        let saved = route.clone();
        self.update_settings(|s| {
            let mut candidate = route.clone();
            if candidate.preset == "twitch" {
                // A Twitch route's server is the Twitch server setting.
                if candidate.upstream.trim().is_empty() {
                    candidate.upstream = s.twitch_server.clone();
                }
                routes::validate_twitch_server(&candidate.upstream)?;
                s.twitch_server = candidate.upstream.clone();
            }
            routes::validate(&candidate, &s.routes)?;
            match existing {
                Some(id) => {
                    let slot = s.routes.iter_mut().find(|r| r.id == id).ok_or("No such route.")?;
                    *slot = candidate;
                }
                None => s.routes.push(candidate),
            }
            Ok(())
        })?;
        Ok(saved)
    }

    pub fn delete_route(&self, id: &str) -> Result<(), String> {
        if id == DEFAULT_ROUTE_ID {
            return Err("The default Twitch route can be edited or disabled, but not deleted.".into());
        }
        if self.streaming_route(id) {
            return Err("This route is streaming. Stop the stream in your encoder before deleting it.".into());
        }
        self.update_settings(|s| {
            let before = s.routes.len();
            s.routes.retain(|r| r.id != id);
            if s.routes.len() == before { Err("No such route.".into()) } else { Ok(()) }
        })
    }

    // ---- network and Twitch server ----

    pub fn set_listener_mode(&self, mode: ListenerMode) -> Result<String, String> {
        self.update_settings(|s| {
            s.listener_mode = mode;
            Ok(())
        })?;
        Ok(if mode == self.active_listener {
            "Listener mode unchanged.".into()
        } else {
            "Listener mode saved. Restart OpenDelay for it to take effect.".into()
        })
    }

    pub fn set_twitch_server(&self, url: &str) -> Result<(), String> {
        routes::validate_twitch_server(url)?;
        self.update_settings(|s| {
            s.twitch_server = url.trim().to_owned();
            Ok(())
        })
    }

    // ---- diagnostics (SPEC §9) ----

    pub fn log_dir(&self) -> Option<&PathBuf> {
        self.log_dir.as_ref()
    }

    pub fn diagnostics(&self) -> Vec<LogInfo> {
        let Some(dir) = &self.log_dir else { return Vec::new() };
        diag::list_logs(dir)
            .into_iter()
            .filter_map(|p| {
                let id = p.file_stem()?.to_str()?.to_owned();
                Some(LogInfo { size: p.metadata().map_or(0, |m| m.len()), id })
            })
            .collect()
    }

    /// Contents of one session log; `id` is validated (no path traversal).
    pub fn diagnostic(&self, id: &str) -> Option<String> {
        if !diag::valid_log_id(id) {
            return None;
        }
        std::fs::read_to_string(self.log_dir.as_ref()?.join(format!("{id}.log"))).ok()
    }

    /// Stop accepting encoders and wait (bounded) for streams to end.
    pub async fn shutdown(&self) {
        self.relay.shutdown(SHUTDOWN_WAIT).await;
    }
}

/// This machine's IPv4 LAN addresses (shown in LAN listener mode).
pub fn lan_addresses() -> Vec<String> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if !v4.is_link_local() => Some(v4.to_string()),
            _ => None,
        })
        .collect()
}
