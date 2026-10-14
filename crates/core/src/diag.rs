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

/// Random-enough short id (not security relevant).
pub fn short_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let v = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:08x}", v as u32)
}

pub struct SessionLog {
    file: Mutex<Option<(File, u64)>>,
    started: Instant,
}

impl SessionLog {
    /// Create `<dir>/<YYYYMMDD-HHMMSS>-<connection id>.log` with a header.
    /// Without a directory (or on I/O errors) events are dropped.
    pub fn create(dir: Option<&Path>, connection_id: u64, header: &[(&str, String)]) -> SessionLog {
        let now = SystemTime::now();
        let file = dir.and_then(|dir| {
            fs::create_dir_all(dir).ok()?;
            rotate(dir, MAX_LOGS - 1);
            let (y, mo, d, h, mi, s) = utc(now);
            let path = dir.join(format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{connection_id}.log"));
            let mut f = File::create(path).ok()?;
            let mut text = format!("OpenDelay session log\nStart: {}\nConnection: {connection_id}\n", iso_utc(now));
            for (k, v) in header {
                text.push_str(&format!("{k}: {v}\n"));
            }
            text.push('\n');
            f.write_all(text.as_bytes()).ok()?;
            Some((f, text.len() as u64))
        });
        SessionLog { file: Mutex::new(file), started: Instant::now() }
    }

    pub fn event(&self, text: impl AsRef<str>) {
        let line = format!("[{:>9.3}s] {}\n", self.started.elapsed().as_secs_f64(), text.as_ref());
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

/// Log files in `dir`, newest first.
pub fn list_logs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect())
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
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
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("opendelay-diag-{}", short_id()));
        for i in 0..MAX_LOGS + 3 {
            // distinct names: same second, different connection ids
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("20000101-000000-{i:03}.log")), "x").unwrap();
        }
        let log = SessionLog::create(Some(&dir), 7, &[("Route", "twitch".into())]);
        log.event("hello");
        log.finish();
        let logs = list_logs(&dir);
        assert_eq!(logs.len(), MAX_LOGS);
        let text = fs::read_to_string(&logs[0]).unwrap();
        assert!(text.contains("Connection: 7") && text.contains("Route: twitch") && text.contains("hello"));
        assert!(text.contains("End: "));
        fs::remove_dir_all(dir).unwrap();
    }
}
