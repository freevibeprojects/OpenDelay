// SPDX-License-Identifier: GPL-3.0-only
//! Settings file (SPEC §10): JSON in the per-user config directory, versioned
//! with migrations, tolerant of unknown fields.

use crate::routes::{Route, TWITCH_AUTO, default_routes};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
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
    pub delay_mode_enabled: bool,
    pub default_buffer_secs: u32,
    pub delay_secs: u32,
    /// Every new stream starts with the delay on (viewers see it from the start, delayed).
    pub delay_at_start: bool,
    pub listener_mode: ListenerMode,
    pub twitch_server: String,
    /// Access key other computers add to the server URL (`?key=`) in LAN mode.
    pub lan_key: String,
    /// UI language code; empty = follow the system.
    pub language: String,
    pub update_check: bool,
    /// The first start asked whether to check for updates (`false` asks again).
    pub update_check_asked: bool,
    pub hotkeys: Hotkeys,
    /// Overlay badge colour (palette name).
    pub overlay_color: String,
    /// Overlay badge design: `minimal` (delay time) or `classic` (icon, label and time).
    pub overlay_style: String,
    /// Overlay label while the delay is on; empty = the default ("DELAY ON" in classic).
    pub overlay_text: String,
    /// Show the overlay only while the delay is on (otherwise whenever a stream is connected).
    pub overlay_only_delayed: bool,
    pub reduce_motion: bool,
    /// Fields this version does not know (e.g. from a newer version), kept on save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: VERSION,
            routes: default_routes(),
            start_with_os: false,
            // Without a tray (detected at startup) closing always quits.
            quit_on_close: false,
            delay_mode_enabled: true,
            default_buffer_secs: 60,
            delay_secs: 30,
            delay_at_start: false,
            listener_mode: ListenerMode::Local,
            twitch_server: TWITCH_AUTO.into(),
            lan_key: String::new(),
            language: String::new(),
            update_check: false,
            update_check_asked: false,
            hotkeys: Hotkeys::default(),
            overlay_color: "violet".into(),
            overlay_style: "minimal".into(),
            overlay_text: String::new(),
            overlay_only_delayed: true,
            reduce_motion: false,
            extra: serde_json::Map::new(),
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
    /// Parse a settings document. Missing fields take defaults; unknown ones
    /// (and a newer version number) are kept so that saving loses nothing.
    pub fn from_json(text: &str) -> serde_json::Result<Settings> {
        let mut s: Settings = serde_json::from_value(migrate(serde_json::from_str(text)?))?;
        s.sanitize();
        Ok(s)
    }

    /// Load from `path`; missing file → defaults. A corrupt file is kept as
    /// `.bad` and the previous version (`.bak`) is used instead.
    pub fn load(path: &Path) -> Settings {
        let read = |p: &Path| fs::read_to_string(p).ok().map(|text| Settings::from_json(&text));
        match read(path) {
            None => Settings::default(),
            Some(Ok(s)) => s,
            Some(Err(_)) => {
                let _ = fs::rename(path, path.with_extension("json.bad"));
                read(&path.with_extension("json.bak")).and_then(Result::ok).unwrap_or_default()
            }
        }
    }

    /// Durable, atomic save: write and sync a temporary file, keep the
    /// previous file as `.bak`, then rename.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new(""));
        crate::diag::private_dir(dir)?;
        let tmp = path.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp); // a leftover may have other permissions
        let mut file = crate::diag::create_private(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        if path.exists() {
            fs::copy(path, path.with_extension("json.bak"))?;
        }
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if !dir.as_os_str().is_empty() {
            fs::File::open(dir)?.sync_all()?; // make the rename itself durable
        }
        Ok(())
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
        if self.overlay_style != "classic" {
            self.overlay_style = "minimal".into();
        }
        self.overlay_text = clean_overlay_text(&self.overlay_text);
    }
}

/// At most 40 visible characters on one line, without control characters.
pub fn clean_overlay_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(40).collect::<String>().trim().to_owned()
}

pub(crate) fn home() -> Option<PathBuf> {
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
        // newer files still load, and keep what this version does not know
        let newer = Settings::from_json(r#"{"version": 99, "languageX": 1}"#).unwrap();
        assert_eq!((newer.version, &newer.extra["languageX"]), (99, &Value::from(1)));
        let saved: Value = serde_json::to_value(&s).unwrap();
        assert_eq!(saved["futureThing"], serde_json::json!([1, 2]), "unknown fields survive a save");
        assert!(Settings::from_json("not json").is_err());
    }

    #[test]
    fn migrations_and_repairs() {
        let s = Settings::from_json(r#"{"defaultBufferSecs": 120, "quitOnClose": true}"#).unwrap();
        assert_eq!((s.version, s.default_buffer_secs, s.quit_on_close), (VERSION, 120, true));
        let s = Settings::from_json(r#"{"version": 1, "twitchServer": "rtmp://evil/app", "delaySecs": 0}"#).unwrap();
        assert_eq!(s.twitch_server, TWITCH_AUTO, "invalid Twitch server reset");
        assert_eq!(s.delay_secs, 1);
        let long = "x".repeat(60);
        let s = Settings::from_json(&format!(r#"{{"overlayStyle": "fancy", "overlayText": " a\nb{long}"}}"#)).unwrap();
        assert_eq!(s.overlay_style, "minimal", "unknown overlay style reset");
        assert_eq!(s.overlay_text.chars().count(), 39, "one line, at most 40 characters, trimmed");
        assert!(s.overlay_only_delayed);
    }

    #[test]
    fn save_and_load_files() {
        let dir = env::temp_dir().join(format!("opendelay-settings-{}", crate::diag::short_id()));
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let s = Settings { delay_secs: 12, ..Default::default() };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).delay_secs, 12);
        let s = Settings { delay_secs: 13, ..Default::default() };
        s.save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&dir), mode(&path), mode(&path.with_extension("json.bak"))), (0o700, 0o600, 0o600));
        }
        fs::write(&path, "{broken").unwrap();
        assert_eq!(Settings::load(&path).delay_secs, 12, "falls back to the previous save");
        assert!(path.with_extension("json.bad").exists());
        fs::remove_file(path.with_extension("json.bak")).unwrap();
        fs::write(&path, "{broken").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        // A failing save reports an error (the folder is a file here).
        let blocked = dir.join("blocked");
        fs::write(&blocked, "").unwrap();
        assert!(Settings::default().save(&blocked.join("settings.json")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
