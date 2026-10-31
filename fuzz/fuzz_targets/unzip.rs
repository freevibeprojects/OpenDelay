// SPDX-License-Identifier: GPL-3.0-only
//! ZIP reading for update archives (the content is checked against a signed hash afterwards).
#![no_main]

use libfuzzer_sys::fuzz_target;
use opendelay_core::update;

fuzz_target!(|data: &[u8]| {
    let _ = update::unzip(data, update::EXE);
});
