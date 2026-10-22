// SPDX-License-Identifier: GPL-3.0-only
//! RTMP chunk stream: reassembles incoming chunks into messages and splits
//! outgoing messages into chunks.
//!
//! Reader rules (RTMP 1.0 §5.3 plus common practice):
//! - format 0 carries an absolute timestamp, formats 1/2 a delta;
//! - a format 3 chunk that starts a new message repeats the previous delta
//!   (0 after a format 0 header, as encoders based on librtmp expect);
//! - the extended timestamp is repeated on format 3 chunks when the latest
//!   format 0/1/2 header used one. Peers that omit it are tolerated.

use crate::message::{Message, msg_type};
use bytes::{Buf, Bytes, BytesMut};
use std::collections::HashMap;
use std::fmt;

pub const DEFAULT_CHUNK_SIZE: u32 = 128;
/// No message can exceed 2^24-1 bytes, so larger chunk sizes are clamped.
const MAX_CHUNK_SIZE: u32 = 0x00FF_FFFF;
const EXTENDED: u32 = 0x00FF_FFFF;
/// Bound on distinct chunk stream ids a peer may use (memory safety).
const MAX_CHUNK_STREAMS: usize = 64;
/// Bound on the declared size of all unfinished messages together: without
/// it a peer could keep a maximum-size message open on every chunk stream.
const MAX_UNFINISHED_BYTES: u64 = 2 * MAX_CHUNK_SIZE as u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkError(pub &'static str);

impl fmt::Display for ChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RTMP chunk stream: {}", self.0)
    }
}
impl std::error::Error for ChunkError {}

#[derive(Default)]
struct InStream {
    timestamp: u32,
    delta: u32,
    length: u32,
    type_id: u8,
    stream_id: u32,
    /// Extended timestamp value of the latest format 0/1/2 header, if it had one.
    extended: Option<u32>,
    partial: Option<Vec<u8>>,
}

/// Incoming chunk stream decoder (sans-IO): `feed` bytes, then drain `next_message`.
pub struct ChunkReader {
    buf: BytesMut,
    chunk_size: u32,
    streams: HashMap<u32, InStream>,
    received: u64,
    max_message: u32,
}

impl Default for ChunkReader {
    fn default() -> Self {
        Self::new()
    }
}

fn u24(b: &[u8]) -> u32 {
    u32::from_be_bytes([0, b[0], b[1], b[2]])
}

fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl ChunkReader {
    pub fn new() -> Self {
        ChunkReader {
            buf: BytesMut::new(),
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
            received: 0,
            max_message: MAX_CHUNK_SIZE,
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        self.received += data.len() as u64;
    }

    /// Total bytes fed so far (for acknowledgements).
    pub fn bytes_received(&self) -> u64 {
        self.received
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// Refuse messages longer than `bytes` (an error, before any payload is
    /// buffered). Default: the protocol maximum of 2^24-1.
    pub fn set_max_message_size(&mut self, bytes: u32) {
        self.max_message = bytes.min(MAX_CHUNK_SIZE);
    }

    /// Next complete message, or `Ok(None)` when more input is needed.
    /// Set Chunk Size and Abort are applied here and still returned.
    pub fn next_message(&mut self) -> Result<Option<Message>, ChunkError> {
        loop {
            let Some(done) = self.read_chunk()? else { return Ok(None) };
            let Some(msg) = done else { continue };
            match msg.type_id {
                msg_type::SET_CHUNK_SIZE => {
                    let size = msg.u32_arg().ok_or(ChunkError("short Set Chunk Size"))? & 0x7FFF_FFFF;
                    if size == 0 {
                        return Err(ChunkError("chunk size 0"));
                    }
                    self.chunk_size = size.min(MAX_CHUNK_SIZE);
                }
                msg_type::ABORT => {
                    if let Some(st) = msg.u32_arg().and_then(|csid| self.streams.get_mut(&csid)) {
                        st.partial = None;
                    }
                }
                _ => {}
            }
            return Ok(Some(msg));
        }
    }

    /// Parse one chunk. `None`: need more bytes. `Some(None)`: chunk consumed, message incomplete.
    fn read_chunk(&mut self) -> Result<Option<Option<Message>>, ChunkError> {
        let b = &self.buf[..];
        let Some(&first) = b.first() else { return Ok(None) };
        let fmt = first >> 6;
        let (csid, mut p) = match first & 0x3f {
            0 if b.len() < 2 => return Ok(None),
            0 => (64 + b[1] as u32, 2),
            1 if b.len() < 3 => return Ok(None),
            1 => (64 + b[1] as u32 + 256 * b[2] as u32, 3),
            n => (n as u32, 1),
        };
        let hdr_len = [11, 7, 3, 0][fmt as usize];
        if b.len() < p + hdr_len {
            return Ok(None);
        }
        let h = &b[p..p + hdr_len];
        p += hdr_len;

        let unfinished: u64 = self
            .streams
            .iter()
            .filter(|(id, s)| **id != csid && s.partial.is_some())
            .map(|(_, s)| s.length as u64)
            .sum();
        if !self.streams.contains_key(&csid) {
            if fmt != 0 {
                return Err(ChunkError("first chunk on a chunk stream must use format 0"));
            }
            if self.streams.len() >= MAX_CHUNK_STREAMS {
                return Err(ChunkError("too many chunk streams"));
            }
        }
        let st = self.streams.entry(csid).or_default();

        // Timestamp field (absolute for fmt 0, delta for fmt 1/2) with its extension.
        let mut field = None;
        let mut ext = None;
        if fmt <= 2 {
            let v = u24(&h[0..3]);
            if v == EXTENDED {
                if b.len() < p + 4 {
                    return Ok(None);
                }
                ext = Some(u32be(&b[p..p + 4]));
                p += 4;
            }
            field = Some(ext.unwrap_or(v));
        } else if let Some(prev) = st.extended {
            if b.len() < p + 4 {
                return Ok(None);
            }
            if u32be(&b[p..p + 4]) == prev {
                p += 4;
            }
        }

        let (length, type_id, stream_id) = match fmt {
            0 => (u24(&h[3..6]), h[6], u32::from_le_bytes([h[7], h[8], h[9], h[10]])),
            1 => (u24(&h[3..6]), h[6], st.stream_id),
            _ => (st.length, st.type_id, st.stream_id),
        };

        // fmt 0/1/2 always start a new message (dropping any unfinished one);
        // fmt 3 continues the pending message or starts one with the previous delta.
        let starts = fmt < 3 || st.partial.is_none();
        if starts && length > self.max_message {
            return Err(ChunkError("message too large"));
        }
        if starts && unfinished + length as u64 > MAX_UNFINISHED_BYTES {
            return Err(ChunkError("too many unfinished messages"));
        }
        let have = if starts { 0 } else { st.partial.as_ref().map_or(0, Vec::len) as u32 };
        let n = (length - have).min(self.chunk_size) as usize;
        if b.len() < p + n {
            return Ok(None);
        }

        if starts {
            let (timestamp, delta) = match (fmt, field) {
                (0, Some(abs)) => (abs, 0),
                (_, Some(delta)) => (st.timestamp.wrapping_add(delta), delta),
                _ => (st.timestamp.wrapping_add(st.delta), st.delta),
            };
            st.timestamp = timestamp;
            st.delta = delta;
            st.length = length;
            st.type_id = type_id;
            st.stream_id = stream_id;
            if fmt < 3 {
                st.extended = ext;
            }
            st.partial = Some(Vec::with_capacity((length as usize).min(64 * 1024)));
        }
        let partial = st.partial.as_mut().expect("partial set above");
        partial.extend_from_slice(&b[p..p + n]);
        let complete = partial.len() as u32 == length;
        let msg = if complete {
            let payload = st.partial.take().unwrap_or_default();
            Some(Message { type_id, stream_id, timestamp: st.timestamp, payload: Bytes::from(payload) })
        } else {
            None
        };
        self.buf.advance(p + n);
        Ok(Some(msg))
    }
}

struct OutStream {
    timestamp: u32,
    length: u32,
    type_id: u8,
    stream_id: u32,
}

/// Outgoing chunk stream encoder. Uses format 0/1/2 headers for new messages
/// (never format 3, which peers interpret inconsistently) and format 3 for
/// continuation chunks.
pub struct ChunkWriter {
    chunk_size: u32,
    last: HashMap<u32, OutStream>,
}

impl Default for ChunkWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn basic_header(out: &mut Vec<u8>, fmt: u8, csid: u32) {
    match csid {
        2..=63 => out.push(fmt << 6 | csid as u8),
        64..=319 => out.extend_from_slice(&[fmt << 6, (csid - 64) as u8]),
        _ => {
            let v = csid - 64;
            out.extend_from_slice(&[fmt << 6 | 1, v as u8, (v >> 8) as u8]);
        }
    }
}

impl ChunkWriter {
    pub fn new() -> Self {
        ChunkWriter { chunk_size: DEFAULT_CHUNK_SIZE, last: HashMap::new() }
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// Write a Set Chunk Size message and switch to the new size.
    pub fn write_set_chunk_size(&mut self, out: &mut Vec<u8>, size: u32) {
        let size = size.clamp(1, MAX_CHUNK_SIZE);
        self.write(out, 2, &Message::set_chunk_size(size));
        self.chunk_size = size;
    }

    /// Forget previous headers so the next message on every chunk stream
    /// carries a full (format 0) header.
    pub fn reset(&mut self) {
        self.last.clear();
    }

    /// Append `msg` as chunks on chunk stream `csid` (2..=65599).
    ///
    /// # Panics
    /// If `csid` is outside 2..=65599 or the payload is longer than 2^24-1
    /// bytes (neither can be encoded).
    pub fn write(&mut self, out: &mut Vec<u8>, csid: u32, msg: &Message) {
        assert!((2..=65_599).contains(&csid), "chunk stream id {csid} cannot be encoded");
        assert!(msg.payload.len() <= MAX_CHUNK_SIZE as usize, "message longer than 2^24-1 bytes");
        let len = msg.payload.len() as u32;
        let (fmt, field) = match self.last.get(&csid) {
            Some(prev) if prev.stream_id == msg.stream_id && msg.timestamp >= prev.timestamp => {
                let delta = msg.timestamp - prev.timestamp;
                if prev.length == len && prev.type_id == msg.type_id { (2, delta) } else { (1, delta) }
            }
            _ => (0, msg.timestamp),
        };
        let extended = field >= EXTENDED;

        basic_header(out, fmt, csid);
        out.extend_from_slice(&field.min(EXTENDED).to_be_bytes()[1..]);
        if fmt <= 1 {
            out.extend_from_slice(&len.to_be_bytes()[1..]);
            out.push(msg.type_id);
        }
        if fmt == 0 {
            out.extend_from_slice(&msg.stream_id.to_le_bytes());
        }
        if extended {
            out.extend_from_slice(&field.to_be_bytes());
        }
        let mut chunks = msg.payload.chunks(self.chunk_size as usize);
        out.extend_from_slice(chunks.next().unwrap_or_default());
        for c in chunks {
            basic_header(out, 3, csid);
            if extended {
                out.extend_from_slice(&field.to_be_bytes());
            }
            out.extend_from_slice(c);
        }
        self.last.insert(
            csid,
            OutStream { timestamp: msg.timestamp, length: len, type_id: msg.type_id, stream_id: msg.stream_id },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(r: &mut ChunkReader) -> Vec<Message> {
        let mut v = Vec::new();
        while let Some(m) = r.next_message().unwrap() {
            v.push(m);
        }
        v
    }

    fn msg(type_id: u8, sid: u32, ts: u32, len: usize, seed: u8) -> Message {
        Message::new(
            type_id,
            sid,
            ts,
            (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn roundtrip_with_chunk_size_change_and_compression() {
        let mut w = ChunkWriter::new();
        let mut out = Vec::new();
        let mut sent = vec![
            msg(msg_type::VIDEO, 1, 0, 5000, 1),
            msg(msg_type::AUDIO, 1, 0, 300, 2),
            msg(msg_type::VIDEO, 1, 33, 5000, 3), // fmt 2 (same len/type)
            msg(msg_type::VIDEO, 1, 66, 77, 4),   // fmt 1
        ];
        for (i, m) in sent.iter().enumerate() {
            w.write(&mut out, if m.type_id == msg_type::AUDIO { 4 } else { 6 }, m);
            if i == 1 {
                w.write_set_chunk_size(&mut out, 4096);
            }
        }
        sent.insert(2, Message::set_chunk_size(4096));
        w.reset();
        let after_reset = msg(msg_type::VIDEO, 1, 100, 10, 5);
        let before = out.len();
        w.write(&mut out, 6, &after_reset);
        assert_eq!(out[before] >> 6, 0, "reset forces a format 0 header");
        sent.push(after_reset);

        // Feed in awkward slices to exercise partial input.
        let mut r = ChunkReader::new();
        let mut got = Vec::new();
        for piece in out.chunks(7) {
            r.feed(piece);
            got.extend(read_all(&mut r));
        }
        assert_eq!(got, sent);
        assert_eq!(r.chunk_size(), 4096);
        assert_eq!(r.bytes_received(), out.len() as u64);
    }

    #[test]
    fn extended_timestamps_roundtrip() {
        let mut w = ChunkWriter::new();
        let mut out = Vec::new();
        let sent = vec![
            msg(msg_type::VIDEO, 1, 0x0100_0000, 300, 1), // fmt 0 extended, 3 chunks
            msg(msg_type::VIDEO, 1, 0x0100_0021, 300, 2), // fmt 2 small delta
            msg(msg_type::VIDEO, 1, 0xFFFF_FFF0, 10, 3),  // fmt 2 with extended delta
        ];
        for m in &sent {
            w.write(&mut out, 6, m);
        }
        let mut r = ChunkReader::new();
        r.feed(&out);
        assert_eq!(read_all(&mut r), sent);
    }

    #[test]
    fn hand_built_header_formats_and_csid_forms() {
        // RTMP spec example: audio on one chunk stream, fmt 0 then fmt 2 then fmt 3.
        let mut b = vec![0x04, 0, 0x03, 0xE8, 0, 0, 2, 8, 1, 0, 0, 0, 0xA1, 0xA2]; // ts 1000, len 2
        b.extend_from_slice(&[0x84, 0, 0, 20, 0xB1, 0xB2]); // fmt 2 delta 20
        b.extend_from_slice(&[0xC4, 0xC1, 0xC2]); // fmt 3: new message, same delta
        // 2-byte csid (64 + 10 = 74), fmt 0, then fmt 1 with a new length.
        b.extend_from_slice(&[0x00, 10, 0, 0, 5, 0, 0, 1, 9, 1, 0, 0, 0, 0xD1]);
        b.extend_from_slice(&[0x40, 10, 0, 0, 1, 0, 0, 2, 9, 0xE1, 0xE2]);
        // 3-byte csid (64 + 0x0102 = 322).
        b.extend_from_slice(&[0x01, 0x02, 0x01, 0, 0, 7, 0, 0, 1, 18, 1, 0, 0, 0, 0xF1]);
        let mut r = ChunkReader::new();
        r.feed(&b);
        let got: Vec<(u8, u32, Vec<u8>)> =
            read_all(&mut r).into_iter().map(|m| (m.type_id, m.timestamp, m.payload.to_vec())).collect();
        assert_eq!(
            got,
            vec![
                (8, 1000, vec![0xA1, 0xA2]),
                (8, 1020, vec![0xB1, 0xB2]),
                (8, 1040, vec![0xC1, 0xC2]),
                (9, 5, vec![0xD1]),
                (9, 6, vec![0xE1, 0xE2]),
                (18, 7, vec![0xF1]),
            ]
        );
    }

    #[test]
    fn interleaved_chunk_streams_and_abort() {
        // Two 200-byte messages interleaved chunk by chunk (128 + 72) on csid 4 and 6.
        let a: Vec<u8> = vec![0xAA; 200];
        let v: Vec<u8> = vec![0x55; 200];
        let mut b = vec![0x04, 0, 0, 1, 0, 0, 200, 8, 1, 0, 0, 0];
        b.extend_from_slice(&a[..128]);
        b.extend_from_slice(&[0x06, 0, 0, 1, 0, 0, 200, 9, 1, 0, 0, 0]);
        b.extend_from_slice(&v[..128]);
        b.push(0xC4);
        b.extend_from_slice(&a[128..]);
        // Abort the video message on csid 6 instead of finishing it.
        b.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 4, 2, 0, 0, 0, 0, 0, 0, 0, 6]);
        // A fresh message on csid 6 (fmt 3 is allowed after abort: same header, delta 0).
        b.push(0xC6);
        b.extend_from_slice(&v[..128]);
        b.push(0xC6);
        b.extend_from_slice(&v[128..]);
        let mut r = ChunkReader::new();
        r.feed(&b);
        let got = read_all(&mut r);
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].type_id, &got[0].payload[..]), (8, &a[..]));
        assert_eq!(got[1].type_id, msg_type::ABORT);
        assert_eq!((got[2].type_id, &got[2].payload[..]), (9, &v[..]));
    }

    #[test]
    fn tolerates_missing_extended_timestamp_on_continuations() {
        // fmt 0 with extended ts, 130-byte message: continuation chunk without the 4 ext bytes.
        let mut b = vec![0x06, 0xFF, 0xFF, 0xFF, 0, 0, 130, 9, 1, 0, 0, 0, 0x01, 0, 0, 0];
        b.extend_from_slice(&[7u8; 128]);
        b.push(0xC6);
        b.extend_from_slice(&[7u8; 2]);
        b.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 4, 1, 0, 0, 0, 0, 0, 0, 0, 128]); // trailing data
        let mut r = ChunkReader::new();
        r.feed(&b);
        let got = read_all(&mut r);
        assert_eq!(got[0].timestamp, 0x0100_0000);
        assert_eq!(got[0].payload.len(), 130);
    }

    #[test]
    #[should_panic(expected = "cannot be encoded")]
    fn reserved_chunk_stream_ids_are_refused() {
        ChunkWriter::new().write(&mut Vec::new(), 1, &msg(msg_type::AUDIO, 0, 0, 1, 1));
    }

    #[test]
    fn max_message_size() {
        let mut w = ChunkWriter::new();
        let mut out = Vec::new();
        w.write(&mut out, 3, &msg(msg_type::COMMAND_AMF0, 0, 0, 2000, 1));
        let mut r = ChunkReader::new();
        r.set_max_message_size(1000);
        r.feed(&out[..20]); // header only: refused before buffering the payload
        assert_eq!(r.next_message(), Err(ChunkError("message too large")));
        let mut r = ChunkReader::new();
        r.set_max_message_size(4000);
        r.feed(&out);
        assert_eq!(r.next_message().unwrap().unwrap().payload.len(), 2000);
    }

    #[test]
    fn unfinished_messages_are_bounded() {
        // Maximum-size messages opened on three chunk streams, first chunk only.
        let mut r = ChunkReader::new();
        for csid in 4..=6u8 {
            let mut b = vec![csid, 0, 0, 0, 0xFF, 0xFF, 0xFF, msg_type::VIDEO, 1, 0, 0, 0];
            b.extend_from_slice(&[0; DEFAULT_CHUNK_SIZE as usize]);
            r.feed(&b);
        }
        assert_eq!(r.next_message(), Err(ChunkError("too many unfinished messages")));
    }

    #[test]
    fn zero_length_messages_and_errors() {
        let mut r = ChunkReader::new();
        r.feed(&[0x04, 0, 0, 5, 0, 0, 0, 8, 1, 0, 0, 0]);
        assert_eq!(r.next_message().unwrap().unwrap().payload.len(), 0);

        let mut r = ChunkReader::new();
        r.feed(&[0x44, 0, 0, 5, 0, 0, 1, 8, 0]); // fmt 1 on an unknown chunk stream
        assert!(r.next_message().is_err());

        let mut r = ChunkReader::new();
        r.feed(&[0x02, 0, 0, 0, 0, 0, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0]); // chunk size 0
        assert!(r.next_message().is_err());

        // Garbage never panics.
        for seed in 0u32..500 {
            let bytes: Vec<u8> =
                (0..200u32).map(|i| (seed.wrapping_mul(2654435761).wrapping_add(i * 7919) >> 9) as u8).collect();
            let mut r = ChunkReader::new();
            r.feed(&bytes);
            while let Ok(Some(_)) = r.next_message() {}
        }
    }
}
