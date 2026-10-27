// SPDX-License-Identifier: GPL-3.0-only
//! Stream health checks (SPEC §5): inspects the encoder's stream and flags
//! settings that break or degrade instant switching.

use crate::engine::{Parsed, Unwrap};
use opendelay_rtmp::amf0::Amf0;
use opendelay_rtmp::media::{Codec, VideoKind};
use opendelay_rtmp::{Message, msg_type};
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Keyframe interval that keeps switches close to the requested delay.
pub const RECOMMENDED_KEYFRAME_MS: u64 = 2000;
const KEYFRAME_WARN_MS: u64 = 2500;
const KEYFRAME_MISSING: Duration = Duration::from_secs(10);
const SLOW_START: Duration = Duration::from_secs(5);
/// Composition-time offsets seen before B-frames are reported.
const B_FRAME_EVIDENCE: u32 = 3;
/// A timestamp step this large is a jump, not jitter.
const JUMP_MS: i64 = 5000;
/// Video below this bitrate looks blocky at any common resolution.
const LOW_KBPS: u64 = 500;
/// Span the bitrate is averaged over.
const BITRATE_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// Stable id: `codec`, `keyframe_interval`, `keyframes_missing`,
    /// `b_frames`, `timestamps`, `slow_start`, `resolution`, `no_video`,
    /// `no_audio`, `bitrate_low`, `bitrate_high`, `upload_slow`.
    pub code: &'static str,
    pub severity: Severity,
    /// 0..1: how sure the check is.
    pub confidence: f32,
    /// The observed value (e.g. `4.0 s`, `2560x1440`, `VP6`).
    pub value: String,
    /// The limit or recommendation it is compared with, when there is one.
    pub limit: String,
    /// English explanation (UIs translate by `code`).
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub keyframe_interval_ms: Option<u64>,
    pub b_frames: bool,
    /// Video frames observed.
    pub frames: u64,
    pub findings: Vec<Finding>,
}

pub struct HealthAnalyzer {
    started: Instant,
    /// Platform limits for the route (`Kick`: 1920x1080).
    max_resolution: Option<(u32, u32)>,
    max_kbps: Option<u64>,
    codec: Option<Codec>,
    width: Option<u32>,
    height: Option<u32>,
    frames: u64,
    audio_frames: u64,
    first_keyframe: Option<Instant>,
    last_keyframe: Option<(Instant, i64)>,
    /// Recent keyframe intervals (source ms).
    intervals: VecDeque<u64>,
    /// Timestamps jumped since the last keyframe: the next interval is not measured.
    irregular: bool,
    cts_frames: u32,
    last_ts: [Option<i64>; 2],
    clocks: [Unwrap; 2],
    backwards: u32,
    gaps: u32,
    jumps: u32,
    /// (window start, media bytes since then)
    window: (Instant, u64),
    kbps: Option<u64>,
}

/// Highest resolution accepted by a destination preset.
pub fn platform_limit(preset: &str) -> Option<(u32, u32)> {
    match preset {
        "kick" => Some((1920, 1080)),
        _ => None,
    }
}

/// Sending runs behind the stream (`behind_ms`) or had to skip content to
/// keep up: the upload is too slow for the bitrate.
pub fn upload_finding(behind_ms: u64, skipping: bool) -> Option<Finding> {
    (behind_ms > 2000 || skipping).then(|| Finding {
        code: "upload_slow",
        severity: Severity::High,
        confidence: 0.9,
        value: format!("{:.1} s", behind_ms as f64 / 1000.0),
        limit: String::new(),
        message: "The upload is slower than the stream: OpenDelay skips ahead to keep viewers at the delay. Lower the bitrate in your encoder.".into(),
    })
}

/// Highest bitrate a destination preset takes reliably.
fn platform_max_kbps(preset: &str) -> Option<u64> {
    match preset {
        "twitch" => Some(8500),
        "kick" => Some(8000),
        _ => None,
    }
}

fn num(v: Option<&Amf0>) -> Option<u32> {
    v.and_then(Amf0::as_f64).filter(|n| *n > 0.0 && *n < 100_000.0).map(|n| n as u32)
}

impl HealthAnalyzer {
    /// Checks for a route with destination preset `preset` (limits differ per platform).
    pub fn new(preset: &str, now: Instant) -> Self {
        HealthAnalyzer {
            started: now,
            max_resolution: platform_limit(preset),
            max_kbps: platform_max_kbps(preset),
            codec: None,
            width: None,
            height: None,
            frames: 0,
            audio_frames: 0,
            first_keyframe: None,
            last_keyframe: None,
            intervals: VecDeque::new(),
            irregular: false,
            cts_frames: 0,
            last_ts: [None, None],
            clocks: [Unwrap::default(); 2],
            backwards: 0,
            gaps: 0,
            jumps: 0,
            window: (now, 0),
            kbps: None,
        }
    }

    pub fn observe(&mut self, msg: &Message, now: Instant) {
        self.observe_parsed(msg, &Parsed::of(msg), now);
    }

    /// [`HealthAnalyzer::observe`] with the message already inspected.
    pub fn observe_parsed(&mut self, msg: &Message, parsed: &Parsed, now: Instant) {
        if matches!(msg.type_id, msg_type::VIDEO | msg_type::AUDIO) {
            self.window.1 += msg.payload.len() as u64;
            let span = now.saturating_duration_since(self.window.0);
            if span >= BITRATE_WINDOW {
                self.kbps = Some((self.window.1 as f64 * 8.0 / span.as_secs_f64() / 1000.0) as u64);
                self.window = (now, 0);
            }
        }
        match msg.type_id {
            msg_type::VIDEO => {
                let Some(v) = parsed.video else { return };
                if v.track_id.unwrap_or(0) != 0 {
                    return;
                }
                if v.codec != Codec::None {
                    self.codec = Some(v.codec);
                }
                if v.kind != VideoKind::CodedFrames {
                    return;
                }
                self.frames += 1;
                let ts = self.track_time(1, msg.timestamp);
                if v.composition_time != 0 {
                    self.cts_frames += 1;
                }
                if v.is_keyframe() {
                    self.first_keyframe.get_or_insert(now);
                    let jumped = std::mem::take(&mut self.irregular);
                    if let Some((_, prev)) = self.last_keyframe
                        && ts > prev
                        && !jumped
                    {
                        self.intervals.push_back((ts - prev) as u64);
                        if self.intervals.len() > 8 {
                            self.intervals.pop_front();
                        }
                    }
                    self.last_keyframe = Some((now, ts));
                }
            }
            msg_type::AUDIO => {
                if !msg.payload.is_empty() {
                    self.audio_frames += 1;
                    self.track_time(0, msg.timestamp);
                }
            }
            _ => {
                if let Some(meta) = &parsed.metadata {
                    self.width = num(meta.get("width")).or(self.width);
                    self.height = num(meta.get("height")).or(self.height);
                }
            }
        }
    }

    /// Unwrapped timestamp of a track's message, counting irregularities.
    fn track_time(&mut self, track: usize, raw: u32) -> i64 {
        let ts = self.clocks[track].next(raw);
        if let Some(last) = self.last_ts[track] {
            if ts < last {
                self.backwards += 1;
                self.irregular = true;
            } else if ts - last > JUMP_MS {
                self.jumps += 1;
                self.irregular = true;
            } else if ts - last > 1000 {
                self.gaps += 1;
            }
        }
        self.last_ts[track] = Some(ts);
        ts
    }

    pub fn report(&self, now: Instant) -> HealthReport {
        let mut findings = Vec::new();
        let mut add = |code, severity, confidence, value: String, limit: String, message: String| {
            findings.push(Finding { code, severity, confidence, value, limit, message })
        };

        if let Some(c) = self.codec.filter(|c| !c.is_supported_video()) {
            add(
                "codec",
                Severity::High,
                0.9,
                c.video_name(),
                String::new(),
                format!(
                    "Video codec {} is not supported for instant switching; use H.264, HEVC or AV1.",
                    c.video_name()
                ),
            );
        }
        // Only measured intervals: the time since the last keyframe would show
        // a number growing while the first GOP arrives.
        let interval =
            (!self.intervals.is_empty()).then(|| self.intervals.iter().sum::<u64>() / self.intervals.len() as u64);
        if let Some(ms) = interval.filter(|ms| *ms > KEYFRAME_WARN_MS) {
            let severity = if ms > 2 * KEYFRAME_WARN_MS { Severity::High } else { Severity::Medium };
            add(
                "keyframe_interval",
                severity,
                0.95,
                format!("{:.1} s", ms as f64 / 1000.0),
                "2 s".into(),
                format!(
                    "Keyframes arrive every {:.1} s. Platforms such as Twitch may show viewers a black screen, and delay switches can only land on keyframes. Set the keyframe interval to 2 s.",
                    ms as f64 / 1000.0
                ),
            );
        }
        let since_key = self.last_keyframe.map_or(now.duration_since(self.started), |(t, _)| now.duration_since(t));
        if self.frames > 0 && since_key > KEYFRAME_MISSING {
            add(
                "keyframes_missing",
                Severity::High,
                0.9,
                format!("{} s", since_key.as_secs()),
                String::new(),
                format!("No keyframe for {} s: the delay cannot switch and recovery stalls.", since_key.as_secs()),
            );
        }
        if self.cts_frames >= B_FRAME_EVIDENCE {
            add(
                "b_frames",
                Severity::Medium,
                0.7,
                String::new(),
                String::new(),
                "B-frames are likely enabled; turning them off (B-frames = 0) makes switches cleaner.".into(),
            );
        }
        if self.backwards > 0 || self.jumps > 0 || self.gaps > 2 {
            add(
                "timestamps",
                Severity::Medium,
                if self.backwards + self.jumps > 0 { 0.9 } else { 0.6 },
                format!("{}", self.backwards + self.jumps + self.gaps),
                String::new(),
                "The encoder sends irregular or non-monotonic timestamps.".into(),
            );
        }
        let start_wait = match self.first_keyframe {
            Some(t) => t.duration_since(self.started),
            None => now.duration_since(self.started),
        };
        // Only video streams have a keyframe to wait for.
        if self.frames > 0 && start_wait > SLOW_START {
            add(
                "slow_start",
                Severity::Low,
                0.8,
                format!("{:.1} s", start_wait.as_secs_f64()),
                format!("{} s", SLOW_START.as_secs()),
                format!("The first keyframe took {:.1} s after publishing started.", start_wait.as_secs_f64()),
            );
        }
        if let (Some(w), Some(h), Some((mw, mh))) = (self.width, self.height, self.max_resolution)
            && (w as u64 * h as u64 > mw as u64 * mh as u64 || w.max(h) > mw.max(mh))
        {
            add(
                "resolution",
                Severity::High,
                0.9,
                format!("{w}x{h}"),
                format!("{mw}x{mh}"),
                format!("{w}x{h} is above the platform's limit of {mw}x{mh}."),
            );
        }
        if now.duration_since(self.started) > SLOW_START {
            if self.frames == 0 && self.audio_frames > 0 {
                add(
                    "no_video",
                    Severity::Low,
                    0.9,
                    String::new(),
                    String::new(),
                    "The stream has no video: viewers see a black picture.".into(),
                );
            }
            if self.frames > 0 && self.audio_frames == 0 {
                add(
                    "no_audio",
                    Severity::Low,
                    0.9,
                    String::new(),
                    String::new(),
                    "The stream has no audio; some platforms mute or refuse such streams.".into(),
                );
            }
        }
        if let Some(kbps) = self.kbps {
            let kbit = |k: u64| format!("{k} kbit/s");
            if self.frames > 0 && kbps < LOW_KBPS {
                add(
                    "bitrate_low",
                    Severity::Low,
                    0.7,
                    kbit(kbps),
                    kbit(LOW_KBPS),
                    format!("The stream is only {kbps} kbit/s; video will look blocky."),
                );
            }
            if let Some(max) = self.max_kbps.filter(|max| kbps > *max) {
                add(
                    "bitrate_high",
                    Severity::Medium,
                    0.7,
                    kbit(kbps),
                    kbit(max),
                    format!("{kbps} kbit/s is above the {max} kbit/s the platform takes reliably."),
                );
            }
        }
        findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
        HealthReport {
            codec: self.codec.map(|c| c.video_name()),
            width: self.width,
            height: self.height,
            keyframe_interval_ms: interval,
            b_frames: self.cts_frames >= B_FRAME_EVIDENCE,
            frames: self.frames,
            findings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendelay_rtmp::amf0;

    fn video(ts: u32, key: bool, cts: u8) -> Message {
        let mut payload = vec![if key { 0x17 } else { 0x27 }, 1, 0, 0, cts];
        payload.resize(10_000, 0xAB); // 30 fps: about 2.4 Mbit/s
        Message::new(msg_type::VIDEO, 1, ts, payload)
    }

    fn audio(ts: u32) -> Message {
        Message::new(msg_type::AUDIO, 1, ts, vec![0xAF, 1, 0x21])
    }

    /// 30 fps video with audio, keyframe every `gop` frames.
    fn feed(h: &mut HealthAnalyzer, t0: Instant, frames: u32, gop: u32, cts: u8) {
        for i in 0..frames {
            let ts = i * 33;
            let at = t0 + Duration::from_millis(ts as u64);
            h.observe(&video(ts, i % gop == 0, cts), at);
            h.observe(&audio(ts), at);
        }
    }

    fn codes(r: &HealthReport) -> Vec<&'static str> {
        r.findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn healthy_stream_has_no_findings() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("twitch", t0);
        feed(&mut h, t0, 600, 60, 0);
        let r = h.report(t0 + Duration::from_secs(20));
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        assert_eq!(r.codec.as_deref(), Some("H.264"));
        assert_eq!(r.frames, 600);
        assert!((1950..=2000).contains(&r.keyframe_interval_ms.unwrap()));
    }

    #[test]
    fn long_gop_reported_once_measured() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("", t0);
        for i in 0..250u32 {
            if i == 230 {
                // first keyframe, then 7.6 s without one
                let r = h.report(t0 + Duration::from_secs(8));
                assert_eq!(r.keyframe_interval_ms, None);
                assert!(!codes(&r).contains(&"keyframe_interval"), "no guessed interval: {:?}", codes(&r));
            }
            h.observe(&video(i * 33, i % 240 == 0, 0), t0 + Duration::from_millis(i as u64 * 33));
        }
        let r = h.report(t0 + Duration::from_secs(9));
        let f = r.findings.iter().find(|f| f.code == "keyframe_interval").expect("8 s GOP reported");
        assert_eq!((f.severity, f.value.as_str()), (Severity::High, "7.9 s"));
    }

    #[test]
    fn timestamp_jump_is_not_a_long_keyframe_interval() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("", t0);
        for i in 0..600u32 {
            // 2 s GOP; the source clock jumps 40 s ahead at frame 300
            let ts = i * 33 + if i >= 300 { 40_000 } else { 0 };
            let at = t0 + Duration::from_millis(i as u64 * 33);
            h.observe(&video(ts, i % 60 == 0, 0), at);
            h.observe(&audio(ts), at);
        }
        let r = h.report(t0 + Duration::from_secs(20));
        assert_eq!(codes(&r), ["timestamps"]);
        assert!((1950..=2000).contains(&r.keyframe_interval_ms.unwrap()));
    }

    #[test]
    fn bitrate_and_missing_tracks() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("twitch", t0);
        for i in 0..400u32 {
            let ts = i * 33;
            let mut v = video(ts, i % 60 == 0, 0);
            v.payload = v.payload.slice(..700); // 30 fps: about 170 kbit/s, no audio
            h.observe(&v, t0 + Duration::from_millis(ts as u64));
        }
        let r = h.report(t0 + Duration::from_secs(14));
        assert_eq!(codes(&r), ["no_audio", "bitrate_low"]);

        let mut h = HealthAnalyzer::new("twitch", t0);
        for i in 0..400u32 {
            let ts = i * 33;
            let mut v = video(ts, i % 60 == 0, 0);
            v.payload = vec![0x17; 40_000].into(); // about 9.7 Mbit/s
            h.observe(&v, t0 + Duration::from_millis(ts as u64));
            h.observe(&audio(ts), t0 + Duration::from_millis(ts as u64));
        }
        let r = h.report(t0 + Duration::from_secs(14));
        let f = r.findings.iter().find(|f| f.code == "bitrate_high").expect("above Twitch's limit");
        assert_eq!(f.limit, "8500 kbit/s");
        assert!(HealthAnalyzer::new("youtube", t0).max_kbps.is_none(), "no limit for other platforms");
    }

    #[test]
    fn long_gop_b_frames_and_resolution() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("kick", t0);
        let meta = amf0::encode_all(&[
            Amf0::str("@setDataFrame"),
            Amf0::str("onMetaData"),
            Amf0::EcmaArray(vec![("width".into(), Amf0::Number(2560.0)), ("height".into(), Amf0::Number(1440.0))]),
        ]);
        h.observe(&Message::new(msg_type::DATA_AMF0, 1, 0, meta), t0);
        feed(&mut h, t0, 900, 150, 66); // 5 s GOP, composition offsets
        let r = h.report(t0 + Duration::from_secs(30));
        assert!(codes(&r).contains(&"keyframe_interval"));
        assert!(codes(&r).contains(&"resolution"));
        assert!(codes(&r).contains(&"b_frames"));
        assert_eq!(r.findings[0].severity, Severity::High, "sorted by severity");
        assert!(r.b_frames);
    }

    #[test]
    fn missing_keyframes_bad_codec_and_slow_start() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("", t0);
        for i in 0..400u32 {
            // Sorenson H.263 inter frames only
            h.observe(
                &Message::new(msg_type::VIDEO, 1, i * 33, vec![0x22, 0, 0]),
                t0 + Duration::from_millis(i as u64 * 33),
            );
        }
        let r = h.report(t0 + Duration::from_secs(14));
        for c in ["codec", "keyframes_missing", "slow_start"] {
            assert!(codes(&r).contains(&c), "{c} in {:?}", codes(&r));
        }
    }

    #[test]
    fn absurd_metadata_does_not_overflow() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("kick", t0);
        let meta = amf0::encode_all(&[
            Amf0::str("onMetaData"),
            Amf0::Object(vec![("width".into(), Amf0::Number(99_999.0)), ("height".into(), Amf0::Number(99_999.0))]),
        ]);
        h.observe(&Message::new(msg_type::DATA_AMF0, 1, 0, meta), t0);
        assert!(codes(&h.report(t0)).contains(&"resolution"));
    }

    #[test]
    fn audio_only_stream_is_noted() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("", t0);
        for i in 0..500u32 {
            h.observe(
                &Message::new(msg_type::AUDIO, 1, i * 21, vec![0xAF, 1, 0x21]),
                t0 + Duration::from_millis(i as u64 * 21),
            );
        }
        assert_eq!(codes(&h.report(t0 + Duration::from_secs(10))), ["no_video"]);
    }

    #[test]
    fn timestamp_wrap_is_not_irregular() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("", t0);
        let start = u32::MAX - 100;
        for i in 0..10u32 {
            h.observe(&video(start.wrapping_add(i * 33), i == 0, 0), t0);
        }
        assert!(!codes(&h.report(t0)).contains(&"timestamps"));
    }

    #[test]
    fn non_monotonic_timestamps() {
        let t0 = Instant::now();
        let mut h = HealthAnalyzer::new("", t0);
        for ts in [0, 33, 66, 40, 100] {
            h.observe(&video(ts, ts == 0, 0), t0);
        }
        assert!(codes(&h.report(t0)).contains(&"timestamps"));
    }
}
