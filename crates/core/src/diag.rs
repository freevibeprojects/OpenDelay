// SPDX-License-Identifier: GPL-3.0-only
//! Per-session plain-text diagnostic logs (SPEC §9).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Newest log files kept in the logs folder.
pub const MAX_LOGS: usize = 50;
/// A session log stops growing at this size.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// (year, month, day, hour, minute, second) in UTC.
pub fn utc(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to civil date (proleptic Gregorian), after H. Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// `2026-10-14T21:05:09Z`
pub fn iso_utc(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = utc(t);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Owner-only permissions on Unix (settings and logs are personal data).
pub fn make_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Random-enough short id (not security relevant).
pub fn short_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let v = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:08x}", v as u32)
}

pub struct SessionLog {
    file: Mutex<Option<(File, u64)>>,
    started: Instant,
    /// Stream key, masked wherever it would appear in a log line or status.
    secret: Mutex<Option<String>>,
}

impl SessionLog {
    /// Create `<dir>/<YYYYMMDD-HHMMSS>-<connection id>.log` with a header.
    /// Without a directory (or on I/O errors) events are dropped.
    pub fn create(dir: Option<&Path>, connection_id: u64, header: &[(&str, String)]) -> SessionLog {
        let now = SystemTime::now();
        let file = dir.and_then(|dir| {
            fs::create_dir_all(dir).ok()?;
            make_private(dir);
            rotate(dir, MAX_LOGS - 1);
            let (y, mo, d, h, mi, s) = utc(now);
            let path = dir.join(format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{connection_id}.log"));
            let mut f = File::create(&path).ok()?;
            make_private(&path);
            let mut text = format!("OpenDelay session log\nStart: {}\nConnection: {connection_id}\n", iso_utc(now));
            for (k, v) in header {
                text.push_str(&format!("{k}: {v}\n"));
            }
            text.push('\n');
            f.write_all(text.as_bytes()).ok()?;
            Some((f, text.len() as u64))
        });
        SessionLog { file: Mutex::new(file), started: Instant::now(), secret: Mutex::new(None) }
    }

    /// Remember the stream key so it never reaches the log. Very short keys
    /// are not masked (they would garble ordinary words and are not secret).
    pub fn set_secret(&self, key: &str) {
        if key.len() >= 6 {
            *self.secret.lock().unwrap_or_else(|e| e.into_inner()) = Some(key.to_owned());
        }
    }

    pub fn redact(&self, text: &str) -> String {
        match &*self.secret.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(key) => text.replace(key.as_str(), "••••"),
            None => text.to_owned(),
        }
    }

    pub fn event(&self, text: impl AsRef<str>) {
        let line = format!("[{:>9.3}s] {}\n", self.started.elapsed().as_secs_f64(), self.redact(text.as_ref()));
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((f, size)) = guard.as_mut() {
            if *size + line.len() as u64 > MAX_LOG_BYTES {
                let _ = f.write_all(b"(log size limit reached)\n");
                *guard = None;
                return;
            }
            *size += line.len() as u64;
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn finish(&self) {
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((f, _)) = guard.as_mut() {
            let _ = writeln!(f, "\nEnd: {}", iso_utc(SystemTime::now()));
        }
        *guard = None;
    }
}

/// Session log files in `dir` (valid ids only), newest first.
pub fn list_logs(dir: &Path) -> Vec<PathBuf> {
    let is_log = |p: &PathBuf| {
        p.extension().is_some_and(|x| x == "log") && p.file_stem().and_then(|s| s.to_str()).is_some_and(valid_log_id)
    };
    let mut v: Vec<PathBuf> =
        fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(is_log).collect()).unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

/// Log ids are file stems `YYYYMMDD-HHMMSS-<connection>`; anything else
/// (separators, dots, …) is rejected so ids can never escape the logs folder.
pub fn valid_log_id(id: &str) -> bool {
    let b = id.as_bytes();
    (17..=40).contains(&b.len())
        && b.iter().enumerate().all(|(i, c)| if i == 8 || i == 15 { *c == b'-' } else { c.is_ascii_digit() })
}

/// Keep at most `keep` log files (names sort by start time).
fn rotate(dir: &Path, keep: usize) {
    for old in list_logs(dir).into_iter().skip(keep) {
        let _ = fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn civil_dates() {
        assert_eq!(utc(UNIX_EPOCH), (1970, 1, 1, 0, 0, 0));
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(utc(t), (2000, 2, 29, 0, 0, 0));
        let t = UNIX_EPOCH + Duration::from_secs(1_791_997_509);
        assert_eq!(iso_utc(t), "2026-10-14T17:05:09Z");
    }

    #[test]
    fn log_ids() {
        assert!(valid_log_id("20261014-170509-3"));
        for bad in ["../../etc/passwd", "20261014-170509-3/../x", "20261014-170509-", "20261014_170509-3", ""] {
            assert!(!valid_log_id(bad), "{bad}");
        }
    }

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("opendelay-diag-{}", short_id()));
        for i in 0..MAX_LOGS + 3 {
            // distinct names: same second, different connection ids
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("20000101-000000-{i:03}.log")), "x").unwrap();
        }
        let log = SessionLog::create(Some(&dir), 7, &[("Route", "twitch".into())]);
        log.event("hello");
        log.set_secret("live_123456_secret");
        log.event("rejected: Stream live_123456_secret is already publishing");
        log.finish();
        let logs = list_logs(&dir);
        assert_eq!(logs.len(), MAX_LOGS);
        let text = fs::read_to_string(&logs[0]).unwrap();
        assert!(text.contains("Connection: 7") && text.contains("Route: twitch") && text.contains("hello"));
        assert!(text.contains("End: "));
        assert!(!text.contains("live_123456_secret") && text.contains("Stream •••• is already"), "key masked");
        fs::remove_dir_all(dir).unwrap();
    }
}
