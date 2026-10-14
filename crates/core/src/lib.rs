// SPDX-License-Identifier: GPL-3.0-only
//! OpenDelay core: RTMP relay, delay engine, settings and the local control API.

pub mod conn;
pub mod diag;
pub mod engine;
pub mod relay;
pub mod routes;
pub mod session;
pub mod upstream;
pub mod url;

pub use relay::{Relay, RelayOptions};
pub use routes::Route;
