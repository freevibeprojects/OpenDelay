// SPDX-License-Identifier: GPL-3.0-only
//! Developer tool: render every page offscreen with Slint's software renderer
//! and write PPM images, without opening a window.
//!
//! `cargo run -p opendelay --example screenshot --features screenshot -- <out-dir>`

slint::include_modules!();

#[allow(dead_code)]
#[path = "../src/platform.rs"]
mod platform;
#[allow(dead_code)]
#[path = "../src/ui.rs"]
mod ui;

use opendelay_core::app::{App, AppOptions};
use slint::ComponentHandle;
use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use std::cell::Cell;
use std::io::Write;
use std::rc::Rc;
use std::time::Duration;

struct Offscreen {
    window: Rc<MinimalSoftwareWindow>,
    time: Rc<Cell<Duration>>,
}

impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.time.get()
    }
}

const W: u32 = 1180;
const H: u32 = 820;

fn shoot(window: &MinimalSoftwareWindow, time: &Cell<Duration>, path: &str) {
    // let animations finish
    for _ in 0..30 {
        time.set(time.get() + Duration::from_millis(100));
        slint::platform::update_timers_and_animations();
    }
    let mut buf = vec![PremultipliedRgbaColor::default(); (W * H) as usize];
    window.request_redraw();
    window.draw_if_needed(|r| {
        r.render(&mut buf, W as usize);
    });
    let mut f = std::fs::File::create(path).unwrap();
    write!(f, "P6\n{W} {H}\n255\n").unwrap();
    let bytes: Vec<u8> = buf.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
    f.write_all(&bytes).unwrap();
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "screenshots".into());
    std::fs::create_dir_all(&out).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let time = Rc::new(Cell::new(Duration::ZERO));
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone(), time: time.clone() })).unwrap();

    let dir = std::env::temp_dir().join(format!("opendelay-shot-{}", std::process::id()));
    let app = App::new(AppOptions { config_dir: Some(dir.join("config")), data_dir: Some(dir.join("data")), tls: None });
    let ui = ui::create(app).unwrap();
    let lang = std::env::var("SHOT_LANG").unwrap_or_default();
    let _ = slint::select_bundled_translation(&lang);
    window.set_size(slint::PhysicalSize::new(W, H));
    let w = ui.window();
    w.show().unwrap();
    let s = w.global::<AppState>();

    s.set_onboarding_open(true);
    shoot(&window, &time, &format!("{out}/onboarding.ppm"));
    s.set_onboarding_open(false);

    let pages = ["dashboard", "multistream", "controllers", "overlay", "setup", "help", "settings"];
    for (i, name) in pages.iter().enumerate() {
        w.set_page(i as i32);
        shoot(&window, &time, &format!("{out}/{name}.ppm"));
    }

    // A delayed stream, as the dashboard shows it.
    w.set_page(0);
    s.set_state("delayed".into());
    s.set_delay_active(true);
    s.set_current_delay_s(31);
    s.set_target_delay_s(30);
    s.set_buffer_fill(1.0);
    s.set_buffer_text("60 / 60 s".into());
    s.set_ready(true);
    s.set_bitrate_text("6.0 Mb/s".into());
    s.set_uptime_text("0:12:34".into());
    shoot(&window, &time, &format!("{out}/dashboard-delayed.ppm"));
    let _ = std::fs::remove_dir_all(dir);
}
