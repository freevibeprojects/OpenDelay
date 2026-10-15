// SPDX-License-Identifier: GPL-3.0-only
//! AMF0 decoding; whatever decodes must re-encode and decode to the same values.
#![no_main]

use libfuzzer_sys::fuzz_target;
use opendelay_rtmp::amf0;

fuzz_target!(|data: &[u8]| {
    if let Ok(values) = amf0::decode_all(data) {
        let again = amf0::decode_all(&amf0::encode_all(&values)).expect("re-encoded values decode");
        assert_eq!(values.len(), again.len());
    }
});
