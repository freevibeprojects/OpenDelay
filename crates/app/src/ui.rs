// SPDX-License-Identifier: GPL-3.0-only
//! Desktop UI glue: pushes status and settings into the Slint `AppState`
//! global and turns its callbacks into core actions. Status is polled every
//! second, within 100 ms of a change (a stream connects or ends, the delay
//! jumps), and every 100 ms for a few seconds after a command or while a
//! transition runs.

use crate::hotkeys::{self, Binder, Which};
use crate::{AppState, Choice, Finding, LogItem, MainWindow, RouteItem, StreamItem, Strings, Theme, Tray};
use crate::{autostart, platform};
use opendelay_core::app::{Action, App, Reply, Status};
use opendelay_core::net::{self, TwitchIngest};
use opendelay_core::routes::{PRESETS, Problem, Route, TWITCH_AUTO};
use opendelay_core::settings::{ListenerMode, Settings};
use opendelay_core::update::{self, UpdateInfo};
use opendelay_core::url::UpstreamUrl;
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

include!(concat!(env!("OUT_DIR"), "/languages.rs"));

const FAST_POLL_FOR: Duration = Duration::from_secs(3);

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn mbps(kbps: u64) -> String {
    format!("{:.1} Mb/s", kbps as f64 / 1000.0)
}

fn kilobytes(bytes: u64) -> String {
    format!("{} KB", bytes.div_ceil(1024))
}

/// `20261014-170509-3` → `2026-10-14 17:05:09 UTC · #3`
fn log_label(id: &str) -> String {
    let b = id.as_bytes();
    if b.len() < 16 {
        return id.into();
    }
    let s = |r: std::ops::Range<usize>| &id[r];
    format!("{}-{}-{} {}:{}:{} UTC · #{}", s(0..4), s(4..6), s(6..8), s(9..11), s(11..13), s(13..15), &id[16..])
}

/// What the UI last showed, so models are only replaced when they change
/// (replacing a model re-creates its rows and loses keyboard focus).
#[derive(Default)]
struct Shown {
    routes: Vec<RouteItem>,
    findings: Vec<Finding>,
    streams: Vec<StreamItem>,
    settings: Option<Settings>,
}

pub struct Ui {
    window: MainWindow,
    /// `None` when the platform has no tray (closing the window then quits).
    tray: Option<Tray>,
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    shown: RefCell<Shown>,
    fast_until: Cell<Instant>,
    hotkeys: RefCell<Binder>,
    twitch: RefCell<Vec<TwitchIngest>>,
    update: RefCell<Option<UpdateInfo>>,
}

impl Ui {
    fn state(&self) -> AppState<'_> {
        self.window.global::<AppState>()
    }

    fn strings(&self) -> Strings<'_> {
        self.window.global::<Strings>()
    }

    /// Translated text for a core problem.
    fn problem(&self, p: &Problem) -> SharedString {
        self.strings().invoke_problem(ss(p.code), ss(if p.detail.is_empty() { &p.message } else { &p.detail }))
    }

    pub fn toast_problem(&self, code: &str, detail: &str) {
        let text = self.strings().invoke_problem(ss(code), ss(detail));
        self.state().invoke_toast(text, ss("error"));
    }

    fn poll_fast(&self) {
        self.fast_until.set(Instant::now() + FAST_POLL_FOR);
    }

    /// Run an action and toast its outcome.
    fn act(&self, action: Action, name: &str) -> Reply {
        let reply = self.app.act(&action);
        let building = reply.status.state == "buffering" && reply.status.delay.progress.is_some_and(|p| p < 1.0);
        let name = if name == "enable" && reply.success && building { "enable-building" } else { name };
        let detail = reply.detail.map_or_else(|| ss(&reply.message), ss);
        self.state().invoke_report(ss(name), reply.success, ss(reply.code.unwrap_or("")), detail);
        self.poll_fast();
        self.refresh_status(&reply.status, &self.app.settings());
        reply
    }

    fn refresh(&self) {
        let settings = self.app.settings();
        self.refresh_status(&self.app.status(), &settings);
        if self.shown.borrow().settings.as_ref() != Some(&settings) {
            self.apply_settings(&settings);
            self.shown.borrow_mut().settings = Some(settings);
        }
    }

    fn refresh_status(&self, st: &Status, settings: &Settings) {
        let s = self.state();
        let cfg = self.app.relay.delay_config();
        s.set_state(ss(st.state));
        let active = delay_active(st);
        s.set_delay_active(active);
        if let Some(t) = &self.tray {
            t.set_delay_active(active);
        }
        // rounded: a switch lands up to half a second past the delay set
        s.set_current_delay_s(((st.delay.current_ms + 500) / 1000) as i32);
        s.set_target_delay_s((st.delay.target_ms / 1000) as i32);
        s.set_configured_delay_s((st.delay.configured_ms / 1000) as i32);
        s.set_buffer_s(cfg.buffer.as_secs() as i32);
        let target = st.delay.buffer_target_ms.max(1);
        let fill = st.delay.buffer_fill_ms.min(target);
        s.set_buffer_fill(fill as f32 / target as f32);
        s.set_buffer_text(if st.connected && cfg.mode_enabled {
            ss(format!("{} / {} s", fill / 1000, target / 1000))
        } else {
            ss("—")
        });
        s.set_ready(st.delay.ready);
        s.set_progress(st.delay.progress.unwrap_or(-1.0));
        s.set_buffer_limit_s(st.delay.buffer_limit_ms.map_or(-1, |m| (m / 1000) as i32));
        s.set_bitrate_text(ss(if st.connected { mbps(st.bitrate_kbps) } else { "—".into() }));
        s.set_uptime_text(ss(if st.connected { hms(st.uptime_ms) } else { "—".into() }));
        let primary = st.streams.iter().find(|x| x.phase == "recovering").or(st.streams.first());
        s.set_recovery_attempt(primary.map_or(0, |p| p.recovery_attempt as i32));
        s.set_last_error(ss(primary.and_then(|p| p.last_error.clone()).unwrap_or_default()));
        s.set_delay_mode(cfg.mode_enabled);
        s.set_emergency(st.emergency_stop);
        s.set_encoder_refused(ss(st.encoder_refused.as_deref().unwrap_or_default()));
        s.set_overlay_visible(st.overlay_visible);
        s.set_overlay_color(ss(&st.overlay_color));
        s.set_listener_mode(ss(if st.listener.configured_mode == ListenerMode::Lan { "lan" } else { "local" }));
        s.set_restart_required(st.listener.restart_required);
        s.set_lan_addresses(ss(st.listener.lan_addresses.join(", ")));

        let health = st.health.clone().unwrap_or_default();
        let findings: Vec<Finding> = health
            .findings
            .iter()
            .map(|f| Finding {
                severity: ss(format!("{:?}", f.severity).to_lowercase()),
                code: ss(f.code),
                value: ss(&f.value),
                limit: ss(&f.limit),
            })
            .collect();
        if self.shown.borrow().findings != findings {
            s.set_findings(model(findings.clone()));
            self.shown.borrow_mut().findings = findings;
        }
        s.set_health_codec(ss(health.codec.clone().unwrap_or_default()));
        s.set_health_resolution(ss(match (health.width, health.height) {
            (Some(w), Some(h)) => format!("{w}×{h}"),
            _ => String::new(),
        }));
        s.set_health_gop(ss(health
            .keyframe_interval_ms
            .map_or(String::new(), |ms| format!("{:.1} s", ms as f64 / 1000.0))));
        s.set_health_b_frames(health.b_frames);
        s.set_health_frames(health.frames.min(i32::MAX as u64) as i32);

        let streams: Vec<StreamItem> = st
            .streams
            .iter()
            .map(|x| StreamItem {
                route: ss(&x.route_name),
                phase: ss(if x.phase == "live" { x.delay.state } else { x.phase }),
                bitrate: ss(mbps(x.bitrate_kbps)),
                uptime: ss(hms(x.uptime_ms)),
                delay: ss(format!("{} s", x.delay.current_ms / 1000)),
                error: ss(x.last_error.clone().unwrap_or_default()),
            })
            .collect();
        if self.shown.borrow().streams != streams {
            s.set_streams(model(streams.clone()));
            self.shown.borrow_mut().streams = streams;
        }

        let port = self.app.rtmp_port();
        let routes: Vec<RouteItem> = settings
            .routes
            .iter()
            .map(|r| {
                let upstream = if r.preset == "twitch" { settings.twitch_server.clone() } else { r.upstream.clone() };
                RouteItem {
                    host: ss(UpstreamUrl::parse(&upstream).map(|u| u.host).unwrap_or_default()),
                    id: ss(&r.id),
                    name: ss(&r.name),
                    preset: ss(&r.preset),
                    path: ss(&r.path),
                    upstream: ss(upstream),
                    enabled: r.enabled,
                    streaming: st.streams.iter().any(|x| x.route_id == r.id),
                    local_url: ss(format!("rtmp://127.0.0.1:{port}/{}", r.path)),
                    source: ss(&r.source),
                    source_name: ss(settings.routes.iter().find(|s| s.id == r.source).map_or("", |s| &s.name)),
                    key: SharedString::default(),
                    has_key: !r.key.is_empty(),
                }
            })
            .collect();
        if self.shown.borrow().routes != routes {
            let first = routes.iter().find(|r| r.enabled && r.source.is_empty()).or(routes.first());
            s.set_encoder_url(first.map_or_else(SharedString::default, |r| r.local_url.clone()));
            let sources = routes.iter().filter(|r| r.source.is_empty());
            s.set_route_sources(model(
                sources.map(|r| Choice { value: r.id.clone(), label: r.name.clone() }).collect(),
            ));
            s.set_routes(model(routes.clone()));
            self.shown.borrow_mut().routes = routes;
        }
    }

    fn apply_settings(&self, st: &Settings) {
        let s = self.state();
        s.set_start_with_os(st.start_with_os);
        s.set_quit_on_close(st.quit_on_close);
        s.set_update_check(st.update_check);
        s.set_reduce_motion(st.reduce_motion);
        self.window.global::<Theme>().set_reduce_motion(st.reduce_motion);
        s.set_overlay_color(ss(&st.overlay_color));
        s.set_custom_delay_s(st.hotkeys.custom_delay_secs as i32);
        s.set_hk_enable(ss(hotkeys::display(&st.hotkeys.enable_delay)));
        s.set_hk_go_live(ss(hotkeys::display(&st.hotkeys.go_live)));
        s.set_hk_overlay(ss(hotkeys::display(&st.hotkeys.toggle_overlay)));
        s.set_hk_custom(ss(hotkeys::display(&st.hotkeys.custom_delay)));

        let system = self.strings().invoke_system_default();
        let mut langs = vec![Choice { value: ss(""), label: system.clone() }];
        langs.extend(LANGUAGES.iter().map(|(c, n)| Choice { value: ss(c), label: ss(n) }));
        let current = langs.iter().find(|c| c.value == st.language).map_or(system, |c| c.label.clone());
        s.set_language(ss(&st.language));
        s.set_language_label(current);
        s.set_languages(model(langs));

        let auto = self.strings().invoke_twitch_auto();
        let mut servers = vec![Choice { value: ss(TWITCH_AUTO), label: auto.clone() }];
        servers.extend(self.twitch.borrow().iter().map(|i| Choice { value: ss(&i.url), label: ss(&i.name) }));
        if !servers.iter().any(|c| c.value == st.twitch_server) {
            let host = UpstreamUrl::parse(&st.twitch_server).map(|u| u.host).unwrap_or_default();
            servers.push(Choice { value: ss(&st.twitch_server), label: ss(host) });
        }
        let label = servers.iter().find(|c| c.value == st.twitch_server).map_or(auto, |c| c.label.clone());
        s.set_twitch_server(ss(&st.twitch_server));
        s.set_twitch_server_label(label);
        s.set_twitch_servers(model(servers));
    }

    fn refresh_logs(&self) {
        let logs: Vec<LogItem> = self
            .app
            .diagnostics()
            .into_iter()
            .map(|l| LogItem { label: ss(log_label(&l.id)), size: ss(kilobytes(l.size)), id: ss(l.id) })
            .collect();
        self.state().set_logs(model(logs));
    }

    /// Persist a settings change; returns translated error text ("" = ok).
    fn save(&self, f: impl FnOnce(&mut Settings)) -> SharedString {
        match self.app.update_settings(|s| {
            f(s);
            Ok(())
        }) {
            Ok(()) => {
                self.refresh();
                SharedString::default()
            }
            Err(p) => self.problem(&p),
        }
    }

    fn save_route(&self, item: RouteItem) -> SharedString {
        let route = Route {
            id: item.id.to_string(),
            name: item.name.trim().to_owned(),
            preset: item.preset.to_string(),
            path: item.path.to_string(),
            upstream: item.upstream.trim().to_owned(),
            enabled: item.enabled,
            source: item.source.to_string(),
            key: item.key.trim().to_owned(),
        };
        let result = if route.id.is_empty() {
            self.app.add_route(route)
        } else {
            self.app.update_route(&route.id.clone(), route)
        };
        match result {
            Ok(_) => {
                self.refresh();
                SharedString::default()
            }
            Err(p) => self.problem(&p),
        }
    }

    fn set_language(&self, code: &str) {
        let effective = if code.is_empty() { platform::system_language(LANGUAGES) } else { code.to_owned() };
        let _ = slint::select_bundled_translation(&effective);
    }
}

impl Ui {
    /// Quit: at once when nothing streams, otherwise refuse new encoders and
    /// wait (bounded) for the streams to end (SPEC §3.4).
    fn quit(self: &Rc<Self>) {
        if self.app.relay.sessions().is_empty() {
            let _ = slint::quit_event_loop();
            return;
        }
        let _ = self.window.show();
        self.state().set_quitting(true);
        let app = self.app.clone();
        let done = self.rt.spawn(async move { app.shutdown().await });
        let _ = slint::spawn_local(async move {
            let _ = done.await;
            let _ = slint::quit_event_loop();
        });
    }

    fn quit_now(&self) {
        self.app.relay.emergency_stop();
        let _ = slint::quit_event_loop();
    }

    fn show_window(&self) {
        let _ = self.window.show();
        self.window.window().set_minimized(false);
        self.refresh();
    }

    /// Hidden in the tray or minimized: only the tray icon shows the state.
    fn refresh_tray(&self) {
        if let Some(t) = &self.tray {
            t.set_delay_active(delay_active(&self.app.status()));
        }
    }

    /// `user`: the refresh button (bypass the cache, report the outcome).
    fn fetch_twitch_servers(self: &Rc<Self>, user: bool) {
        let announce = user;
        let job = self.rt.spawn_blocking(move || net::twitch_ingests(user));
        let ui = self.clone();
        let _ = slint::spawn_local(async move {
            match job.await.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(list) => {
                    *ui.twitch.borrow_mut() = list;
                    ui.shown.borrow_mut().settings = None;
                    ui.refresh();
                    if announce {
                        ui.state().invoke_report(ss("twitch-list"), true, ss(""), ss(""));
                    }
                }
                Err(e) if announce => ui.toast_problem("network", &e),
                Err(_) => {}
            }
        });
    }

    fn set_update_status(&self, kind: &str, value: &str) {
        self.state().set_update_status(self.strings().invoke_update_status(ss(kind), ss(value)));
    }

    fn check_update(self: &Rc<Self>) {
        self.set_update_status("checking", "");
        let job = self.rt.spawn_blocking(|| update::check(env!("CARGO_PKG_VERSION")));
        let ui = self.clone();
        let _ = slint::spawn_local(async move {
            match job.await.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(Some(info)) => {
                    ui.set_update_status("available", &info.version);
                    ui.state().set_update_available(true);
                    *ui.update.borrow_mut() = Some(info);
                }
                Ok(None) => ui.set_update_status("none", ""),
                Err(e) => ui.set_update_status("error", &e),
            }
        });
    }

    /// Only on the user's request: download, verify the signature, keep the file.
    fn download_update(self: &Rc<Self>) {
        let Some(info) = self.update.borrow().clone() else { return };
        let Some(dir) = self.app.log_dir().and_then(|d| d.parent()).map(|d| d.join("updates")) else { return };
        self.set_update_status("downloading", "");
        let job = self.rt.spawn_blocking(move || update::download(&info, &dir, env!("CARGO_PKG_VERSION")));
        let ui = self.clone();
        let _ = slint::spawn_local(async move {
            match job.await.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(path) => {
                    ui.set_update_status("saved", &path.to_string_lossy());
                    if let Some(folder) = path.parent() {
                        platform::open(&folder.to_string_lossy());
                    }
                }
                Err(e) => ui.set_update_status("failed", &e),
            }
        });
    }

    fn apply_hotkeys(&self) {
        let failed = self.hotkeys.borrow_mut().apply(&self.app.settings().hotkeys);
        if !failed.is_empty() {
            self.toast_problem("hotkey_taken", "");
        }
    }

    fn capture_hotkey(&self, which: &str, text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> SharedString {
        let Some(which) = Which::parse(which) else { return SharedString::default() };
        let combo = match hotkeys::from_key(text, ctrl, alt, shift, meta)
            .and_then(|c| self.hotkeys.borrow().try_register(&c, which).map(|_| c))
        {
            Ok(c) => c,
            Err(code) => return self.strings().invoke_problem(ss(code), ss("")),
        };
        let err = self.save(|s| *which.slot(&mut s.hotkeys) = combo);
        if err.is_empty() {
            self.apply_hotkeys();
        }
        err
    }

    fn on_hotkey(&self, which: Which) {
        match which {
            Which::Enable => self.act(Action::EnableDelay { seconds: None }, "enable"),
            Which::GoLive => self.act(Action::GoLive, "golive"),
            Which::Overlay => {
                let visible = !self.app.status().overlay_visible;
                self.act(Action::SetOverlayVisible { visible }, if visible { "overlay-on" } else { "overlay-off" })
            }
            Which::Custom => {
                let secs = self.app.settings().hotkeys.custom_delay_secs as f64;
                self.act(Action::EnableDelay { seconds: Some(secs) }, "enable")
            }
        };
    }
}

fn open_url(key: &str) {
    // Release builds can point the links at the published project.
    let base = option_env!("OPENDELAY_REPOSITORY").unwrap_or(env!("CARGO_PKG_REPOSITORY"));
    let url = match key {
        "docs" => format!("{base}#readme"),
        "protocol" => format!("{base}/blob/main/docs/controller-protocol.md"),
        "issues" => format!("{base}/issues"),
        _ => base.to_owned(),
    };
    platform::open(&url);
}

/// Build the window, wire every callback and start polling.
pub fn create(app: Arc<App>, rt: tokio::runtime::Handle) -> Result<Rc<Ui>, slint::PlatformError> {
    let window = MainWindow::new()?;
    let tray = if platform::tray_host_available() { Tray::new().ok() } else { None };
    let ui = Rc::new(Ui {
        window,
        tray,
        app,
        rt,
        shown: RefCell::default(),
        fast_until: Cell::new(Instant::now()),
        hotkeys: RefCell::new(Binder::new()),
        twitch: RefCell::default(),
        update: RefCell::default(),
    });
    let settings = ui.app.settings();
    ui.set_language(&settings.language);

    let s = ui.state();
    s.set_version(ss(env!("CARGO_PKG_VERSION")));
    s.set_lan_key(ss(&settings.lan_key));
    let http = ui.app.http_port();
    s.set_overlay_url(ss(format!("http://127.0.0.1:{http}/overlay")));
    s.set_dock_url(ss(format!("http://127.0.0.1:{http}/dock")));
    s.set_rtmp_port(ui.app.rtmp_port().into());
    s.set_ws_url(ss(format!("ws://127.0.0.1:{http}/ws")));
    s.set_api_url(ss(format!("http://127.0.0.1:{http}/api/status")));
    s.set_presets(model(
        PRESETS
            .iter()
            .map(|p| RouteItem {
                name: ss(p.name),
                preset: ss(p.id),
                path: ss(p.path),
                upstream: ss(p.upstream),
                enabled: true,
                ..Default::default()
            })
            .collect(),
    ));
    if !ui.hotkeys.borrow().available() {
        s.set_hotkeys_note(ui.strings().invoke_problem(ss("hotkey_unsupported"), ss("")));
    } else if ui.hotkeys.borrow().uses_portal() {
        s.set_hotkeys_note(ui.strings().invoke_problem(ss("hotkey_portal"), ss("")));
    } else if hotkeys::wayland() {
        s.set_hotkeys_note(ui.strings().invoke_problem(ss("hotkey_wayland"), ss("")));
    }
    ui.apply_hotkeys();
    if settings.start_with_os {
        // keep the entry pointing at this executable
        let _ = autostart::set(true);
    }
    ui.refresh();
    ui.refresh_logs();
    if settings.update_check {
        ui.check_update();
    }

    // A weak handle per callback keeps the Ui free of reference cycles.
    macro_rules! with_ui {
        (|$ui:ident $(, $arg:ident)*| $body:expr) => {{
            let weak = Rc::downgrade(&ui);
            move |$($arg),*| {
                let $ui = weak.upgrade().expect("UI alive while its callbacks run");
                $body
            }
        }};
    }

    s.on_enable_delay(with_ui!(|u| {
        u.act(Action::EnableDelay { seconds: None }, "enable");
    }));
    s.on_go_live(with_ui!(|u| {
        u.act(Action::GoLive, "golive");
    }));
    s.on_set_delay(with_ui!(|u, secs| {
        u.act(Action::SetDelay { seconds: secs as f64 }, "setdelay");
    }));
    s.on_set_buffer(with_ui!(|u, secs| {
        u.act(Action::SetBuffer { seconds: secs as f64 }, "setbuffer");
    }));
    s.on_set_delay_mode(with_ui!(|u, on| {
        u.act(Action::SetDelayMode { enabled: on }, if on { "mode-on" } else { "mode-off" });
    }));
    s.on_toggle_overlay(with_ui!(|u| {
        let visible = !u.app.status().overlay_visible;
        u.act(Action::SetOverlayVisible { visible }, if visible { "overlay-on" } else { "overlay-off" });
    }));
    s.on_clear_emergency(with_ui!(|u| {
        u.act(Action::ClearEmergencyStop, "allow");
    }));
    s.on_set_overlay_color(with_ui!(|u, name| {
        u.save(|s| s.overlay_color = name.to_string());
    }));
    s.on_save_route(with_ui!(|u, item| u.save_route(item)));
    s.on_route_name(with_ui!(|u, id| {
        ss(u.app.routes().into_iter().find(|r| r.id == id.as_str()).map_or(String::new(), |r| r.name))
    }));
    s.on_delete_route(with_ui!(|u, id| match u.app.delete_route(&id) {
        Ok(()) => {
            u.refresh();
            SharedString::default()
        }
        Err(p) => u.problem(&p),
    }));
    s.on_set_route_enabled(with_ui!(|u, id, on| {
        let Some(mut r) = u.app.routes().into_iter().find(|r| r.id == id.as_str()) else {
            return SharedString::default();
        };
        r.enabled = on;
        let result = u.app.update_route(&id, r);
        u.refresh();
        result.err().map_or_else(SharedString::default, |p| u.problem(&p))
    }));
    s.on_set_quit_on_close(with_ui!(|u, on| {
        u.save(|s| s.quit_on_close = on);
    }));
    s.on_set_language(with_ui!(|u, code| {
        u.save(|s| s.language = code.to_string());
        u.set_language(&code);
        // Re-apply translated labels built on the Rust side.
        u.shown.borrow_mut().settings = None;
        u.refresh();
    }));
    s.on_set_update_check(with_ui!(|u, on| {
        u.save(|s| s.update_check = on);
    }));
    s.on_set_reduce_motion(with_ui!(|u, on| {
        u.save(|s| s.reduce_motion = on);
    }));
    s.on_set_listener_mode(with_ui!(|u, mode| {
        let mode = if mode == "lan" { ListenerMode::Lan } else { ListenerMode::Local };
        match u.app.set_listener_mode(mode) {
            Ok(_) => u.state().invoke_report(ss("listener"), true, ss(""), ss("")),
            Err(p) => u.state().invoke_toast(u.problem(&p), ss("error")),
        }
        u.refresh();
    }));
    s.on_new_lan_key(with_ui!(|u| {
        match u.app.new_lan_key() {
            Ok(key) => {
                u.state().set_lan_key(ss(&key));
                u.state().invoke_report(ss("lan-key"), true, ss(""), ss(""));
            }
            Err(p) => u.state().invoke_toast(u.problem(&p), ss("error")),
        }
    }));
    s.on_set_twitch_server(with_ui!(|u, url| {
        if let Err(p) = u.app.set_twitch_server(&url) {
            u.state().invoke_toast(u.problem(&p), ss("error"));
        }
        u.refresh();
    }));
    s.on_set_custom_delay(with_ui!(|u, secs| {
        u.save(|s| s.hotkeys.custom_delay_secs = secs.clamp(1, 600) as u32);
    }));
    s.on_open_url(|key| open_url(&key));
    s.on_open_logs_folder(with_ui!(|u| {
        if let Some(dir) = u.app.log_dir() {
            let _ = std::fs::create_dir_all(dir);
            platform::open(&dir.to_string_lossy());
        }
    }));
    s.on_open_log(with_ui!(|u, id| {
        if opendelay_core::diag::valid_log_id(&id)
            && let Some(dir) = u.app.log_dir()
        {
            platform::open(&dir.join(format!("{id}.log")).to_string_lossy());
        }
    }));
    s.on_refresh_logs(with_ui!(|u| u.refresh_logs()));
    s.on_save_log(with_ui!(|u, id| {
        let saved = (|| {
            let text = u.app.diagnostic(&id).ok_or("log not found")?;
            let dir = platform::downloads_dir().ok_or("no Downloads folder")?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            std::fs::write(dir.join(format!("opendelay-{id}.log")), text).map_err(|e| e.to_string())
        })();
        match saved {
            Ok(()) => u.state().invoke_report(ss("log-saved"), true, ss(""), ss("")),
            Err(e) => u.toast_problem("save_failed", &e.to_string()),
        }
    }));
    s.on_refresh_twitch_servers(with_ui!(|u| u.fetch_twitch_servers(true)));
    s.on_load_twitch_servers(with_ui!(|u| if u.twitch.borrow().is_empty() {
        u.fetch_twitch_servers(false)
    }));
    s.on_check_update(with_ui!(|u| u.check_update()));
    s.on_download_update(with_ui!(|u| u.download_update()));
    s.on_capture_hotkey(with_ui!(|u, which, text, ctrl, alt, shift, meta| {
        u.capture_hotkey(&which, &text, ctrl, alt, shift, meta)
    }));
    s.on_clear_hotkey(with_ui!(|u, which| {
        if let Some(w) = Which::parse(&which) {
            u.save(|s| w.slot(&mut s.hotkeys).clear());
            u.apply_hotkeys();
        }
    }));
    s.on_set_start_with_os(with_ui!(|u, on| {
        match autostart::set(on) {
            Ok(()) => {
                u.save(|s| s.start_with_os = on);
            }
            Err(e) => {
                u.toast_problem("autostart", &e);
                u.shown.borrow_mut().settings = None;
                u.refresh();
            }
        }
    }));
    s.on_quit(with_ui!(|u| u.quit()));
    s.on_quit_now(with_ui!(|u| u.quit_now()));

    if let Some(t) = &ui.tray {
        let _ = t.show();
        t.on_show_window(with_ui!(|u| u.show_window()));
        t.on_toggle_delay(with_ui!(|u| {
            let name = if u.app.status().delay.state == "live" { "enable" } else { "golive" };
            u.act(Action::ToggleDelay, name);
        }));
        t.on_go_live(with_ui!(|u| {
            u.act(Action::GoLive, "golive");
        }));
        t.on_quit(with_ui!(|u| u.quit()));
    }

    // Closing the window keeps OpenDelay in the tray unless the user chose to
    // quit (or there is no tray to come back from).
    ui.window.window().on_close_requested(with_ui!(|u| {
        if u.app.settings().quit_on_close || u.tray.is_none() {
            u.quit();
            CloseRequestResponse::KeepWindowShown
        } else {
            CloseRequestResponse::HideWindow
        }
    }));

    Ok(ui)
}

fn delay_active(st: &Status) -> bool {
    st.connected && matches!(st.delay.state, "delayed" | "buffering" | "draining")
}

impl Ui {
    pub fn window(&self) -> &MainWindow {
        &self.window
    }

    pub fn has_tray(&self) -> bool {
        self.tray.is_some()
    }

    /// Poll status: every second, within 100 ms of a change, or every 100 ms
    /// right after a command and while the delay is changing.
    pub fn start_polling(self: &Rc<Self>) -> Timer {
        let timer = Timer::default();
        let weak = Rc::downgrade(self);
        let last = Cell::new(Instant::now());
        let mut changes = self.app.subscribe();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(u) = weak.upgrade() else { return };
            if u.app.take_show_request() {
                u.show_window();
            }
            let pressed = u.hotkeys.borrow().pressed();
            for which in pressed {
                u.on_hotkey(which);
            }
            let now = Instant::now();
            // A minimized window still renders every property change.
            if !u.window.window().is_visible() || u.window.window().is_minimized() {
                if now.duration_since(last.get()) >= Duration::from_secs(1) {
                    last.set(now);
                    u.refresh_tray();
                }
                return;
            }
            let state = u.state().get_state();
            let busy = now < u.fast_until.get() || state == "buffering" || state == "draining";
            let changed = changes.has_changed().unwrap_or(false);
            if changed {
                changes.borrow_and_update();
            }
            if busy || changed || now.duration_since(last.get()) >= Duration::from_secs(1) {
                last.set(now);
                u.refresh();
            }
        });
        timer
    }
}
