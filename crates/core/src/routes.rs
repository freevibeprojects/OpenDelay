// SPDX-License-Identifier: GPL-3.0-only
//! Relay routes: a local application path on the listener and an upstream URL.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub id: String,
    pub name: String,
    /// Local application path, e.g. `twitch` in `rtmp://127.0.0.1:1935/twitch`.
    pub path: String,
    /// Upstream server URL (without stream key); the encoder's key is forwarded.
    pub upstream: String,
    pub enabled: bool,
}

/// Normalise an RTMP `app` / local path: no surrounding slashes, no query, lowercase.
pub fn normalize_path(app: &str) -> String {
    app.split('?').next().unwrap_or("").trim_matches('/').to_ascii_lowercase()
}
