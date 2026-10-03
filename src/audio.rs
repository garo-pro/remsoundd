//! Sample-format helpers, a streaming resampler, and the earcons.
//!
//! Inside the daemon, audio is interleaved `f32` at 48 kHz stereo: what RemSound puts on the
//! wire. The bridge converts at its edge: mic audio to 16 kHz mono s16le, TTS from any rate.

use rubato::{FftFixedIn, Resampler};

pub const WIRE_RATE: u32 = 48_000;
pub const WIRE_CHANNELS: usize = 2;

pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| s as f32 / 32768.0).collect()
}

pub fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Interleaved stereo to mono, averaging left and right.
pub fn stereo_to_mono(stereo: &[f32]) -> Vec<f32> {
    stereo
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[l, r]| (l + r) * 0.5)
        .collect()
}

/// Any channel count to interleaved stereo: mono is duplicated, more than two keeps the first two.
pub fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        2 => samples.to_vec(),
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        n => samples.chunks_exact(n).flat_map(|f| [f[0], f[1]]).collect(),
    }
}

/// Resamples an interleaved stream fed in arbitrary-sized pieces. Its delay is compensated, so
/// output sample N lines up with input sample N, and [`flush`](Self::flush) returns exactly the
/// tail that the input length implies.
pub struct StreamResampler {
    channels: usize,
    inner: Option<FftFixedIn<f32>>,
    pending: Vec<Vec<f32>>,
    skip: usize,
    in_frames: u64,
    out_frames: u64,
    in_rate: u32,
    out_rate: u32,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(channels >= 1, "a stream needs at least one channel");
        anyhow::ensure!(
            (1000..=384_000).contains(&in_rate),
            "sample rate {in_rate} is out of range"
        );
        let inner = if in_rate == out_rate {
            None
        } else {
            // About 10 ms chunks: small enough to keep latency low, big enough to be efficient.
            let chunk = (in_rate as usize / 100).max(64);
            Some(FftFixedIn::<f32>::new(
                in_rate as usize,
                out_rate as usize,
                chunk,
                1,
                channels,
            )?)
        };
        let skip = inner.as_ref().map(|r| r.output_delay()).unwrap_or(0);
        Ok(Self {
            channels,
            inner,
            pending: vec![Vec::new(); channels],
            skip,
            in_frames: 0,
            out_frames: 0,
            in_rate,
            out_rate,
        })
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    fn expected_out(&self) -> u64 {
        (self.in_frames * self.out_rate as u64 + self.in_rate as u64 / 2) / self.in_rate as u64
    }

    fn emit(&mut self, planar: Vec<Vec<f32>>, out: &mut Vec<f32>, limit: Option<u64>) {
        let frames = planar.first().map(Vec::len).unwrap_or(0);
        for i in 0..frames {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            if limit.is_some_and(|l| self.out_frames >= l) {
                return;
            }
            for ch in &planar {
                out.push(ch[i]);
            }
            self.out_frames += 1;
        }
    }

    /// Feed interleaved samples; returns whatever output is ready, interleaved.
    pub fn process(&mut self, interleaved: &[f32]) -> Vec<f32> {
        self.in_frames += (interleaved.len() / self.channels) as u64;
        let Some(mut inner) = self.inner.take() else {
            self.out_frames = self.in_frames;
            return interleaved[..interleaved.len() / self.channels * self.channels].to_vec();
        };
        for frame in interleaved.chunks_exact(self.channels) {
            for (ch, &s) in frame.iter().enumerate() {
                self.pending[ch].push(s);
            }
        }
        let mut out = Vec::new();
        loop {
            let need = inner.input_frames_next();
            if self.pending[0].len() < need {
                break;
            }
            let chunk: Vec<Vec<f32>> = self
                .pending
                .iter_mut()
                .map(|p| p.drain(..need).collect())
                .collect();
            match inner.process(&chunk, None) {
                Ok(planar) => self.emit(planar, &mut out, None),
                Err(_) => break,
            }
        }
        self.inner = Some(inner);
        out
    }

    /// Push out the delayed tail, padding with silence, so the total output matches the input length.
    pub fn flush(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        let Some(mut inner) = self.inner.take() else {
            return out;
        };
        let expected = self.expected_out();
        let mut rounds = 0;
        while self.out_frames < expected && rounds < 64 {
            rounds += 1;
            let partial: Vec<Vec<f32>> =
                std::mem::replace(&mut self.pending, vec![Vec::new(); self.channels]);
            let planar = if partial[0].is_empty() {
                inner.process_partial::<Vec<f32>>(None, None)
            } else {
                inner.process_partial(Some(&partial), None)
            };
            match planar {
                Ok(p) => self.emit(p, &mut out, Some(expected)),
                Err(_) => break,
            }
        }
        self.inner = Some(inner);
        out
    }
}

/// The three earcons the bridge can ask for, as 48 kHz stereo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// A rising two-tone: the assistant is listening.
    Listening,
    /// A falling two-tone: the assistant has finished.
    Done,
    /// Two low beeps: something went wrong.
    Error,
}

impl Cue {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "listening" => Some(Self::Listening),
            "done" => Some(Self::Done),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn render(self) -> Vec<f32> {
        let notes: &[(f32, f32)] = match self {
            Cue::Listening => &[(660.0, 0.09), (880.0, 0.12)],
            Cue::Done => &[(880.0, 0.09), (660.0, 0.12)],
            Cue::Error => &[(330.0, 0.1), (0.0, 0.06), (330.0, 0.1)],
        };
        let mut out = Vec::new();
        for &(freq, secs) in notes {
            let n = (secs * WIRE_RATE as f32) as usize;
            let fade = (0.008 * WIRE_RATE as f32) as usize;
            for i in 0..n {
                let env = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
                let s = if freq > 0.0 {
                    0.3 * env * (std::f32::consts::TAU * freq * i as f32 / WIRE_RATE as f32).sin()
                } else {
                    0.0
                };
                out.push(s);
                out.push(s);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Dominant frequency by zero crossings: rough but enough to show the pitch survived.
    pub fn zero_crossing_hz(mono: &[f32], rate: u32) -> f32 {
        let crossings = mono
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f32 / 2.0 / (mono.len() as f32 / rate as f32)
    }

    #[test]
    fn resampler_keeps_length_and_pitch_across_odd_pieces() {
        for (from, to) in [
            (24_000, 48_000),
            (22_050, 48_000),
            (48_000, 16_000),
            (16_000, 48_000),
        ] {
            let input = sine(from, 440.0, 1.0);
            let mut r = StreamResampler::new(from, to, 1).unwrap();
            let mut out = Vec::new();
            for piece in input.chunks(337) {
                out.extend(r.process(piece));
            }
            out.extend(r.flush());
            assert_eq!(
                out.len(),
                (input.len() as u64 * to as u64 / from as u64) as usize,
                "{from}->{to}"
            );
            let hz = zero_crossing_hz(&out[out.len() / 10..out.len() * 9 / 10], to);
            assert!((hz - 440.0).abs() < 5.0, "{from}->{to}: {hz} Hz");
        }
    }

    #[test]
    fn passthrough_when_rates_match() {
        let mut r = StreamResampler::new(48_000, 48_000, 2).unwrap();
        assert_eq!(r.process(&[0.1, 0.2, 0.3, 0.4]), vec![0.1, 0.2, 0.3, 0.4]);
        assert!(r.flush().is_empty());
    }

    #[test]
    fn channel_conversions() {
        assert_eq!(stereo_to_mono(&[1.0, 0.0, -1.0, -1.0]), vec![0.5, -1.0]);
        assert_eq!(to_stereo(&[0.5, 0.25], 1), vec![0.5, 0.5, 0.25, 0.25]);
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32767);
    }

    #[test]
    fn cues_render_and_rise_or_fall() {
        for cue in [Cue::Listening, Cue::Done, Cue::Error] {
            let s = cue.render();
            assert!(!s.is_empty() && s.len() % 2 == 0);
            assert!(s.iter().all(|v| v.abs() <= 0.31));
        }
        let mono = stereo_to_mono(&Cue::Listening.render());
        let half = (0.09 * 48_000.0) as usize;
        assert!(zero_crossing_hz(&mono[..half], 48_000) < zero_crossing_hz(&mono[half..], 48_000));
        assert_eq!(Cue::from_name("done"), Some(Cue::Done));
        assert_eq!(Cue::from_name("nope"), None);
    }
}
