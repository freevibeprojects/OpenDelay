// SPDX-License-Identifier: GPL-3.0-only
//! Settings file (SPEC §10): JSON in the per-user config directory, versioned
//! with migrations, tolerant of unknown fields.

use crate::routes::{Route, TWITCH_AUTO, default_routes};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ListenerMode {
    /// Loopback only.
    #[default]
    Local,
    /// All interfaces (an encoder on another PC can publish).
    Lan,
}

/// Global shortcuts in `global-hotkey` notation (e.g. `ctrl+shift+KeyD`); empty = unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Hotkeys {
    pub enable_delay: String,
    pub go_live: String,
    pub toggle_overlay: String,
    pub custom_delay: String,
    pub custom_delay_secs: u32,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys {
            enable_delay: String::new(),
            go_live: String::new(),
            toggle_overlay: String::new(),
            custom_delay: String::new(),
            custom_delay_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub version: u32,
    pub routes: Vec<Route>,
    pub start_with_os: bool,
    /// `false`: closing the window keeps OpenDelay running in the tray.
    pub quit_on_close: bool,
    pub onboarding_complete: bool,
    pub delay_mode_enabled: bool,
    pub default_buffer_secs: u32,
    pub delay_secs: u32,
    pub listener_mode: ListenerMode,
    pub twitch_server: String,
    /// UI language code; empty = follow the system.
    pub language: String,
    pub update_check: bool,
    pub hotkeys: Hotkeys,
    /// Overlay badge colour (palette name).
    pub overlay_color: String,
    pub reduce_motion: bool,
    pub background_animation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: VERSION,
            routes: default_routes(),
            start_with_os: false,
            quit_on_close: false,
            onboarding_complete: false,
            delay_mode_enabled: true,
            default_buffer_secs: 60,
            delay_secs: 30,
            listener_mode: ListenerMode::Local,
            twitch_server: TWITCH_AUTO.into(),
            language: String::new(),
            update_check: false,
            hotkeys: Hotkeys::default(),
            overlay_color: "violet".into(),
            reduce_motion: false,
            background_animation: true,
        }
    }
}

/// Bring an older settings document up to [`VERSION`], one version at a time.
/// Each future format change adds a step here (rename/convert fields).
fn migrate(mut v: Value) -> Value {
    let Some(obj) = v.as_object_mut() else { return v };
    let mut version = obj.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version < 1 {
        // Unversioned (hand-written) files use the version 1 field names.
        version = 1;
    }
    obj.insert("version".into(), Value::from(version.max(VERSION as u64)));
    v
}

impl Settings {
    /// Parse a settings document. Unknown fields are ignored and missing ones
    /// take defaults.
    pub fn from_json(text: &str) -> serde_json::Result<Settings> {
        let mut s: Settings = serde_json::from_value(migrate(serde_json::from_str(text)?))?;
        s.version = VERSION;
        s.sanitize();
        Ok(s)
    }

    /// Load from `path`; missing file → defaults; corrupt file is kept as `.bad`.
    pub fn load(path: &Path) -> Settings {
        match fs::read_to_string(path) {
            Ok(text) => Settings::from_json(&text).unwrap_or_else(|_| {
                let _ = fs::rename(path, path.with_extension("json.bad"));
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    /// Atomic save (write a temporary file, then rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        fs::rename(tmp, path)
    }

    /// Repair values a hand-edited file may have broken.
    fn sanitize(&mut self) {
        if !self.routes.iter().any(|r| r.id == crate::routes::DEFAULT_ROUTE_ID) {
            self.routes.splice(0..0, default_routes());
        }
        self.default_buffer_secs = self.default_buffer_secs.clamp(5, 600);
        self.delay_secs = self.delay_secs.clamp(1, 600);
        self.hotkeys.custom_delay_secs = self.hotkeys.custom_delay_secs.clamp(1, 600);
        if crate::routes::validate_twitch_server(&self.twitch_server).is_err() {
            self.twitch_server = TWITCH_AUTO.into();
        }
    }
}

fn home() -> Option<PathBuf> {
    env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Per-user config directory (`OPENDELAY_CONFIG_DIR` overrides).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(d) = env::var_os("OPENDELAY_CONFIG_DIR") {
        return Some(d.into());
    }
    if cfg!(windows) {
        env::var_os("APPDATA").map(|d| PathBuf::from(d).join("OpenDelay"))
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".config")))
            .map(|d| d.join("opendelay"))
    }
}

/// Per-user data directory for logs (`OPENDELAY_DATA_DIR` overrides).
pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = env::var_os("OPENDELAY_DATA_DIR") {
        return Some(d.into());
    }
    if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("OpenDelay"))
    } else {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".local/share")))
            .map(|d| d.join("opendelay"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let s = Settings::default();
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(Settings::from_json(&text).unwrap(), s);
        assert_eq!(s.routes[0].id, "twitch");
    }

    #[test]
    fn unknown_fields_and_partial_files() {
        let s = Settings::from_json(r#"{"version": 1, "futureThing": [1,2], "delaySecs": 45}"#).unwrap();
        assert_eq!(s.delay_secs, 45);
        assert_eq!(s.default_buffer_secs, 60);
        assert_eq!(s.routes[0].id, "twitch", "default route restored");
        // newer files still load
        assert_eq!(Settings::from_json(r#"{"version": 99, "languageX": 1}"#).unwrap().version, VERSION);
        assert!(Settings::from_json("not json").is_err());
    }

    #[test]
    fn migrations_and_repairs() {
        let s = Settings::from_json(r#"{"defaultBufferSecs": 120, "quitOnClose": true}"#).unwrap();
        assert_eq!((s.version, s.default_buffer_secs, s.quit_on_close), (VERSION, 120, true));
        let s = Settings::from_json(r#"{"version": 1, "twitchServer": "rtmp://evil/app", "delaySecs": 0}"#).unwrap();
        assert_eq!(s.twitch_server, TWITCH_AUTO, "invalid Twitch server reset");
        assert_eq!(s.delay_secs, 1);
    }

    #[test]
    fn save_and_load_files() {
        let dir = env::temp_dir().join(format!("opendelay-settings-{}", crate::diag::short_id()));
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let s = Settings { delay_secs: 12, ..Default::default() };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).delay_secs, 12);
        fs::write(&path, "{broken").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(path.with_extension("json.bad").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
