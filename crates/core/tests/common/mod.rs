// SPDX-License-Identifier: GPL-3.0-only
//! Test doubles: an in-process mock RTMP ingest server and a mock encoder.
#![allow(dead_code)]

use opendelay_core::conn::{self, BoxIo, MsgReader, MsgWriter};
use opendelay_rtmp::amf0::{self, Amf0};
use opendelay_rtmp::message::status_object;
use opendelay_rtmp::{Command, Message, msg_type};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::rustls;

pub const WAIT: Duration = Duration::from_secs(5);

/// What the mock ingest saw.
#[derive(Debug, Clone)]
pub enum Seen {
    Connect { conn: usize, app: String, tc_url: String },
    Publish { conn: usize, key: String },
    Media { conn: usize, msg: Message },
    Unpublish { conn: usize },
    Closed { conn: usize },
}

#[derive(Default)]
pub struct Behaviour {
    /// Publishing with this key is refused with NetStream.Publish.BadName.
    pub reject_key: Option<String>,
    /// The first connection is dropped after this many media messages.
    pub drop_first_after: Option<usize>,
}

pub struct MockIngest {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl MockIngest {
    pub async fn start(b: Behaviour) -> MockIngest {
        Self::start_inner(b, None).await
    }

    pub async fn start_tls(b: Behaviour) -> MockIngest {
        use rustls::pki_types::pem::PemObject;
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(format!("{dir}/cert.pem"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls::pki_types::PrivateKeyDer::from_pem_file(format!("{dir}/key.pem")).unwrap();
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        Self::start_inner(b, Some(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))).await
    }

    async fn start_inner(b: Behaviour, tls: Option<tokio_rustls::TlsAcceptor>) -> MockIngest {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let b = Arc::new(b);
        let s = seen.clone();
        tokio::spawn(async move {
            let mut conn = 0;
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let io: BoxIo = match &tls {
                    Some(acc) => match acc.accept(tcp).await {
                        Ok(t) => Box::new(t),
                        Err(_) => continue,
                    },
                    None => Box::new(tcp),
                };
                tokio::spawn(serve_ingest(io, conn, s.clone(), b.clone()));
                conn += 1;
            }
        });
        MockIngest { addr, seen }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn media(&self, conn: usize) -> Vec<Message> {
        self.seen()
            .into_iter()
            .filter_map(|s| match s {
                Seen::Media { conn: c, msg } if c == conn => Some(msg),
                _ => None,
            })
            .collect()
    }

    /// Wait until `pred` holds over what was seen.
    pub async fn wait_for(&self, what: &str, pred: impl Fn(&[Seen]) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !pred(&self.seen()) {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}: {:#?}", self.seen().len());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn serve_ingest(mut io: BoxIo, conn: usize, seen: Arc<Mutex<Vec<Seen>>>, b: Arc<Behaviour>) {
    if conn::server_handshake(&mut io, 0).await.is_err() {
        return;
    }
    let (mut r, mut w) = conn::split(io);
    w.queue_chunk_size(4096);
    let mut media = 0usize;
    let push = |s: Seen| seen.lock().unwrap().push(s);
    while let Ok(Some(msg)) = r.read().await {
        if msg.is_media() {
            push(Seen::Media { conn, msg });
            media += 1;
            if conn == 0 && b.drop_first_after == Some(media) {
                break;
            }
            continue;
        }
        if !matches!(msg.type_id, msg_type::COMMAND_AMF0 | msg_type::COMMAND_AMF3) {
            continue;
        }
        let cmd = Command::parse(&msg).unwrap();
        let tx = cmd.transaction_id;
        let reply = |name: &str, args: Vec<Amf0>| Command::new(name, tx, Amf0::Null, args).to_message(0);
        match cmd.name.as_str() {
            "connect" => {
                let get = |k| cmd.object.get(k).and_then(Amf0::as_str).unwrap_or("").to_owned();
                push(Seen::Connect { conn, app: get("app"), tc_url: get("tcUrl") });
                w.queue(&Message::window_ack_size(2_500_000));
                let props = Amf0::Object(vec![("fmsVer".into(), Amf0::str("MockIngest/1.0"))]);
                let info = status_object("status", "NetConnection.Connect.Success", "ok");
                w.queue(&Command::new("_result", tx, props, vec![info]).to_message(0));
            }
            "releaseStream" | "FCPublish" => w.queue(&reply("_result", vec![Amf0::Undefined])),
            "createStream" => w.queue(&reply("_result", vec![Amf0::Number(7.0)])),
            "publish" => {
                let key = cmd.arg_str(0).unwrap_or("").to_owned();
                push(Seen::Publish { conn, key: key.clone() });
                assert_eq!(msg.stream_id, 7, "publish must use the stream id from createStream");
                let info = if b.reject_key.as_deref() == Some(key.as_str()) {
                    status_object("error", "NetStream.Publish.BadName", "bad stream key")
                } else {
                    status_object("status", "NetStream.Publish.Start", "publishing")
                };
                w.queue(&Command::new("onStatus", 0.0, Amf0::Null, vec![info]).to_message(7));
            }
            "FCUnpublish" | "deleteStream" => push(Seen::Unpublish { conn }),
            _ => {}
        }
        if w.flush().await.is_err() {
            break;
        }
    }
    push(Seen::Closed { conn });
}

pub struct Encoder {
    pub r: MsgReader,
    pub w: MsgWriter,
    pub stream_id: u32,
}

impl Encoder {
    /// Connect + handshake + `connect` command; returns the reply command.
    pub async fn connect(addr: SocketAddr, app: &str) -> (Encoder, Command) {
        Self::connect_from(addr, app, "127.0.0.1").await
    }

    /// Connect from a specific local address (e.g. 127.0.0.2 to act as another machine).
    pub async fn connect_from(addr: SocketAddr, app: &str, local_ip: &str) -> (Encoder, Command) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(format!("{local_ip}:0").parse().unwrap()).unwrap();
        let tcp = socket.connect(addr).await.unwrap();
        let mut io: BoxIo = Box::new(tcp);
        conn::client_handshake(&mut io).await.unwrap();
        let (r, mut w) = conn::split(io);
        w.queue_chunk_size(4096);
        let obj = Amf0::Object(vec![
            ("app".into(), Amf0::str(app)),
            ("type".into(), Amf0::str("nonprivate")),
            ("flashVer".into(), Amf0::str("FMLE/3.0 (compatible; MockEncoder)")),
            ("tcUrl".into(), Amf0::String(format!("rtmp://{addr}/{app}"))),
        ]);
        w.send(&Command::new("connect", 1.0, obj, vec![]).to_message(0)).await.unwrap();
        let mut enc = Encoder { r, w, stream_id: 0 };
        let reply = enc.command(|c| c.transaction_id == 1.0).await.expect("connect reply");
        (enc, reply)
    }

    /// Next command matching `pred`; `None` if the connection closed.
    pub async fn command(&mut self, pred: impl Fn(&Command) -> bool) -> Option<Command> {
        tokio::time::timeout(WAIT, async {
            loop {
                let msg = self.r.read().await.ok()??;
                if matches!(msg.type_id, msg_type::COMMAND_AMF0 | msg_type::COMMAND_AMF3) {
                    let cmd = Command::parse(&msg).unwrap();
                    if pred(&cmd) {
                        return Some(cmd);
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for a command")
    }

    /// The OBS-style publish sequence; returns the `onStatus` info object.
    pub async fn publish(&mut self, key: &str) -> Amf0 {
        let k = || vec![Amf0::str(key)];
        self.w.queue(&Command::new("releaseStream", 2.0, Amf0::Null, k()).to_message(0));
        self.w.queue(&Command::new("FCPublish", 3.0, Amf0::Null, k()).to_message(0));
        self.w.send(&Command::new("createStream", 4.0, Amf0::Null, vec![]).to_message(0)).await.unwrap();
        let created = self.command(|c| c.transaction_id == 4.0).await.unwrap();
        self.stream_id = created.args[0].as_f64().unwrap() as u32;
        let args = vec![Amf0::str(key), Amf0::str("live")];
        self.w.send(&Command::new("publish", 5.0, Amf0::Null, args).to_message(self.stream_id)).await.unwrap();
        let status = self.command(|c| c.name == "onStatus").await.unwrap();
        status.args.into_iter().next().unwrap()
    }

    pub async fn send(&mut self, msgs: &[Message]) {
        for m in msgs {
            let mut m = m.clone();
            m.stream_id = self.stream_id;
            self.w.queue(&m);
        }
        self.w.flush().await.unwrap();
    }

    /// Send messages in real time: each at `start + timestamp - from_ts`.
    pub async fn send_paced(&mut self, msgs: &[Message], start: tokio::time::Instant, from_ts: u32) {
        for m in msgs {
            tokio::time::sleep_until(start + Duration::from_millis((m.timestamp - from_ts) as u64)).await;
            self.send(std::slice::from_ref(m)).await;
        }
    }

    pub async fn unpublish(&mut self) {
        self.w.queue(&Command::new("FCUnpublish", 6.0, Amf0::Null, vec![Amf0::str("k")]).to_message(0));
        let _ =
            self.w.send(&Command::new("deleteStream", 7.0, Amf0::Null, vec![Amf0::Number(1.0)]).to_message(0)).await;
    }

    /// True once the relay has closed this connection.
    pub async fn closed_within(&mut self, d: Duration) -> bool {
        tokio::time::timeout(d, async { while let Ok(Some(_)) = self.r.read().await {} }).await.is_ok()
    }
}

pub fn status_code(info: &Amf0) -> &str {
    info.get("code").and_then(Amf0::as_str).unwrap_or("")
}

/// Synthetic OBS-like stream: metadata, sequence headers, then 30 fps video
/// (keyframe every `gop` frames) interleaved with ~21 ms AAC audio, in
/// timestamp order. Payload bytes encode the frame number.
pub fn synthetic_stream(video_frames: u32, gop: u32) -> Vec<Message> {
    let meta = amf0::encode_all(&[
        Amf0::str("@setDataFrame"),
        Amf0::str("onMetaData"),
        Amf0::EcmaArray(vec![
            ("width".into(), Amf0::Number(1920.0)),
            ("height".into(), Amf0::Number(1080.0)),
            ("framerate".into(), Amf0::Number(30.0)),
        ]),
    ]);
    let mut out = vec![
        Message::new(msg_type::DATA_AMF0, 1, 0, meta),
        Message::new(msg_type::VIDEO, 1, 0, vec![0x17, 0, 0, 0, 0, 1, 0x64]),
        Message::new(msg_type::AUDIO, 1, 0, vec![0xAF, 0, 0x12, 0x10]),
    ];
    let mut audio_ts = 0f64;
    for i in 0..video_frames {
        let vts = (i as f64 * 1000.0 / 30.0) as u32;
        while (audio_ts as u32) < vts {
            out.push(Message::new(msg_type::AUDIO, 1, audio_ts as u32, vec![0xAF, 1, (audio_ts as u32 % 251) as u8]));
            audio_ts += 1024.0 * 1000.0 / 48000.0;
        }
        let key = i % gop == 0;
        let mut p = vec![if key { 0x17 } else { 0x27 }, 1, 0, 0, 0];
        p.extend_from_slice(&i.to_be_bytes());
        out.push(Message::new(msg_type::VIDEO, 1, vts, p));
    }
    out
}

/// Video frame number encoded by [`synthetic_stream`] (None for non-frames).
pub fn frame_no(m: &Message) -> Option<u32> {
    (m.type_id == msg_type::VIDEO && m.payload.len() == 9 && m.payload[1] == 1)
        .then(|| u32::from_be_bytes([m.payload[5], m.payload[6], m.payload[7], m.payload[8]]))
}

pub fn is_keyframe(m: &Message) -> bool {
    m.type_id == msg_type::VIDEO && m.payload[0] == 0x17 && m.payload[1] == 1
}
