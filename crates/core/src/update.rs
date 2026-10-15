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
//! The signed version must be newer than the running one (no rollback) and the
//! archive must be the one listed for this platform with that SHA-256 (no
//! swapped files); the feed itself is not trusted.

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub page_url: String,
    /// Archive for this platform and the signed manifest, when the release has them.
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub manifest_url: Option<String>,
    pub signature_url: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
struct ManifestFile {
    platform: String,
    name: String,
    sha256: String,
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

/// `1.10.0` > `1.9.3`; missing components count as 0, suffixes are ignored.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(candidate), parts(current));
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

pub fn parse_release(json: &[u8], current: &str, platform: &str) -> Result<Option<UpdateInfo>, String> {
    let r: Release = serde_json::from_slice(json).map_err(|e| format!("Unexpected release feed: {e}"))?;
    let version = r.tag_name.trim_start_matches('v').to_owned();
    if !newer(&version, current) {
        return Ok(None);
    }
    let url = |name: &str| r.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone());
    let asset = r.assets.iter().find(|a| a.name.contains(platform) && !a.name.ends_with(".sig"));
    Ok(Some(UpdateInfo {
        version,
        notes: r.body.clone(),
        page_url: r.html_url.clone(),
        asset_name: asset.map(|a| a.name.clone()),
        asset_url: asset.map(|a| a.browser_download_url.clone()),
        manifest_url: url(MANIFEST),
        signature_url: url(&format!("{MANIFEST}.sig")),
    }))
}

/// Ask the release feed whether a newer version exists.
pub fn check(current: &str) -> Result<Option<UpdateInfo>, String> {
    parse_release(&crate::net::get(FEED_URL, 1 << 20)?, current, platform())
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

/// Check the manifest's signature, then return the SHA-256 it lists for the
/// archive `name` on `platform`, provided its version is newer than `current`.
pub fn check_manifest(
    manifest: &[u8],
    signature_b64: &str,
    key: &[u8; 32],
    current: &str,
    platform: &str,
    name: &str,
) -> Result<[u8; 32], String> {
    verify(manifest, signature_b64, key)?;
    let m: Manifest = serde_json::from_slice(manifest).map_err(|e| format!("Unexpected update manifest: {e}"))?;
    if !newer(&m.version, current) {
        return Err(format!(
            "The signed release ({}) is not newer than this version; nothing was downloaded.",
            m.version
        ));
    }
    let file = m
        .files
        .iter()
        .find(|f| f.platform == platform && f.name == name)
        .ok_or("The signed manifest does not list this download.")?;
    parse_sha256(&file.sha256).ok_or_else(|| "The update manifest has an invalid checksum.".into())
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

/// Copy `from` into `path` while hashing; the file is kept only if its
/// SHA-256 is `expected` (written to a temporary name, renamed when it matches).
pub fn save_verified(mut from: impl Read, expected: &[u8; 32], path: &Path) -> Result<(), String> {
    let tmp = path.with_extension("part");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut hash = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = from.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
        if hash.finish().as_ref() != expected {
            return Err("The download does not match the signed checksum; it was discarded.".to_owned());
        }
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Download the archive after checking the signed manifest, and save it in
/// `dir`. Returns the saved file; nothing is kept if verification fails.
pub fn download(info: &UpdateInfo, dir: &Path, current: &str) -> Result<PathBuf, String> {
    let (Some(name), Some(url), Some(manifest_url), Some(sig_url)) =
        (&info.asset_name, &info.asset_url, &info.manifest_url, &info.signature_url)
    else {
        return Err("This release has no signed download for this system.".into());
    };
    let file_name = Path::new(name).file_name().ok_or("Invalid file name in the release feed.")?.to_owned();
    let manifest = crate::net::get(manifest_url, 64 * 1024)?;
    let sig = crate::net::get(sig_url, 4096)?;
    let sha = check_manifest(&manifest, &String::from_utf8_lossy(&sig), &PUBLIC_KEY, current, platform(), name)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(file_name);
    save_verified(crate::net::get_reader(url, MAX_DOWNLOAD)?, &sha, &path)?;
    Ok(path)
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
        assert!(!newer("0.1.0-beta", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn release_feed() {
        let json = br#"{"tag_name": "v0.3.0", "body": "Notes", "html_url": "https://example.invalid/r/0.3.0",
            "assets": [
                {"name": "opendelay-0.3.0-linux-x86_64.tar.gz", "browser_download_url": "https://example.invalid/l"},
                {"name": "opendelay-0.3.0-windows-x86_64.zip", "browser_download_url": "https://example.invalid/w"},
                {"name": "manifest.json", "browser_download_url": "https://example.invalid/m"},
                {"name": "manifest.json.sig", "browser_download_url": "https://example.invalid/m.sig"}
            ]}"#;
        let u = parse_release(json, "0.1.0", "linux-x86_64").unwrap().unwrap();
        assert_eq!(u.version, "0.3.0");
        assert_eq!(u.asset_url.as_deref(), Some("https://example.invalid/l"));
        assert_eq!(u.manifest_url.as_deref(), Some("https://example.invalid/m"));
        assert_eq!(u.signature_url.as_deref(), Some("https://example.invalid/m.sig"));
        let w = parse_release(json, "0.1.0", "windows-x86_64").unwrap().unwrap();
        assert_eq!(w.asset_url.as_deref(), Some("https://example.invalid/w"));
        assert_eq!(parse_release(json, "0.3.0", "linux-x86_64").unwrap(), None);
        assert!(parse_release(b"<html>", "0.1.0", "linux-x86_64").is_err());
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
    fn signed_manifest() {
        let (pair, key) = signer();
        let name = "opendelay-0.3.0-linux-x86_64.tar.gz";
        let archive = b"the real archive";
        let sha = hex(digest::digest(&digest::SHA256, archive).as_ref());
        let manifest = |version: &str| {
            format!(
                r#"{{"version":"{version}","files":[{{"platform":"linux-x86_64","name":"{name}","sha256":"{sha}"}}]}}"#
            )
        };
        let ok = manifest("0.3.0");
        let expected = check_manifest(ok.as_bytes(), &sign(&pair, ok.as_bytes()), &key, "0.1.0", "linux-x86_64", name);
        assert_eq!(expected.map(|h| hex(&h)), Ok(sha.clone()));

        // Rollback: an older (validly signed) release is refused.
        let old = manifest("0.0.9");
        let r = check_manifest(old.as_bytes(), &sign(&pair, old.as_bytes()), &key, "0.1.0", "linux-x86_64", name);
        assert!(r.unwrap_err().contains("not newer"));
        // Mix-and-match: another platform's or another file's entry is not used.
        let sig = sign(&pair, ok.as_bytes());
        assert!(check_manifest(ok.as_bytes(), &sig, &key, "0.1.0", "windows-x86_64", name).is_err());
        assert!(check_manifest(ok.as_bytes(), &sig, &key, "0.1.0", "linux-x86_64", "other.tar.gz").is_err());
        // An edited manifest fails its signature.
        let edited = ok.replace("0.3.0\"", "9.0.0\"");
        assert!(check_manifest(edited.as_bytes(), &sig, &key, "0.1.0", "linux-x86_64", name).is_err());
    }

    #[test]
    fn download_is_kept_only_when_the_hash_matches() {
        let dir = std::env::temp_dir().join(format!("opendelay-update-{}", crate::diag::short_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.tar.gz");
        let expected: [u8; 32] = digest::digest(&digest::SHA256, b"the real archive").as_ref().try_into().unwrap();
        assert!(save_verified(&b"a swapped archive"[..], &expected, &path).is_err());
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing kept");
        save_verified(&b"the real archive"[..], &expected, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"the real archive");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
