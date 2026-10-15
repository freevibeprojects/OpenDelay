// SPDX-License-Identifier: GPL-3.0-only
//! Delay engine (sans-IO): a rolling buffer of the encoder's media messages and
//! an output cursor. The upstream writer pulls messages with [`Engine::poll`];
//! all timing comes from the `now` arguments, so tests run on synthetic clocks.
//!
//! Modes (SPEC §4):
//! - **Live**: every message is sent as soon as it arrives.
//! - **Delayed**: a message is sent `delay` after it arrived.
//! - **Holding** ("buffering"): output pauses until unsent content is old
//!   enough; used when the buffer cannot yet supply the requested delay.
//! - **Draining**: the backlog is sent faster than real time until the lag
//!   reaches the target (0 = back to live), bounded in time and messages.
//!
//! Switching delay on jumps the cursor back to the latest keyframe that is at
//! least the requested delay old. Output timestamps are `source + offset`; the
//! offset changes only at jumps, always so that the upstream timeline continues
//! one frame after the last message sent. Audio and video are additionally
//! clamped so neither track ever goes backwards; script data is clamped to the
//! highest timestamp sent on any track.

use opendelay_rtmp::media::{self, VideoKind};
use opendelay_rtmp::{Message, msg_type};
use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

/// Streams whose keyframes are further apart than this are trimmed mid-GOP.
const MAX_GOP: Duration = Duration::from_secs(30);
/// The keyframe chosen for an instant switch is at least this much older than the delay.
pub const SAFETY_MARGIN: Duration = Duration::from_millis(200);
/// Bounds on a drain before falling back to a forced switch at a keyframe.
pub const DRAIN_MAX_TIME: Duration = Duration::from_secs(10);
pub const DRAIN_MAX_MESSAGES: u32 = 10_000;
/// Delay changes smaller than this while delayed only retarget, without a pause or drain.
const RETARGET_TOLERANCE: Duration = Duration::from_millis(500);
/// Memory cap for one stream's buffer.
pub const MAX_BUFFER_BYTES: u64 = 1 << 30;
pub const MAX_DELAY: Duration = Duration::from_secs(600);

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

/// Why an action was not performed (SPEC §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NoStream,
    BufferNotReady {
        fill_pct: u8,
    },
    AlreadyActive,
    AlreadyLive,
    DelayModeOff,
    /// Detail code: `go_live_first`, `buffer_below_delay`.
    InvalidInMode(&'static str),
    TransitionInProgress,
    UpstreamRecovering,
    Unsustainable {
        max_secs: u32,
    },
    TransportError,
    /// Detail code: `delay_range`, `buffer_range`, `not_a_number`.
    InvalidValue(&'static str),
}

impl Refusal {
    /// Stable machine-readable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NoStream => "no_stream",
            Refusal::BufferNotReady { .. } => "buffer_not_ready",
            Refusal::AlreadyActive => "already_active",
            Refusal::AlreadyLive => "already_live",
            Refusal::DelayModeOff => "delay_mode_off",
            Refusal::InvalidInMode(_) => "invalid_in_mode",
            Refusal::TransitionInProgress => "transition_in_progress",
            Refusal::UpstreamRecovering => "upstream_recovering",
            Refusal::Unsustainable { .. } => "unsustainable",
            Refusal::TransportError => "transport_error",
            Refusal::InvalidValue(_) => "invalid_value",
        }
    }

    /// Stable sub-code for refusals that have several causes.
    pub fn detail(&self) -> Option<&'static str> {
        match self {
            Refusal::InvalidInMode(d) | Refusal::InvalidValue(d) => Some(d),
            _ => None,
        }
    }
}

fn detail_text(code: &str) -> &'static str {
    match code {
        "go_live_first" => "Go live before turning delay mode off.",
        "buffer_below_delay" => "The buffer cannot be shorter than the active delay.",
        "delay_range" => "The delay must be between 1 second and 10 minutes.",
        "buffer_range" => "The buffer must be between 5 seconds and 10 minutes.",
        "not_a_number" => "Invalid number of seconds.",
        _ => "The value is out of range.",
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NoStream => f.write_str("No stream is connected."),
            Refusal::BufferNotReady { fill_pct } => {
                write!(f, "The delay buffer is not ready yet ({fill_pct}% filled).")
            }
            Refusal::AlreadyActive => f.write_str("The delay is already active."),
            Refusal::AlreadyLive => f.write_str("The stream is already live."),
            Refusal::DelayModeOff => f.write_str("Delay mode is turned off."),
            Refusal::InvalidInMode(d) | Refusal::InvalidValue(d) => f.write_str(detail_text(d)),
            Refusal::TransitionInProgress => f.write_str("Another transition is still in progress."),
            Refusal::UpstreamRecovering => f.write_str("The upstream connection is recovering."),
            Refusal::Unsustainable { max_secs } => {
                write!(f, "This delay cannot be sustained at the current bitrate (about {max_secs} s fit in memory).")
            }
            Refusal::TransportError => f.write_str("The stream connection failed while applying the action."),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayState {
    Live,
    /// Waiting for content to age to the requested delay.
    Buffering,
    Delayed,
    /// Catching up (towards live or a shorter delay).
    Draining,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EngineStatus {
    pub state: DelayState,
    /// How far behind the input the output currently is.
    pub current_delay_ms: u64,
    /// Delay being established or kept (0 when live).
    pub target_delay_ms: u64,
    /// Age of the oldest buffered content.
    pub buffer_fill_ms: u64,
    pub buffer_target_ms: u64,
    /// A keyframe old enough for the requested delay is buffered (instant switch possible).
    pub ready: bool,
    /// Largest delay an instant switch can reach right now.
    pub max_delay_ms: u64,
    pub buffer_bytes: u64,
    /// Average input bitrate over the buffered span.
    pub bitrate_bps: u64,
    /// 0..1 during buffering and draining.
    pub progress: Option<f32>,
    /// Set when the bitrate makes the buffer target unsustainable.
    pub degraded: Option<String>,
    /// With `degraded`: the buffer length that fits in memory.
    pub buffer_limit_ms: Option<u64>,
}

/// Result of a successful enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enabled {
    /// Jumped back: viewers now see delayed content.
    Instant,
    /// Content must age first; `buffer_grown` when the buffer was enlarged to fit the delay.
    Building { buffer_grown: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Live,
    Delayed { delay: Duration, target: Duration },
    Holding { target: Duration },
    Draining { target: Duration, started: Instant, sent: u32, from_lag: Duration },
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

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Unwraps 32-bit RTMP timestamps into an i64 timeline.
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
    mode: Mode,
    reset_headers: bool,
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
    /// Rolling buffer length: sent content kept for future instant switches.
    retain: Duration,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(Duration::ZERO)
    }
}

impl Engine {
    pub fn new(retain: Duration) -> Self {
        Engine {
            entries: VecDeque::new(),
            base: 0,
            keys: VecDeque::new(),
            cursor: 0,
            mode: Mode::Live,
            reset_headers: false,
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
            retain,
        }
    }

    fn end(&self) -> u64 {
        self.base + self.entries.len() as u64
    }

    fn entry(&self, i: u64) -> Option<&Entry> {
        self.entries.get(i.checked_sub(self.base)? as usize)
    }

    fn age(&self, i: u64, now: Instant) -> Duration {
        self.entry(i).map_or(Duration::ZERO, |e| now.saturating_duration_since(e.at))
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

    /// A new upstream connection replaced a failed one. Resume from a keyframe
    /// (re-sending codec configuration): the latest one when live, otherwise
    /// the one at or before the cursor so delayed viewers miss nothing.
    pub fn upstream_reset(&mut self) {
        self.pending.clear();
        self.cursor = match self.mode {
            Mode::Live => self.keys.back().copied(),
            _ => self.keys.iter().rev().copied().find(|&k| k <= self.cursor),
        }
        .unwrap_or(self.cursor.max(self.base));
        self.resync = true;
    }

    pub fn is_active(&self) -> bool {
        self.mode != Mode::Live
    }

    /// Whether the rolling buffer may shrink to `retain`.
    pub fn check_retain(&self, retain: Duration) -> Result<(), Refusal> {
        if self.target() > retain {
            return Err(Refusal::InvalidInMode("buffer_below_delay"));
        }
        Ok(())
    }

    /// Rolling buffer length (0 when delay mode is off).
    pub fn set_retain(&mut self, retain: Duration) {
        self.retain = retain;
    }

    pub fn retain(&self) -> Duration {
        self.retain
    }

    fn target(&self) -> Duration {
        match self.mode {
            Mode::Live => Duration::ZERO,
            Mode::Delayed { target, .. } | Mode::Holding { target } | Mode::Draining { target, .. } => target,
        }
    }

    fn fill_pct(&self, now: Instant, of: Duration) -> u8 {
        let fill = self.entries.front().map_or(Duration::ZERO, |e| now.saturating_duration_since(e.at));
        (fill.as_secs_f64() / of.as_secs_f64().max(0.001) * 100.0).min(100.0) as u8
    }

    /// Switch the delay on (SPEC §4.2).
    pub fn enable(&mut self, delay: Duration, now: Instant) -> Result<Enabled, Refusal> {
        match self.mode {
            Mode::Live => {}
            Mode::Draining { .. } => return Err(Refusal::TransitionInProgress),
            _ => return Err(Refusal::AlreadyActive),
        }
        if delay.is_zero() || delay > MAX_DELAY {
            return Err(Refusal::InvalidValue("delay_range"));
        }
        if self.keys.is_empty() {
            return Err(Refusal::BufferNotReady { fill_pct: self.fill_pct(now, delay) });
        }
        if let Some(max) = self.sustainable_max().filter(|max| delay > *max) {
            return Err(Refusal::Unsustainable { max_secs: max.as_secs() as u32 });
        }
        let buffer_grown = delay > self.retain;
        self.retain = self.retain.max(delay);
        let want = delay + SAFETY_MARGIN;
        let key = self.keys.iter().rev().copied().find(|&k| self.age(k, now) >= want);
        Ok(match key {
            Some(k) => {
                let actual = self.age(k, now);
                self.start_at(k);
                self.mode = Mode::Delayed { delay: actual, target: delay };
                Enabled::Instant
            }
            None => {
                self.mode = Mode::Holding { target: delay };
                Enabled::Building { buffer_grown }
            }
        })
    }

    /// Return to live (SPEC §4.3): drain the backlog, then rejoin the live edge.
    pub fn go_live(&mut self, now: Instant) -> Result<(), Refusal> {
        match self.mode {
            Mode::Live => Err(Refusal::AlreadyLive),
            Mode::Draining { .. } => Err(Refusal::TransitionInProgress),
            _ => {
                let from_lag = self.lag(now);
                self.mode = Mode::Draining { target: Duration::ZERO, started: now, sent: 0, from_lag };
                Ok(())
            }
        }
    }

    /// Change the delay while it is active: longer delays age in place,
    /// shorter ones drain. Inactive engines have nothing to change.
    pub fn set_delay(&mut self, delay: Duration, now: Instant) -> Result<(), Refusal> {
        if delay.is_zero() || delay > MAX_DELAY {
            return Err(Refusal::InvalidValue("delay_range"));
        }
        if let Some(max) = self.sustainable_max().filter(|max| delay > *max) {
            return Err(Refusal::Unsustainable { max_secs: max.as_secs() as u32 });
        }
        self.mode = match self.mode {
            Mode::Live => return Ok(()),
            Mode::Draining { .. } => return Err(Refusal::TransitionInProgress),
            Mode::Holding { .. } => Mode::Holding { target: delay },
            Mode::Delayed { delay: current, .. } => {
                if delay > current + RETARGET_TOLERANCE {
                    Mode::Holding { target: delay }
                } else if delay + RETARGET_TOLERANCE < current {
                    Mode::Draining { target: delay, started: now, sent: 0, from_lag: current }
                } else {
                    Mode::Delayed { delay: current, target: delay }
                }
            }
        };
        self.retain = self.retain.max(delay);
        Ok(())
    }

    /// Next output action.
    pub fn poll(&mut self, now: Instant) -> Poll {
        loop {
            if self.reset_headers {
                self.reset_headers = false;
                return Poll::ResetHeaders;
            }
            if let Some(m) = self.pending.pop_front() {
                return Poll::Send(m);
            }
            if self.resync {
                match self.keys.iter().copied().find(|&k| k >= self.cursor) {
                    Some(k) => {
                        self.resync = false;
                        self.start_at(k);
                        continue;
                    }
                    None => {
                        self.cursor = self.end();
                        return self.idle();
                    }
                }
            }
            let Some(e) = self.entry(self.cursor) else {
                match self.mode {
                    Mode::Draining { target, .. } if target.is_zero() => self.mode = Mode::Live,
                    Mode::Draining { target, .. } => self.mode = Mode::Holding { target },
                    _ => {}
                }
                return self.idle();
            };
            let age = now.saturating_duration_since(e.at);
            match self.mode {
                Mode::Live => {}
                Mode::Delayed { delay, .. } if age < delay => return Poll::Wait(e.at + delay),
                Mode::Delayed { .. } => {}
                Mode::Holding { target } if age < target => return Poll::Wait(e.at + target),
                Mode::Holding { target } => self.mode = Mode::Delayed { delay: target, target },
                Mode::Draining { target, .. } if !target.is_zero() && age <= target => {
                    self.mode = Mode::Delayed { delay: target, target };
                    continue;
                }
                Mode::Draining { target, started, sent, from_lag } => {
                    if now.saturating_duration_since(started) >= DRAIN_MAX_TIME || sent >= DRAIN_MAX_MESSAGES {
                        self.force_switch(target, now);
                        continue;
                    }
                    self.mode = Mode::Draining { target, started, sent: sent + 1, from_lag };
                }
            }
            let m = self.out_message(self.cursor);
            self.cursor += 1;
            return Poll::Send(m);
        }
    }

    fn idle(&self) -> Poll {
        if self.input_done { Poll::Finished } else { Poll::Idle }
    }

    /// Drain bound reached: jump forward to the latest keyframe that keeps
    /// at least `target` of delay (the live edge for 0).
    fn force_switch(&mut self, target: Duration, now: Instant) {
        let key = self.keys.iter().rev().copied().find(|&k| k >= self.cursor && self.age(k, now) >= target);
        if let Some(k) = key {
            self.start_at(k);
        }
        self.mode = if target.is_zero() {
            Mode::Live
        } else {
            Mode::Delayed { delay: key.map_or(target, |k| self.age(k, now).max(target)), target }
        };
    }

    /// Move the cursor to key entry `k`, continue the upstream timeline one
    /// frame after the last message sent and queue the codec configuration.
    fn start_at(&mut self, k: u64) {
        self.cursor = k;
        self.reset_headers = true;
        self.pending.clear();
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
            if self.cursor < self.base {
                // Unsent content had to go: skip to the next keyframe.
                self.cursor = self.base;
                self.resync = true;
            }
        }
    }

    /// Bound memory: drop sent GOPs once a younger keyframe is `retain` old;
    /// beyond the hard limits (age, bytes) drop the oldest content regardless.
    fn trim(&mut self, now: Instant) {
        while let Some(&k1) = self.keys.iter().find(|&&k| k > self.base) {
            if k1 > self.cursor || self.age(k1, now) < self.retain {
                break;
            }
            while self.base < k1 {
                self.pop_front();
            }
        }
        let hard_age = self.retain.max(self.target()) + MAX_GOP;
        while self.entries.front().is_some_and(|e| now.saturating_duration_since(e.at) > hard_age)
            || self.bytes > MAX_BUFFER_BYTES
        {
            let next_key = self.keys.iter().copied().find(|&k| k > self.base).unwrap_or(self.base + 1);
            while self.base < next_key && !self.entries.is_empty() {
                self.pop_front();
            }
        }
    }

    /// Current lag of the output behind the input.
    fn lag(&self, now: Instant) -> Duration {
        match self.mode {
            Mode::Live => Duration::ZERO,
            Mode::Delayed { delay, .. } => delay,
            Mode::Holding { .. } => match self.cursor.checked_sub(1).and_then(|i| self.entry(i)) {
                Some(e) => now.saturating_duration_since(e.at),
                None => Duration::ZERO,
            },
            Mode::Draining { .. } => self.age(self.cursor, now),
        }
    }

    fn bytes_per_sec(&self) -> Option<f64> {
        let span = self.buffered().as_secs_f64();
        (span >= 1.0).then(|| self.bytes as f64 / span)
    }

    /// Largest buffer length that fits in memory at the current bitrate.
    fn sustainable_max(&self) -> Option<Duration> {
        let bps = self.bytes_per_sec().filter(|b| *b > 0.0)?;
        // Tiny bitrates give huge (or infinite) values: no practical limit then.
        let secs = Duration::try_from_secs_f64(MAX_BUFFER_BYTES as f64 / bps).ok()?;
        Some(secs.saturating_sub(MAX_GOP / 2))
    }

    /// Span of buffered content by arrival time.
    pub fn buffered(&self) -> Duration {
        match (self.entries.front(), self.entries.back()) {
            (Some(a), Some(b)) => b.at.duration_since(a.at),
            _ => Duration::ZERO,
        }
    }

    /// Status for display; `requested` is the delay an enable would use.
    pub fn status(&self, now: Instant, requested: Duration) -> EngineStatus {
        let lag = self.lag(now);
        let (state, progress) = match self.mode {
            Mode::Live => (DelayState::Live, None),
            Mode::Delayed { .. } => (DelayState::Delayed, None),
            Mode::Holding { target } => {
                (DelayState::Buffering, Some((lag.as_secs_f32() / target.as_secs_f32().max(0.001)).min(1.0)))
            }
            Mode::Draining { target, from_lag, .. } => {
                let span = from_lag.saturating_sub(target).as_secs_f32().max(0.001);
                let left = lag.saturating_sub(target).as_secs_f32();
                (DelayState::Draining, Some((1.0 - left / span).clamp(0.0, 1.0)))
            }
        };
        let max_delay = self.keys.front().map_or(Duration::ZERO, |&k| self.age(k, now));
        let bps = self.bytes_per_sec();
        let limit = self.sustainable_max().filter(|max| self.retain > *max);
        let degraded = limit.map(|max| {
            format!(
                "At {:.1} Mbit/s only about {} s fit in memory; the buffer is limited to that.",
                bps.unwrap_or(0.0) * 8.0 / 1e6,
                max.as_secs()
            )
        });
        EngineStatus {
            state,
            current_delay_ms: ms(lag),
            target_delay_ms: ms(self.target()),
            buffer_fill_ms: ms(self.entries.front().map_or(Duration::ZERO, |e| now.saturating_duration_since(e.at))),
            buffer_target_ms: ms(self.retain),
            ready: max_delay >= requested + SAFETY_MARGIN,
            max_delay_ms: ms(max_delay.saturating_sub(SAFETY_MARGIN)),
            buffer_bytes: self.bytes,
            bitrate_bps: bps.map_or(0, |b| (b * 8.0) as u64),
            progress,
            degraded,
            buffer_limit_ms: limit.map(ms),
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
        let mut e = Engine::default();
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
        let mut e = Engine::default();
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
        let mut e = Engine::default();
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

    #[test]
    fn refusals() {
        let t0 = Instant::now();
        let mut e = Engine::new(Duration::from_secs(60));
        assert!(matches!(e.enable(Duration::from_secs(30), t0), Err(Refusal::BufferNotReady { fill_pct: 0 })));
        assert_eq!(e.go_live(t0), Err(Refusal::AlreadyLive));
        e.push(video(0, true), t0);
        assert_eq!(e.enable(Duration::ZERO, t0).unwrap_err().code(), "invalid_value");
        assert_eq!(e.enable(Duration::from_secs(30), t0), Ok(Enabled::Building { buffer_grown: false }));
        assert_eq!(e.enable(Duration::from_secs(30), t0), Err(Refusal::AlreadyActive));
        assert_eq!(e.check_retain(Duration::from_secs(10)).unwrap_err().code(), "invalid_in_mode");
        e.go_live(t0).unwrap();
        assert_eq!(e.go_live(t0), Err(Refusal::TransitionInProgress));
        assert_eq!(e.enable(Duration::from_secs(5), t0), Err(Refusal::TransitionInProgress));
        for r in [Refusal::NoStream, Refusal::BufferNotReady { fill_pct: 40 }, Refusal::Unsustainable { max_secs: 9 }] {
            assert!(!r.to_string().is_empty() && !r.code().is_empty());
        }
    }

    #[test]
    fn empty_payloads_do_not_break_the_bitrate_estimate() {
        let t0 = Instant::now();
        let mut e = Engine::new(Duration::from_secs(60));
        e.push(video(0, true), t0);
        for i in 1..200u32 {
            let t = t0 + Duration::from_millis(i as u64 * 20);
            e.push(Message::new(msg_type::AUDIO, 1, i * 20, Vec::new()), t);
        }
        let now = t0 + Duration::from_secs(5);
        assert_eq!(e.status(now, Duration::from_secs(1)).degraded, None);
        assert!(e.enable(Duration::from_secs(1), now).is_ok());
    }

    #[test]
    fn unsustainable_bitrate_is_refused_and_reported() {
        let t0 = Instant::now();
        let mut e = Engine::new(Duration::from_secs(600));
        // ~40 Mbit/s: 5 MB per second, keyframe every second
        for s in 0..10u32 {
            let t = t0 + Duration::from_secs(s as u64);
            let mut m = video(s * 1000, true);
            let mut p = vec![0u8; 5_000_000];
            p[..2].copy_from_slice(&[0x17, 1]);
            m.payload = p.into();
            e.push(m, t);
            drain(&mut e, t);
        }
        let now = t0 + Duration::from_secs(10);
        let st = e.status(now, Duration::from_secs(30));
        assert!(st.degraded.is_some(), "600 s at 40 Mbit/s cannot fit");
        assert!(matches!(e.enable(Duration::from_secs(590), now), Err(Refusal::Unsustainable { .. })));
        assert!(e.enable(Duration::from_secs(5), now).is_ok());
    }
}
