// SPDX-License-Identifier: GPL-3.0-only
//! Global hotkeys (SPEC §8.1): `global-hotkey` on Windows and X11; on Wayland
//! the XDG GlobalShortcuts portal when the desktop provides it (the desktop
//! confirms the bindings). Without that portal Wayland allows no global grabs;
//! users there bind desktop shortcuts to the HTTP/WebSocket API instead.

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use opendelay_core::settings::Hotkeys;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Enable,
    GoLive,
    Overlay,
    Custom,
}

impl Which {
    pub fn parse(s: &str) -> Option<Which> {
        Some(match s {
            "enable" => Which::Enable,
            "golive" => Which::GoLive,
            "overlay" => Which::Overlay,
            "custom" => Which::Custom,
            _ => return None,
        })
    }

    pub fn slot(self, h: &mut Hotkeys) -> &mut String {
        match self {
            Which::Enable => &mut h.enable_delay,
            Which::GoLive => &mut h.go_live,
            Which::Overlay => &mut h.toggle_overlay,
            Which::Custom => &mut h.custom_delay,
        }
    }
}

pub struct Binder {
    manager: Option<GlobalHotKeyManager>,
    bound: Vec<(HotKey, Which)>,
    #[cfg(target_os = "linux")]
    portal: Option<crate::portal::Portal>,
}

const ALL: [Which; 4] = [Which::Enable, Which::GoLive, Which::Overlay, Which::Custom];

/// True when global grabs cannot work (Wayland session).
pub fn wayland() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
            || std::env::var_os("WAYLAND_DISPLAY").is_some())
}

impl Binder {
    /// Must be created on the main (event loop) thread.
    pub fn new() -> Binder {
        #[cfg(target_os = "linux")]
        if wayland()
            && let Some(portal) = crate::portal::Portal::start()
        {
            return Binder { manager: None, bound: Vec::new(), portal: Some(portal) };
        }
        Binder {
            manager: GlobalHotKeyManager::new().ok(),
            bound: Vec::new(),
            #[cfg(target_os = "linux")]
            portal: None,
        }
    }

    pub fn available(&self) -> bool {
        self.manager.is_some() || self.uses_portal()
    }

    /// Bindings go through the desktop's GlobalShortcuts portal.
    pub fn uses_portal(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.portal.is_some();
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// Register the shortcuts from settings; returns the ones that failed.
    pub fn apply(&mut self, settings: &Hotkeys) -> Vec<Which> {
        let mut copy = settings.clone();
        #[cfg(target_os = "linux")]
        if let Some(p) = &self.portal {
            p.bind(ALL.into_iter().map(|w| (w, w.slot(&mut copy).clone())).filter(|(_, k)| !k.is_empty()).collect());
            return Vec::new();
        }
        let Some(m) = &self.manager else { return Vec::new() };
        for (k, _) in self.bound.drain(..) {
            let _ = m.unregister(k);
        }
        let mut failed = Vec::new();
        for which in ALL {
            let text = which.slot(&mut copy).clone();
            if text.is_empty() {
                continue;
            }
            match HotKey::from_str(&text) {
                Ok(k) if m.register(k).is_ok() => self.bound.push((k, which)),
                _ => failed.push(which),
            }
        }
        failed
    }

    /// Would `hotkey` register? (Free and valid.)
    pub fn try_register(&self, hotkey: &str, except: Which) -> Result<(), &'static str> {
        if self.uses_portal() {
            // The desktop decides; it reports conflicts in its own dialog.
            return HotKey::from_str(hotkey).map(|_| ()).map_err(|_| "hotkey_key");
        }
        let Some(m) = &self.manager else { return Err("hotkey_unsupported") };
        let k = HotKey::from_str(hotkey).map_err(|_| "hotkey_key")?;
        if self.bound.iter().any(|(b, w)| b.id() == k.id() && *w != except) {
            return Err("hotkey_taken");
        }
        if self.bound.iter().any(|(b, w)| b.id() == k.id() && *w == except) {
            return Ok(());
        }
        m.register(k).map_err(|_| "hotkey_taken")?;
        let _ = m.unregister(k);
        Ok(())
    }

    /// Shortcuts pressed since the last call.
    pub fn pressed(&self) -> Vec<Which> {
        #[cfg(target_os = "linux")]
        if let Some(p) = &self.portal {
            return p.pressed();
        }
        let mut out = Vec::new();
        while let Ok(e) = GlobalHotKeyEvent::receiver().try_recv() {
            if e.state() == HotKeyState::Pressed {
                out.extend(self.bound.iter().filter(|(k, _)| k.id() == e.id()).map(|(_, w)| *w));
            }
        }
        out
    }
}

/// Key code (`global-hotkey` / W3C name) for a Slint key event text.
fn code_for(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let named = match c {
        'a'..='z' | 'A'..='Z' => return Some(format!("Key{}", c.to_ascii_uppercase())),
        '0'..='9' => return Some(format!("Digit{c}")),
        '\u{F704}'..='\u{F71B}' => return Some(format!("F{}", c as u32 - 0xF704 + 1)),
        // US layout shifted digits
        '!' => "Digit1",
        '@' => "Digit2",
        '#' => "Digit3",
        '$' => "Digit4",
        '%' => "Digit5",
        '^' => "Digit6",
        '&' => "Digit7",
        '*' => "Digit8",
        '(' => "Digit9",
        ')' => "Digit0",
        ' ' => "Space",
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "BracketLeft",
        ']' | '}' => "BracketRight",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Quote",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        '\\' | '|' => "Backslash",
        '`' | '~' => "Backquote",
        '\t' => "Tab",
        '\n' | '\r' => "Enter",
        '\u{8}' => "Backspace",
        '\u{7f}' => "Delete",
        '\u{F700}' => "ArrowUp",
        '\u{F701}' => "ArrowDown",
        '\u{F702}' => "ArrowLeft",
        '\u{F703}' => "ArrowRight",
        '\u{F727}' => "Insert",
        '\u{F729}' => "Home",
        '\u{F72B}' => "End",
        '\u{F72C}' => "PageUp",
        '\u{F72D}' => "PageDown",
        _ => return None,
    };
    Some(named.into())
}

/// Build a hotkey string from a captured key press. Plain keys need a
/// modifier (so typing is never swallowed), except function keys.
pub fn from_key(text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Result<String, &'static str> {
    let code = code_for(text).ok_or("hotkey_key")?;
    if !(ctrl || alt || meta || code.starts_with('F') && code.len() <= 3) {
        return Err("hotkey_key");
    }
    let mut s = String::new();
    for (on, name) in [(shift, "shift+"), (ctrl, "control+"), (alt, "alt+"), (meta, "super+")] {
        if on {
            s.push_str(name);
        }
    }
    s.push_str(&code);
    HotKey::from_str(&s).map(|k| k.into_string()).map_err(|_| "hotkey_key")
}

/// `shift+control+KeyD` → `Ctrl+Shift+D` for display.
pub fn display(hotkey: &str) -> String {
    if hotkey.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = hotkey.split('+').collect();
    let (mods, key) = parts.split_at(parts.len() - 1);
    let mut out: Vec<String> = Vec::new();
    for (m, label) in [
        ("control", "Ctrl"),
        ("alt", "Alt"),
        ("shift", "Shift"),
        ("super", if cfg!(windows) { "Win" } else { "Super" }),
    ] {
        if mods.contains(&m) {
            out.push(label.into());
        }
    }
    let k = key[0];
    out.push(k.strip_prefix("Key").or_else(|| k.strip_prefix("Digit")).unwrap_or(k).to_owned());
    out.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_and_display() {
        let k = from_key("d", true, false, true, false).unwrap();
        assert_eq!(display(&k), "Ctrl+Shift+D");
        assert_eq!(display(&from_key("!", true, false, true, false).unwrap()), "Ctrl+Shift+1");
        assert_eq!(display(&from_key("\u{F70F}", false, false, false, false).unwrap()), "F12");
        assert_eq!(from_key("d", false, false, false, false), Err("hotkey_key"), "plain letters need a modifier");
        assert_eq!(from_key("é", true, false, false, false), Err("hotkey_key"));
        assert!(HotKey::from_str(&from_key("\u{F700}", false, true, false, false).unwrap()).is_ok());
    }
}
