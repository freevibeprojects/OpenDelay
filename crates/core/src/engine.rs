// SPDX-License-Identifier: GPL-3.0-only
//! Delay engine (sans-IO): a rolling buffer of the encoder's media messages and
//! an output cursor. The upstream writer pulls messages with [`Engine::poll`];
//! all timing comes from the `now` arguments, so tests run on synthetic clocks.
//!
//! Output timestamps are `source + offset`. The offset changes only when the
//! cursor jumps (reconnect, delay switch), always so that the upstream timeline
//! continues one frame after the last message sent. Each track (audio, video)
//! is additionally clamped so its timestamps never decrease; script data is
//! clamped to the highest timestamp sent on any track.

use opendelay_rtmp::media::{self, VideoKind};
use opendelay_rtmp::{Message, msg_type};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Streams whose keyframes are further apart than this are trimmed mid-GOP.
const MAX_GOP: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Poll {
    /// Send this message upstream (stream id is set by the writer).
    Send(Message),
    /// A jump happened: the next message on every chunk stream needs a full header.
    ResetHeaders,
    /// Nothing due before this instant.
    Wait(Instant),
    /// Nothing to send until more input arrives.
    Idle,
    /// Input ended and everything has been sent.
    Finished,
}

const AUDIO: usize = 0;
const VIDEO: usize = 1;
const DATA: usize = 2;

fn track_of(type_id: u8) -> usize {
    match type_id {
        msg_type::AUDIO => AUDIO,
        msg_type::VIDEO => VIDEO,
        _ => DATA,
    }
}

/// Unwraps 32-bit RTMP timestamps into a monotonic-ish i64 timeline.
#[derive(Default, Clone, Copy)]
struct Unwrap {
    last: Option<u32>,
    epoch: i64,
}

impl Unwrap {
    fn next(&mut self, raw: u32) -> i64 {
        if let Some(last) = self.last {
            if raw < last && last - raw > 1 << 31 {
                self.epoch += 1 << 32;
            } else if raw > last && raw - last > 1 << 31 && self.epoch > 0 {
                self.epoch -= 1 << 32;
            }
        }
        self.last = Some(raw);
        self.epoch + raw as i64
    }
}

struct Entry {
    msg: Message,
    /// Unwrapped source timestamp (ms).
    ts: i64,
    /// Arrival time.
    at: Instant,
}

/// (message type, track id, sub-kind) of a cached codec configuration message.
type ConfigKey = (u8, u8, u8);

pub struct Engine {
    entries: VecDeque<Entry>,
    /// Absolute index of `entries[0]`.
    base: u64,
    /// Absolute indices of entries where output may start, ascending: keyframes
    /// on the default video track (or audio while no video has been seen).
    keys: VecDeque<u64>,
    /// Absolute index of the next entry to send.
    cursor: u64,
    /// Messages to send before the cursor entry (re-sent codec configuration).
    pending: VecDeque<Message>,
    configs: Vec<(ConfigKey, Message)>,
    /// Skip to the next key entry and re-send configuration before continuing.
    resync: bool,
    offset: i64,
    last_out: [Option<i64>; 3],
    clocks: [Unwrap; 3],
    /// Typical video frame interval (ms), the gap inserted at jumps.
    frame_ms: i64,
    last_video_ts: Option<i64>,
    seen_video: bool,
    input_done: bool,
    bytes: u64,
    /// How much already-sent content to keep for future jumps.
    retain: Duration,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            entries: VecDeque::new(),
            base: 0,
            keys: VecDeque::new(),
            cursor: 0,
            pending: VecDeque::new(),
            configs: Vec::new(),
            resync: false,
            offset: 0,
            last_out: [None; 3],
            clocks: [Unwrap::default(); 3],
            frame_ms: 33,
            last_video_ts: None,
            seen_video: false,
            input_done: false,
            bytes: 0,
            retain: Duration::ZERO,
        }
    }

    fn end(&self) -> u64 {
        self.base + self.entries.len() as u64
    }

    fn entry(&self, i: u64) -> Option<&Entry> {
        self.entries.get(i.checked_sub(self.base)? as usize)
    }

    /// Add a media message (audio, video or script data) received from the encoder.
    pub fn push(&mut self, msg: Message, now: Instant) {
        let track = track_of(msg.type_id);
        let ts = self.clocks[track].next(msg.timestamp);
        let mut key = false;
        let mut config = None;
        match track {
            VIDEO => {
                if let Some(v) = media::parse_video(&msg.payload) {
                    self.seen_video = true;
                    key = v.is_primary_keyframe();
                    if v.is_config() {
                        config =
                            Some((msg_type::VIDEO, v.track_id.unwrap_or(0), (v.kind == VideoKind::Metadata) as u8));
                    }
                    if v.kind == VideoKind::CodedFrames && v.track_id.unwrap_or(0) == 0 {
                        if let Some(prev) = self.last_video_ts.filter(|p| (1..=200).contains(&(ts - p))) {
                            self.frame_ms = ts - prev;
                        }
                        self.last_video_ts = Some(ts);
                    }
                }
            }
            AUDIO => {
                if let Some(a) = media::parse_audio(&msg.payload) {
                    key = !self.seen_video;
                    if a.is_config {
                        config = Some((msg_type::AUDIO, a.track_id.unwrap_or(0), 0));
                    }
                }
            }
            _ => {
                if media::metadata(msg.amf0_body()).is_some() {
                    config = Some((msg_type::DATA_AMF0, 0, 0));
                }
            }
        }
        if let Some(k) = config {
            match self.configs.iter_mut().find(|(c, _)| *c == k) {
                Some(slot) => slot.1 = msg.clone(),
                None => self.configs.push((k, msg.clone())),
            }
        }
        if key {
            self.keys.push_back(self.end());
        }
        self.bytes += msg.payload.len() as u64;
        self.entries.push_back(Entry { msg, ts, at: now });
        self.trim(now);
    }

    /// The encoder has stopped; finish sending what is buffered.
    pub fn finish_input(&mut self) {
        self.input_done = true;
    }

    /// A new upstream connection replaced a failed one: resume from the latest
    /// keyframe, re-sending codec configuration first.
    pub fn upstream_reset(&mut self) {
        self.pending.clear();
        self.cursor = self.keys.back().copied().unwrap_or(self.end());
        self.resync = true;
    }

    /// Next output action.
    pub fn poll(&mut self, _now: Instant) -> Poll {
        if let Some(m) = self.pending.pop_front() {
            return Poll::Send(m);
        }
        if self.resync {
            let Some(k) = self.keys.iter().copied().find(|&k| k >= self.cursor) else {
                self.cursor = self.end();
                return self.idle();
            };
            self.resync = false;
            self.start_at(k);
            return Poll::ResetHeaders;
        }
        if self.cursor >= self.end() {
            return self.idle();
        }
        let m = self.out_message(self.cursor);
        self.cursor += 1;
        Poll::Send(m)
    }

    fn idle(&self) -> Poll {
        if self.input_done { Poll::Finished } else { Poll::Idle }
    }

    /// Move the cursor to key entry `k`, continue the upstream timeline one
    /// frame after the last message sent and queue the codec configuration.
    fn start_at(&mut self, k: u64) {
        self.cursor = k;
        let k_ts = self.entry(k).map_or(0, |e| e.ts);
        let next = self.max_out().map_or(k_ts, |m| m + self.frame_ms);
        self.offset = next - k_ts;
        let configs: Vec<Message> = self.configs.iter().map(|(_, m)| m.clone()).collect();
        for mut m in configs {
            m.timestamp = self.stamp(track_of(m.type_id), next);
            self.pending.push_back(m);
        }
    }

    fn max_out(&self) -> Option<i64> {
        self.last_out.iter().flatten().copied().max()
    }

    /// Clamp an output timestamp so the track never goes backwards.
    fn stamp(&mut self, track: usize, ts: i64) -> u32 {
        let floor = if track == DATA { self.max_out() } else { self.last_out[track] };
        let ts = floor.map_or(ts, |f| ts.max(f)).max(0);
        self.last_out[track] = Some(ts);
        ts as u32 // RTMP timestamps wrap at 2^32
    }

    fn out_message(&mut self, i: u64) -> Message {
        let e = self.entry(i).expect("cursor within buffer");
        let (mut m, ts) = (e.msg.clone(), e.ts + self.offset);
        m.timestamp = self.stamp(track_of(m.type_id), ts);
        m
    }

    fn pop_front(&mut self) {
        if let Some(e) = self.entries.pop_front() {
            self.bytes -= e.msg.payload.len() as u64;
            if self.keys.front() == Some(&self.base) {
                self.keys.pop_front();
            }
            self.base += 1;
        }
    }

    /// Drop sent content that is no longer needed: whole GOPs older than
    /// `retain`, keeping a key entry at least `retain` old.
    fn trim(&mut self, now: Instant) {
        while let Some(&k1) = self.keys.iter().find(|&&k| k > self.base) {
            let old_enough = self.entry(k1).is_some_and(|e| now.duration_since(e.at) >= self.retain);
            if k1 > self.cursor || !old_enough {
                break;
            }
            while self.base < k1 {
                self.pop_front();
            }
        }
        // Without usable keyframes, still bound memory by age.
        while self.base < self.cursor
            && self.entries.front().is_some_and(|e| now.duration_since(e.at) > self.retain + MAX_GOP)
        {
            self.pop_front();
        }
    }

    pub fn buffered_bytes(&self) -> u64 {
        self.bytes
    }

    /// Span of buffered content by arrival time.
    pub fn buffered(&self) -> Duration {
        match (self.entries.front(), self.entries.back()) {
            (Some(a), Some(b)) => b.at.duration_since(a.at),
            _ => Duration::ZERO,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn video(ts: u32, key: bool) -> Message {
        Message::new(msg_type::VIDEO, 1, ts, vec![if key { 0x17 } else { 0x27 }, 1, 0, 0, 0, 0xAB])
    }
    pub fn audio(ts: u32) -> Message {
        Message::new(msg_type::AUDIO, 1, ts, vec![0xAF, 1, 0x21])
    }
    pub fn video_seq() -> Message {
        Message::new(msg_type::VIDEO, 1, 0, vec![0x17, 0, 0, 0, 0, 1])
    }
    pub fn audio_seq() -> Message {
        Message::new(msg_type::AUDIO, 1, 0, vec![0xAF, 0, 0x12, 0x10])
    }

    pub fn drain(e: &mut Engine, now: Instant) -> Vec<Message> {
        let mut out = Vec::new();
        loop {
            match e.poll(now) {
                Poll::Send(m) => out.push(m),
                Poll::ResetHeaders => {}
                _ => return out,
            }
        }
    }

    #[test]
    fn passthrough_keeps_timestamps_and_order() {
        let t0 = Instant::now();
        let mut e = Engine::new();
        let input = [video_seq(), audio_seq(), video(0, true), audio(10), video(33, false), audio(33)];
        for m in &input {
            e.push(m.clone(), t0);
        }
        assert_eq!(drain(&mut e, t0), input);
        assert!(matches!(e.poll(t0), Poll::Idle));
        e.finish_input();
        assert!(matches!(e.poll(t0), Poll::Finished));
    }

    #[test]
    fn reset_resumes_at_keyframe_with_config_and_continuous_timestamps() {
        let t0 = Instant::now();
        let mut e = Engine::new();
        e.push(video_seq(), t0);
        e.push(audio_seq(), t0);
        for i in 0..10u32 {
            e.push(video(i * 33, i % 5 == 0), t0);
            e.push(audio(i * 33 + 5), t0);
        }
        let first = drain(&mut e, t0);
        let last_ts = first.iter().map(|m| m.timestamp).max().unwrap();
        e.upstream_reset();
        e.push(video(330, false), t0);
        let out = drain(&mut e, t0);
        // configuration first, then the latest keyframe (source ts 165)
        assert_eq!(out[0].payload, video_seq().payload);
        assert_eq!(out[1].payload, audio_seq().payload);
        assert_eq!(out[2].payload[0], 0x17);
        assert_eq!(out[2].timestamp, last_ts + 33);
        assert!(out[2..].windows(2).all(|w| w[1].timestamp >= w[0].timestamp));
        assert_eq!(out.len(), 2 + 11);
    }

    #[test]
    fn trims_to_current_gop_in_passthrough() {
        let t0 = Instant::now();
        let mut e = Engine::new();
        for i in 0..100u32 {
            e.push(video(i * 33, i % 10 == 0), t0);
            drain(&mut e, t0);
        }
        assert!(e.entries.len() <= 10, "kept {}", e.entries.len());
        assert_eq!(e.keys.front(), Some(&e.base));
    }

    #[test]
    fn timestamp_unwrap() {
        let mut u = Unwrap::default();
        assert_eq!(u.next(u32::MAX - 10), u32::MAX as i64 - 10);
        assert_eq!(u.next(5), (1i64 << 32) + 5);
    }
}
