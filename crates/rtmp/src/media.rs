// SPDX-License-Identifier: GPL-3.0-only
//! Inspection of FLV / Enhanced RTMP audio, video and script-data payloads:
//! codec, keyframe and sequence-header detection, composition time, track id
//! and `onMetaData`.

use crate::amf0::{self, Amf0};

const AVC1: [u8; 4] = *b"avc1";
const HVC1: [u8; 4] = *b"hvc1";
const AV01: [u8; 4] = *b"av01";
const VP09: [u8; 4] = *b"vp09";
const VP08: [u8; 4] = *b"vp08";
const VVC1: [u8; 4] = *b"vvc1";
const MP4A: [u8; 4] = *b"mp4a";
const MP3: [u8; 4] = *b".mp3";
const OPUS: [u8; 4] = *b"Opus";
const FLAC: [u8; 4] = *b"fLaC";
const AC3: [u8; 4] = *b"ac-3";
const EAC3: [u8; 4] = *b"ec-3";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Legacy FLV codec id (video CodecID or audio SoundFormat).
    Legacy(u8),
    /// Enhanced RTMP FourCC (`avc1`, `hvc1`, `av01`, `mp4a`, `Opus`, ...).
    FourCc([u8; 4]),
    /// Command frames carry no codec.
    None,
}

impl Codec {
    pub fn video_name(&self) -> String {
        match self {
            Codec::Legacy(7) | Codec::FourCc(AVC1) => "H.264".into(),
            Codec::Legacy(12) | Codec::FourCc(HVC1) => "HEVC".into(),
            Codec::Legacy(13) | Codec::FourCc(AV01) => "AV1".into(),
            Codec::FourCc(VP09) => "VP9".into(),
            Codec::FourCc(VP08) => "VP8".into(),
            Codec::FourCc(VVC1) => "VVC".into(),
            Codec::Legacy(2) => "Sorenson H.263".into(),
            Codec::Legacy(3) | Codec::Legacy(6) => "Screen video".into(),
            Codec::Legacy(4) | Codec::Legacy(5) => "VP6".into(),
            other => other.raw_name(),
        }
    }

    pub fn audio_name(&self) -> String {
        match self {
            Codec::Legacy(10) | Codec::FourCc(MP4A) => "AAC".into(),
            Codec::Legacy(2) | Codec::Legacy(14) | Codec::FourCc(MP3) => "MP3".into(),
            Codec::FourCc(OPUS) => "Opus".into(),
            Codec::FourCc(FLAC) => "FLAC".into(),
            Codec::FourCc(AC3) => "AC-3".into(),
            Codec::FourCc(EAC3) => "E-AC-3".into(),
            other => other.raw_name(),
        }
    }

    fn raw_name(&self) -> String {
        match self {
            Codec::Legacy(id) => format!("codec id {id}"),
            Codec::FourCc(f) => String::from_utf8_lossy(f).into_owned(),
            Codec::None => "none".into(),
        }
    }

    /// Video codecs whose keyframes and sequence headers the delay engine understands.
    pub fn is_supported_video(&self) -> bool {
        matches!(self, Codec::Legacy(7 | 12 | 13) | Codec::FourCc(AVC1 | HVC1 | AV01 | VP09 | VP08 | VVC1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoKind {
    SequenceStart,
    CodedFrames,
    SequenceEnd,
    /// Enhanced RTMP metadata frame (e.g. HDR colour info).
    Metadata,
    /// Video info/command frame (no picture).
    Command,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoInfo {
    pub codec: Codec,
    /// 1 key, 2 inter, 3 disposable, 4 generated key, 5 command.
    pub frame_type: u8,
    pub kind: VideoKind,
    pub enhanced: bool,
    /// First track id of a multitrack message; `None` for single-track messages.
    pub track_id: Option<u8>,
    /// Composition time offset in ms (0 when absent).
    pub composition_time: i32,
}

impl VideoInfo {
    pub fn is_keyframe(&self) -> bool {
        self.frame_type == 1 && self.kind == VideoKind::CodedFrames
    }

    /// Keyframe on the default track (track 0 or a single-track stream).
    pub fn is_primary_keyframe(&self) -> bool {
        self.is_keyframe() && self.track_id.unwrap_or(0) == 0
    }

    /// Codec configuration that must precede coded frames after a jump.
    pub fn is_config(&self) -> bool {
        matches!(self.kind, VideoKind::SequenceStart | VideoKind::Metadata)
    }
}

fn si24(b: &[u8]) -> i32 {
    (i32::from_be_bytes([b[0], b[1], b[2], 0])) >> 8
}

fn fourcc(p: &[u8], i: &mut usize) -> Option<[u8; 4]> {
    let f = p.get(*i..*i + 4)?.try_into().ok()?;
    *i += 4;
    Some(f)
}

/// Skip ModEx blocks; returns the final packet type.
fn skip_modex(p: &[u8], i: &mut usize, mut packet_type: u8, modex: u8) -> Option<u8> {
    while packet_type == modex {
        let mut size = *p.get(*i)? as usize + 1;
        *i += 1;
        if size == 256 {
            size = u16::from_be_bytes([*p.get(*i)?, *p.get(*i + 1)?]) as usize + 1;
            *i += 2;
        }
        *i += size;
        packet_type = *p.get(*i)? & 0x0f;
        *i += 1;
    }
    Some(packet_type)
}

/// Parse a video message payload. `None` for empty or truncated headers.
pub fn parse_video(p: &[u8]) -> Option<VideoInfo> {
    let b0 = *p.first()?;
    if b0 & 0x80 == 0 {
        let frame_type = b0 >> 4;
        let codec = Codec::Legacy(b0 & 0x0f);
        let (kind, composition_time) = if frame_type == 5 {
            (VideoKind::Command, 0)
        } else if matches!(codec, Codec::Legacy(7 | 12 | 13)) {
            let kind = match *p.get(1)? {
                0 => VideoKind::SequenceStart,
                1 => VideoKind::CodedFrames,
                2 => VideoKind::SequenceEnd,
                _ => VideoKind::Unknown,
            };
            (kind, p.get(2..5).map_or(0, si24))
        } else {
            (VideoKind::CodedFrames, 0)
        };
        return Some(VideoInfo { codec, frame_type, kind, enhanced: false, track_id: None, composition_time });
    }

    let frame_type = (b0 >> 4) & 0x07;
    let mut i = 1;
    let mut packet_type = skip_modex(p, &mut i, b0 & 0x0f, 7)?;
    let mut info = VideoInfo {
        codec: Codec::None,
        frame_type,
        kind: VideoKind::Unknown,
        enhanced: true,
        track_id: None,
        composition_time: 0,
    };
    if packet_type != 4 && frame_type == 5 {
        info.kind = VideoKind::Command;
        return Some(info);
    }
    if packet_type == 6 {
        let b = *p.get(i)?;
        i += 1;
        let multitrack_type = b >> 4;
        packet_type = b & 0x0f;
        // One shared FourCC unless every track has its own (ManyTracksManyCodecs).
        info.codec = Codec::FourCc(fourcc(p, &mut i)?);
        info.track_id = Some(*p.get(i)?);
        i += 1;
        if multitrack_type != 0 {
            i += 3; // sizeOfVideoTrack
        }
    } else {
        info.codec = Codec::FourCc(fourcc(p, &mut i)?);
    }
    info.kind = match packet_type {
        0 | 5 => VideoKind::SequenceStart,
        1 | 3 => VideoKind::CodedFrames,
        2 => VideoKind::SequenceEnd,
        4 => VideoKind::Metadata,
        _ => VideoKind::Unknown,
    };
    if packet_type == 1 && matches!(info.codec, Codec::FourCc(AVC1 | HVC1 | VVC1)) {
        info.composition_time = p.get(i..i + 3).map_or(0, si24);
    }
    Some(info)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInfo {
    pub codec: Codec,
    /// Sequence header / multichannel config that must precede coded frames.
    pub is_config: bool,
    pub track_id: Option<u8>,
}

/// Parse an audio message payload. `None` for empty (silence) or truncated messages.
pub fn parse_audio(p: &[u8]) -> Option<AudioInfo> {
    let b0 = *p.first()?;
    let sound_format = b0 >> 4;
    if sound_format != 9 {
        let is_config = sound_format == 10 && p.get(1) == Some(&0);
        return Some(AudioInfo { codec: Codec::Legacy(sound_format), is_config, track_id: None });
    }
    let mut i = 1;
    let mut packet_type = skip_modex(p, &mut i, b0 & 0x0f, 7)?;
    let mut track_id = None;
    if packet_type == 5 {
        let b = *p.get(i)?;
        i += 1;
        packet_type = b & 0x0f;
        let codec = Codec::FourCc(fourcc(p, &mut i)?);
        track_id = Some(*p.get(i)?);
        return Some(AudioInfo { codec, is_config: matches!(packet_type, 0 | 4), track_id });
    }
    let codec = Codec::FourCc(fourcc(p, &mut i)?);
    Some(AudioInfo { codec, is_config: matches!(packet_type, 0 | 4), track_id })
}

/// Script data larger than this is never parsed as `onMetaData` (real
/// metadata is a few hundred bytes; this bounds work on hostile input).
pub const MAX_METADATA_BYTES: usize = 64 * 1024;

/// The `onMetaData` object of a script-data message (`@setDataFrame` form or
/// plain). Only the leading values are decoded unless the name matches.
pub fn metadata(amf0_body: &[u8]) -> Option<Amf0> {
    if amf0_body.len() > MAX_METADATA_BYTES {
        return None;
    }
    let mut rest = amf0_body;
    let mut name = amf0::decode_one(&mut rest).ok()?;
    if name.as_str() == Some("@setDataFrame") {
        name = amf0::decode_one(&mut rest).ok()?;
    }
    if name.as_str()? != "onMetaData" {
        return None;
    }
    while !rest.is_empty() {
        let v = amf0::decode_one(&mut rest).ok()?;
        if matches!(v, Amf0::Object(_) | Amf0::EcmaArray(_)) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_avc() {
        let seq = parse_video(&[0x17, 0, 0, 0, 0, 1, 2]).unwrap();
        assert!(seq.is_config() && !seq.is_keyframe());
        assert_eq!(seq.codec.video_name(), "H.264");

        let key = parse_video(&[0x17, 1, 0, 0, 0x21, 0, 0, 0, 1]).unwrap();
        assert!(key.is_keyframe() && key.is_primary_keyframe());
        assert_eq!(key.composition_time, 0x21);

        let inter = parse_video(&[0x27, 1, 0xFF, 0xFF, 0xF6]).unwrap();
        assert!(!inter.is_keyframe());
        assert_eq!(inter.composition_time, -10);

        assert_eq!(parse_video(&[0x52, 0]).unwrap().kind, VideoKind::Command);
        assert!(parse_video(&[]).is_none());
        assert!(parse_video(&[0x17]).is_none());
        assert!(!Codec::Legacy(2).is_supported_video());
    }

    #[test]
    fn enhanced_single_track() {
        // KeyFrame, CodedFramesX, hvc1
        let v = parse_video(&[0x93, b'h', b'v', b'c', b'1', 0, 0, 0, 1]).unwrap();
        assert!(v.enhanced && v.is_keyframe());
        assert_eq!(v.codec, Codec::FourCc(*b"hvc1"));
        // KeyFrame, SequenceStart, av01
        assert!(parse_video(&[0x90, b'a', b'v', b'0', b'1', 0x81]).unwrap().is_config());
        // InterFrame, CodedFrames with composition time, avc1
        let v = parse_video(&[0xA1, b'a', b'v', b'c', b'1', 0, 0, 40, 9]).unwrap();
        assert!(!v.is_keyframe());
        assert_eq!(v.composition_time, 40);
        // Metadata packet: frame type ignored, config.
        assert!(parse_video(&[0xD4, b'h', b'v', b'c', b'1', 2]).unwrap().is_config());
        // Command frame has no FourCC.
        assert_eq!(parse_video(&[0xD1, 0]).unwrap().kind, VideoKind::Command);
    }

    #[test]
    fn enhanced_modex_and_multitrack() {
        // ModEx (1 byte of data: size byte 0 -> 1 byte), then CodedFramesX keyframe hvc1.
        let v = parse_video(&[0x97, 0, 0xAA, 0x03, b'h', b'v', b'c', b'1', 9]).unwrap();
        assert!(v.is_keyframe());
        // Multitrack, OneTrack, CodedFramesX, avc1, track 2.
        let v = parse_video(&[0x96, 0x03, b'a', b'v', b'c', b'1', 2, 0, 0, 1]).unwrap();
        assert_eq!(v.track_id, Some(2));
        assert!(v.is_keyframe() && !v.is_primary_keyframe());
        // Multitrack, ManyTracks, SequenceStart, avc1, track 0 with size.
        let v = parse_video(&[0x96, 0x10, b'a', b'v', b'c', b'1', 0, 0, 0, 1, 7]).unwrap();
        assert_eq!((v.track_id, v.kind), (Some(0), VideoKind::SequenceStart));
        // ManyTracksManyCodecs: FourCC precedes the track id.
        let v = parse_video(&[0x96, 0x21, b'h', b'v', b'c', b'1', 0, 0, 0, 4, 0, 0, 0, 1]).unwrap();
        assert_eq!((v.codec, v.track_id), (Codec::FourCc(*b"hvc1"), Some(0)));
        assert!(v.is_primary_keyframe());
        // Truncated ModEx does not panic.
        assert!(parse_video(&[0x97, 0xFF]).is_none());
    }

    #[test]
    fn audio() {
        let aac_seq = parse_audio(&[0xAF, 0, 0x12, 0x10]).unwrap();
        assert!(aac_seq.is_config);
        assert_eq!(aac_seq.codec.audio_name(), "AAC");
        assert!(!parse_audio(&[0xAF, 1, 0x21]).unwrap().is_config);
        assert!(parse_audio(&[]).is_none());
        // Enhanced: SequenceStart Opus.
        let a = parse_audio(&[0x90, b'O', b'p', b'u', b's', 1]).unwrap();
        assert!(a.is_config);
        assert_eq!(a.codec.audio_name(), "Opus");
        // Enhanced multitrack OneTrack CodedFrames mp4a track 1.
        let a = parse_audio(&[0x95, 0x01, b'm', b'p', b'4', b'a', 1, 0x21]).unwrap();
        assert_eq!((a.is_config, a.track_id), (false, Some(1)));
    }

    #[test]
    fn metadata_forms() {
        let obj = Amf0::EcmaArray(vec![("width".into(), Amf0::Number(1920.0))]);
        let a = amf0::encode_all(&[Amf0::str("@setDataFrame"), Amf0::str("onMetaData"), obj.clone()]);
        assert_eq!(metadata(&a), Some(obj.clone()));
        let b = amf0::encode_all(&[Amf0::str("onMetaData"), obj.clone()]);
        assert_eq!(metadata(&b).unwrap().get("width").and_then(Amf0::as_f64), Some(1920.0));
        assert_eq!(metadata(&amf0::encode_all(&[Amf0::str("onCuePoint"), obj.clone()])), None);
        let mut huge = amf0::encode_all(&[Amf0::str("onMetaData"), obj]);
        huge.resize(MAX_METADATA_BYTES + 1, 0x05);
        assert_eq!(metadata(&huge), None, "oversized script data is not parsed");
    }
}
