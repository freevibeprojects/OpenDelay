// SPDX-License-Identifier: GPL-3.0-only
//! Update check (SPEC §11). Asks the project's release feed (GitHub Releases
//! "latest release" format) for a newer version. Nothing is downloaded until
//! the user asks, and a download is kept only if its Ed25519 signature
//! (`<asset>.sig`, base64) verifies against [`PUBLIC_KEY`].

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The project's release feed. Placeholder until the project is published.
pub const FEED_URL: &str = "https://example.invalid/opendelay/releases/latest";
/// Ed25519 key that signs release archives. Placeholder: with this all-zero
/// key every download fails verification (fail closed) until the real key is set.
pub const PUBLIC_KEY: [u8; 32] = [0; 32];
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub page_url: String,
    /// Archive for this platform and its signature, when the release has them.
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub signature_url: Option<String>,
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
    let asset = r.assets.iter().find(|a| a.name.contains(platform) && !a.name.ends_with(".sig"));
    let signature = asset.and_then(|a| r.assets.iter().find(|s| s.name == format!("{}.sig", a.name)));
    Ok(Some(UpdateInfo {
        version,
        notes: r.body,
        page_url: r.html_url,
        asset_name: asset.map(|a| a.name.clone()),
        asset_url: asset.map(|a| a.browser_download_url.clone()),
        signature_url: signature.map(|s| s.browser_download_url.clone()),
    }))
}

/// Ask the release feed whether a newer version exists.
pub fn check(current: &str) -> Result<Option<UpdateInfo>, String> {
    parse_release(&crate::net::get(FEED_URL, 1 << 20)?, current, platform())
}

pub fn verify(data: &[u8], signature_b64: &str, key: &[u8; 32]) -> Result<(), String> {
    let key = VerifyingKey::from_bytes(key).map_err(|_| "The update signing key is not configured.".to_owned())?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|_| "The update signature is malformed.".to_owned())?;
    let sig = Signature::from_slice(&raw).map_err(|_| "The update signature is malformed.".to_owned())?;
    key.verify_strict(data, &sig).map_err(|_| "The update signature does not match; the download was discarded.".into())
}

/// Download the archive and its signature, verify, and save it in `dir`.
/// Returns the saved file; nothing is written if verification fails.
pub fn download(info: &UpdateInfo, dir: &Path) -> Result<PathBuf, String> {
    let (Some(name), Some(url), Some(sig_url)) = (&info.asset_name, &info.asset_url, &info.signature_url) else {
        return Err("This release has no signed download for this system.".into());
    };
    let name = Path::new(name).file_name().ok_or("Invalid file name in the release feed.")?.to_owned();
    let data = crate::net::get(url, MAX_DOWNLOAD)?;
    let sig = crate::net::get(sig_url, 4096)?;
    verify(&data, &String::from_utf8_lossy(&sig), &PUBLIC_KEY)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    std::fs::write(&path, data).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

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
                {"name": "opendelay-0.3.0-linux-x86_64.tar.gz.sig", "browser_download_url": "https://example.invalid/l.sig"},
                {"name": "opendelay-0.3.0-windows-x86_64.zip", "browser_download_url": "https://example.invalid/w"}
            ]}"#;
        let u = parse_release(json, "0.1.0", "linux-x86_64").unwrap().unwrap();
        assert_eq!(u.version, "0.3.0");
        assert_eq!(u.signature_url.as_deref(), Some("https://example.invalid/l.sig"));
        let w = parse_release(json, "0.1.0", "windows-x86_64").unwrap().unwrap();
        assert_eq!((w.asset_url.as_deref(), w.signature_url), (Some("https://example.invalid/w"), None));
        assert_eq!(parse_release(json, "0.3.0", "linux-x86_64").unwrap(), None);
        assert!(parse_release(b"<html>", "0.1.0", "linux-x86_64").is_err());
    }

    #[test]
    fn signatures() {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let key = signing.verifying_key().to_bytes();
        let data = b"release archive";
        let sig = base64::engine::general_purpose::STANDARD.encode(signing.sign(data).to_bytes());
        assert!(verify(data, &sig, &key).is_ok());
        assert!(verify(b"tampered archive", &sig, &key).is_err());
        assert!(verify(data, "not base64!", &key).is_err());
        assert!(verify(data, &sig, &PUBLIC_KEY).is_err(), "placeholder key never verifies");
    }
}
