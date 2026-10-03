//! The RemSound packet layer: header, packet types, the Format and Heartbeat payloads, and the
//! PCM multipart sub-header. Byte layouts are in docs/PROTOCOL.md, with citations.

use crate::crypto::{Fingerprint, FINGERPRINT_BYTES};

pub const MAGIC: u32 = 0x444E_4D52; // "RMND" little-endian
pub const VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 12;
pub const DEFAULT_AUDIO_PORT: u16 = 47830;
pub const DEFAULT_DISCOVERY_PORT: u16 = 47821;
/// Largest ciphertext slice in one PCM part (1500 - 20 IP - 8 UDP - 12 header - 6 sub-header).
pub const MAX_AUDIO_PAYLOAD_BYTES: usize = 1454;
/// Stream id used by heartbeats and tick proofs.
pub const CONTROL_STREAM_ID: u16 = 0xFFFF;

pub const FORMAT_PAYLOAD_SIZE: usize = 32;
pub const FORMAT_PAYLOAD_EXTENDED_SIZE: usize = 36;
pub const FORMAT_PAYLOAD_WITH_FINGERPRINT_SIZE: usize = 44;
pub const FORMAT_PAYLOAD_WITH_CAPTURE_SIZE: usize = 46;
pub const CAPTURE_LATENCY_TICKS_PER_MS: f64 = 10.0;
pub const HEARTBEAT_PAYLOAD_SIZE: usize = 9;
pub const HEARTBEAT_PAYLOAD_WITH_FLAGS_SIZE: usize = 10;
/// Largest Opus frame at 48 kHz (60 ms). Nothing legitimate exceeds it.
pub const MAX_FRAME_SAMPLES_PER_CHANNEL: i32 = 2880;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Format = 1,
    Audio = 2,
    KeepAlive = 3,
    Heartbeat = 4,
    Control = 5,
    AddrCheck = 10,
    TickProof = 11,
    Metronome = 12,
}

impl PacketType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Format,
            2 => Self::Audio,
            3 => Self::KeepAlive,
            4 => Self::Heartbeat,
            5 => Self::Control,
            10 => Self::AddrCheck,
            11 => Self::TickProof,
            12 => Self::Metronome,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The raw type byte; see [`PacketType::from_u8`]. Unknown types are kept so callers can count them.
    pub kind: u8,
    pub stream_id: u16,
    pub sequence: u32,
}

impl Header {
    pub fn packet_type(&self) -> Option<PacketType> {
        PacketType::from_u8(self.kind)
    }
}

/// Write a header. A stream id of 0 never reaches the wire: it goes out as 1.
pub fn write_header(out: &mut Vec<u8>, kind: PacketType, stream_id: u16, sequence: u32) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.push(VERSION);
    out.push(kind as u8);
    out.extend_from_slice(&(if stream_id == 0 { 1 } else { stream_id }).to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
}

/// A whole packet: header then payload.
pub fn packet(kind: PacketType, stream_id: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    write_header(&mut out, kind, stream_id, sequence);
    out.extend_from_slice(payload);
    out
}

/// Read a header and return it with the payload. None for short packets, a wrong magic or a wrong version.
pub fn read_header(packet: &[u8]) -> Option<(Header, &[u8])> {
    if packet.len() < HEADER_SIZE {
        return None;
    }
    if u32::from_le_bytes(packet[0..4].try_into().unwrap()) != MAGIC || packet[4] != VERSION {
        return None;
    }
    let mut stream_id = u16::from_le_bytes([packet[6], packet[7]]);
    if stream_id == 0 {
        stream_id = 1;
    }
    let sequence = u32::from_le_bytes(packet[8..12].try_into().unwrap());
    Some((
        Header {
            kind: packet[5],
            stream_id,
            sequence,
        },
        &packet[HEADER_SIZE..],
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Codec {
    Pcm = 1,
    Opus = 2,
    OpusCustom = 3,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: i32,
    pub channels: i32,
    pub bits_per_sample: i32,
    pub encoding: i32,
    pub block_align: i32,
    pub avg_bytes_per_sec: i32,
    pub codec: i32,
    pub frame_samples_per_channel: i32,
    /// 0 Mixed, 1 WasapiLane, 2 AsioLane. Unknown values read as 0.
    pub lane: u8,
    pub lab_flags: u8,
    /// Milliseconds; 0 means not stated.
    pub capture_latency_ms: f64,
}

impl AudioFormat {
    /// What a Windows Opus sender announces (SenderLane.EnsureFormatPacketSent), for our own stream.
    pub fn opus_48k_stereo(frame_samples_per_channel: i32) -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 16,
            encoding: 1,
            block_align: 4,
            avg_bytes_per_sec: 192_000,
            codec: Codec::Opus as i32,
            frame_samples_per_channel,
            lane: 0,
            lab_flags: 0,
            capture_latency_ms: 0.0,
        }
    }

    /// What a Windows PCM sender announces.
    pub fn pcm_48k_stereo(frame_samples_per_channel: i32) -> Self {
        Self {
            bits_per_sample: 24,
            block_align: 6,
            avg_bytes_per_sec: 288_000,
            codec: Codec::Pcm as i32,
            ..Self::opus_48k_stereo(frame_samples_per_channel)
        }
    }

    pub fn codec(&self) -> Option<Codec> {
        match self.codec {
            1 => Some(Codec::Pcm),
            2 => Some(Codec::Opus),
            3 => Some(Codec::OpusCustom),
            _ => None,
        }
    }

    /// Mirrors AudioFormatInfo.IsUsable, except that custom-mode Opus is refused outright because
    /// this build has no decoder for it. Fails closed: the Format packet is unauthenticated.
    pub fn check_usable(&self) -> Result<(), String> {
        if !(1..=2).contains(&self.channels) {
            return Err(format!("channel count {} (must be 1 or 2)", self.channels));
        }
        if !(8000..=192_000).contains(&self.sample_rate) {
            return Err(format!(
                "sample rate {} (outside 8000-192000)",
                self.sample_rate
            ));
        }
        if !(1..=MAX_FRAME_SAMPLES_PER_CHANNEL).contains(&self.frame_samples_per_channel) {
            return Err(format!(
                "frame size {} samples (must be 1-{MAX_FRAME_SAMPLES_PER_CHANNEL})",
                self.frame_samples_per_channel
            ));
        }
        match self.codec() {
            None => Err(format!("unknown codec {}", self.codec)),
            Some(Codec::OpusCustom) => {
                Err("Jamulus-style custom Opus, which this daemon cannot decode".into())
            }
            Some(Codec::Opus)
                if ![8000, 12000, 16000, 24000, 48000].contains(&self.sample_rate) =>
            {
                Err(format!(
                    "Opus at {} Hz (Opus supports 8000, 12000, 16000, 24000 or 48000)",
                    self.sample_rate
                ))
            }
            _ => Ok(()),
        }
    }

    /// Plain words for logs: "Opus, 48 kHz stereo, 2.5 ms frames".
    pub fn describe(&self) -> String {
        let khz = self.sample_rate as f64 / 1000.0;
        let layout = match self.channels {
            1 => "mono",
            2 => "stereo",
            _ => "multichannel",
        };
        let frame_ms =
            self.frame_samples_per_channel as f64 * 1000.0 / self.sample_rate.max(1) as f64;
        let codec = match self.codec() {
            Some(Codec::Pcm) => "PCM",
            Some(Codec::Opus) => "Opus",
            Some(Codec::OpusCustom) => "custom Opus",
            None => "an unknown codec",
        };
        format!("{codec}, {khz} kHz {layout}, {frame_ms} ms frames")
    }
}

/// Write the Format payload. With a fingerprint it is 46 bytes (fingerprint plus capture latency), without one 36.
pub fn write_format_payload(format: &AudioFormat, fingerprint: Option<&Fingerprint>) -> Vec<u8> {
    let mut out = Vec::with_capacity(FORMAT_PAYLOAD_WITH_CAPTURE_SIZE);
    for v in [
        format.sample_rate,
        format.channels,
        format.bits_per_sample,
        format.encoding,
        format.block_align,
        format.avg_bytes_per_sec,
        format.codec,
        format.frame_samples_per_channel,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&[format.lane, format.lab_flags, 0, 0]);
    if let Some(print) = fingerprint {
        out.extend_from_slice(print);
        let ticks = format.capture_latency_ms * CAPTURE_LATENCY_TICKS_PER_MS;
        let clamped: u16 = if ticks <= 0.0 {
            0
        } else if ticks >= u16::MAX as f64 {
            u16::MAX
        } else {
            ticks.round() as u16
        };
        out.extend_from_slice(&clamped.to_le_bytes());
    }
    out
}

/// Read a Format payload of at least 32 bytes. The fingerprint is Some only when the payload is at least 44 bytes.
pub fn read_format_payload(payload: &[u8]) -> Option<(AudioFormat, Option<Fingerprint>)> {
    if payload.len() < FORMAT_PAYLOAD_SIZE {
        return None;
    }
    let i = |at: usize| i32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
    let (lane, lab_flags) = if payload.len() >= FORMAT_PAYLOAD_EXTENDED_SIZE {
        (if payload[32] <= 2 { payload[32] } else { 0 }, payload[33])
    } else {
        (0, 0)
    };
    let fingerprint = (payload.len() >= FORMAT_PAYLOAD_WITH_FINGERPRINT_SIZE)
        .then(|| payload[36..36 + FINGERPRINT_BYTES].try_into().unwrap());
    let capture_latency_ms = if payload.len() >= FORMAT_PAYLOAD_WITH_CAPTURE_SIZE {
        u16::from_le_bytes([payload[44], payload[45]]) as f64 / CAPTURE_LATENCY_TICKS_PER_MS
    } else {
        0.0
    };
    Some((
        AudioFormat {
            sample_rate: i(0),
            channels: i(4),
            bits_per_sample: i(8),
            encoding: i(12),
            block_align: i(16),
            avg_bytes_per_sec: i(20),
            codec: i(24),
            frame_samples_per_channel: i(28),
            lane,
            lab_flags,
            capture_latency_ms,
        },
        fingerprint,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HeartbeatKind {
    Ping = 0,
    Pong = 1,
}

/// The 9-byte heartbeat payload. We never set the flags byte (it advertises custom Opus).
pub fn write_heartbeat_payload(
    kind: HeartbeatKind,
    originator_ms: i64,
) -> [u8; HEARTBEAT_PAYLOAD_SIZE] {
    let mut out = [0u8; HEARTBEAT_PAYLOAD_SIZE];
    out[0] = kind as u8;
    out[1..].copy_from_slice(&originator_ms.to_le_bytes());
    out
}

/// Read a heartbeat of at least 9 bytes; any flags byte or later bytes are ignored.
pub fn read_heartbeat_payload(payload: &[u8]) -> Option<(HeartbeatKind, i64)> {
    if payload.len() < HEARTBEAT_PAYLOAD_SIZE {
        return None;
    }
    let kind = match payload[0] {
        0 => HeartbeatKind::Ping,
        1 => HeartbeatKind::Pong,
        _ => return None,
    };
    Some((kind, i64::from_le_bytes(payload[1..9].try_into().unwrap())))
}

pub mod pcm {
    pub const SUB_HEADER_SIZE: usize = 6;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SubHeader {
        pub frame_id: u32,
        pub part_index: u8,
        pub total_parts: u8,
    }

    pub fn write_sub_header(out: &mut Vec<u8>, header: SubHeader) {
        out.extend_from_slice(&header.frame_id.to_le_bytes());
        out.push(header.part_index);
        out.push(header.total_parts);
    }

    /// Read a sub-header; None if short, totalParts is 0, or partIndex is not below totalParts.
    pub fn read_sub_header(payload: &[u8]) -> Option<(SubHeader, &[u8])> {
        if payload.len() < SUB_HEADER_SIZE {
            return None;
        }
        let header = SubHeader {
            frame_id: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
            part_index: payload[4],
            total_parts: payload[5],
        };
        if header.total_parts == 0 || header.part_index >= header.total_parts {
            return None;
        }
        Some((header, &payload[SUB_HEADER_SIZE..]))
    }

    /// Floats to packed signed 24-bit little-endian, exactly as every port does it:
    /// clamp to plus or minus 1, scale by 2^23 - 1, truncate.
    pub fn float_to_int24le(samples: &[f32], out: &mut Vec<u8>) {
        out.reserve(samples.len() * 3);
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
            out.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }

    pub fn int24le_to_float(bytes: &[u8], out: &mut Vec<f32>) {
        out.reserve(bytes.len() / 3);
        for chunk in bytes.chunks_exact(3) {
            let packed = (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
            out.push(((packed << 8) >> 8) as f32 / 8_388_607.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let p = packet(PacketType::Format, 0xBEEF, 42, b"xyz");
        let (h, payload) = read_header(&p).unwrap();
        assert_eq!(
            h,
            Header {
                kind: 1,
                stream_id: 0xBEEF,
                sequence: 42
            }
        );
        assert_eq!(payload, b"xyz");
        assert!(read_header(&p[..11]).is_none());
        let mut bad = p.clone();
        bad[4] = 2;
        assert!(read_header(&bad).is_none());
        bad = p.clone();
        bad[0] = 0;
        assert!(read_header(&bad).is_none());
    }

    #[test]
    fn format_round_trip_all_lengths() {
        let mut f = AudioFormat::opus_48k_stereo(960);
        f.capture_latency_ms = 12.3;
        let print = [9u8; 8];
        let bytes = write_format_payload(&f, Some(&print));
        assert_eq!(bytes.len(), 46);
        let (back, got) = read_format_payload(&bytes).unwrap();
        assert_eq!(got, Some(print));
        assert_eq!(back.frame_samples_per_channel, 960);
        assert!((back.capture_latency_ms - 12.3).abs() < 0.01);
        for len in [32, 36, 44] {
            let (short, got) = read_format_payload(&bytes[..len]).unwrap();
            assert_eq!(short.sample_rate, 48000);
            assert_eq!(got.is_some(), len >= 44);
            assert_eq!(short.capture_latency_ms, 0.0);
        }
        assert!(read_format_payload(&bytes[..31]).is_none());
        assert_eq!(write_format_payload(&f, None).len(), 36);
    }

    #[test]
    fn usable_rules() {
        assert!(AudioFormat::opus_48k_stereo(960).check_usable().is_ok());
        assert!(AudioFormat::pcm_48k_stereo(233).check_usable().is_ok());
        let mut f = AudioFormat::opus_48k_stereo(960);
        f.channels = 0;
        assert!(f.check_usable().is_err());
        f = AudioFormat::opus_48k_stereo(2881);
        assert!(f.check_usable().is_err());
        f = AudioFormat::opus_48k_stereo(960);
        f.sample_rate = 44100;
        assert!(f.check_usable().is_err());
        f = AudioFormat::pcm_48k_stereo(233);
        f.sample_rate = 44100;
        assert!(f.check_usable().is_ok());
        f.codec = 3;
        assert!(f.check_usable().is_err());
        f.codec = 9;
        assert!(f.check_usable().is_err());
    }

    #[test]
    fn heartbeat_round_trip_and_flags_ignored() {
        let p = write_heartbeat_payload(HeartbeatKind::Ping, -5);
        assert_eq!(read_heartbeat_payload(&p), Some((HeartbeatKind::Ping, -5)));
        let mut ten = p.to_vec();
        ten.push(1);
        assert_eq!(
            read_heartbeat_payload(&ten),
            Some((HeartbeatKind::Ping, -5))
        );
        assert!(read_heartbeat_payload(&p[..8]).is_none());
        let mut bad = p;
        bad[0] = 2;
        assert!(read_heartbeat_payload(&bad).is_none());
    }

    #[test]
    fn pcm_sub_header_rules() {
        let mut v = Vec::new();
        pcm::write_sub_header(
            &mut v,
            pcm::SubHeader {
                frame_id: 7,
                part_index: 1,
                total_parts: 2,
            },
        );
        v.extend_from_slice(b"ab");
        let (h, rest) = pcm::read_sub_header(&v).unwrap();
        assert_eq!(
            (h.frame_id, h.part_index, h.total_parts, rest),
            (7, 1, 2, &b"ab"[..])
        );
        v[4] = 2;
        assert!(pcm::read_sub_header(&v).is_none());
        v[5] = 0;
        v[4] = 0;
        assert!(pcm::read_sub_header(&v).is_none());
    }
}
