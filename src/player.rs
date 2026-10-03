//! The outbound side: queued utterances, earcons and a raw stream, mixed into 48 kHz stereo frames.
//!
//! Utterances play in order. Each one's id is reported done exactly once: after its last frame has
//! been pulled for sending, when it is aborted, or when there is nobody to send it to.

use std::collections::VecDeque;

use crate::audio::{i16_to_f32, to_stereo, Cue, StreamResampler, WIRE_RATE};

struct Utterance {
    id: String,
    channels: usize,
    resampler: StreamResampler,
    samples: VecDeque<f32>,
    ended: bool,
    /// No peer to hear it: its audio is thrown away and it is reported done when it ends.
    discard: bool,
}

pub struct Player {
    utterances: VecDeque<Utterance>,
    cues: Vec<(Vec<f32>, usize)>,
    stream: VecDeque<f32>,
    stream_cap: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlayerError {
    BadFormat(String),
    NoOpenUtterance,
    UnknownId(String),
    DuplicateId(String),
}

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadFormat(why) => write!(f, "TTS format not usable: {why}"),
            Self::NoOpenUtterance => write!(f, "TTS audio arrived with no utterance open; send TTS_BEGIN first"),
            Self::UnknownId(id) => write!(f, "no queued utterance has id {id:?}"),
            Self::DuplicateId(id) => write!(f, "utterance id {id:?} is already queued"),
        }
    }
}

impl Player {
    /// `stream_cap_secs` bounds the raw stream (loopback), dropping the oldest audio past it.
    pub fn new(stream_cap_secs: f32) -> Self {
        Self {
            utterances: VecDeque::new(),
            cues: Vec::new(),
            stream: VecDeque::new(),
            stream_cap: (stream_cap_secs * WIRE_RATE as f32) as usize * 2,
        }
    }

    pub fn begin(&mut self, id: &str, sample_rate: u32, channels: usize) -> Result<(), PlayerError> {
        if !(1..=2).contains(&channels) {
            return Err(PlayerError::BadFormat(format!("{channels} channels (must be 1 or 2)")));
        }
        if self.utterances.iter().any(|u| u.id == id) {
            return Err(PlayerError::DuplicateId(id.to_string()));
        }
        let resampler = StreamResampler::new(sample_rate, WIRE_RATE, channels).map_err(|e| PlayerError::BadFormat(e.to_string()))?;
        if let Some(open) = self.utterances.iter_mut().rev().find(|u| !u.ended) {
            // A new utterance implicitly ends the previous one; its audio is kept.
            let tail = open.resampler.flush();
            open.samples.extend(to_stereo(&tail, open.channels));
            open.ended = true;
        }
        self.utterances.push_back(Utterance {
            id: id.to_string(),
            channels,
            resampler,
            samples: VecDeque::new(),
            ended: false,
            discard: false,
        });
        Ok(())
    }

    /// Append s16 samples, at the declared format, to the open utterance.
    pub fn pcm(&mut self, samples: &[i16]) -> Result<(), PlayerError> {
        let open = self.utterances.iter_mut().rev().find(|u| !u.ended).ok_or(PlayerError::NoOpenUtterance)?;
        if open.discard {
            return Ok(());
        }
        let usable = samples.len() / open.channels * open.channels;
        let resampled = open.resampler.process(&i16_to_f32(&samples[..usable]));
        open.samples.extend(to_stereo(&resampled, open.channels));
        Ok(())
    }

    pub fn end(&mut self, id: &str) -> Result<(), PlayerError> {
        let u = self.utterances.iter_mut().find(|u| u.id == id).ok_or_else(|| PlayerError::UnknownId(id.to_string()))?;
        if !u.ended {
            let tail = u.resampler.flush();
            if !u.discard {
                u.samples.extend(to_stereo(&tail, u.channels));
            }
            u.ended = true;
        }
        Ok(())
    }

    /// Drop an utterance's queued audio. Returns its id to report done, or None if it is not queued.
    pub fn abort(&mut self, id: &str) -> Option<String> {
        let at = self.utterances.iter().position(|u| u.id == id)?;
        self.utterances.remove(at).map(|u| u.id)
    }

    /// Drop everything queued, returning every id to report done.
    pub fn abort_all(&mut self) -> Vec<String> {
        self.cues.clear();
        self.stream.clear();
        self.utterances.drain(..).map(|u| u.id).collect()
    }

    pub fn cue(&mut self, cue: Cue) {
        self.cues.push((cue.render(), 0));
    }

    /// Append interleaved 48 kHz stereo to the raw stream.
    pub fn push_stream(&mut self, stereo: &[f32]) {
        self.stream.extend(stereo);
        let excess = self.stream.len().saturating_sub(self.stream_cap);
        self.stream.drain(..excess - excess % 2);
    }

    pub fn stream_len_frames(&self) -> usize {
        self.stream.len() / 2
    }

    /// Anything to send? Burst mode sends only while this is true.
    pub fn has_content(&self) -> bool {
        !self.utterances.is_empty() || !self.cues.is_empty() || !self.stream.is_empty()
    }

    pub fn queued(&self) -> usize {
        self.utterances.len()
    }

    /// Mix the next `frames` stereo frames. Returns the audio and the ids finished by this frame.
    /// With nobody connected, everything queued is let go and reported done instead.
    pub fn pull(&mut self, frames: usize, connected: bool) -> (Vec<f32>, Vec<String>) {
        let mut out = vec![0f32; frames * 2];
        let mut done = Vec::new();
        if !connected {
            self.cues.clear();
            self.stream.clear();
            self.utterances.retain_mut(|u| {
                if u.ended {
                    done.push(u.id.clone());
                    return false;
                }
                u.discard = true;
                u.samples.clear();
                true
            });
            return (out, done);
        }

        let mut pos = 0;
        while let Some(front) = self.utterances.front_mut() {
            while pos < out.len() {
                match front.samples.pop_front() {
                    Some(s) => {
                        out[pos] = s;
                        pos += 1;
                    }
                    None => break,
                }
            }
            if front.samples.is_empty() && front.ended {
                done.push(self.utterances.pop_front().unwrap().id);
                continue;
            }
            // Either the frame is full, or the open utterance is waiting for more audio: pad with silence.
            break;
        }

        for (cue, at) in &mut self.cues {
            let take = (cue.len() - *at).min(out.len());
            for (o, s) in out.iter_mut().zip(&cue[*at..*at + take]) {
                *o += s;
            }
            *at += take;
        }
        self.cues.retain(|(cue, at)| *at < cue.len());

        for o in out.iter_mut() {
            match self.stream.pop_front() {
                Some(s) => *o += s,
                None => break,
            }
        }
        for o in out.iter_mut() {
            *o = o.clamp(-1.0, 1.0);
        }
        (out, done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: usize = 960;

    #[test]
    fn utterances_play_in_order_and_finish_once() {
        let mut p = Player::new(2.0);
        p.begin("a", 48_000, 1).unwrap();
        p.pcm(&vec![1000i16; 1500]).unwrap();
        p.end("a").unwrap();
        p.begin("b", 48_000, 2).unwrap();
        p.pcm(&vec![-1000i16; 400]).unwrap();
        p.end("b").unwrap();

        let (audio, done) = p.pull(F, true);
        assert!(done.is_empty());
        assert!(audio.iter().all(|&s| s > 0.0));
        let (audio, done) = p.pull(F, true);
        assert_eq!(done, ["a", "b"], "a's tail and all of b fit in the second frame");
        assert!(audio[..540 * 2].iter().all(|&s| s > 0.0));
        assert!(audio[540 * 2..540 * 2 + 400].iter().all(|&s| s < 0.0));
        assert!(audio[540 * 2 + 400..].iter().all(|&s| s == 0.0));
        let (_, done) = p.pull(F, true);
        assert!(done.is_empty() && !p.has_content());
    }

    #[test]
    fn an_open_utterance_waits_and_pads_with_silence() {
        let mut p = Player::new(2.0);
        p.begin("a", 48_000, 2).unwrap();
        p.pcm(&vec![1000i16; 100]).unwrap();
        let (audio, done) = p.pull(F, true);
        assert!(done.is_empty());
        assert!(audio[100..].iter().all(|&s| s == 0.0));
        p.end("a").unwrap();
        assert_eq!(p.pull(F, true).1, ["a"]);
    }

    #[test]
    fn resamples_to_the_wire_rate() {
        let mut p = Player::new(2.0);
        p.begin("a", 24_000, 1).unwrap();
        p.pcm(&vec![1000i16; 24_000]).unwrap();
        p.end("a").unwrap();
        let mut frames = 0;
        loop {
            let (_, done) = p.pull(F, true);
            frames += F;
            if !done.is_empty() {
                break;
            }
        }
        assert_eq!(frames, 50 * F, "one second at 24 kHz is fifty 20 ms frames at 48 kHz");
    }

    #[test]
    fn abort_reports_once_and_unknown_ids_are_refused() {
        let mut p = Player::new(2.0);
        p.begin("a", 16_000, 1).unwrap();
        assert_eq!(p.abort("a"), Some("a".into()));
        assert_eq!(p.abort("a"), None);
        assert_eq!(p.end("a"), Err(PlayerError::UnknownId("a".into())));
        assert_eq!(p.pcm(&[1, 2]), Err(PlayerError::NoOpenUtterance));
        assert!(matches!(p.begin("x", 16_000, 3), Err(PlayerError::BadFormat(_))));
    }

    #[test]
    fn nobody_connected_lets_everything_go() {
        let mut p = Player::new(2.0);
        p.begin("a", 48_000, 1).unwrap();
        p.pcm(&[5; 10_000]).unwrap();
        p.end("a").unwrap();
        p.begin("b", 48_000, 1).unwrap();
        p.pcm(&[5; 10_000]).unwrap();
        assert_eq!(p.pull(F, false).1, ["a"]);
        p.pcm(&[5; 10_000]).unwrap();
        p.end("b").unwrap();
        let (audio, done) = p.pull(F, true);
        assert_eq!(done, ["b"]);
        assert!(audio.iter().all(|&s| s == 0.0), "b's audio was discarded");
    }

    #[test]
    fn cues_and_stream_mix_over_speech() {
        let mut p = Player::new(0.01);
        p.cue(Cue::Done);
        p.push_stream(&vec![0.1; 4000]);
        assert_eq!(p.stream_len_frames(), 480, "the stream is capped at 10 ms");
        let (audio, _) = p.pull(F, true);
        assert!(audio[..960].iter().any(|&s| (s - 0.1).abs() > 0.01));
        assert!(p.has_content());
    }
}
