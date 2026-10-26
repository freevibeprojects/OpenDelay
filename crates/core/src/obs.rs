// SPDX-License-Identifier: GPL-3.0-only
//! OBS Studio service entry. With a custom server OBS drops the platform's
//! recommended encoder settings, so its keyframe interval falls back to
//! "auto" (4–8 s) and the delay can only switch that coarsely. An
//! "OpenDelay" entry in OBS's service list carries Twitch's recommendation
//! (2 s keyframes; OBS also sets CBR), which OBS then applies by itself.

use crate::routes::Route;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::{env, fs, io};

pub const SERVICE_NAME: &str = "OpenDelay";

/// OBS's service list, in the first OBS settings folder that has one.
fn services_path() -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        dirs.extend(env::var_os("APPDATA").map(|d| PathBuf::from(d).join("obs-studio")));
    } else {
        let home = crate::settings::home();
        let config =
            env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".config")));
        dirs.extend(config.map(|d| d.join("obs-studio")));
        dirs.extend(home.map(|h| h.join(".var/app/com.obsproject.Studio/config/obs-studio")));
    }
    dirs.into_iter().map(|d| d.join("plugin_config/rtmp-services/services.json")).find(|p| p.is_file())
}

/// The entry: one server per route.
fn entry(routes: &[Route], port: u16) -> Value {
    let servers: Vec<Value> = routes
        .iter()
        .map(|r| json!({ "name": r.name, "url": format!("rtmp://127.0.0.1:{port}/{}", r.path) }))
        .collect();
    json!({
        "name": SERVICE_NAME,
        "common": true,
        "servers": servers,
        "recommended": { "keyint": 2 },
        "supported video codecs": ["h264", "hevc", "av1"],
    })
}

/// `text` with the entry added or replaced; `None` when it is already there.
fn with_entry(text: &str, entry: Value) -> io::Result<Option<String>> {
    let mut root: Value = serde_json::from_str(text).map_err(io::Error::other)?;
    let services = root
        .get_mut("services")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| io::Error::other("OBS's service list has an unknown format"))?;
    match services.iter_mut().find(|s| s["name"] == SERVICE_NAME) {
        Some(old) if *old == entry => return Ok(None),
        Some(old) => *old = entry,
        None => services.push(entry),
    }
    serde_json::to_string(&root).map(Some).map_err(io::Error::other)
}

/// Add or update the entry in OBS's service list (the previous file is
/// kept as `services.json.opendelay.bak`). OBS reads the list each time its
/// settings open.
pub fn register(routes: &[Route], port: u16) -> io::Result<()> {
    let path = services_path().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "OBS's service list was not found. Start OBS once, then try again.")
    })?;
    let text = fs::read_to_string(&path)?;
    let Some(new) = with_entry(&text, entry(routes, port))? else { return Ok(()) };
    fs::write(path.with_extension("json.opendelay.bak"), &text)?;
    let tmp = path.with_extension("json.opendelay.tmp");
    fs::write(&tmp, new)?;
    fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::default_routes;

    #[test]
    fn adds_replaces_and_keeps_the_rest() {
        let obs = r#"{"$schema":"schema/service-schema-v5.json","format_version":5,"services":[{"name":"Twitch","common":true,"servers":[{"name":"Auto","url":"rtmp://live.twitch.tv/app"}],"recommended":{"keyint":2,"max video bitrate":6000}}]}"#;
        let routes = default_routes();
        let added = with_entry(obs, entry(&routes, 1935)).unwrap().unwrap();
        assert!(added.starts_with(
            r#"{"$schema":"schema/service-schema-v5.json","format_version":5,"services":[{"name":"Twitch""#
        ));
        let v: Value = serde_json::from_str(&added).unwrap();
        let ours = &v["services"][1];
        assert_eq!((&ours["name"], &ours["recommended"]["keyint"]), (&json!("OpenDelay"), &json!(2)));
        assert_eq!(ours["servers"][0]["url"], "rtmp://127.0.0.1:1935/twitch");
        assert_eq!(with_entry(&added, entry(&routes, 1935)).unwrap(), None, "unchanged entry: no write");
        let moved = with_entry(&added, entry(&routes, 1936)).unwrap().unwrap();
        let v: Value = serde_json::from_str(&moved).unwrap();
        assert_eq!(v["services"].as_array().unwrap().len(), 2, "replaced, not added twice");
        assert_eq!(v["services"][1]["servers"][0]["url"], "rtmp://127.0.0.1:1936/twitch");
        assert!(with_entry("{}", entry(&routes, 1935)).is_err());
        assert!(with_entry("not json", entry(&routes, 1935)).is_err());
    }
}
