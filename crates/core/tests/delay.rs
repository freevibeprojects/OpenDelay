// SPDX-License-Identifier: GPL-3.0-only
//! Delay engine acceptance tests on synthetic real-time streams (SPEC §13, 2–4).

mod common;

use common::{frame_no, is_keyframe, synthetic_stream};
use opendelay_core::engine::{DelayState, Enabled, Engine, Poll};
use opendelay_rtmp::{Message, msg_type};
use std::time::{Duration, Instant};

const STEP: Duration = Duration::from_millis(10);

/// Feeds a synthetic stream in real time (arrival = source timestamp + `base`)
/// and records what the engine sends and when.
struct Sim {
    e: Engine,
    t0: Instant,
    now: Duration,
    input: Vec<Message>,
    next: usize,
    /// (send time, message)
    out: Vec<(Duration, Message)>,
    /// Max messages sent per step (`usize::MAX` = unlimited uplink).
    per_step: usize,
}

impl Sim {
    fn new(buffer_secs: u64, input: Vec<Message>) -> Sim {
        Sim {
            e: Engine::new(Duration::from_secs(buffer_secs)),
            t0: Instant::now(),
            now: Duration::ZERO,
            input,
            next: 0,
            out: Vec::new(),
            per_step: usize::MAX,
        }
    }

    fn at(&self) -> Instant {
        self.t0 + self.now
    }

    fn run_until(&mut self, until: Duration) {
        while self.now < until {
            self.now += STEP;
            let now = self.at();
            while self.next < self.input.len()
                && Duration::from_millis(self.input[self.next].timestamp as u64) <= self.now
            {
                self.e.push(self.input[self.next].clone(), now);
                self.next += 1;
            }
            let mut sent = 0;
            while sent < self.per_step {
                match self.e.poll(now) {
                    Poll::Send(m) => {
                        self.out.push((self.now, m));
                        sent += 1;
                    }
                    Poll::ResetHeaders => {}
                    _ => break,
                }
            }
        }
    }

    fn secs(&mut self, s: f64) {
        self.run_until(Duration::from_secs_f64(s));
    }

    fn sent_after(&self, t: Duration) -> Vec<&(Duration, Message)> {
        self.out.iter().filter(|(at, _)| *at > t).collect()
    }
}

fn assert_monotonic(out: &[(Duration, Message)]) {
    let ts = |m: &Message| m.timestamp;
    for w in out.windows(2) {
        assert!(
            (ts(&w[1].1).wrapping_sub(ts(&w[0].1)) as i32) >= 0,
            "upstream timestamp decreased: {} -> {} (types {} -> {})",
            ts(&w[0].1),
            ts(&w[1].1),
            w[0].1.type_id,
            w[1].1.type_id
        );
    }
}

/// 15 minutes of 30 fps video with 2 s keyframe interval plus audio.
fn stream() -> Vec<Message> {
    synthetic_stream(30 * 900, 60)
}

#[test]
fn ac2_enable_30s_with_full_60s_buffer_is_instant_and_seamless() {
    let mut sim = Sim::new(60, stream());
    sim.secs(70.0);
    assert_eq!(sim.out.len(), sim.next, "passthrough sends everything immediately");
    let st = sim.e.status(sim.at(), Duration::from_secs(30));
    assert!(st.ready && st.max_delay_ms >= 59_000, "{st:?}");

    let t = sim.now;
    assert_eq!(sim.e.enable(Duration::from_secs(30), sim.at()), Ok(Enabled::Instant));
    sim.secs(100.0);

    // Sequence headers are re-sent first (script data only on reconnects),
    // then a keyframe from ~30 s earlier.
    let after = sim.sent_after(t);
    assert_eq!(&after[0].1.payload[..2], &[0x17, 0]);
    assert_eq!(&after[1].1.payload[..2], &[0xAF, 0]);
    assert!(after.iter().all(|(_, m)| m.type_id != msg_type::DATA_AMF0), "no onMetaData mid-stream");
    let key = &after[2].1;
    assert!(is_keyframe(key));
    let key_src_ms = frame_no(key).unwrap() as u64 * 1000 / 30;
    let behind = t.as_millis() as u64 - key_src_ms;
    assert!((30_200..=32_300).contains(&behind), "replay starts {behind} ms back (within one keyframe interval)");

    // No interruption: video keeps flowing every frame interval across the switch.
    let video_sends: Vec<Duration> = sim.out.iter().filter(|(_, m)| frame_no(m).is_some()).map(|(at, _)| *at).collect();
    let max_gap = video_sends.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(max_gap <= Duration::from_millis(50), "video output stalled for {max_gap:?}");

    // Viewers stay ~30 s behind.
    let last = sim.out.iter().rev().find_map(|(_, m)| frame_no(m)).unwrap() as u64 * 1000 / 30;
    let lag = 100_000 - last;
    assert!((30_000..=32_400).contains(&lag), "lag {lag} ms");
    let st = sim.e.status(sim.at(), Duration::from_secs(30));
    assert_eq!(st.state, DelayState::Delayed);
    assert_eq!(st.target_delay_ms, 30_000);
    assert!((30_200..=32_300).contains(&st.current_delay_ms));

    // Upstream timeline is continuous: one frame between the last live frame and the replayed keyframe.
    let video_ts: Vec<u32> = sim.out.iter().filter(|(_, m)| frame_no(m).is_some()).map(|(_, m)| m.timestamp).collect();
    assert!(video_ts.windows(2).all(|w| (33..=34).contains(&(w[1] - w[0]))), "video timestamps continuous");
    assert_monotonic(&sim.out);
}

#[test]
fn ac3_go_live_drains_and_returns_to_live_edge() {
    let mut sim = Sim::new(60, stream());
    sim.secs(70.0);
    sim.e.enable(Duration::from_secs(30), sim.at()).unwrap();
    sim.secs(90.0);
    let t = sim.now;
    sim.e.go_live(sim.at()).unwrap();
    assert_eq!(sim.e.status(sim.at(), Duration::from_secs(30)).state, DelayState::Draining);
    sim.secs(91.0);
    let st = sim.e.status(sim.at(), Duration::from_secs(30));
    assert_eq!(st.state, DelayState::Live);
    // Backlog went out faster than real time (no frames skipped) and output is now at the live edge.
    let frames: Vec<u32> = sim.sent_after(t).iter().filter_map(|(_, m)| frame_no(m)).collect();
    assert!(frames.windows(2).all(|w| w[1] == w[0] + 1), "drain sends every frame in order");
    let last = *frames.last().unwrap() as u64 * 1000 / 30;
    assert!(91_000 - last <= 50, "back at the live edge");
    assert_monotonic(&sim.out);
    sim.secs(95.0);
    let last = sim.out.iter().rev().find_map(|(_, m)| frame_no(m)).unwrap() as u64 * 1000 / 30;
    assert!(95_000 - last <= 50, "passthrough after going live");
    assert_monotonic(&sim.out);
}

#[test]
fn ac3_drain_bound_forces_switch_at_keyframe() {
    let mut sim = Sim::new(60, stream());
    sim.secs(70.0);
    sim.e.enable(Duration::from_secs(60), sim.at()).unwrap();
    sim.secs(80.0);
    // Uplink barely faster than the stream: the 60 s backlog cannot drain in time.
    sim.per_step = 1;
    let t = sim.now;
    sim.e.go_live(sim.at()).unwrap();
    sim.secs(89.0);
    assert_eq!(sim.e.status(sim.at(), Duration::ZERO).state, DelayState::Draining);
    sim.secs(90.5);
    assert_eq!(sim.e.status(sim.at(), Duration::ZERO).state, DelayState::Live, "forced switch after 10 s");
    sim.per_step = usize::MAX;
    sim.secs(92.0);
    let frames: Vec<&Message> =
        sim.sent_after(t).into_iter().map(|(_, m)| m).filter(|m| frame_no(m).is_some()).collect();
    let jump = frames.windows(2).position(|w| frame_no(w[1]).unwrap() > frame_no(w[0]).unwrap() + 1);
    let jump = jump.expect("forced switch skips forward");
    assert!(is_keyframe(frames[jump + 1]), "forced switch lands on a keyframe");
    let last = sim.out.iter().rev().find_map(|(_, m)| frame_no(m)).unwrap() as u64 * 1000 / 30;
    assert!(92_000 - last <= 50, "live edge reached");
    assert_monotonic(&sim.out);
}

#[test]
fn short_buffer_ages_in_place_without_jumping() {
    let mut sim = Sim::new(60, stream());
    sim.secs(10.0);
    let t = sim.now;
    let last_live = sim.out.iter().rev().find_map(|(_, m)| frame_no(m)).unwrap();
    assert_eq!(sim.e.enable(Duration::from_secs(30), sim.at()), Ok(Enabled::Building { buffer_grown: false }));
    sim.secs(25.0);
    let st = sim.e.status(sim.at(), Duration::from_secs(30));
    assert_eq!(st.state, DelayState::Buffering);
    assert!((0.45..0.55).contains(&st.progress.unwrap()), "{st:?}");
    assert!(sim.sent_after(t).is_empty(), "output holds while content ages");
    sim.secs(41.0);
    let st = sim.e.status(sim.at(), Duration::from_secs(30));
    assert_eq!((st.state, st.current_delay_ms), (DelayState::Delayed, 30_000));
    let resumed = sim.sent_after(t).iter().find_map(|(_, m)| frame_no(m)).unwrap();
    assert_eq!(resumed, last_live + 1, "no jump: output continues with the next frame");
    assert_monotonic(&sim.out);
}

#[test]
fn larger_delay_grows_buffer() {
    let mut sim = Sim::new(30, stream());
    sim.secs(40.0);
    assert_eq!(sim.e.enable(Duration::from_secs(45), sim.at()), Ok(Enabled::Building { buffer_grown: true }));
    assert_eq!(sim.e.retain(), Duration::from_secs(45));
    sim.secs(90.0);
    assert_eq!(sim.e.status(sim.at(), Duration::ZERO).state, DelayState::Delayed);
}

#[test]
fn changing_delay_while_active() {
    let mut sim = Sim::new(120, stream());
    sim.secs(130.0);
    sim.e.enable(Duration::from_secs(30), sim.at()).unwrap();
    sim.secs(140.0);
    // Longer: ages in place to the new delay.
    sim.e.set_delay(Duration::from_secs(45), sim.at()).unwrap();
    assert_eq!(sim.e.status(sim.at(), Duration::ZERO).state, DelayState::Buffering);
    sim.secs(160.0);
    let st = sim.e.status(sim.at(), Duration::ZERO);
    assert_eq!((st.state, st.current_delay_ms), (DelayState::Delayed, 45_000));
    // Shorter: drains to the new delay.
    sim.e.set_delay(Duration::from_secs(20), sim.at()).unwrap();
    assert_eq!(sim.e.status(sim.at(), Duration::ZERO).state, DelayState::Draining);
    sim.secs(161.0);
    let st = sim.e.status(sim.at(), Duration::ZERO);
    assert_eq!((st.state, st.current_delay_ms), (DelayState::Delayed, 20_000));
    let last = sim.out.iter().rev().find_map(|(_, m)| frame_no(m)).unwrap() as u64 * 1000 / 30;
    assert!((19_900..=20_100).contains(&(161_000 - last)));
    assert_monotonic(&sim.out);
}

#[test]
fn ac4_upstream_timestamps_never_decrease() {
    // Source timestamps start just below the 32-bit wrap; actions and
    // reconnects at awkward moments over 15 minutes.
    let start = u32::MAX - 120_000;
    let mut input = stream();
    for m in &mut input {
        m.timestamp = m.timestamp.wrapping_add(start);
    }
    // Arrival follows the original (unshifted) timeline.
    let arrival: Vec<u32> = stream().iter().map(|m| m.timestamp).collect();
    let mut sim = Sim::new(90, input);

    let mut actions = 0;
    let mut i = 0u64;
    while sim.now < Duration::from_secs(890) {
        // custom run loop honouring the original arrival times
        sim.now += STEP;
        let now = sim.at();
        while sim.next < sim.input.len() && Duration::from_millis(arrival[sim.next] as u64) <= sim.now {
            sim.e.push(sim.input[sim.next].clone(), now);
            sim.next += 1;
        }
        loop {
            match sim.e.poll(now) {
                Poll::Send(m) => sim.out.push((sim.now, m)),
                Poll::ResetHeaders => {}
                _ => break,
            }
        }
        i += 1;
        if i.is_multiple_of(1337) {
            let d = Duration::from_secs(5 + (i * 7919) % 80);
            let r = match (i / 1337) % 5 {
                0 | 3 => sim.e.enable(d, now).map(|_| ()),
                1 => sim.e.go_live(now),
                2 => sim.e.set_delay(d, now),
                _ => {
                    sim.e.upstream_reset();
                    Ok(())
                }
            };
            actions += r.is_ok() as u32;
        }
    }
    assert!(actions >= 20, "exercised {actions} actions");
    assert!(sim.out.iter().any(|(_, m)| m.timestamp < 1000), "upstream timestamps wrapped");
    assert_monotonic(&sim.out);
    for t in [msg_type::AUDIO, msg_type::VIDEO] {
        let track: Vec<(Duration, Message)> = sim.out.iter().filter(|(_, m)| m.type_id == t).cloned().collect();
        assert_monotonic(&track);
    }
}
