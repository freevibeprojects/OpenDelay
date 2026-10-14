// SPDX-License-Identifier: GPL-3.0-only
//! AMF0 encoding and decoding (the subset used by RTMP command and data messages).

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Amf0 {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Amf0)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Amf0)>),
    StrictArray(Vec<Amf0>),
    Date { millis: f64, tz: i16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amf0Error(pub &'static str);

impl fmt::Display for Amf0Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AMF0: {}", self.0)
    }
}
impl std::error::Error for Amf0Error {}

const NUMBER: u8 = 0x00;
const BOOLEAN: u8 = 0x01;
const STRING: u8 = 0x02;
const OBJECT: u8 = 0x03;
const NULL: u8 = 0x05;
const UNDEFINED: u8 = 0x06;
const REFERENCE: u8 = 0x07;
const ECMA_ARRAY: u8 = 0x08;
const OBJECT_END: u8 = 0x09;
const STRICT_ARRAY: u8 = 0x0A;
const DATE: u8 = 0x0B;
const LONG_STRING: u8 = 0x0C;
const UNSUPPORTED: u8 = 0x0D;
const XML_DOCUMENT: u8 = 0x0F;
const TYPED_OBJECT: u8 = 0x10;
const AVMPLUS: u8 = 0x11;

const MAX_DEPTH: usize = 32;

impl Amf0 {
    pub fn str(s: &str) -> Self {
        Amf0::String(s.to_owned())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf0::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf0::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Property lookup for objects and ECMA arrays.
    pub fn get(&self, key: &str) -> Option<&Amf0> {
        match self {
            Amf0::Object(p) | Amf0::EcmaArray(p) => p.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Set (or add) a property on an object or ECMA array.
    pub fn set(&mut self, key: &str, value: Amf0) {
        if let Amf0::Object(p) | Amf0::EcmaArray(p) = self {
            match p.iter_mut().find(|(k, _)| k == key) {
                Some((_, v)) => *v = value,
                None => p.push((key.to_owned(), value)),
            }
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Amf0::Number(n) => {
                out.push(NUMBER);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Amf0::Boolean(b) => {
                out.push(BOOLEAN);
                out.push(*b as u8);
            }
            Amf0::String(s) => {
                if s.len() > u16::MAX as usize {
                    out.push(LONG_STRING);
                    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
                    out.extend_from_slice(s.as_bytes());
                } else {
                    out.push(STRING);
                    write_short_str(out, s);
                }
            }
            Amf0::Object(props) => {
                out.push(OBJECT);
                write_props(out, props);
            }
            Amf0::Null => out.push(NULL),
            Amf0::Undefined => out.push(UNDEFINED),
            Amf0::EcmaArray(props) => {
                out.push(ECMA_ARRAY);
                out.extend_from_slice(&(props.len() as u32).to_be_bytes());
                write_props(out, props);
            }
            Amf0::StrictArray(items) => {
                out.push(STRICT_ARRAY);
                out.extend_from_slice(&(items.len() as u32).to_be_bytes());
                for v in items {
                    v.encode(out);
                }
            }
            Amf0::Date { millis, tz } => {
                out.push(DATE);
                out.extend_from_slice(&millis.to_be_bytes());
                out.extend_from_slice(&tz.to_be_bytes());
            }
        }
    }
}

fn write_short_str(out: &mut Vec<u8>, s: &str) {
    let b = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
    out.extend_from_slice(&(b.len() as u16).to_be_bytes());
    out.extend_from_slice(b);
}

fn write_props(out: &mut Vec<u8>, props: &[(String, Amf0)]) {
    for (k, v) in props {
        write_short_str(out, k);
        v.encode(out);
    }
    out.extend_from_slice(&[0, 0, OBJECT_END]);
}

/// Encode a sequence of values (e.g. a command: name, transaction id, object, args...).
pub fn encode_all(values: &[Amf0]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        v.encode(&mut out);
    }
    out
}

/// Decode every value in `data`.
pub fn decode_all(mut data: &[u8]) -> Result<Vec<Amf0>, Amf0Error> {
    let mut values = Vec::new();
    while !data.is_empty() {
        values.push(decode_value(&mut data, 0)?);
    }
    Ok(values)
}

fn take<'a>(data: &mut &'a [u8], n: usize) -> Result<&'a [u8], Amf0Error> {
    if data.len() < n {
        return Err(Amf0Error("truncated"));
    }
    let (head, rest) = data.split_at(n);
    *data = rest;
    Ok(head)
}

fn read_u16(data: &mut &[u8]) -> Result<u16, Amf0Error> {
    let b = take(data, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(data: &mut &[u8]) -> Result<u32, Amf0Error> {
    let b = take(data, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_f64(data: &mut &[u8]) -> Result<f64, Amf0Error> {
    let b = take(data, 8)?;
    Ok(f64::from_be_bytes(b.try_into().unwrap()))
}

fn read_string(data: &mut &[u8], len: usize) -> Result<String, Amf0Error> {
    Ok(String::from_utf8_lossy(take(data, len)?).into_owned())
}

fn read_props(data: &mut &[u8], depth: usize) -> Result<Vec<(String, Amf0)>, Amf0Error> {
    let mut props = Vec::new();
    loop {
        let len = read_u16(data)? as usize;
        if len == 0 {
            // Empty key: expect the object-end marker. Some encoders omit it at the very end.
            match data.first() {
                Some(&OBJECT_END) => {
                    *data = &data[1..];
                    return Ok(props);
                }
                None => return Ok(props),
                Some(_) => {
                    let v = decode_value(data, depth + 1)?;
                    props.push((String::new(), v));
                    continue;
                }
            }
        }
        let key = read_string(data, len)?;
        let v = decode_value(data, depth + 1)?;
        props.push((key, v));
    }
}

fn decode_value(data: &mut &[u8], depth: usize) -> Result<Amf0, Amf0Error> {
    if depth > MAX_DEPTH {
        return Err(Amf0Error("nesting too deep"));
    }
    let marker = take(data, 1)?[0];
    Ok(match marker {
        NUMBER => Amf0::Number(read_f64(data)?),
        BOOLEAN => Amf0::Boolean(take(data, 1)?[0] != 0),
        STRING => {
            let len = read_u16(data)? as usize;
            Amf0::String(read_string(data, len)?)
        }
        LONG_STRING | XML_DOCUMENT => {
            let len = read_u32(data)? as usize;
            Amf0::String(read_string(data, len)?)
        }
        OBJECT => Amf0::Object(read_props(data, depth)?),
        TYPED_OBJECT => {
            let len = read_u16(data)? as usize;
            take(data, len)?; // class name, ignored
            Amf0::Object(read_props(data, depth)?)
        }
        NULL => Amf0::Null,
        UNDEFINED | UNSUPPORTED => Amf0::Undefined,
        REFERENCE => {
            take(data, 2)?;
            Amf0::Undefined
        }
        ECMA_ARRAY => {
            read_u32(data)?; // advisory count, not trusted
            Amf0::EcmaArray(read_props(data, depth)?)
        }
        STRICT_ARRAY => {
            let count = read_u32(data)? as usize;
            let mut items = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                items.push(decode_value(data, depth + 1)?);
            }
            Amf0::StrictArray(items)
        }
        DATE => {
            let millis = read_f64(data)?;
            let tz = read_u16(data)? as i16;
            Amf0::Date { millis, tz }
        }
        AVMPLUS => return Err(Amf0Error("AMF3 values are not supported")),
        _ => return Err(Amf0Error("unknown marker")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_types() {
        let values = vec![
            Amf0::str("connect"),
            Amf0::Number(1.0),
            Amf0::Object(vec![
                ("app".into(), Amf0::str("live")),
                ("fpad".into(), Amf0::Boolean(false)),
                ("nested".into(), Amf0::Object(vec![("x".into(), Amf0::Number(-2.5))])),
            ]),
            Amf0::Null,
            Amf0::Undefined,
            Amf0::EcmaArray(vec![("width".into(), Amf0::Number(1920.0))]),
            Amf0::StrictArray(vec![Amf0::str("hvc1"), Amf0::str("av01")]),
            Amf0::Date { millis: 1.5e12, tz: 0 },
        ];
        let bytes = encode_all(&values);
        assert_eq!(decode_all(&bytes).unwrap(), values);
    }

    #[test]
    fn known_encoding() {
        let mut out = Vec::new();
        Amf0::str("ab").encode(&mut out);
        assert_eq!(out, [0x02, 0, 2, b'a', b'b']);
        out.clear();
        Amf0::Number(1.0).encode(&mut out);
        assert_eq!(out, [0x00, 0x3F, 0xF0, 0, 0, 0, 0, 0, 0]);
        out.clear();
        Amf0::Object(vec![]).encode(&mut out);
        assert_eq!(out, [0x03, 0, 0, 0x09]);
    }

    #[test]
    fn long_string_and_typed_object() {
        let long = "x".repeat(70_000);
        let bytes = encode_all(&[Amf0::String(long.clone())]);
        assert_eq!(bytes[0], LONG_STRING);
        assert_eq!(decode_all(&bytes).unwrap(), vec![Amf0::String(long)]);

        let typed = [TYPED_OBJECT, 0, 1, b'C', 0, 1, b'k', NULL, 0, 0, OBJECT_END];
        assert_eq!(
            decode_all(&typed).unwrap(),
            vec![Amf0::Object(vec![("k".into(), Amf0::Null)])]
        );
    }

    #[test]
    fn errors_do_not_panic() {
        assert!(decode_all(&[0x00, 1, 2]).is_err());
        assert!(decode_all(&[0x02, 0, 5, b'a']).is_err());
        assert!(decode_all(&[0x42]).is_err());
        // deep nesting is rejected instead of overflowing the stack
        let deep: Vec<u8> = std::iter::repeat_n([STRICT_ARRAY, 0, 0, 0, 1], 100).flatten().collect();
        assert!(decode_all(&deep).is_err());
        // garbage never panics
        for seed in 0u32..2000 {
            let bytes: Vec<u8> = (0..(seed % 40)).map(|i| (seed.wrapping_mul(2654435761).wrapping_add(i * 97) >> 7) as u8).collect();
            let _ = decode_all(&bytes);
        }
    }

    #[test]
    fn get_and_set() {
        let mut o = Amf0::Object(vec![("app".into(), Amf0::str("a"))]);
        o.set("app", Amf0::str("b"));
        o.set("tcUrl", Amf0::str("rtmp://h/b"));
        assert_eq!(o.get("app").and_then(Amf0::as_str), Some("b"));
        assert_eq!(o.get("tcUrl").and_then(Amf0::as_str), Some("rtmp://h/b"));
    }
}
