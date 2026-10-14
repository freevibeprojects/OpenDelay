// SPDX-License-Identifier: GPL-3.0-only
// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod autostart;
mod hotkeys;
mod platform;
mod ui;

use opendelay_core::app::{App, AppOptions, HTTP_PORT, RTMP_PORT};

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let app = App::new(AppOptions::default());
    let rtmp = rt.block_on(app.start_rtmp(RTMP_PORT));
    let http = rt.block_on(app.start_http(HTTP_PORT));

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
