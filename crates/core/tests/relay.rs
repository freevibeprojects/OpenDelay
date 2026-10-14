// SPDX-License-Identifier: GPL-3.0-only
//! Relay integration tests: mock encoder → relay → in-process mock ingest.

mod common;

use common::*;
use opendelay_core::relay::DelayConfig;
use opendelay_core::{Relay, RelayOptions, Route};
use opendelay_rtmp::amf0::Amf0;
use opendelay_rtmp::msg_type;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_rustls::rustls;

fn route(path: &str, upstream: String) -> Route {
    Route { id: path.into(), name: path.into(), path: path.into(), upstream, ..Default::default() }
}

async fn relay_for(routes: Vec<Route>, opts: RelayOptions) -> (Relay, SocketAddr) {
    let relay = Relay::new(routes, opts);
    let addr = relay.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    (relay, addr)
}

async fn wait_no_sessions(relay: &Relay) {
    let deadline = Instant::now() + WAIT;
    while !relay.sessions().is_empty() {
        assert!(Instant::now() < deadline, "sessions did not end");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn same(a: &opendelay_rtmp::Message, b: &opendelay_rtmp::Message) -> bool {
    a.type_id == b.type_id && a.timestamp == b.timestamp && a.payload == b.payload
}

#[tokio::test]
async fn passthrough_relays_publish_sequence_and_media() {
    let ingest = MockIngest::start(Behaviour::default()).await;
    let (relay, addr) =
        relay_for(vec![route("test", format!("rtmp://{}/live", ingest.addr))], Default::default()).await;

    let (mut enc, reply) = Encoder::connect(addr, "test").await;
    assert_eq!(reply.name, "_result");
    assert_eq!(reply.object.get("fmsVer").and_then(Amf0::as_str), Some("MockIngest/1.0"), "platform reply relayed");
    let status = enc.publish("secret-key").await;
    assert_eq!(status_code(&status), "NetStream.Publish.Start");

    let seen = ingest.seen();
    assert!(matches!(&seen[0], Seen::Connect { app, tc_url, .. }
        if app == "live" && *tc_url == format!("rtmp://{}/live", ingest.addr)));
    assert!(matches!(&seen[1], Seen::Publish { key, .. } if key == "secret-key"));

    let stream = synthetic_stream(300, 60);
    enc.send(&stream).await;
    ingest
        .wait_for("all media", |s| s.iter().filter(|x| matches!(x, Seen::Media { .. })).count() == stream.len())
        .await;
    let got = ingest.media(0);
    assert!(got.iter().zip(&stream).all(|(a, b)| same(a, b)), "passthrough must not alter media");
    assert!(got.iter().all(|m| m.stream_id == 7), "upstream stream id used");

    enc.unpublish().await;
    ingest.wait_for("unpublish", |s| s.iter().any(|x| matches!(x, Seen::Unpublish { .. }))).await;
    wait_no_sessions(&relay).await;
}

#[tokio::test]
async fn unknown_and_disabled_routes_are_rejected() {
    let ingest = MockIngest::start(Behaviour::default()).await;
    let mut off = route("off", format!("rtmp://{}/live", ingest.addr));
    off.enabled = false;
    let (_relay, addr) = relay_for(vec![off], Default::default()).await;
    for app in ["nope", "off"] {
        let (_enc, reply) = Encoder::connect(addr, app).await;
        assert_eq!(reply.name, "_error");
        assert_eq!(status_code(&reply.args[0]), "NetConnection.Connect.Rejected");
    }
    assert!(ingest.seen().is_empty());
}

#[tokio::test]
async fn platform_rejection_is_relayed_to_encoder() {
    let ingest = MockIngest::start(Behaviour { reject_key: Some("bad".into()), ..Default::default() }).await;
    let (relay, addr) = relay_for(vec![route("t", format!("rtmp://{}/app", ingest.addr))], Default::default()).await;
    let (mut enc, _) = Encoder::connect(addr, "t").await;
    let status = enc.publish("bad").await;
    assert_eq!(status_code(&status), "NetStream.Publish.BadName");
    assert_eq!(status.get("level").and_then(Amf0::as_str), Some("error"));
    assert!(enc.closed_within(WAIT).await);
    wait_no_sessions(&relay).await;
}

#[tokio::test]
async fn upstream_failure_is_reported_on_connect() {
    // Nothing listens on this port.
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let (_relay, addr) = relay_for(vec![route("t", format!("rtmp://{free}/app"))], Default::default()).await;
    let (_enc, reply) = Encoder::connect(addr, "t").await;
    assert_eq!(reply.name, "_error");
    assert_eq!(status_code(&reply.args[0]), "NetConnection.Connect.Failed");
}

#[tokio::test]
async fn upstream_recovery_resumes_from_keyframe_with_config() {
    let ingest = MockIngest::start(Behaviour { drop_first_after: Some(100), ..Default::default() }).await;
    let (relay, addr) = relay_for(vec![route("t", format!("rtmp://{}/app", ingest.addr))], Default::default()).await;
    let (mut enc, _) = Encoder::connect(addr, "t").await;
    enc.publish("k").await;

    let stream = synthetic_stream(600, 60);
    let (part1, part2) = stream.split_at(250);
    enc.send(part1).await;
    ingest.wait_for("reconnect", |s| s.iter().any(|x| matches!(x, Seen::Publish { conn: 1, .. }))).await;
    assert_eq!(relay.sessions()[0].info().recovery_attempt, 1);
    enc.send(part2).await;
    ingest.wait_for("tail", |_| ingest.media(1).last().is_some_and(|m| frame_no(m) == Some(599))).await;

    let before = ingest.media(0);
    let after = ingest.media(1);
    assert_eq!(after[0].type_id, msg_type::DATA_AMF0, "metadata re-sent first");
    assert_eq!(&after[1].payload[..2], &[0x17, 0], "video sequence header re-sent");
    assert_eq!(&after[2].payload[..2], &[0xAF, 0], "audio sequence header re-sent");
    assert!(is_keyframe(&after[3]), "resumes on a keyframe");
    let last_before = before.iter().map(|m| m.timestamp).max().unwrap();
    assert!(after[3].timestamp > last_before, "upstream timeline continues across the reconnect");
    for t in [msg_type::AUDIO, msg_type::VIDEO] {
        let ts: Vec<u32> = after.iter().filter(|m| m.type_id == t).map(|m| m.timestamp).collect();
        assert!(ts.windows(2).all(|w| w[1] >= w[0]), "track {t} timestamps never decrease");
    }
}

#[tokio::test]
async fn emergency_stop_disconnects_everything_within_a_second() {
    let ingest = MockIngest::start(Behaviour::default()).await;
    let routes =
        vec![route("a", format!("rtmp://{}/live", ingest.addr)), route("b", format!("rtmp://{}/live", ingest.addr))];
    let (relay, addr) = relay_for(routes, Default::default()).await;
    let (mut a, _) = Encoder::connect(addr, "a").await;
    a.publish("ka").await;
    let (mut b, _) = Encoder::connect(addr, "b").await;
    b.publish("kb").await;
    let stream = synthetic_stream(60, 30);
    a.send(&stream).await;
    b.send(&stream).await;
    ingest
        .wait_for("both routes live", |_| {
            ingest.media(0).len() == stream.len() && ingest.media(1).len() == stream.len()
        })
        .await;
    assert_eq!(relay.sessions().len(), 2, "two routes live at the same time");

    let t0 = Instant::now();
    relay.emergency_stop();
    assert!(a.closed_within(Duration::from_secs(1)).await);
    assert!(b.closed_within(Duration::from_secs(1)).await);
    while ingest.seen().iter().filter(|s| matches!(s, Seen::Closed { .. })).count() < 2 {
        assert!(t0.elapsed() < Duration::from_secs(1), "upstreams not closed within 1 s");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    wait_no_sessions(&relay).await;

    let (_c, reply) = Encoder::connect(addr, "a").await;
    assert_eq!(reply.name, "_error", "publishing refused while the emergency stop is latched");
    relay.clear_emergency();
    let (_d, reply) = Encoder::connect(addr, "a").await;
    assert_eq!(reply.name, "_result");
}

#[tokio::test]
async fn graceful_shutdown_refuses_new_encoders_and_waits() {
    let ingest = MockIngest::start(Behaviour::default()).await;
    let (relay, addr) = relay_for(vec![route("t", format!("rtmp://{}/app", ingest.addr))], Default::default()).await;
    let (mut enc, _) = Encoder::connect(addr, "t").await;
    enc.publish("k").await;
    enc.send(&synthetic_stream(30, 30)).await;

    let r = relay.clone();
    let shutdown = tokio::spawn(async move { r.shutdown(Duration::from_secs(10)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err(), "listener closed");
    assert!(!shutdown.is_finished(), "waits for the active stream");
    enc.unpublish().await;
    tokio::time::timeout(Duration::from_secs(3), shutdown).await.expect("shutdown finished").unwrap();
    assert!(relay.sessions().is_empty());
}

#[tokio::test]
async fn rtmps_upstream() {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let ingest = MockIngest::start_tls(Behaviour::default()).await;
    let mut roots = rustls::RootCertStore::empty();
    let cert = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ca.pem");
    roots.add(CertificateDer::from_pem_file(cert).unwrap()).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let opts = RelayOptions { tls: Some(Arc::new(tls)), ..Default::default() };
    let upstream = format!("rtmps://localhost:{}/live", ingest.addr.port());
    let (_relay, addr) = relay_for(vec![route("s", upstream)], opts).await;
    let (mut enc, reply) = Encoder::connect(addr, "s").await;
    assert_eq!(reply.name, "_result", "{:?}", reply.args);
    assert_eq!(status_code(&enc.publish("k").await), "NetStream.Publish.Start");
    let stream = synthetic_stream(30, 30);
    enc.send(&stream).await;
    ingest.wait_for("media over TLS", |_| ingest.media(0).len() == stream.len()).await;
}

#[tokio::test]
async fn delay_actions_through_the_relay() {
    let ingest = MockIngest::start(Behaviour::default()).await;
    let delay = DelayConfig { mode_enabled: true, buffer: Duration::from_secs(3), delay: Duration::from_secs(1) };
    let opts = RelayOptions { delay, ..Default::default() };
    let (relay, addr) = relay_for(vec![route("t", format!("rtmp://{}/app", ingest.addr))], opts).await;
    assert_eq!(relay.enable_delay(None).unwrap_err().code(), "no_stream");

    let (mut enc, _) = Encoder::connect(addr, "t").await;
    enc.publish("k").await;
    let stream = synthetic_stream(120, 15); // 4 s, keyframe every 0.5 s
    let split = stream.iter().position(|m| m.timestamp >= 2000).unwrap();
    let start = tokio::time::Instant::now();
    enc.send_paced(&stream[..split], start, 0).await;

    let msg = relay.enable_delay(None).expect("instant enable");
    assert!(msg.starts_with("Delay on"), "{msg}");
    assert_eq!(relay.enable_delay(None).unwrap_err().code(), "already_active");
    assert_eq!(relay.set_delay_mode(false).unwrap_err().code(), "invalid_in_mode");
    enc.send_paced(&stream[split..], start, 0).await;

    let got = ingest.media(0);
    let replay = got.iter().position(|m| m.type_id == msg_type::DATA_AMF0 && m.timestamp > 0).expect("config re-sent");
    let key = got[replay..].iter().find(|m| frame_no(m).is_some()).unwrap();
    assert!(is_keyframe(key), "replay starts on a keyframe");
    let before = got[..replay].iter().rev().find_map(frame_no).unwrap();
    assert!(frame_no(key).unwrap() + 30 <= before, "replay jumps back at least 1 s");
    for t in [msg_type::AUDIO, msg_type::VIDEO] {
        let ts: Vec<u32> = got.iter().filter(|m| m.type_id == t).map(|m| m.timestamp).collect();
        assert!(ts.windows(2).all(|w| w[1] >= w[0]), "track {t} never decreases upstream");
    }

    relay.go_live().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(relay.go_live().unwrap_err().code(), "already_live");
    relay.set_delay_mode(false).unwrap();
    assert_eq!(relay.enable_delay(None).unwrap_err().code(), "delay_mode_off");
    relay.set_delay(Duration::from_secs(5)).unwrap();
    assert_eq!(relay.delay_config().buffer, Duration::from_secs(5), "buffer grows to fit the delay");
    assert_eq!(relay.set_buffer(Duration::from_secs(1)).unwrap_err().code(), "invalid_value");
}
