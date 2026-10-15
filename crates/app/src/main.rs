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
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Relaunch flag used after OpenGL failed: software rendering, no
/// single-instance probe (the old process is exiting), port binds retried.
const SOFTWARE_FLAG: &str = "--software-renderer";

/// Remembers that this machine has no usable OpenGL.
fn software_marker() -> Option<PathBuf> {
    opendelay_core::settings::config_dir().map(|d| d.join("software-renderer"))
}

/// Switch to the software renderer for good and start again.
fn fall_back_to_software() -> ! {
    if let Some(m) = software_marker() {
        let _ = std::fs::create_dir_all(m.parent().unwrap_or(&m));
        let _ = std::fs::write(&m, "OpenGL is not available here; OpenDelay uses its software renderer.\n");
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(std::env::args().skip(1)).arg(SOFTWARE_FLAG).spawn();
    }
    std::process::exit(1);
}

/// OpenGL setup failures inside the renderer surface as panics; turn them
/// into a software-renderer relaunch instead of a crash.
fn install_gl_fallback() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = payload.downcast_ref::<&str>().copied().or(payload.downcast_ref::<String>().map(String::as_str));
        if msg.is_some_and(|m| m.contains("GL configuration") || m.contains("OpenGL")) {
            fall_back_to_software();
        }
        default(info);
    }));
}

/// Bind with retries when relaunched (the previous process may still be exiting).
fn retry<T, E>(retrying: bool, mut f: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match f() {
            Err(_) if retrying && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            r => return r,
        }
    }
}

fn main() {
    let relaunched = std::env::args().any(|a| a == SOFTWARE_FLAG);
    if relaunched || software_marker().is_some_and(|m| m.exists()) {
        let selected =
            slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select();
        if let Err(e) = selected {
            eprintln!("OpenDelay could not select the software renderer: {e}");
        }
    } else {
        install_gl_fallback();
    }

    // Ports can be moved when another program already uses the defaults.
    let port = |var: &str, default: u16| std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let http_port = port("OPENDELAY_HTTP_PORT", HTTP_PORT);
    if !relaunched && platform::activate_running_instance(http_port) {
        return; // the running instance shows its window
    }
    // Two workers are plenty for a handful of streams and the local API.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let app = App::new(AppOptions::default());
    let rtmp = retry(relaunched, || rt.block_on(app.start_rtmp(port("OPENDELAY_RTMP_PORT", RTMP_PORT))));
    let http = retry(relaunched, || rt.block_on(app.start_http(http_port)));

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
            if !relaunched {
                fall_back_to_software();
            }
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
