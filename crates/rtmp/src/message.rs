// SPDX-License-Identifier: GPL-3.0-only
//! RTMP messages, protocol control messages and AMF0 commands.

use crate::amf0::{self, Amf0, Amf0Error};
use bytes::Bytes;

/// RTMP message type ids.
pub mod msg_type {
    pub const SET_CHUNK_SIZE: u8 = 1;
    pub const ABORT: u8 = 2;
    pub const ACK: u8 = 3;
    pub const USER_CONTROL: u8 = 4;
    pub const WINDOW_ACK_SIZE: u8 = 5;
    pub const SET_PEER_BANDWIDTH: u8 = 6;
    pub const AUDIO: u8 = 8;
    pub const VIDEO: u8 = 9;
    pub const DATA_AMF3: u8 = 15;
    pub const COMMAND_AMF3: u8 = 17;
    pub const DATA_AMF0: u8 = 18;
    pub const COMMAND_AMF0: u8 = 20;
    pub const AGGREGATE: u8 = 22;
}

/// User control event types (message type 4).
pub mod user_control {
    pub const STREAM_BEGIN: u16 = 0;
    pub const STREAM_EOF: u16 = 1;
    pub const PING_REQUEST: u16 = 6;
    pub const PING_RESPONSE: u16 = 7;
}

/// One complete RTMP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub type_id: u8,
    pub stream_id: u32,
    /// Milliseconds; wraps at 2^32.
    pub timestamp: u32,
    pub payload: Bytes,
}

impl Message {
    pub fn new(type_id: u8, stream_id: u32, timestamp: u32, payload: impl Into<Bytes>) -> Self {
        Message { type_id, stream_id, timestamp, payload: payload.into() }
    }

    pub fn set_chunk_size(size: u32) -> Self {
        Self::new(msg_type::SET_CHUNK_SIZE, 0, 0, size.to_be_bytes().to_vec())
    }

    pub fn ack(sequence: u32) -> Self {
        Self::new(msg_type::ACK, 0, 0, sequence.to_be_bytes().to_vec())
    }

    pub fn window_ack_size(size: u32) -> Self {
        Self::new(msg_type::WINDOW_ACK_SIZE, 0, 0, size.to_be_bytes().to_vec())
    }

    /// `limit`: 0 = hard, 1 = soft, 2 = dynamic.
    pub fn set_peer_bandwidth(size: u32, limit: u8) -> Self {
        let mut p = size.to_be_bytes().to_vec();
        p.push(limit);
        Self::new(msg_type::SET_PEER_BANDWIDTH, 0, 0, p)
    }

    pub fn user_control(event: u16, data: &[u8]) -> Self {
        let mut p = event.to_be_bytes().to_vec();
        p.extend_from_slice(data);
        Self::new(msg_type::USER_CONTROL, 0, 0, p)
    }

    pub fn is_media(&self) -> bool {
        matches!(self.type_id, msg_type::AUDIO | msg_type::VIDEO | msg_type::DATA_AMF0 | msg_type::DATA_AMF3)
    }

    /// First big-endian u32 of the payload (control message argument).
    pub fn u32_arg(&self) -> Option<u32> {
        let b = self.payload.get(..4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// AMF0 body of a command or data message (AMF3 variants carry a leading format byte).
    pub fn amf0_body(&self) -> &[u8] {
        match self.type_id {
            msg_type::COMMAND_AMF3 | msg_type::DATA_AMF3 if self.payload.first() == Some(&0) => &self.payload[1..],
            _ => &self.payload,
        }
    }
}

/// A NetConnection / NetStream command (`connect`, `publish`, `_result`, `onStatus`, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    pub transaction_id: f64,
    pub object: Amf0,
    pub args: Vec<Amf0>,
}

impl Command {
    pub fn new(name: &str, transaction_id: f64, object: Amf0, args: Vec<Amf0>) -> Self {
        Command { name: name.to_owned(), transaction_id, object, args }
    }

    pub fn parse(msg: &Message) -> Result<Self, Amf0Error> {
        let mut values = amf0::decode_all(msg.amf0_body())?.into_iter();
        let name = match values.next() {
            Some(Amf0::String(s)) => s,
            _ => return Err(Amf0Error("command name missing")),
        };
        let transaction_id = values.next().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let object = values.next().unwrap_or(Amf0::Null);
        Ok(Command { name, transaction_id, object, args: values.collect() })
    }

    pub fn to_message(&self, stream_id: u32) -> Message {
        let mut out = Vec::new();
        Amf0::String(self.name.clone()).encode(&mut out);
        Amf0::Number(self.transaction_id).encode(&mut out);
        self.object.encode(&mut out);
        for a in &self.args {
            a.encode(&mut out);
        }
        Message::new(msg_type::COMMAND_AMF0, stream_id, 0, out)
    }

    /// First argument as a string (stream key of `publish`, `releaseStream`, ...).
    pub fn arg_str(&self, i: usize) -> Option<&str> {
        self.args.get(i).and_then(Amf0::as_str)
    }
}

/// `onStatus` info object, e.g. `NetStream.Publish.Start`.
pub fn status_object(level: &str, code: &str, description: &str) -> Amf0 {
    Amf0::Object(vec![
        ("level".into(), Amf0::str(level)),
        ("code".into(), Amf0::str(code)),
        ("description".into(), Amf0::str(description)),
    ])
}

/// Split an aggregate message (type 22) into its audio/video/data sub-messages.
/// Sub-message timestamps are re-based on the aggregate's own timestamp.
pub fn split_aggregate(msg: &Message) -> Vec<Message> {
    let mut out = Vec::new();
    let p = &msg.payload;
    let mut i = 0usize;
    let mut base: Option<u32> = None;
    while i + 11 <= p.len() {
        let type_id = p[i] & 0x1f;
        let len = u32::from_be_bytes([0, p[i + 1], p[i + 2], p[i + 3]]) as usize;
        let ts = u32::from_be_bytes([p[i + 7], p[i + 4], p[i + 5], p[i + 6]]);
        let start = i + 11;
        if start + len > p.len() {
            break;
        }
        let first = *base.get_or_insert(ts);
        let timestamp = msg.timestamp.wrapping_add(ts.wrapping_sub(first));
        out.push(Message::new(type_id, msg.stream_id, timestamp, p.slice(start..start + len)));
        i = start + len + 4; // skip the back-pointer
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrip_and_amf3_prefix() {
        let cmd = Command::new("publish", 5.0, Amf0::Null, vec![Amf0::str("key"), Amf0::str("live")]);
        let msg = cmd.to_message(1);
        assert_eq!(msg.type_id, msg_type::COMMAND_AMF0);
        assert_eq!(Command::parse(&msg).unwrap(), cmd);
        assert_eq!(Command::parse(&msg).unwrap().arg_str(0), Some("key"));

        let mut p = vec![0u8];
        p.extend_from_slice(&msg.payload);
        let amf3 = Message::new(msg_type::COMMAND_AMF3, 1, 0, p);
        assert_eq!(Command::parse(&amf3).unwrap(), cmd);
    }

    #[test]
    fn control_messages() {
        assert_eq!(Message::set_chunk_size(4096).u32_arg(), Some(4096));
        assert_eq!(&Message::set_peer_bandwidth(5, 2).payload[..], &[0, 0, 0, 5, 2]);
        assert_eq!(&Message::user_control(user_control::PING_RESPONSE, &[0, 0, 0, 9]).payload[..], &[0, 7, 0, 0, 0, 9]);
    }

    #[test]
    fn aggregate_split() {
        let mut p = Vec::new();
        for (t, ts, body) in [(9u8, 1000u32, &b"vv"[..]), (8, 1020, &b"a"[..])] {
            p.push(t);
            p.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            p.extend_from_slice(&ts.to_be_bytes()[1..]);
            p.push((ts >> 24) as u8);
            p.extend_from_slice(&[0, 0, 0]);
            p.extend_from_slice(body);
            p.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
        }
        let agg = Message::new(msg_type::AGGREGATE, 1, 5000, p);
        let parts = split_aggregate(&agg);
        assert_eq!(parts.len(), 2);
        assert_eq!((parts[0].type_id, parts[0].timestamp, &parts[0].payload[..]), (9, 5000, &b"vv"[..]));
        assert_eq!((parts[1].type_id, parts[1].timestamp, &parts[1].payload[..]), (8, 5020, &b"a"[..]));
    }
}
