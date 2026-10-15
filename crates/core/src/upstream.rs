// SPDX-License-Identifier: GPL-3.0-only
//! Upstream (platform ingest) side: TCP/TLS connect, handshake and the publish
//! command sequence, performed on behalf of the encoder.

use crate::conn::{self, BoxIo, MsgReader, MsgWriter, OUT_CHUNK_SIZE};
use crate::url::UpstreamUrl;
use opendelay_rtmp::amf0::Amf0;
use opendelay_rtmp::message::user_control;
use opendelay_rtmp::{Command, Message, msg_type};
use std::fmt;
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, pki_types::ServerName};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum UpstreamError {
    Io(io::Error),
    Timeout(&'static str),
    /// The server answered with an error status (`code`, `description`).
    Rejected(String, String),
    Protocol(String),
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamError::Io(e) => write!(f, "network error: {e}"),
            UpstreamError::Timeout(stage) => write!(f, "timed out during {stage}"),
            UpstreamError::Rejected(code, d) if d.is_empty() => write!(f, "rejected by server: {code}"),
            UpstreamError::Rejected(code, d) => write!(f, "rejected by server: {code} ({d})"),
            UpstreamError::Protocol(s) => write!(f, "protocol error: {s}"),
        }
    }
}
impl std::error::Error for UpstreamError {}

impl From<io::Error> for UpstreamError {
    fn from(e: io::Error) -> Self {
        UpstreamError::Io(e)
    }
}

/// TLS client configuration using the operating system's trust store (ring provider).
/// The operating system's trusted root certificates, read once and shared by
/// the relay's TLS and the HTTPS client.
pub fn native_roots() -> &'static [rustls::pki_types::CertificateDer<'static>] {
    static ROOTS: OnceLock<Vec<rustls::pki_types::CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| rustls_native_certs::load_native_certs().certs)
}

pub fn default_tls() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        for cert in native_roots() {
            let _ = roots.add(cert.clone());
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

/// `info.code` / `info.description` of a status-like command.
pub fn status_of(cmd: &Command) -> (String, String, String) {
    let info = cmd.args.iter().find(|a| a.get("code").is_some()).unwrap_or(&Amf0::Null);
    let s = |k| info.get(k).and_then(Amf0::as_str).unwrap_or("").to_owned();
    (s("level"), s("code"), s("description"))
}

/// The encoder's `connect` command object, pointed at the upstream app.
pub fn connect_object(encoder_object: &Amf0, url: &UpstreamUrl) -> Amf0 {
    let mut obj = match encoder_object {
        Amf0::Object(_) => encoder_object.clone(),
        _ => Amf0::Object(vec![
            ("type".into(), Amf0::str("nonprivate")),
            ("flashVer".into(), Amf0::str("FMLE/3.0 (compatible; OpenDelay)")),
        ]),
    };
    obj.set("app", Amf0::String(url.app.clone()));
    obj.set("tcUrl", Amf0::String(url.tc_url()));
    obj
}

pub struct Upstream {
    pub reader: MsgReader,
    pub writer: MsgWriter,
    /// Message stream id returned by `createStream`.
    pub stream_id: u32,
    next_tx: f64,
}

impl Upstream {
    /// Open TCP (+TLS), handshake and send `connect`. Returns the server's `_result`.
    pub async fn connect(
        url: &UpstreamUrl,
        connect_obj: &Amf0,
        tls: &Arc<ClientConfig>,
    ) -> Result<(Upstream, Command), UpstreamError> {
        let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((url.host.as_str(), url.port)))
            .await
            .map_err(|_| UpstreamError::Timeout("TCP connect"))??;
        tcp.set_nodelay(true)?;
        let mut io: BoxIo = if url.tls {
            let name = ServerName::try_from(url.host.clone())
                .map_err(|_| UpstreamError::Protocol("invalid TLS server name".into()))?;
            let stream = timeout(CONNECT_TIMEOUT, TlsConnector::from(tls.clone()).connect(name, tcp))
                .await
                .map_err(|_| UpstreamError::Timeout("TLS handshake"))??;
            Box::new(stream)
        } else {
            Box::new(tcp)
        };
        timeout(CONNECT_TIMEOUT, conn::client_handshake(&mut io))
            .await
            .map_err(|_| UpstreamError::Timeout("RTMP handshake"))??;
        let (reader, mut writer) = conn::split(io);
        writer.queue_chunk_size(OUT_CHUNK_SIZE);
        writer.send(&Command::new("connect", 1.0, connect_obj.clone(), vec![]).to_message(0)).await?;
        let mut up = Upstream { reader, writer, stream_id: 0, next_tx: 2.0 };
        let reply = up.wait_for("connect", |c| c.transaction_id == 1.0 && is_reply(c)).await?;
        if reply.name != "_result" {
            let (_, code, desc) = status_of(&reply);
            return Err(UpstreamError::Rejected(or(code, "NetConnection.Connect.Rejected"), desc));
        }
        Ok((up, reply))
    }

    /// releaseStream, FCPublish, createStream, publish. Returns the `onStatus` info object.
    pub async fn publish(&mut self, key: &str) -> Result<Amf0, UpstreamError> {
        let key_arg = || vec![Amf0::str(key)];
        let release = self.tx();
        self.writer.queue(&Command::new("releaseStream", release, Amf0::Null, key_arg()).to_message(0));
        let fc = self.tx();
        self.writer.queue(&Command::new("FCPublish", fc, Amf0::Null, key_arg()).to_message(0));
        let create = self.tx();
        self.writer.send(&Command::new("createStream", create, Amf0::Null, vec![]).to_message(0)).await?;
        let reply = self.wait_for("createStream", |c| c.transaction_id == create && is_reply(c)).await?;
        let stream_id = reply.args.first().and_then(Amf0::as_f64).filter(|_| reply.name == "_result");
        let Some(stream_id) = stream_id else {
            let (_, code, desc) = status_of(&reply);
            return Err(UpstreamError::Rejected(or(code, "createStream failed"), desc));
        };
        self.stream_id = stream_id as u32;

        let publish = self.tx();
        let args = vec![Amf0::str(key), Amf0::str("live")];
        self.writer.send(&Command::new("publish", publish, Amf0::Null, args).to_message(self.stream_id)).await?;
        let reply = self
            .wait_for("publish", |c| {
                c.name == "onStatus" && status_of(c).1.starts_with("NetStream.Publish")
                    || c.name == "_error" && c.transaction_id == publish
            })
            .await?;
        let (level, code, desc) = status_of(&reply);
        if code != "NetStream.Publish.Start" || level == "error" {
            return Err(UpstreamError::Rejected(or(code, "NetStream.Publish.Failed"), desc));
        }
        Ok(reply.args.into_iter().find(|a| a.get("code").is_some()).unwrap_or(Amf0::Null))
    }

    /// Full sequence used for recovery: connect + publish.
    pub async fn open(
        url: &UpstreamUrl,
        connect_obj: &Amf0,
        key: &str,
        tls: &Arc<ClientConfig>,
    ) -> Result<Upstream, UpstreamError> {
        let (mut up, _) = Self::connect(url, connect_obj, tls).await?;
        up.publish(key).await?;
        Ok(up)
    }

    /// FCUnpublish + deleteStream, then close (best effort).
    pub async fn close(&mut self, key: &str) {
        let fc = self.tx();
        self.writer.queue(&Command::new("FCUnpublish", fc, Amf0::Null, vec![Amf0::str(key)]).to_message(0));
        let del = self.tx();
        let args = vec![Amf0::Number(self.stream_id as f64)];
        self.writer.queue(&Command::new("deleteStream", del, Amf0::Null, args).to_message(0));
        let _ = timeout(Duration::from_secs(2), self.writer.shutdown()).await;
    }

    fn tx(&mut self) -> f64 {
        let t = self.next_tx;
        self.next_tx += 1.0;
        t
    }

    /// Read until a command matches, answering pings and acknowledgements meanwhile.
    async fn wait_for(
        &mut self,
        stage: &'static str,
        want: impl Fn(&Command) -> bool,
    ) -> Result<Command, UpstreamError> {
        let fut = async {
            loop {
                let msg = self.reader.read().await?.ok_or_else(|| {
                    UpstreamError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed the connection"))
                })?;
                if let Some(reply) = control_reply(&msg) {
                    self.writer.send(&reply).await?;
                }
                if let Some(ack) = self.reader.ack_due() {
                    self.writer.send(&ack).await?;
                }
                if matches!(msg.type_id, msg_type::COMMAND_AMF0 | msg_type::COMMAND_AMF3) {
                    let cmd = Command::parse(&msg).map_err(|e| UpstreamError::Protocol(e.to_string()))?;
                    if want(&cmd) {
                        return Ok(cmd);
                    }
                }
            }
        };
        timeout(COMMAND_TIMEOUT, fut).await.map_err(|_| UpstreamError::Timeout(stage))?
    }
}

fn is_reply(c: &Command) -> bool {
    c.name == "_result" || c.name == "_error"
}

fn or(s: String, default: &str) -> String {
    if s.is_empty() { default.to_owned() } else { s }
}

/// Reply owed for a protocol message (ping → pong). Shared by both legs.
pub fn control_reply(msg: &Message) -> Option<Message> {
    if msg.type_id == msg_type::USER_CONTROL && msg.payload.len() >= 6 {
        let event = u16::from_be_bytes([msg.payload[0], msg.payload[1]]);
        if event == user_control::PING_REQUEST {
            return Some(Message::user_control(user_control::PING_RESPONSE, &msg.payload[2..6]));
        }
    }
    None
}
