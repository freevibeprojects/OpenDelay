// SPDX-License-Identifier: GPL-3.0-only
//! Wayland global shortcuts through the XDG desktop portal
//! (`org.freedesktop.portal.GlobalShortcuts`, provided by KDE Plasma,
//! GNOME 48+, Hyprland and others). Wayland forbids global key grabs, so the
//! desktop owns the bindings: it shows its own confirmation dialog and the
//! shortcuts captured in OpenDelay are passed as preferred triggers.

use crate::hotkeys::Which;
use std::collections::HashMap;
use std::sync::mpsc;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

pub struct Portal {
    bind: mpsc::Sender<Vec<(Which, String)>>,
    pressed: mpsc::Receiver<Which>,
}

fn id_of(w: Which) -> &'static str {
    match w {
        Which::Enable => "enable-delay",
        Which::GoLive => "go-live",
        Which::Overlay => "toggle-overlay",
        Which::Custom => "custom-delay",
    }
}

fn which_of(id: &str) -> Option<Which> {
    [Which::Enable, Which::GoLive, Which::Overlay, Which::Custom].into_iter().find(|w| id_of(*w) == id)
}

fn description(w: Which) -> &'static str {
    match w {
        Which::Enable => "OpenDelay: enable delay",
        Which::GoLive => "OpenDelay: go live",
        Which::Overlay => "OpenDelay: show or hide the overlay",
        Which::Custom => "OpenDelay: enable the custom delay",
    }
}

/// XDG shortcut trigger (`CTRL+SHIFT+d`) for a `global-hotkey` string
/// (`shift+control+KeyD`).
pub fn trigger(hotkey: &str) -> String {
    let parts: Vec<&str> = hotkey.split('+').collect();
    let Some((key, mods)) = parts.split_last() else { return String::new() };
    let mut out: Vec<String> = Vec::new();
    for (m, name) in [("control", "CTRL"), ("alt", "ALT"), ("shift", "SHIFT"), ("super", "LOGO")] {
        if mods.contains(&m) {
            out.push(name.into());
        }
    }
    let key = match *key {
        k if k.starts_with("Key") => k[3..].to_ascii_lowercase(),
        k if k.starts_with("Digit") => k[5..].to_owned(),
        k if k.starts_with("Arrow") => k[5..].to_owned(),
        "Space" => "space".into(),
        "Enter" => "Return".into(),
        k => k.to_owned(),
    };
    out.push(key);
    out.join("+")
}

/// The request object a portal call with `token` will answer on; subscribe
/// before calling so the response cannot be missed.
fn responses(conn: &Connection, token: &str) -> zbus::Result<zbus::blocking::proxy::SignalIterator<'static>> {
    let sender = conn.unique_name().map(|n| n.trim_start_matches(':').replace('.', "_")).unwrap_or_default();
    let path = format!("{PATH}/request/{sender}/{token}");
    let request = Proxy::new(conn, DEST, path, "org.freedesktop.portal.Request")?;
    request.receive_signal("Response")
}

fn wait(mut it: zbus::blocking::proxy::SignalIterator<'static>) -> zbus::Result<HashMap<String, OwnedValue>> {
    let msg = it.next().ok_or_else(|| zbus::Error::Failure("portal closed".into()))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    if code != 0 {
        return Err(zbus::Error::Failure(format!("portal request ended with code {code}")));
    }
    Ok(results)
}

fn create_session(conn: &Connection, n: u32) -> zbus::Result<OwnedObjectPath> {
    let token = format!("opendelay_{}_{n}", std::process::id());
    let pending = responses(conn, &token)?;
    let opts: HashMap<&str, Value> = HashMap::from([
        ("handle_token", Value::from(token.as_str())),
        ("session_handle_token", Value::from(format!("opendelay_s{}_{n}", std::process::id()))),
    ]);
    Proxy::new(conn, DEST, PATH, IFACE)?.call_method("CreateSession", &(opts,))?;
    let handle = wait(pending)?.remove("session_handle").ok_or_else(|| zbus::Error::Failure("no session".into()))?;
    // Specified as a string; some portals send an object path.
    match String::try_from(handle.try_clone()?) {
        Ok(s) => Ok(OwnedObjectPath::try_from(s)?),
        Err(_) => Ok(OwnedObjectPath::try_from(handle)?),
    }
}

fn bind(conn: &Connection, session: &OwnedObjectPath, list: &[(Which, String)], n: u32) -> zbus::Result<()> {
    let token = format!("opendelay_b{}_{n}", std::process::id());
    let pending = responses(conn, &token)?;
    let shortcuts: Vec<(&str, HashMap<&str, Value>)> = list
        .iter()
        .map(|(w, hotkey)| {
            let mut props = HashMap::from([("description", Value::from(description(*w)))]);
            if !hotkey.is_empty() {
                props.insert("preferred_trigger", Value::from(trigger(hotkey)));
            }
            (id_of(*w), props)
        })
        .collect();
    let opts: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token.as_str()))]);
    Proxy::new(conn, DEST, PATH, IFACE)?.call_method("BindShortcuts", &(session, shortcuts, "", opts))?;
    wait(pending).map(|_| ())
}

fn close(conn: &Connection, session: &OwnedObjectPath) {
    if let Ok(p) = Proxy::new(conn, DEST, session.as_ref(), "org.freedesktop.portal.Session") {
        let _ = p.call_method("Close", &());
    }
}

impl Portal {
    /// `None` when the desktop has no GlobalShortcuts portal.
    pub fn start() -> Option<Portal> {
        let conn = Connection::session().ok()?;
        Proxy::new(&conn, DEST, PATH, IFACE).ok()?.get_property::<u32>("version").ok()?;
        let (bind_tx, bind_rx) = mpsc::channel::<Vec<(Which, String)>>();
        let (press_tx, press_rx) = mpsc::channel();

        let listen = conn.clone();
        std::thread::spawn(move || {
            let Ok(signals) = Proxy::new(&listen, DEST, PATH, IFACE).and_then(|p| p.receive_signal("Activated")) else {
                return;
            };
            for msg in signals {
                let body: zbus::Result<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)> =
                    msg.body().deserialize();
                if let Some(w) = body.ok().and_then(|(_, id, _, _)| which_of(&id)) {
                    let _ = press_tx.send(w);
                }
            }
        });

        // Each (re)binding uses a fresh session: portals bind once per session.
        std::thread::spawn(move || {
            let mut session: Option<OwnedObjectPath> = None;
            let mut n = 0;
            while let Ok(list) = bind_rx.recv() {
                if let Some(s) = session.take() {
                    close(&conn, &s);
                }
                n += 1;
                match create_session(&conn, n) {
                    Ok(s) => {
                        if let Err(e) = bind(&conn, &s, &list, n) {
                            eprintln!("global shortcuts portal: {e}");
                        }
                        session = Some(s);
                    }
                    Err(e) => eprintln!("global shortcuts portal: {e}"),
                }
            }
        });
        Some(Portal { bind: bind_tx, pressed: press_rx })
    }

    /// Ask the desktop to bind these shortcuts (it may show a dialog).
    pub fn bind(&self, list: Vec<(Which, String)>) {
        let _ = self.bind.send(list);
    }

    pub fn pressed(&self) -> Vec<Which> {
        self.pressed.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn triggers() {
        assert_eq!(super::trigger("shift+control+KeyD"), "CTRL+SHIFT+d");
        assert_eq!(super::trigger("alt+super+F5"), "ALT+LOGO+F5");
        assert_eq!(super::trigger("control+ArrowUp"), "CTRL+Up");
        assert_eq!(super::trigger("control+Digit1"), "CTRL+1");
    }
}
