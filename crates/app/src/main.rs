// SPDX-License-Identifier: GPL-3.0-only
// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod autostart;
mod hotkeys;
mod platform;
#[cfg(target_os = "linux")]
mod portal;
mod ui;

use opendelay_core::app::{App, AppOptions, HTTP_PORT, RTMP_PORT};

fn main() {
    #[cfg(windows)]
    platform::restrict_dll_search();

    // Ports can be moved when another program already uses the defaults.
    let port = |var: &str, default: u16| std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let http_port = port("OPENDELAY_HTTP_PORT", HTTP_PORT);
    if std::env::args().any(|a| a == "--updated") {
        // Started by the version it replaced, which is still quitting: wait (bounded) for its port.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], http_port));
        for _ in 0..75 {
            if std::net::TcpListener::bind(addr).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    if platform::activate_running_instance(http_port) {
        return; // the running instance shows its window
    }
    if let Ok(exe) = std::env::current_exe() {
        opendelay_core::update::remove_old(&exe); // left by an installed update
    }
    // Two workers are plenty for a handful of streams and the local API.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let app = App::new(AppOptions::default());
    let rtmp = rt.block_on(app.start_rtmp(port("OPENDELAY_RTMP_PORT", RTMP_PORT)));
    let http = rt.block_on(app.start_http(http_port));

    let ui = match ui::create(app.clone(), rt.handle().clone()) {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("OpenDelay could not open its window: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = &rtmp {
        ui.toast_problem("rtmp_port", &e.to_string());
    }
    if let Err(e) = &http {
        ui.toast_problem("http_port", &e.to_string());
    }
    let _poll = ui.start_polling();
    // Autostart launches with --minimized: stay in the tray, or minimized
    // when there is no tray / the user prefers quitting on close.
    let minimized = std::env::args().any(|a| a == "--minimized");
    let to_tray = minimized && ui.has_tray() && !app.settings().quit_on_close;
    if !to_tray {
        if let Err(e) = ui.window().show() {
            eprintln!("OpenDelay could not show its window: {e}");
            std::process::exit(1);
        }
        if minimized {
            ui.window().window().set_minimized(true);
        }
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        eprintln!("UI error: {e}");
    }
    rt.block_on(app.shutdown());
}
