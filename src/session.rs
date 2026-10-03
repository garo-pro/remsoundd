//! One received stream: keyed by (source endpoint, streamId), decoded to 48 kHz stereo.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::audio::{to_stereo, StreamResampler, WIRE_CHANNELS, WIRE_RATE};
use crate::crypto::Cipher;
use crate::jitter::{Released, ReorderBuffer};
use crate::protocol::{pcm, AudioFormat, Codec};

/// Losses longer than this are not concealed: the stream stopped, it did not stutter.
const MAX_CONCEAL: Duration = Duration::from_millis(200);
/// Largest Opus packet duration (120 ms) at 48 kHz, per channel.
const MAX_OPUS_FRAME: usize = 5760;
/// Packets the reorder buffer may hold before it gives up waiting.
const MAX_HELD: usize = 512;
/// PCM frames being assembled at once, at most.
const MAX_PARTIAL_PCM: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct SessionStats {
    pub packets: u64,
    pub decrypt_failures: u64,
    pub decode_failures: u64,
    pub fec_recovered: u64,
    pub concealed_frames: u64,
    pub unconcealed_gaps: u64,
}

pub struct ReceiveSession {
    pub format: AudioFormat,
    pub last_audio: Instant,
    pub stats: SessionStats,
    kind: Kind,
}

// One session per incoming stream: the size difference between the variants does not matter.
#[allow(clippy::large_enum_variant)]
enum Kind {
    Opus {
        decoder: opus::Decoder,
        reorder: ReorderBuffer<Vec<u8>>,
        /// Samples per channel of the last decoded packet, at 48 kHz: the size to conceal a loss with.
        last_frame: usize,
    },
    Pcm {
        partial: HashMap<u32, PartialFrame>,
        reorder: ReorderBuffer<Vec<f32>>,
        channels: usize,
        resampler: Option<StreamResampler>,
        last_frame: usize,
        window: Duration,
    },
}

struct PartialFrame {
    parts: Vec<Option<Vec<u8>>>,
    first_seen: Instant,
}

impl ReceiveSession {
    /// Create a session for a validated format.
    pub fn new(format: AudioFormat, jitter: Duration, now: Instant) -> anyhow::Result<Self> {
        format.check_usable().map_err(anyhow::Error::msg)?;
        let kind = match format.codec() {
            Some(Codec::Opus) => Kind::Opus {
                // Opus decodes to any supported rate whatever the encoder used, so decode straight to the wire rate.
                decoder: opus::Decoder::new(WIRE_RATE, opus::Channels::Stereo)?,
                reorder: ReorderBuffer::new(jitter, MAX_HELD),
                last_frame: (format.frame_samples_per_channel as usize * WIRE_RATE as usize
                    / format.sample_rate as usize)
                    .max(120),
            },
            Some(Codec::Pcm) => Kind::Pcm {
                partial: HashMap::new(),
                reorder: ReorderBuffer::new(jitter, MAX_HELD),
                channels: format.channels as usize,
                resampler: (format.sample_rate as u32 != WIRE_RATE)
                    .then(|| {
                        StreamResampler::new(format.sample_rate as u32, WIRE_RATE, WIRE_CHANNELS)
                    })
                    .transpose()?,
                last_frame: format.frame_samples_per_channel as usize,
                window: jitter,
            },
            _ => anyhow::bail!("unsupported codec {}", format.codec),
        };
        Ok(Self {
            format,
            last_audio: now,
            stats: SessionStats::default(),
            kind,
        })
    }

    /// Take one Audio packet's payload. Opus is decrypted here; PCM after its parts are assembled.
    pub fn push_audio(&mut self, sequence: u32, payload: &[u8], cipher: &Cipher, now: Instant) {
        self.stats.packets += 1;
        match &mut self.kind {
            Kind::Opus { reorder, .. } => match cipher.open(payload) {
                Some(plain) => {
                    self.last_audio = now;
                    reorder.push(sequence, plain, now);
                }
                None => self.stats.decrypt_failures += 1,
            },
            Kind::Pcm {
                partial,
                reorder,
                channels,
                ..
            } => {
                let Some((header, slice)) = pcm::read_sub_header(payload) else {
                    self.stats.decode_failures += 1;
                    return;
                };
                let total = header.total_parts as usize;
                let entry = partial
                    .entry(header.frame_id)
                    .or_insert_with(|| PartialFrame {
                        parts: vec![None; total],
                        first_seen: now,
                    });
                if entry.parts.len() != total {
                    self.stats.decode_failures += 1;
                    partial.remove(&header.frame_id);
                    return;
                }
                entry.parts[header.part_index as usize] = Some(slice.to_vec());
                if entry.parts.iter().all(Option::is_some) {
                    let frame = partial.remove(&header.frame_id).unwrap();
                    let sealed: Vec<u8> = frame.parts.into_iter().flatten().flatten().collect();
                    let Some(plain) = cipher.open(&sealed) else {
                        self.stats.decrypt_failures += 1;
                        return;
                    };
                    let mut floats = Vec::with_capacity(plain.len() / 3);
                    pcm::int24le_to_float(&plain, &mut floats);
                    let usable = floats.len() / *channels * *channels;
                    floats.truncate(usable);
                    self.last_audio = now;
                    reorder.push(header.frame_id, floats, now);
                }
            }
        }
    }

    /// Release decoded audio that is ready: 48 kHz interleaved stereo.
    pub fn poll(&mut self, now: Instant) -> Vec<f32> {
        let mut out = Vec::new();
        let stats = &mut self.stats;
        match &mut self.kind {
            Kind::Opus {
                decoder,
                reorder,
                last_frame,
            } => {
                let released = reorder.pop(now);
                let mut buf = vec![0f32; MAX_OPUS_FRAME * 2];
                let mut iter = released.into_iter().peekable();
                while let Some(r) = iter.next() {
                    match r {
                        Released::Item(_, packet) => {
                            match decoder.decode_float(&packet, &mut buf, false) {
                                Ok(n) => {
                                    *last_frame = n;
                                    out.extend_from_slice(&buf[..n * 2]);
                                }
                                Err(_) => stats.decode_failures += 1,
                            }
                        }
                        Released::Lost { count, .. } => {
                            let frame = (*last_frame).clamp(120, MAX_OPUS_FRAME);
                            let concealable =
                                (MAX_CONCEAL.as_millis() as usize * 48 / frame).max(1);
                            if count as usize > concealable {
                                stats.unconcealed_gaps += 1;
                                continue;
                            }
                            // Every lost frame but the last is concealed by PLC; the last, when the next
                            // packet is here, comes from that packet's in-band FEC.
                            let fec_from = match (count, iter.peek()) {
                                (_, Some(Released::Item(_, next))) => Some(next.clone()),
                                _ => None,
                            };
                            for i in 0..count {
                                let last = i + 1 == count;
                                let result = match (&fec_from, last) {
                                    (Some(next), true) => decoder
                                        .decode_float(next, &mut buf[..frame * 2], true)
                                        .inspect(|_| stats.fec_recovered += 1),
                                    _ => decoder.decode_float(&[], &mut buf[..frame * 2], false),
                                };
                                match result {
                                    Ok(n) => {
                                        stats.concealed_frames += 1;
                                        out.extend_from_slice(&buf[..n * 2]);
                                    }
                                    Err(_) => stats.decode_failures += 1,
                                }
                            }
                        }
                    }
                }
            }
            Kind::Pcm {
                partial,
                reorder,
                channels,
                resampler,
                last_frame,
                window,
            } => {
                // A frame whose parts never all arrived is dropped; the reorder buffer then counts it as lost.
                let stale = *window * 2;
                partial.retain(|_, f| now.duration_since(f.first_seen) <= stale);
                while partial.len() > MAX_PARTIAL_PCM {
                    let oldest = *partial.iter().min_by_key(|(_, f)| f.first_seen).unwrap().0;
                    partial.remove(&oldest);
                }
                let mut native = Vec::new();
                for r in reorder.pop(now) {
                    match r {
                        Released::Item(_, samples) => {
                            *last_frame = (samples.len() / *channels).max(1);
                            native.extend(to_stereo(&samples, *channels));
                        }
                        Released::Lost { count, .. } => {
                            let frames = count as usize * *last_frame;
                            let rate = self.format.sample_rate as usize;
                            if frames * 1000 / rate > MAX_CONCEAL.as_millis() as usize {
                                stats.unconcealed_gaps += 1;
                                continue;
                            }
                            stats.concealed_frames += count as u64;
                            native.extend(std::iter::repeat_n(0.0, frames * 2));
                        }
                    }
                }
                out = match resampler {
                    Some(r) => r.process(&native),
                    None => native,
                };
            }
        }
        out
    }

    pub fn idle_for(&self, now: Instant) -> Duration {
        now.duration_since(self.last_audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::NonceSequence;
    use crate::protocol::MAX_AUDIO_PAYLOAD_BYTES;

    fn cipher() -> Cipher {
        Cipher::new(&[3u8; 32])
    }

    fn tone(frames: usize, offset: usize) -> Vec<f32> {
        (offset..offset + frames)
            .flat_map(|i| {
                let s = (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    fn pcm_parts(
        frame_id: u32,
        samples: &[f32],
        c: &Cipher,
        nonces: &mut NonceSequence,
    ) -> Vec<Vec<u8>> {
        let mut plain = Vec::new();
        pcm::float_to_int24le(samples, &mut plain);
        let sealed = c.seal_next(nonces, &plain);
        let total = sealed.len().div_ceil(MAX_AUDIO_PAYLOAD_BYTES) as u8;
        sealed
            .chunks(MAX_AUDIO_PAYLOAD_BYTES)
            .enumerate()
            .map(|(i, chunk)| {
                let mut p = Vec::new();
                pcm::write_sub_header(
                    &mut p,
                    pcm::SubHeader {
                        frame_id,
                        part_index: i as u8,
                        total_parts: total,
                    },
                );
                p.extend_from_slice(chunk);
                p
            })
            .collect()
    }

    #[test]
    fn pcm_parts_reassemble_in_any_order() {
        let c = cipher();
        let mut nonces = NonceSequence::new();
        let t = Instant::now();
        let mut s = ReceiveSession::new(
            AudioFormat::pcm_48k_stereo(240),
            Duration::from_millis(60),
            t,
        )
        .unwrap();
        let frame = tone(240, 0);
        let mut parts = pcm_parts(1, &frame, &c, &mut nonces);
        assert_eq!(parts.len(), 2, "a 240-sample frame needs two parts");
        parts.reverse();
        for (i, p) in parts.iter().enumerate() {
            s.push_audio(i as u32, p, &c, t);
        }
        let out = s.poll(t);
        assert_eq!(out.len(), frame.len());
        assert!(out.iter().zip(&frame).all(|(a, b)| (a - b).abs() < 1e-5));
    }

    #[test]
    fn pcm_missing_part_drops_the_frame_and_it_is_concealed() {
        let c = cipher();
        let mut nonces = NonceSequence::new();
        let t = Instant::now();
        let mut s = ReceiveSession::new(
            AudioFormat::pcm_48k_stereo(240),
            Duration::from_millis(60),
            t,
        )
        .unwrap();
        for id in 1..=3u32 {
            let parts = pcm_parts(id, &tone(240, 0), &c, &mut nonces);
            for (i, p) in parts.iter().enumerate() {
                if id == 2 && i == 1 {
                    continue; // frame 2 loses its second part
                }
                s.push_audio(id * 2 + i as u32, p, &c, t);
            }
        }
        assert_eq!(
            s.poll(t).len(),
            240 * 2,
            "frame 1 plays, frame 3 waits for frame 2"
        );
        let later = t + Duration::from_millis(200);
        assert_eq!(
            s.poll(later).len(),
            2 * 240 * 2,
            "frame 2 is concealed as silence, then frame 3 plays"
        );
        assert_eq!(s.stats.concealed_frames, 1);
    }

    #[test]
    fn pcm_wrong_password_is_counted_not_played() {
        let mut nonces = NonceSequence::new();
        let t = Instant::now();
        let mut s = ReceiveSession::new(
            AudioFormat::pcm_48k_stereo(233),
            Duration::from_millis(60),
            t,
        )
        .unwrap();
        for p in pcm_parts(1, &tone(233, 0), &Cipher::new(&[9u8; 32]), &mut nonces) {
            s.push_audio(1, &p, &cipher(), t);
        }
        assert!(s.poll(t).is_empty());
        assert_eq!(s.stats.decrypt_failures, 1);
    }

    #[test]
    fn opus_decodes_and_conceals_a_lost_packet_with_fec() {
        let c = cipher();
        let mut nonces = NonceSequence::new();
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(20).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(32_000)).unwrap();
        let t = Instant::now();
        let mut s = ReceiveSession::new(
            AudioFormat::opus_48k_stereo(960),
            Duration::from_millis(60),
            t,
        )
        .unwrap();
        let mut packet = vec![0u8; 4000];
        let mut produced = 0;
        for seq in 0..20u32 {
            let n = enc
                .encode_float(&tone(960, seq as usize * 960), &mut packet)
                .unwrap();
            if seq == 10 {
                continue; // lost on the wire
            }
            s.push_audio(seq, &c.seal_next(&mut nonces, &packet[..n]), &c, t);
            produced += s.poll(t).len();
        }
        produced += s.poll(t + Duration::from_millis(100)).len();
        assert_eq!(
            produced,
            20 * 960 * 2,
            "every frame, lost one included, must come out"
        );
        assert_eq!(s.stats.fec_recovered, 1);
    }
}
