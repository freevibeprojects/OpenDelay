// SPDX-License-Identifier: GPL-3.0-only
//! Developer tool: run the RTMP relay and the local HTTP server without the
//! desktop window (settings and logs in a temporary folder).
//!
//! `cargo run -p opendelay-core --example serve -- [http-port] [rtmp-port]`

use opendelay_core::{App, AppOptions};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1).map(|a| a.parse::<u16>().expect("port number"));
    let (http, rtmp) = (args.next().unwrap_or(3000), args.next().unwrap_or(1935));
    let dir = std::env::temp_dir().join("opendelay-serve");
    let app =
        App::new(AppOptions { config_dir: Some(dir.join("config")), data_dir: Some(dir.join("data")), tls: None });
    let rtmp = app.start_rtmp(rtmp).await.expect("RTMP listener");
    let http = app.start_http(http).await.expect("HTTP server");
    println!("RTMP on {rtmp:?}, HTTP on http://{http}/dock (Ctrl+C to stop)");
    tokio::signal::ctrl_c().await.ok();
    app.shutdown().await;
}
