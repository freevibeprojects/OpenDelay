// SPDX-License-Identifier: GPL-3.0-only
//! The delay engine driven by arbitrary input messages, delay actions and
//! clock steps. Checks that upstream timestamps never decrease per track.
#![no_main]

use libfuzzer_sys::fuzz_target;
use opendelay_core::engine::{Engine, Poll};
use opendelay_rtmp::{Message, msg_type};
use std::time::{Duration, Instant};

fuzz_target!(|data: &[u8]| {
    let start = Instant::now();
    let mut now = start;
    let mut engine = Engine::new(Duration::from_secs(20));
    let mut ts = 0u32;
    // last timestamp sent upstream for audio, video, data
    let mut last = [None::<u32>; 3];
    let mut bytes = data.iter().copied();
    while let Some(op) = bytes.next() {
        let arg = bytes.next().unwrap_or(0);
        match op % 8 {
            0..=3 => {
                let type_id = [msg_type::AUDIO, msg_type::VIDEO, msg_type::DATA_AMF0, msg_type::VIDEO][op as usize % 4];
                ts = ts.wrapping_add(u32::from(arg) * 4);
                let len = (bytes.next().unwrap_or(0) % 32) as usize;
                let payload: Vec<u8> = bytes.by_ref().take(len).collect();
                engine.push(Message::new(type_id, 1, ts, payload), now);
            }
            4 => {
                let _ = engine.enable(Duration::from_secs(u64::from(arg % 40)), now);
            }
            5 => {
                let _ = engine.go_live(now);
            }
            6 => {
                let _ = engine.set_delay(Duration::from_secs(u64::from(arg % 40)), now);
            }
            _ if arg == 255 => {
                engine.upstream_reset();
                last = [None; 3]; // a new upstream connection
            }
            _ => now += Duration::from_millis(u64::from(arg) * 40),
        }
        for _ in 0..10_000 {
            match engine.poll(now) {
                Poll::Send(m) => {
                    let track = match m.type_id {
                        msg_type::AUDIO => 0,
                        msg_type::VIDEO => 1,
                        _ => 2,
                    };
                    if let Some(prev) = last[track] {
                        assert!(m.timestamp >= prev, "track {track}: {} after {prev}", m.timestamp);
                    }
                    last[track] = Some(m.timestamp);
                }
                Poll::ResetHeaders => {}
                Poll::Wait(_) | Poll::Idle | Poll::Finished => break,
            }
        }
        let _ = engine.status(now, Duration::from_secs(10));
    }
});
