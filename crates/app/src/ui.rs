// SPDX-License-Identifier: GPL-3.0-only
//! Desktop UI glue: pushes status and settings into the Slint `AppState`
//! global and turns its callbacks into core actions. Status is polled every
//! second, and every 100 ms for a few seconds after a command or while a
//! transition runs.

use crate::platform;
use crate::{AppState, Choice, LogItem, MainWindow, RouteItem, StreamItem, Strings, Theme};
use opendelay_core::app::{Action, App, Reply, Status};
use opendelay_core::routes::{PRESETS, Problem, Route, TWITCH_AUTO};
use opendelay_core::settings::{ListenerMode, Settings};
use opendelay_core::url::UpstreamUrl;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
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
    streams: Vec<StreamItem>,
    settings: Option<Settings>,
}

pub struct Ui {
    window: MainWindow,
    app: Arc<App>,
    shown: RefCell<Shown>,
    fast_until: Cell<Instant>,
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
        let name = if name == "enable" && reply.success && reply.status.state == "buffering" {
            "enable-building"
        } else {
            name
        };
        self.state().invoke_report(ss(name), reply.success, ss(reply.code.unwrap_or("")), ss(&reply.message));
        self.poll_fast();
        self.refresh_status(&reply.status);
        reply
    }

    fn refresh(&self) {
        self.refresh_status(&self.app.status());
        let settings = self.app.settings();
        if self.shown.borrow().settings.as_ref() != Some(&settings) {
            self.apply_settings(&settings);
            self.shown.borrow_mut().settings = Some(settings);
        }
    }

    fn refresh_status(&self, st: &Status) {
        let s = self.state();
        let cfg = self.app.relay.delay_config();
        s.set_state(ss(st.state));
        let active = st.connected && matches!(st.delay.state, "delayed" | "buffering" | "draining");
        s.set_delay_active(active);
        s.set_current_delay_s(st.delay.current_ms.div_ceil(1000) as i32);
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
        s.set_overlay_visible(st.overlay_visible);
        s.set_overlay_color(ss(&st.overlay_color));
        s.set_listener_mode(ss(if st.listener.configured_mode == ListenerMode::Lan { "lan" } else { "local" }));
        s.set_restart_required(st.listener.restart_required);
        s.set_lan_addresses(ss(st.listener.lan_addresses.join(", ")));

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

        let settings = self.app.settings();
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
                }
            })
            .collect();
        if self.shown.borrow().routes != routes {
            let first = routes.iter().find(|r| r.enabled).or(routes.first());
            s.set_encoder_url(first.map_or_else(SharedString::default, |r| r.local_url.clone()));
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
        s.set_background_animation(st.background_animation);
        s.set_overlay_color(ss(&st.overlay_color));
        s.set_custom_delay_s(st.hotkeys.custom_delay_secs as i32);
        s.set_hk_enable(ss(&st.hotkeys.enable_delay));
        s.set_hk_go_live(ss(&st.hotkeys.go_live));
        s.set_hk_overlay(ss(&st.hotkeys.toggle_overlay));
        s.set_hk_custom(ss(&st.hotkeys.custom_delay));

        let system = self.strings().invoke_system_default();
        let mut langs = vec![Choice { value: ss(""), label: system.clone() }];
        langs.extend(LANGUAGES.iter().map(|(c, n)| Choice { value: ss(c), label: ss(n) }));
        let current = langs.iter().find(|c| c.value == st.language).map_or(system, |c| c.label.clone());
        s.set_language(ss(&st.language));
        s.set_language_label(current);
        s.set_languages(model(langs));

        let auto = self.strings().invoke_twitch_auto();
        let mut servers = vec![Choice { value: ss(TWITCH_AUTO), label: auto.clone() }];
        if st.twitch_server != TWITCH_AUTO {
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
        };
        let result =
            if route.id.is_empty() { self.app.add_route(route) } else { self.app.update_route(&route.id.clone(), route) };
        match result {
            Ok(_) => {
                self.refresh();
                SharedString::default()
            }
            Err(p) => self.problem(&p),
        }
    }

    fn finish_onboarding(&self, preset: &str, server: &str) -> SharedString {
        if let Some(p) = PRESETS.iter().find(|p| p.id == preset) {
            let existing = self.app.routes().into_iter().find(|r| r.preset == preset);
            let result = match existing {
                Some(mut r) => {
                    r.enabled = true;
                    if !server.trim().is_empty() {
                        r.upstream = server.trim().to_owned();
                    }
                    self.app.update_route(&r.id.clone(), r).map(|_| ())
                }
                None => {
                    let upstream = if p.upstream.is_empty() { server.trim() } else { p.upstream };
                    let route = Route {
                        name: p.name.into(),
                        preset: p.id.into(),
                        path: p.path.into(),
                        upstream: upstream.into(),
                        ..Default::default()
                    };
                    self.app.add_route(route).map(|_| ())
                }
            };
            if let Err(e) = result {
                return self.problem(&e);
            }
        }
        self.save(|s| s.onboarding_complete = true)
    }

    fn set_language(&self, code: &str) {
        let effective = if code.is_empty() { platform::system_language(LANGUAGES) } else { code.to_owned() };
        let _ = slint::select_bundled_translation(&effective);
    }
}

fn open_url(key: &str) {
    let base = env!("CARGO_PKG_REPOSITORY");
    let url = match key {
        "docs" => format!("{base}#readme"),
        "protocol" => format!("{base}/blob/main/docs/controller-protocol.md"),
        "issues" => format!("{base}/issues"),
        _ => base.to_owned(),
    };
    platform::open(&url);
}

/// Build the window, wire every callback and start polling.
pub fn create(app: Arc<App>) -> Result<Rc<Ui>, slint::PlatformError> {
    let window = MainWindow::new()?;
    let ui = Rc::new(Ui { window, app, shown: RefCell::default(), fast_until: Cell::new(Instant::now()) });
    let settings = ui.app.settings();
    ui.set_language(&settings.language);

    let s = ui.state();
    s.set_version(ss(env!("CARGO_PKG_VERSION")));
    let http = ui.app.http_port();
    s.set_overlay_url(ss(format!("http://127.0.0.1:{http}/overlay")));
    s.set_dock_url(ss(format!("http://127.0.0.1:{http}/dock")));
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
    s.set_onboarding_open(!settings.onboarding_complete);
    ui.refresh();
    ui.refresh_logs();

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
    s.on_emergency_stop(with_ui!(|u| {
        u.act(Action::EmergencyStop, "emergency");
    }));
    s.on_clear_emergency(with_ui!(|u| {
        u.act(Action::ClearEmergencyStop, "allow");
    }));
    s.on_set_overlay_color(with_ui!(|u, name| {
        u.save(|s| s.overlay_color = name.to_string());
    }));
    s.on_save_route(with_ui!(|u, item| u.save_route(item)));
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
    s.on_set_background_animation(with_ui!(|u, on| {
        u.save(|s| s.background_animation = on);
    }));
    s.on_set_listener_mode(with_ui!(|u, mode| {
        let mode = if mode == "lan" { ListenerMode::Lan } else { ListenerMode::Local };
        match u.app.set_listener_mode(mode) {
            Ok(_) => u.state().invoke_report(ss("listener"), true, ss(""), ss("")),
            Err(p) => u.state().invoke_toast(u.problem(&p), ss("error")),
        }
        u.refresh();
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
    s.on_finish_onboarding(with_ui!(|u, preset, server| u.finish_onboarding(&preset, &server)));

    Ok(ui)
}

impl Ui {
    pub fn window(&self) -> &MainWindow {
        &self.window
    }

    /// Poll status: every second, or every 100 ms right after a command and
    /// while the delay is changing.
    pub fn start_polling(self: &Rc<Self>) -> Timer {
        let timer = Timer::default();
        let weak = Rc::downgrade(self);
        let last = Cell::new(Instant::now());
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(u) = weak.upgrade() else { return };
            let now = Instant::now();
            let state = u.state().get_state();
            let busy = now < u.fast_until.get() || state == "buffering" || state == "draining";
            if busy || now.duration_since(last.get()) >= Duration::from_secs(1) {
                last.set(now);
                u.refresh();
            }
        });
        timer
    }
}
