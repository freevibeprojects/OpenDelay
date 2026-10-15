// SPDX-License-Identifier: GPL-3.0-only
//! Outgoing HTTPS for the Twitch ingest list and the update check (blocking;
//! call from a worker thread). Uses the operating system's trust store and the
//! same ring-based TLS stack as the RTMP relay. Sends no identifying data
//! beyond a generic User-Agent.

use crate::routes::{TWITCH_AUTO, validate_twitch_server};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;
use ureq::tls::{Certificate, RootCerts, TlsConfig};

/// Twitch's public ingest server list.
pub const TWITCH_INGESTS_URL: &str = "https://ingest.twitch.tv/ingests";

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let roots: Vec<Certificate<'static>> =
            crate::upstream::native_roots().iter().map(|c| Certificate::from_der(c.as_ref()).to_owned()).collect();
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!("OpenDelay/", env!("CARGO_PKG_VERSION")))
            .tls_config(TlsConfig::builder().root_certs(RootCerts::new_with_certs(&roots)).build())
            .build();
        ureq::Agent::new_with_config(config)
    })
}

/// GET `url`, reading at most `limit` bytes.
pub fn get(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TwitchIngest {
    pub name: String,
    /// Server URL without the stream key, e.g. `rtmp://fra.contribute.live-video.net/app`.
    pub url: String,
}

#[derive(Deserialize)]
struct IngestList {
    ingests: Vec<IngestEntry>,
}

#[derive(Deserialize)]
struct IngestEntry {
    name: String,
    url_template: Option<String>,
}

/// Parse Twitch's ingest list, keeping only valid Twitch servers on port 1935.
pub fn parse_twitch_ingests(json: &[u8]) -> Result<Vec<TwitchIngest>, String> {
    let list: IngestList = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    Ok(list
        .ingests
        .into_iter()
        .filter_map(|i| {
            let url = i.url_template?.replace("{stream_key}", "").trim_end_matches('/').to_owned();
            (validate_twitch_server(&url).is_ok() && url != TWITCH_AUTO).then_some(TwitchIngest { name: i.name, url })
        })
        .collect())
}

/// GET `url` as a stream of at most `limit` bytes (for large downloads,
/// which get a generous time limit).
pub fn get_reader(url: &str, limit: u64) -> Result<impl std::io::Read, String> {
    let response = agent()
        .get(url)
        .config()
        .timeout_global(Some(Duration::from_secs(1800)))
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    Ok(response.into_body().into_with_config().limit(limit).reader())
}

/// Twitch's ingest list, fetched at most once per 10 minutes unless `fresh`.
pub fn twitch_ingests(fresh: bool) -> Result<Vec<TwitchIngest>, String> {
    use std::sync::Mutex;
    use std::time::Instant;
    static CACHE: Mutex<Option<(Instant, Vec<TwitchIngest>)>> = Mutex::new(None);
    if !fresh
        && let Some((at, list)) = &*CACHE.lock().unwrap_or_else(|e| e.into_inner())
        && at.elapsed() < Duration::from_secs(600)
    {
        return Ok(list.clone());
    }
    let list = parse_twitch_ingests(&get(TWITCH_INGESTS_URL, 1 << 20)?)?;
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), list.clone()));
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_list() {
        let json = br#"{"ingests":[
            {"_id": 1, "name": "Europe: Germany, Frankfurt", "url_template": "rtmp://fra.contribute.live-video.net/app/{stream_key}", "priority": 1},
            {"_id": 2, "name": "Elsewhere", "url_template": "rtmp://evil.example/app/{stream_key}"},
            {"_id": 3, "name": "Secure only", "url_template": "rtmps://fra.contribute.live-video.net:443/app/{stream_key}"},
            {"_id": 4, "name": "No template"}
        ]}"#;
        let list = parse_twitch_ingests(json).unwrap();
        assert_eq!(
            list,
            vec![TwitchIngest {
                name: "Europe: Germany, Frankfurt".into(),
                url: "rtmp://fra.contribute.live-video.net/app".into()
            }]
        );
        assert!(parse_twitch_ingests(b"[]").is_err());
    }
}
