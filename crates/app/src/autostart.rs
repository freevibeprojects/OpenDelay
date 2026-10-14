// SPDX-License-Identifier: GPL-3.0-only
//! "Start with the system": the per-user Run key on Windows, an XDG autostart
//! entry on Linux. Both start OpenDelay with `--minimized`.

/// Create or remove the autostart entry for the current executable.
pub fn set(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    imp::set(enabled, &exe.to_string_lossy()).map_err(|e| e.to_string())
}

#[cfg(windows)]
mod imp {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn set(enabled: bool, exe: &str) -> std::io::Result<()> {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN)?;
        if enabled {
            key.set_value("OpenDelay", &format!("\"{exe}\" --minimized"))
        } else {
            match key.delete_value("OpenDelay") {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    fn entry_path() -> Option<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config.join("autostart/opendelay.desktop"))
    }

    /// Quote an argument for a desktop entry `Exec` key.
    pub(super) fn quote(arg: &str) -> String {
        let mut s = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                s.push('\\');
            }
            s.push(c);
        }
        s.push('"');
        s
    }

    pub fn set(enabled: bool, exe: &str) -> std::io::Result<()> {
        let path = entry_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
        if !enabled {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        std::fs::create_dir_all(path.parent().expect("autostart folder"))?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=OpenDelay\nComment=Local RTMP relay with a switchable broadcast delay\nExec={} --minimized\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            quote(exe)
        );
        std::fs::write(path, entry)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn exec_quoting() {
        assert_eq!(super::imp::quote("/opt/Open Delay/opendelay"), "\"/opt/Open Delay/opendelay\"");
        assert_eq!(super::imp::quote("/a\"b$c"), "\"/a\\\"b\\$c\"");
    }
}
