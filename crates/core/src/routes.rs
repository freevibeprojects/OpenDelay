// SPDX-License-Identifier: GPL-3.0-only
//! Relay routes (SPEC §6): a local application path on the listener and an
//! upstream server URL, plus destination presets and validation rules.

use crate::url::{UpstreamUrl, UrlError};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A user-facing validation error: stable `code` (UIs translate it) plus an
/// English message; `detail` carries the offending value when useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: &'static str,
    pub message: String,
    pub detail: String,
}

impl Problem {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Problem { code, message: message.into(), detail: String::new() }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<UrlError> for Problem {
    fn from(e: UrlError) -> Self {
        Problem::new(e.code(), e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Route {
    pub id: String,
    pub name: String,
    /// Preset id (`twitch`, `youtube`, `kick`, `tiktok`, `restream`, `custom`).
    pub preset: String,
    /// Local application path, e.g. `twitch` in `rtmp://127.0.0.1:1935/twitch`.
    pub path: String,
    /// Upstream server URL without the stream key (the encoder's key is forwarded).
    /// Twitch routes use the Twitch server chosen in settings.
    pub upstream: String,
    pub enabled: bool,
}

impl Default for Route {
    fn default() -> Self {
        Route {
            id: String::new(),
            name: String::new(),
            preset: "custom".into(),
            path: String::new(),
            upstream: String::new(),
            enabled: true,
        }
    }
}

/// The route created on first run; it can be edited or disabled, never deleted.
pub const DEFAULT_ROUTE_ID: &str = "twitch";
/// Twitch's global ingest endpoint, which picks the nearest server itself.
pub const TWITCH_AUTO: &str = "rtmp://live.twitch.tv/app";

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub path: &'static str,
    /// Empty when the platform hands out a per-stream server URL.
    pub upstream: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset { id: "twitch", name: "Twitch", path: "twitch", upstream: TWITCH_AUTO },
    Preset { id: "youtube", name: "YouTube", path: "youtube", upstream: "rtmp://a.rtmp.youtube.com/live2" },
    Preset {
        id: "kick",
        name: "Kick",
        path: "kick",
        upstream: "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app",
    },
    Preset { id: "tiktok", name: "TikTok", path: "tiktok", upstream: "" },
    Preset { id: "restream", name: "Restream", path: "restream", upstream: "rtmp://live.restream.io/live" },
    Preset { id: "custom", name: "Custom", path: "custom", upstream: "" },
];

pub fn default_routes() -> Vec<Route> {
    vec![Route {
        id: DEFAULT_ROUTE_ID.into(),
        name: "Twitch".into(),
        preset: "twitch".into(),
        path: "twitch".into(),
        upstream: TWITCH_AUTO.into(),
        enabled: true,
    }]
}

/// Normalise an RTMP `app` / local path: no surrounding slashes, no query, lowercase.
pub fn normalize_path(app: &str) -> String {
    app.split('?').next().unwrap_or("").trim_matches('/').to_ascii_lowercase()
}

/// Only Twitch-owned ingest hosts on port 1935 (plain RTMP).
pub fn validate_twitch_server(url: &str) -> Result<UpstreamUrl, Problem> {
    let u = UpstreamUrl::parse(url)?;
    // Twitch's ingest list uses *.contribute.live-video.net and
    // ingest.global-contribute.live-video.net; live.twitch.tv is the classic endpoint.
    let twitch_host = u.host == "live.twitch.tv"
        || u.host.ends_with(".twitch.tv")
        || u.host.ends_with(".contribute.live-video.net")
        || u.host.ends_with(".global-contribute.live-video.net");
    if u.tls || u.port != 1935 || !twitch_host {
        return Err(Problem::new(
            "not_twitch",
            "Only Twitch ingest servers (rtmp://…twitch.tv or …contribute.live-video.net, port 1935) are accepted.",
        ));
    }
    Ok(u)
}

/// Check `route` against the rules and the other routes (excluding itself).
pub fn validate(route: &Route, others: &[Route]) -> Result<(), Problem> {
    let path = normalize_path(&route.path);
    if path.is_empty() || path.len() > 64 {
        return Err(Problem::new("path_length", "The local path must be 1–64 characters."));
    }
    if !path.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(Problem::new("path_chars", "The local path may only contain letters, digits, '-' and '_'."));
    }
    if others.iter().any(|r| r.id != route.id && normalize_path(&r.path) == path) {
        let mut p = Problem::new("path_taken", format!("Another route already uses the local path \"{path}\"."));
        p.detail = path;
        return Err(p);
    }
    if route.name.trim().is_empty() {
        return Err(Problem::new("name_missing", "The route needs a name."));
    }
    if route.preset == "twitch" {
        validate_twitch_server(&route.upstream)?;
    } else if route.enabled || !route.upstream.trim().is_empty() {
        UpstreamUrl::parse(&route.upstream)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: &str, path: &str, upstream: &str) -> Route {
        Route { id: id.into(), name: id.into(), path: path.into(), upstream: upstream.into(), ..Default::default() }
    }

    #[test]
    fn route_rules() {
        let routes = default_routes();
        assert!(validate(&routes[0], &routes).is_ok());
        assert_eq!(validate(&r("a", "TWITCH/", "rtmp://h/app"), &routes).unwrap_err().code, "path_taken");
        assert!(validate(&r("a", "bad path", "rtmp://h/app"), &routes).is_err());
        assert!(validate(&r("a", "", "rtmp://h/app"), &routes).is_err());
        assert!(validate(&r("a", "yt", "http://h/app"), &routes).is_err());
        assert!(validate(&r("a", "yt", "rtmps://h:443/app"), &routes).is_ok());
        let mut off = r("a", "tt", "");
        off.enabled = false;
        assert!(validate(&off, &routes).is_ok(), "disabled routes may lack a URL");
    }

    #[test]
    fn twitch_servers() {
        assert!(validate_twitch_server(TWITCH_AUTO).is_ok());
        assert!(validate_twitch_server("rtmp://fra.contribute.live-video.net/app").is_ok());
        assert!(validate_twitch_server("rtmp://fra.contribute.live-video.net:1935/app").is_ok());
        assert!(validate_twitch_server("rtmp://ingest.global-contribute.live-video.net/app").is_ok());
        assert!(validate_twitch_server("rtmp://fra.contribute.live-video.net:443/app").is_err());
        assert!(validate_twitch_server("rtmps://live.twitch.tv/app").is_err());
        assert!(validate_twitch_server("rtmp://twitch.tv.evil.example/app").is_err());
        assert!(validate_twitch_server("rtmp://eviltwitch.tv/app").is_err());
    }
}
