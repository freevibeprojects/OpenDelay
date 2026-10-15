// SPDX-License-Identifier: GPL-3.0-only
//! FLV audio/video tag inspection and `onMetaData` extraction.
#![no_main]

use libfuzzer_sys::fuzz_target;
use opendelay_rtmp::media;

fuzz_target!(|data: &[u8]| {
    let _ = media::parse_video(data);
    let _ = media::parse_audio(data);
    let _ = media::metadata(data);
});
