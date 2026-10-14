// SPDX-License-Identifier: GPL-3.0-only
//! Small OS integrations: opening URLs, files and folders.

use std::process::Command;

/// Open a URL, file or folder with the system's default handler.
pub fn open(target: &str) {
    #[cfg(windows)]
    let result = Command::new("explorer.exe").arg(target).spawn();
    #[cfg(not(windows))]
    let result = Command::new("xdg-open").arg(target).spawn();
    if let Err(e) = result {
        eprintln!("could not open {target}: {e}");
    }
}

/// UI language to use for "system default": a bundled language matching the
/// system locale, or "" (English).
pub fn system_language(available: &[(&str, &str)]) -> String {
    let locale = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
    let base = locale.split(['-', '_', '.', '@']).next().unwrap_or("");
    available.iter().find(|(code, _)| *code == base).map(|(code, _)| code.to_string()).unwrap_or_default()
}
