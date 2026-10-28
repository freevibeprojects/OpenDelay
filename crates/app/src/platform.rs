// SPDX-License-Identifier: GPL-3.0-only
//! Small OS integrations: opening URLs, files and folders.

use std::process::Command;

/// Open a URL, file or folder with the system's default handler.
pub fn open(target: &str) {
    // By full path: Windows would also look in this executable's own folder.
    #[cfg(windows)]
    let result = Command::new(
        std::env::var_os("SystemRoot").map_or("C:\\Windows".into(), std::path::PathBuf::from).join("explorer.exe"),
    )
    .arg(target)
    .spawn();
    #[cfg(not(windows))]
    let result = Command::new("xdg-open").arg(target).spawn();
    match result {
        // Wait for the launcher so it does not stay a zombie (it exits quickly).
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!("could not open {target}: {e}"),
    }
}

/// UI language to use for "system default": a bundled language matching the
/// system locale, or "" (English).
pub fn system_language(available: &[(&str, &str)]) -> String {
    let locale = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
    let base = locale.split(['-', '_', '.', '@']).next().unwrap_or("");
    available.iter().find(|(code, _)| *code == base).map(|(code, _)| code.to_string()).unwrap_or_default()
}

/// Load DLLs only from System32, never from this executable's folder (where a
/// planted DLL could wait, e.g. Downloads): covers the GUI libraries'
/// `LoadLibrary("uxtheme.dll")`-style calls, which the link-time
/// `/DEPENDENTLOADFLAG` does not.
#[cfg(windows)]
pub fn restrict_dll_search() {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
    // SAFETY: plain Win32 call with a valid flag.
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
}

/// The user's Downloads folder (XDG `user-dirs.dirs` on Linux, the known
/// folder on Windows, which users can move).
pub fn downloads_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
        let mut p = std::ptr::null_mut();
        // SAFETY: on success `p` is a NUL-terminated wide string the caller
        // frees with CoTaskMemFree (also to be freed on failure).
        unsafe {
            let ok =
                SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut p) == 0;
            let path = ok.then(|| {
                let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(p, len)))
            });
            CoTaskMemFree(p.cast());
            if path.is_some() {
                return path;
            }
        }
    }
    let home = PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?);
    if !cfg!(windows) {
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        if let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
            let dir = text.lines().find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR=")).map(|v| v.trim_matches('"'));
            if let Some(dir) = dir {
                return Some(PathBuf::from(dir.replace("$HOME", &home.to_string_lossy())));
            }
        }
    }
    Some(home.join("Downloads"))
}

/// Whether a tray icon would be visible: always on Windows; on Linux only
/// when a StatusNotifierItem host (the tray of KDE, waybar, GNOME's
/// AppIndicator extension, ...) is on the session bus.
pub fn tray_host_available() -> bool {
    #[cfg(target_os = "linux")]
    return (|| -> zbus::Result<bool> {
        let conn = zbus::blocking::Connection::session()?;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&conn)?;
        Ok(dbus.name_has_owner("org.kde.StatusNotifierWatcher".try_into()?)?)
    })()
    .unwrap_or(false);
    #[cfg(not(target_os = "linux"))]
    true
}

/// Single instance: if OpenDelay already serves on `port`, ask it to show its
/// window and return true. The token comes from its dock page, which any
/// local process may read (the API's protections target web pages).
pub fn activate_running_instance(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::time::Duration;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    // A free port means nothing runs there: skip the connection attempt,
    // which Windows takes half a second to refuse.
    if TcpListener::bind(addr).is_ok() {
        return false;
    }
    let request = |head: String| -> Option<String> {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        s.write_all(head.as_bytes()).ok()?;
        let mut body = String::new();
        s.take(1 << 20).read_to_string(&mut body).ok()?;
        Some(body)
    };
    let host = format!("127.0.0.1:{port}");
    let Some(page) = request(format!("GET /dock HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")) else {
        return false;
    };
    let Some(token) = page.split("name=\"opendelay-token\" content=\"").nth(1).and_then(|t| t.split('"').next()) else {
        return false;
    };
    let token: String = token.chars().filter(char::is_ascii_hexdigit).collect();
    request(format!(
        "POST /api/window/show HTTP/1.1\r\nHost: {host}\r\nX-OpenDelay-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ))
    .is_some_and(|r| r.starts_with("HTTP/1.1 200"))
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn downloads_known_folder() {
        let dir = super::downloads_dir().unwrap();
        assert!(dir.is_absolute() && dir.is_dir(), "{dir:?}");
    }
}
