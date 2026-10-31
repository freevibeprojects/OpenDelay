// SPDX-License-Identifier: GPL-3.0-only
//! Update check (SPEC §11). Asks the project's release feed (GitHub Releases
//! "latest release" format) for a newer version. Nothing is downloaded until
//! the user asks, and a download is kept only if it matches a signed manifest.
//!
//! Each release carries `manifest.json` and `manifest.json.sig` (base64
//! Ed25519 signature of the manifest's bytes, by [`PUBLIC_KEY`]):
//!
//! ```json
//! { "version": "0.3.0",
//!   "files": [{ "platform": "linux-x86_64", "name": "opendelay-0.3.0-linux-x86_64.tar.gz",
//!               "sha256": "<64 hex digits>" }] }
//! ```
//!
//! The signed version must be newer than the running one (no rollback) and not
//! older than the highest signed version seen before (no replay); the feed's
//! tag must agree with it. The archive must be the one listed for this platform
//! with that SHA-256 and size (no swapped files), downloaded only from the
//! project's release path; the feed itself is not trusted.
//!
//! On Windows a verified archive can be installed on request: the archive is
//! checked again, the executable in it must have the signed `exe_sha256`, and
//! it replaces the running one, which is kept as `.old` until the
//! next start (a running executable can be renamed but not deleted).

use base64::Engine;
use ring::digest;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The project's release feed. Release builds set `OPENDELAY_UPDATE_FEED`;
/// the default is a placeholder until the project is published.
pub const FEED_URL: &str = match option_env!("OPENDELAY_UPDATE_FEED") {
    Some(url) => url,
    None => "https://example.invalid/opendelay/releases/latest",
};

/// Ed25519 key that signs release archives, from `OPENDELAY_UPDATE_KEY` (64 hex
/// digits) at build time. Without it the key is all zero and every download
/// fails verification (fail closed).
pub const PUBLIC_KEY: [u8; 32] = match option_env!("OPENDELAY_UPDATE_KEY") {
    Some(hex) => parse_key(hex.as_bytes()),
    None => [0; 32],
};

const fn parse_key(hex: &[u8]) -> [u8; 32] {
    assert!(hex.len() == 64, "OPENDELAY_UPDATE_KEY must be 64 hex digits");
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("OPENDELAY_UPDATE_KEY must be hex"),
        }
    }
    let mut key = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        key[i] = nibble(hex[2 * i]) << 4 | nibble(hex[2 * i + 1]);
        i += 1;
    }
    key
}
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;
pub const MANIFEST: &str = "manifest.json";
/// The executable inside a Windows release archive.
pub const EXE: &str = "opendelay.exe";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// The signed version (the feed's tag must agree).
    pub version: String,
    pub notes: String,
    pub page_url: String,
    /// The archive for this platform, as the signed manifest names it.
    pub asset_name: String,
    pub asset_url: String,
    /// Signed archive size, when the manifest lists it.
    pub size: Option<u64>,
    #[serde(skip)]
    pub sha256: [u8; 32],
    /// Signed SHA-256 of the executable inside the archive (needed to install).
    #[serde(skip)]
    pub exe_sha256: Option<[u8; 32]>,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    platform: String,
    name: String,
    sha256: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    exe_sha256: Option<String>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Plain `1.2.3` (an optional leading `v`, one to four numbers); anything
/// else, such as `1.4.0-rc1`, is not a release version.
fn parse_version(v: &str) -> Option<Vec<u64>> {
    let parts = v
        .strip_prefix('v')
        .unwrap_or(v)
        .split('.')
        .map(|p| if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) { p.parse().ok() } else { None })
        .collect::<Option<Vec<u64>>>()?;
    (1..=4).contains(&parts.len()).then_some(parts)
}

/// `1.10.0` > `1.9.3`; missing components count as 0. An invalid version is
/// never newer, nor older.
pub fn newer(candidate: &str, current: &str) -> bool {
    let (Some(a), Some(b)) = (parse_version(candidate), parse_version(current)) else { return false };
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Release asset name fragment for this build.
pub fn platform() -> &'static str {
    if cfg!(windows) { "windows-x86_64" } else { "linux-x86_64" }
}

/// Where release files may be downloaded from: the GitHub release downloads
/// of the feed's repository, otherwise the feed's own host.
fn download_prefix(feed: &str) -> String {
    match feed.strip_prefix("https://api.github.com/repos/").and_then(|r| r.strip_suffix("/releases/latest")) {
        Some(repo) => format!("https://github.com/{repo}/releases/download/"),
        None => format!("{}/", feed.splitn(4, '/').take(3).collect::<Vec<_>>().join("/")),
    }
}

/// Check the manifest's signature and versions, and return its entry for `platform`.
/// The signed version must be newer than `current` and not older than `floor`,
/// the highest signed version seen before (no replay of older releases).
fn signed_file(
    manifest: &[u8],
    signature_b64: &str,
    key: &[u8; 32],
    current: &str,
    floor: &str,
    platform: &str,
) -> Result<(String, ManifestFile), String> {
    verify(manifest, signature_b64, key)?;
    let m: Manifest = serde_json::from_slice(manifest).map_err(|e| format!("Unexpected update manifest: {e}"))?;
    if !newer(&m.version, current) {
        return Err(format!(
            "The signed release ({}) is not newer than this version; nothing was downloaded.",
            m.version
        ));
    }
    if newer(floor, &m.version) {
        return Err(format!(
            "The signed release ({}) is older than one seen before ({floor}); it was refused.",
            m.version
        ));
    }
    let file = m
        .files
        .into_iter()
        .find(|f| f.platform == platform)
        .ok_or("The signed release has no download for this system.")?;
    let plain = !file.name.is_empty() && !file.name.contains(['/', '\\', ':']) && !file.name.starts_with('.');
    if !plain {
        return Err("The signed manifest has an invalid file name.".into());
    }
    Ok((m.version, file))
}

/// Combine the release feed (untrusted) with its signed manifest into the
/// update to offer: `None` when the feed has nothing newer than `current`.
fn offer(
    feed: &[u8],
    fetch: impl Fn(&str, u64) -> Result<Vec<u8>, String>,
    key: &[u8; 32],
    (current, floor): (&str, &str),
    platform: &str,
    prefix: &str,
) -> Result<Option<UpdateInfo>, String> {
    let r: Release = serde_json::from_slice(feed).map_err(|e| format!("Unexpected release feed: {e}"))?;
    let tag = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name);
    if !newer(tag, current) {
        return Ok(None);
    }
    // Release files come only from the project's own download path.
    let url = |name: &str| {
        r.assets
            .iter()
            .find(|a| a.name == name && a.browser_download_url.starts_with(prefix))
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| format!("This release has no signed download for this system ({name} is missing)."))
    };
    let manifest = fetch(&url(MANIFEST)?, 64 * 1024)?;
    let sig = fetch(&url(&format!("{MANIFEST}.sig"))?, 4096)?;
    let (version, file) = signed_file(&manifest, &String::from_utf8_lossy(&sig), key, current, floor, platform)?;
    if version != tag {
        return Err(format!(
            "The release feed ({tag}) and its signed manifest ({version}) disagree; nothing was offered."
        ));
    }
    let checksum = |hex: &str| parse_sha256(hex).ok_or("The update manifest has an invalid checksum.");
    Ok(Some(UpdateInfo {
        notes: r.body.clone(),
        page_url: r.html_url.clone(),
        asset_url: url(&file.name)?,
        size: file.size,
        sha256: checksum(&file.sha256)?,
        exe_sha256: file.exe_sha256.as_deref().map(checksum).transpose()?,
        asset_name: file.name,
        version,
    }))
}

/// Ask the release feed whether a newer signed version exists (see [`signed_file`]
/// for `floor`). Results are reused for 10 minutes unless `fresh`.
pub fn check(current: &str, floor: &str, fresh: bool) -> Result<Option<UpdateInfo>, String> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Option<UpdateInfo>)>> = Mutex::new(None);
    if !fresh
        && let Some((at, info)) = &*CACHE.lock().unwrap_or_else(|e| e.into_inner())
        && at.elapsed() < Duration::from_secs(600)
    {
        return Ok(info.clone());
    }
    let feed = crate::net::get(FEED_URL, 1 << 20)?;
    let info = offer(&feed, crate::net::get, &PUBLIC_KEY, (current, floor), platform(), &download_prefix(FEED_URL))?;
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), info.clone()));
    Ok(info)
}

pub fn verify(data: &[u8], signature_b64: &str, key: &[u8; 32]) -> Result<(), String> {
    if key == &[0; 32] {
        return Err("The update signing key is not configured.".into());
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|_| "The update signature is malformed.".to_owned())?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(data, &raw)
        .map_err(|_| "The update signature does not match; the download was discarded.".into())
}

fn parse_sha256(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in hex.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, data).as_ref().try_into().expect("32-byte digest")
}

/// Copy `from` into `path` while hashing; the file is kept only if it has the
/// signed SHA-256 (and size, when known). It is written under a unique
/// temporary name that no other process can write to meanwhile.
pub fn save_verified(mut from: impl Read, expected: &[u8; 32], size: Option<u64>, path: &Path) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.part", crate::diag::short_id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 1); // FILE_SHARE_READ only
        let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
        let mut hash = digest::Context::new(&digest::SHA256);
        let (mut buf, mut total) = (vec![0u8; 64 * 1024], 0u64);
        loop {
            let n = from.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            total += n as u64;
            hash.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
        if hash.finish().as_ref() != expected || size.is_some_and(|s| s != total) {
            return Err("The download does not match the signed checksum; it was discarded.".to_owned());
        }
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Download the archive of a checked update into `dir`. Returns the saved
/// file; nothing is kept if it does not match the signed checksum.
pub fn download(info: &UpdateInfo, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(&info.asset_name);
    let limit = info.size.unwrap_or(MAX_DOWNLOAD).min(MAX_DOWNLOAD);
    save_verified(crate::net::get_reader(&info.asset_url, limit)?, &info.sha256, info.size, &path)?;
    Ok(path)
}

/// The file in a ZIP archive whose path ends with `/name` (or is `name`).
/// Its content must still be checked against a signed checksum.
pub fn unzip(zip: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let bad = || "The update archive is damaged.".to_owned();
    let u16_at = |i: usize| zip.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or_else(bad);
    let u32_at =
        |i: usize| zip.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(bad);
    // End of central directory: the last record, before an optional comment.
    let end = (0..zip.len().saturating_sub(21)).rev().find(|&i| zip[i..].starts_with(b"PK\x05\x06")).ok_or_else(bad)?;
    let (count, mut at) = (u16_at(end + 10)?, u32_at(end + 16)?);
    for _ in 0..count {
        if !zip.get(at..).is_some_and(|r| r.starts_with(b"PK\x01\x02")) {
            return Err(bad());
        }
        let (method, size, len) = (u16_at(at + 10)?, u32_at(at + 20)?, u32_at(at + 24)?);
        let name_len = u16_at(at + 28)?;
        let entry = zip.get(at + 46..at + 46 + name_len).ok_or_else(bad)?;
        let offset = u32_at(at + 42)?;
        at += 46 + name_len + u16_at(at + 30)? + u16_at(at + 32)?;
        if entry != name.as_bytes() && !entry.ends_with(format!("/{name}").as_bytes()) {
            continue;
        }
        if !zip.get(offset..).is_some_and(|r| r.starts_with(b"PK\x03\x04")) || len as u64 > MAX_DOWNLOAD {
            return Err(bad());
        }
        let start = offset + 30 + u16_at(offset + 26)? + u16_at(offset + 28)?;
        let data = zip.get(start..start + size).ok_or_else(bad)?;
        return match method {
            0 => Ok(data.to_vec()),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, len).map_err(|_| bad()),
            _ => Err(bad()),
        };
    }
    Err(format!("The update archive has no {name}."))
}

/// Replace the executable `exe` with [`EXE`] from the downloaded `archive`.
/// The archive is checked again against the signed checksum (it sat in a
/// user-writable folder), and so is the executable taken from it. The old
/// executable is kept as `<exe>.old` (see [`remove_old`]).
pub fn install(info: &UpdateInfo, archive: &Path, exe: &Path) -> Result<(), String> {
    let exe_sha256 = info.exe_sha256.ok_or("The signed release does not list the program's checksum.")?;
    let zip = std::fs::read(archive).map_err(|e| e.to_string())?;
    if sha256(&zip) != info.sha256 {
        return Err("The downloaded update changed after it was verified; download it again.".into());
    }
    let new = unzip(&zip, EXE)?;
    if sha256(&new) != exe_sha256 {
        return Err("The program in the update does not match the signed checksum.".into());
    }
    let (next, old) = (exe.with_extension("new"), exe.with_extension("old"));
    let fail = |e: std::io::Error| format!("Could not replace {}: {e}", exe.display());
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&next)?;
        file.write_all(&new)?;
        file.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&next);
        return Err(fail(e));
    }
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&next);
        return Err(fail(e));
    }
    std::fs::rename(&next, exe).map_err(|e| match std::fs::rename(&old, exe) {
        Ok(()) => fail(e),
        Err(_) => format!("{} The previous version is {}, the new one {}.", fail(e), old.display(), next.display()),
    })
}

/// Delete the executable a previous [`install`] replaced.
pub fn remove_old(exe: &Path) {
    let _ = std::fs::remove_file(exe.with_extension("old"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn signer() -> (Ed25519KeyPair, [u8; 32]) {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let key = pair.public_key().as_ref().try_into().unwrap();
        (pair, key)
    }

    fn sign(pair: &Ed25519KeyPair, data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(pair.sign(data).as_ref())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn key_parsing() {
        let k = parse_key(b"00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF");
        assert_eq!((k[0], k[1], k[15], k[31]), (0x00, 0x11, 0xff, 0xff));
    }

    #[test]
    fn version_order() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("v1.10.0", "1.9.3"));
        assert!(newer("1.0.1", "1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
        // Anything but plain numbers is not a release version.
        for odd in ["0.2.0-beta", "1.4.0-rc1", " 2.0.0", "2..0", "99999999999999999999.0", "1.2.3.4.5", "", "v"] {
            assert!(!newer(odd, "0.1.0") && !newer("0.1.0", odd), "{odd:?}");
        }
    }

    #[test]
    fn download_paths() {
        let gh = "https://api.github.com/repos/someone/OpenDelay/releases/latest";
        assert_eq!(download_prefix(gh), "https://github.com/someone/OpenDelay/releases/download/");
        assert_eq!(download_prefix("https://example.invalid/opendelay/releases/latest"), "https://example.invalid/");
    }

    /// A release feed, signed manifest and archive as a project would publish them.
    struct Published {
        pair: Ed25519KeyPair,
        key: [u8; 32],
        archive: Vec<u8>,
        exe: Vec<u8>,
    }

    const PREFIX: &str = "https://dl.example/r/";

    impl Published {
        fn new() -> Published {
            let (pair, key) = signer();
            let exe = b"new version ".repeat(1000);
            let archive =
                zip(&[("opendelay-0.3.0/README.md", 0, b"readme"), ("opendelay-0.3.0/opendelay.exe", 8, &exe)]);
            Published { pair, key, archive, exe }
        }

        fn manifest(&self, version: &str, name: &str) -> String {
            format!(
                r#"{{"version":"{version}","files":[{{"platform":"windows-x86_64","name":"{name}","sha256":"{}","size":{},"exe_sha256":"{}"}}]}}"#,
                hex(&sha256(&self.archive)),
                self.archive.len(),
                hex(&sha256(&self.exe))
            )
        }

        fn feed(tag: &str, host: &str) -> String {
            let asset = |name: &str| format!(r#"{{"name":"{name}","browser_download_url":"{host}{name}"}}"#);
            let assets =
                ["opendelay-0.3.0-windows-x86_64.zip", "manifest.json", "manifest.json.sig"].map(asset).join(",");
            format!(
                r#"{{"tag_name":"{tag}","body":"Notes","html_url":"https://example.invalid/r","assets":[{assets}]}}"#
            )
        }

        /// `offer` with this release's manifest (signed version `signed`) behind a feed tagged `tag`.
        fn offer(&self, tag: &str, signed: &str, floor: &str, host: &str) -> Result<Option<UpdateInfo>, String> {
            let manifest = self.manifest(signed, "opendelay-0.3.0-windows-x86_64.zip");
            let sig = sign(&self.pair, manifest.as_bytes());
            let fetch = |url: &str, _limit: u64| match url.rsplit('/').next() {
                Some("manifest.json") => Ok(manifest.clone().into_bytes()),
                Some("manifest.json.sig") => Ok(sig.clone().into_bytes()),
                _ => Err(format!("unexpected fetch {url}")),
            };
            offer(Published::feed(tag, host).as_bytes(), fetch, &self.key, ("0.1.0", floor), "windows-x86_64", PREFIX)
        }
    }

    #[test]
    fn offers_only_signed_and_agreeing_releases() {
        let r = Published::new();
        let info = r.offer("v0.3.0", "0.3.0", "", PREFIX).unwrap().unwrap();
        assert_eq!((info.version.as_str(), info.asset_name.as_str()), ("0.3.0", "opendelay-0.3.0-windows-x86_64.zip"));
        assert_eq!(info.asset_url, format!("{PREFIX}opendelay-0.3.0-windows-x86_64.zip"));
        assert_eq!(
            (info.sha256, info.exe_sha256, info.size),
            (sha256(&r.archive), Some(sha256(&r.exe)), Some(r.archive.len() as u64))
        );
        assert_eq!(r.offer("v0.1.0", "0.3.0", "", PREFIX), Ok(None), "nothing newer in the feed");
        // The feed's tag must be the signed version.
        assert!(r.offer("v9.9.9", "0.3.0", "", PREFIX).unwrap_err().contains("disagree"));
        // Files from anywhere but the release path are not used.
        assert!(r.offer("v0.3.0", "0.3.0", "", "https://elsewhere.example/r/").unwrap_err().contains("missing"));
        // Rollback (older than running) and replay (older than seen before) are refused.
        assert!(r.offer("v0.0.9", "0.0.9", "", PREFIX).is_ok_and(|i| i.is_none()));
        let (sig_old, old) = {
            let m = r.manifest("0.0.9", "a.zip");
            (sign(&r.pair, m.as_bytes()), m)
        };
        assert!(
            signed_file(old.as_bytes(), &sig_old, &r.key, "0.1.0", "", "windows-x86_64")
                .unwrap_err()
                .contains("not newer")
        );
        assert!(r.offer("v0.3.0", "0.3.0", "0.4.0", PREFIX).unwrap_err().contains("seen before"));
        assert!(r.offer("v0.3.0", "0.3.0", "0.3.0", PREFIX).is_ok(), "the highest seen is still offered");
    }

    #[test]
    fn signed_manifest() {
        let r = Published::new();
        let ok = r.manifest("0.3.0", "x.zip");
        let sig = sign(&r.pair, ok.as_bytes());
        assert!(signed_file(ok.as_bytes(), &sig, &r.key, "0.1.0", "", "windows-x86_64").is_ok());
        // Another platform's entry is not used.
        assert!(signed_file(ok.as_bytes(), &sig, &r.key, "0.1.0", "", "linux-x86_64").is_err());
        // An edited manifest fails its signature.
        let edited = ok.replace("0.3.0\"", "9.0.0\"");
        assert!(signed_file(edited.as_bytes(), &sig, &r.key, "0.1.0", "", "windows-x86_64").is_err());
        assert!(
            signed_file(ok.as_bytes(), &sig, &PUBLIC_KEY, "0.1.0", "", "windows-x86_64").is_err(),
            "placeholder key"
        );
        // File names are plain names, never paths.
        for name in ["../x.zip", "a/b.zip", r"a\\b.zip", "C:x.zip", ".zip", ""] {
            let m = r.manifest("0.3.0", name);
            let s = sign(&r.pair, m.as_bytes());
            assert!(signed_file(m.as_bytes(), &s, &r.key, "0.1.0", "", "windows-x86_64").is_err(), "{name:?}");
        }
    }

    #[test]
    fn signatures() {
        let (pair, key) = signer();
        let data = b"release manifest";
        let sig = sign(&pair, data);
        assert!(verify(data, &sig, &key).is_ok());
        assert!(verify(b"tampered archive", &sig, &key).is_err());
        assert!(verify(data, "not base64!", &key).is_err());
        assert!(verify(data, &sig, &PUBLIC_KEY).is_err(), "placeholder key never verifies");
    }

    #[test]
    fn download_is_kept_only_when_hash_and_size_match() {
        let dir = std::env::temp_dir().join(format!("opendelay-update-{}", crate::diag::short_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.zip");
        let expected = sha256(b"the real archive");
        assert!(save_verified(&b"a swapped archive"[..], &expected, None, &path).is_err());
        assert!(save_verified(&b"the real archive"[..], &expected, Some(3), &path).is_err(), "wrong size");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing kept");
        save_verified(&b"the real archive"[..], &expected, Some(16), &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"the real archive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A ZIP archive of (name, method, content) entries, as release archives are built.
    fn zip(entries: &[(&str, u16, &[u8])]) -> Vec<u8> {
        let (mut out, mut dir) = (Vec::new(), Vec::new());
        for (name, method, content) in entries {
            let data = if *method == 8 { miniz_oxide::deflate::compress_to_vec(content, 6) } else { content.to_vec() };
            let sizes = [(data.len() as u32).to_le_bytes(), (content.len() as u32).to_le_bytes()].concat();
            let common = [&20u16.to_le_bytes()[..], &[0, 0], &method.to_le_bytes(), &[0; 8], &sizes].concat();
            let n = (name.len() as u16).to_le_bytes();
            dir.extend(
                [&b"PK\x01\x02\x14\x00"[..], &common, &n, &[0; 12], &(out.len() as u32).to_le_bytes(), name.as_bytes()]
                    .concat(),
            );
            out.extend([&b"PK\x03\x04"[..], &common, &n, &[0, 0], name.as_bytes(), &data].concat());
        }
        let count = (entries.len() as u16).to_le_bytes();
        let end = [
            &b"PK\x05\x06"[..],
            &[0; 4],
            &count,
            &count,
            &(dir.len() as u32).to_le_bytes(),
            &(out.len() as u32).to_le_bytes(),
            &[0, 0],
        ]
        .concat();
        [out, dir, end].concat()
    }

    #[test]
    fn unzip_finds_and_checks_entries() {
        let r = Published::new();
        assert_eq!(unzip(&r.archive, "README.md").unwrap(), b"readme");
        assert_eq!(unzip(&r.archive, "opendelay.exe").unwrap(), r.exe);
        assert!(unzip(&r.archive, "other.exe").is_err());
        assert!(unzip(&r.archive[..r.archive.len() / 2], "opendelay.exe").is_err(), "damaged archive");
        let mut moved = r.archive.clone();
        moved[0] = b'X'; // the README's local header no longer starts where the directory says
        assert!(unzip(&moved, "README.md").is_err());
        // Damaged archives never panic (cheap stand-in for the fuzz target).
        let mut seed = 1u64;
        for _ in 0..5000 {
            let mut z = r.archive.clone();
            for _ in 0..4 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let i = (seed >> 33) as usize % z.len();
                z[i] = (seed >> 13) as u8;
            }
            let _ = unzip(&z, EXE);
        }
    }

    #[test]
    fn install_replaces_the_executable_only_with_the_signed_one() {
        let r = Published::new();
        let info = r.offer("v0.3.0", "0.3.0", "", PREFIX).unwrap().unwrap();
        let dir = std::env::temp_dir().join(format!("opendelay-install-{}", crate::diag::short_id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A renamed executable is updated too.
        let (exe, zip_path) = (dir.join("OpenDelay (1).exe"), dir.join("update.zip"));
        std::fs::write(&exe, b"old version").unwrap();

        // Swapped after the download was verified: refused, nothing changes.
        let other = zip(&[("x/opendelay.exe", 0, b"evil")]);
        std::fs::write(&zip_path, &other).unwrap();
        assert!(install(&info, &zip_path, &exe).unwrap_err().contains("changed after"));
        // An archive with the signed hash but no signed program checksum: refused.
        std::fs::write(&zip_path, &r.archive).unwrap();
        let unsigned_exe = UpdateInfo { exe_sha256: None, ..info.clone() };
        assert!(install(&unsigned_exe, &zip_path, &exe).is_err());
        // A program that differs from its signed checksum: refused.
        let wrong_exe = UpdateInfo { exe_sha256: Some([0; 32]), ..info.clone() };
        assert!(install(&wrong_exe, &zip_path, &exe).unwrap_err().contains("does not match"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old version");
        assert!(!exe.with_extension("new").exists() && !exe.with_extension("old").exists());

        install(&info, &zip_path, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), r.exe);
        assert_eq!(std::fs::read(exe.with_extension("old")).unwrap(), b"old version");
        remove_old(&exe);
        assert!(!exe.with_extension("old").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
