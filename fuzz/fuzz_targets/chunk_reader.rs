// SPDX-License-Identifier: GPL-3.0-only
//! Arbitrary bytes into the chunk stream parser (fed in two pieces), plus
//! command parsing and aggregate splitting of whatever comes out.
#![no_main]

use libfuzzer_sys::fuzz_target;
use opendelay_rtmp::message::split_aggregate;
use opendelay_rtmp::{ChunkReader, Command};

fuzz_target!(|data: &[u8]| {
    let mut r = ChunkReader::new();
    let cut = data.first().map_or(0, |&b| b as usize).min(data.len());
    for piece in [&data[..cut], &data[cut..]] {
        r.feed(piece);
        while let Ok(Some(msg)) = r.next_message() {
            let _ = Command::parse(&msg);
            let _ = split_aggregate(&msg);
        }
    }
});
