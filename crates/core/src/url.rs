// SPDX-License-Identifier: GPL-3.0-only
//! Upstream server URLs (`rtmp://host[:port]/app`, `rtmps://...`).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// `host[:port]` exactly as written (used for tcUrl).
    pub authority: String,
    /// Application path without surrounding slashes (may contain `/` and a query).
    pub app: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    Scheme,
    NoHost,
    BadHost,
    BadPort,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UrlError::Scheme => "Only rtmp:// and rtmps:// server URLs are supported.",
            UrlError::NoHost => "The server URL has no host name.",
            UrlError::BadHost => "The server URL has an invalid host name.",
            UrlError::BadPort => "The server URL has an invalid port.",
        })
    }
}
impl std::error::Error for UrlError {}

impl UrlError {
    pub fn code(&self) -> &'static str {
        match self {
            UrlError::Scheme => "url_scheme",
            UrlError::NoHost => "url_no_host",
            UrlError::BadHost => "url_bad_host",
            UrlError::BadPort => "url_bad_port",
        }
    }
}

impl UpstreamUrl {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        let s = input.trim();
        let (scheme, rest) = s.split_once("://").ok_or(UrlError::Scheme)?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "rtmp" => false,
            "rtmps" => true,
            _ => return Err(UrlError::Scheme),
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(UrlError::NoHost);
        }
        if authority.contains('@') {
            return Err(UrlError::BadHost);
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or(UrlError::BadHost)?;
            (h, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(UrlError::NoHost);
        }
        if !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':')) {
            return Err(UrlError::BadHost);
        }
        let port = match port {
            None => {
                if tls {
                    443
                } else {
                    1935
                }
            }
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or(UrlError::BadPort)?,
        };
        Ok(UpstreamUrl {
            tls,
            host: host.to_ascii_lowercase(),
            port,
            authority: authority.to_owned(),
            app: path.trim_matches('/').to_owned(),
        })
    }

    pub fn tc_url(&self) -> String {
        format!("{}://{}/{}", if self.tls { "rtmps" } else { "rtmp" }, self.authority, self.app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_ingests() {
        let u = UpstreamUrl::parse("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.app.as_str()), (false, "live.twitch.tv", 1935, "app"));
        assert_eq!(u.tc_url(), "rtmp://live.twitch.tv/app");

        let u = UpstreamUrl::parse(" RTMPS://Ingest.Example.com:443/live/ ").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.app.as_str()), (true, "ingest.example.com", 443, "live"));
        assert_eq!(u.tc_url(), "rtmps://Ingest.Example.com:443/live");

        assert_eq!(UpstreamUrl::parse("rtmps://h").unwrap().port, 443);
        assert_eq!(UpstreamUrl::parse("rtmp://[::1]:1936/a/b?x=1").unwrap().app, "a/b?x=1");
        assert_eq!(UpstreamUrl::parse("rtmp://[::1]:1936/a").unwrap().host, "::1");
    }

    #[test]
    fn rejects_bad_urls() {
        assert_eq!(UpstreamUrl::parse("http://x/app"), Err(UrlError::Scheme));
        assert_eq!(UpstreamUrl::parse("x/app"), Err(UrlError::Scheme));
        assert_eq!(UpstreamUrl::parse("rtmp:///app"), Err(UrlError::NoHost));
        assert_eq!(UpstreamUrl::parse("rtmp://:1935/app"), Err(UrlError::NoHost));
        assert_eq!(UpstreamUrl::parse("rtmp://h:0/app"), Err(UrlError::BadPort));
        assert_eq!(UpstreamUrl::parse("rtmp://h:99999/app"), Err(UrlError::BadPort));
        assert_eq!(UpstreamUrl::parse("rtmp://u:p@h/app"), Err(UrlError::BadHost));
        assert_eq!(UpstreamUrl::parse("rtmp://bad host/app"), Err(UrlError::BadHost));
    }
}
