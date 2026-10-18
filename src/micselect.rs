//! Whose microphone goes to the bridge when several peers send at once (`mic_mode = "active"`).
//!
//! Every sending peer is listened to; the one that is talking is passed on, and keeps the
//! microphone until it has been quiet for the hold time, so a sentence is never cut in half.
//!
//! Selecting rather than mixing, on purpose:
//! - Each peer's stream arrives on its own clock, in packets of 2.5 to 20 ms with its own jitter.
//!   Summing them needs a buffer per peer and a common clock, which adds latency, and drifts
//!   apart over hours. A selected stream is passed through untouched, as in single mode.
//! - A mix adds every open microphone's room noise, and when the same voice reaches two
//!   microphones it arrives twice, offset by the network: wake-word detection and STT do worse
//!   on both. One clean microphone is what they are tuned for.
//! - A peer sending only silence or a steady hum cannot win: it needs voice above both the
//!   threshold and its own noise floor.
//!
//! This is a pure state machine fed with decoded audio and the time, so it is tested without a
//! network.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const DEFAULT_THRESHOLD_DB: f32 = -50.0;
pub const DEFAULT_HOLD: Duration = Duration::from_millis(1500);
/// A peer counts as sending while it delivered audio this recently, as in single mode.
const SENDING_WINDOW: Duration = Duration::from_secs(1);
/// Voice must stand this far above the peer's own noise floor.
const FLOOR_MARGIN_DB: f32 = 10.0;
/// How fast a peer's noise floor creeps up toward a steady level. It drops at once.
const FLOOR_RISE_DB_PER_SEC: f32 = 2.0;
/// With nobody holding the microphone, a peer that had voice this recently may take it.
const VOICE_RECENT: Duration = Duration::from_millis(300);
/// State for a peer that has sent nothing for this long is dropped.
const FORGET_AFTER: Duration = Duration::from_secs(30);
const SILENCE_DB: f32 = -120.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectParams {
    /// Below this level, in dBFS, a chunk is never voice.
    pub threshold_db: f32,
    /// How long the talking peer keeps the microphone after its last voice.
    pub hold: Duration,
}

impl Default for SelectParams {
    fn default() -> Self {
        Self {
            threshold_db: DEFAULT_THRESHOLD_DB,
            hold: DEFAULT_HOLD,
        }
    }
}

/// RMS level of interleaved samples in dBFS; digital silence is -120.
pub fn level_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return SILENCE_DB;
    }
    let mean_square = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    if mean_square <= 0.0 {
        return SILENCE_DB;
    }
    (10.0 * mean_square.log10()).max(SILENCE_DB)
}

#[derive(Debug)]
struct PeerLevel {
    last_audio: Instant,
    floor_db: f32,
    floor_at: Instant,
    last_voice: Option<Instant>,
    /// Level of the most recent voiced chunk: who is loudest when two start at once.
    voice_db: f32,
}

#[derive(Debug)]
pub struct MicSelector {
    params: SelectParams,
    peers: HashMap<IpAddr, PeerLevel>,
    current: Option<IpAddr>,
}

impl MicSelector {
    pub fn new(params: SelectParams) -> Self {
        Self {
            params,
            peers: HashMap::new(),
            current: None,
        }
    }

    /// The peer chosen by the last [`select`](Self::select).
    pub fn current(&self) -> Option<IpAddr> {
        self.current
    }

    /// Take a chunk of decoded audio from a peer.
    pub fn observe(&mut self, from: IpAddr, samples: &[f32], now: Instant) {
        if samples.is_empty() {
            return;
        }
        let db = level_db(samples);
        let threshold = self.params.threshold_db;
        let p = self.peers.entry(from).or_insert(PeerLevel {
            last_audio: now,
            // Low enough that a new peer's first words are measured against the threshold alone.
            floor_db: threshold - FLOOR_MARGIN_DB,
            floor_at: now,
            last_voice: None,
            voice_db: SILENCE_DB,
        });
        p.last_audio = now;
        if db >= threshold.max(p.floor_db + FLOOR_MARGIN_DB) {
            p.last_voice = Some(now);
            p.voice_db = db;
        }
        // Pauses between words pull the floor down to the room's noise at once; only a level
        // that never pauses, a fan or a hum, lifts it, slowly.
        let dt = now.saturating_duration_since(p.floor_at).as_secs_f32();
        p.floor_at = now;
        p.floor_db = if db < p.floor_db {
            db
        } else {
            (p.floor_db + FLOOR_RISE_DB_PER_SEC * dt).min(db)
        };
    }

    /// Choose the peer whose audio goes to the bridge now. `order` is the configured peers in
    /// config order; peers outside it are never chosen.
    pub fn select(&mut self, order: &[IpAddr], now: Instant) -> Option<IpAddr> {
        self.peers
            .retain(|_, p| now.saturating_duration_since(p.last_audio) <= FORGET_AFTER);
        let peers = &self.peers;
        let sending = |ip: &IpAddr| {
            peers
                .get(ip)
                .is_some_and(|p| now.saturating_duration_since(p.last_audio) < SENDING_WINDOW)
        };
        let voiced_within = |ip: &IpAddr, window: Duration| {
            peers
                .get(ip)
                .and_then(|p| p.last_voice)
                .is_some_and(|t| now.saturating_duration_since(t) <= window)
        };

        // The talking peer keeps the microphone until it has been quiet for the hold time.
        if let Some(cur) = self.current {
            if order.contains(&cur) && sending(&cur) && voiced_within(&cur, self.params.hold) {
                return Some(cur);
            }
        }
        // Otherwise whoever is talking now, the loudest; a tie goes to the earlier in config order.
        let voice_db = |ip: &IpAddr| peers.get(ip).map_or(SILENCE_DB, |p| p.voice_db);
        let talker = order
            .iter()
            .filter(|ip| sending(ip) && voiced_within(ip, VOICE_RECENT))
            .copied()
            .reduce(|best, ip| {
                if voice_db(&ip) > voice_db(&best) {
                    ip
                } else {
                    best
                }
            });
        // Nobody talking: stay where we are while that peer still sends, else fall back to single
        // mode's choice, the first sending peer in config order, so the stream keeps flowing.
        let stay = self
            .current
            .filter(|cur| order.contains(cur) && sending(cur));
        let choice = talker
            .or(stay)
            .or_else(|| order.iter().find(|ip| sending(ip)).copied());
        self.current = choice;
        choice
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const PC: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 91, 241, 109));
    const PHONE: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 91, 38, 21));
    const ORDER: [IpAddr; 2] = [PC, PHONE];
    const TICK: Duration = Duration::from_millis(20);

    /// 20 ms of 48 kHz stereo at this RMS level; `None` is digital silence.
    fn chunk(db: Option<f32>) -> Vec<f32> {
        let a = db.map_or(0.0, |db| 10f32.powf(db / 20.0));
        (0..960)
            .flat_map(|i| if i % 2 == 0 { [a, a] } else { [-a, -a] })
            .collect()
    }

    /// Run `ticks` 20 ms ticks, each peer sending the given level, and return the choices.
    fn run(
        sel: &mut MicSelector,
        t: &mut Instant,
        ticks: usize,
        sends: &[(IpAddr, Option<f32>)],
    ) -> Vec<Option<IpAddr>> {
        (0..ticks)
            .map(|_| {
                *t += TICK;
                for (ip, db) in sends {
                    sel.observe(*ip, &chunk(*db), *t);
                }
                sel.select(&ORDER, *t)
            })
            .collect()
    }

    #[test]
    fn level_is_rms_in_dbfs() {
        assert_eq!(level_db(&[]), SILENCE_DB);
        assert_eq!(level_db(&[0.0; 64]), SILENCE_DB);
        assert!(level_db(&[1.0, -1.0]).abs() < 0.01);
        assert!((level_db(&chunk(Some(-30.0))) + 30.0).abs() < 0.01);
    }

    #[test]
    fn nobody_talking_falls_back_to_the_first_sending_peer() {
        let mut sel = MicSelector::new(SelectParams::default());
        let mut t = Instant::now();
        assert_eq!(
            sel.select(&ORDER, t),
            None,
            "nobody sends, nothing to choose"
        );
        let got = run(&mut sel, &mut t, 10, &[(PC, None), (PHONE, None)]);
        assert!(got.iter().all(|c| *c == Some(PC)), "as in single mode");
        let got = run(&mut sel, &mut t, 60, &[(PHONE, None)]);
        assert_eq!(
            got[0],
            Some(PC),
            "the PC counts as sending for a second more"
        );
        assert_eq!(got.last(), Some(&Some(PHONE)), "then only the phone sends");
    }

    #[test]
    fn a_silent_pc_first_in_the_list_does_not_shadow_the_talking_phone() {
        let mut sel = MicSelector::new(SelectParams::default());
        let mut t = Instant::now();
        run(&mut sel, &mut t, 50, &[(PC, None), (PHONE, Some(-70.0))]);
        assert_eq!(sel.current(), Some(PC));
        let got = run(&mut sel, &mut t, 1, &[(PC, None), (PHONE, Some(-25.0))]);
        assert_eq!(
            got,
            [Some(PHONE)],
            "the first voiced chunk switches at once"
        );
        // Back to room noise: the phone keeps the microphone, during the hold and after it.
        let got = run(&mut sel, &mut t, 200, &[(PC, None), (PHONE, Some(-70.0))]);
        assert!(got.iter().all(|c| *c == Some(PHONE)));
    }

    #[test]
    fn the_talker_is_not_cut_off_mid_sentence() {
        let params = SelectParams::default();
        let mut sel = MicSelector::new(params);
        let mut t = Instant::now();
        run(&mut sel, &mut t, 5, &[(PC, Some(-30.0)), (PHONE, None)]);
        assert_eq!(sel.current(), Some(PC));
        // The phone gets louder while the PC is still talking, and through a short pause.
        let got = run(
            &mut sel,
            &mut t,
            25,
            &[(PC, Some(-30.0)), (PHONE, Some(-15.0))],
        );
        assert!(got.iter().all(|c| *c == Some(PC)));
        let pause = (params.hold.as_millis() / 20) as usize - 5;
        let got = run(&mut sel, &mut t, pause, &[(PC, None), (PHONE, Some(-15.0))]);
        assert!(
            got.iter().all(|c| *c == Some(PC)),
            "a pause shorter than the hold"
        );
        // Quiet for longer than the hold: the phone takes over.
        let got = run(&mut sel, &mut t, 10, &[(PC, None), (PHONE, Some(-15.0))]);
        assert_eq!(got.last(), Some(&Some(PHONE)));
    }

    #[test]
    fn the_loudest_wins_when_two_start_together() {
        let mut sel = MicSelector::new(SelectParams::default());
        let mut t = Instant::now();
        let got = run(
            &mut sel,
            &mut t,
            1,
            &[(PC, Some(-35.0)), (PHONE, Some(-20.0))],
        );
        assert_eq!(got, [Some(PHONE)]);
        let mut sel = MicSelector::new(SelectParams::default());
        let got = run(
            &mut sel,
            &mut t,
            1,
            &[(PC, Some(-20.0)), (PHONE, Some(-20.0))],
        );
        assert_eq!(got, [Some(PC)], "a tie goes to the earlier peer");
    }

    #[test]
    fn a_steady_hum_stops_counting_as_voice() {
        let mut sel = MicSelector::new(SelectParams::default());
        let mut t = Instant::now();
        // A PC with an open microphone next to a fan, above the threshold, never pausing.
        run(&mut sel, &mut t, 1, &[(PC, Some(-40.0))]);
        assert_eq!(sel.current(), Some(PC));
        run(
            &mut sel,
            &mut t,
            500,
            &[(PC, Some(-40.0)), (PHONE, Some(-70.0))],
        );
        let got = run(
            &mut sel,
            &mut t,
            1,
            &[(PC, Some(-40.0)), (PHONE, Some(-28.0))],
        );
        assert_eq!(
            got,
            [Some(PHONE)],
            "after 10 s the hum no longer holds the microphone"
        );
    }

    #[test]
    fn a_peer_that_stops_sending_loses_the_microphone() {
        let mut sel = MicSelector::new(SelectParams::default());
        let mut t = Instant::now();
        run(&mut sel, &mut t, 5, &[(PC, None), (PHONE, Some(-20.0))]);
        assert_eq!(sel.current(), Some(PHONE));
        // The phone goes away mid-word; even within the hold the PC takes over once it is gone.
        let got = run(&mut sel, &mut t, 60, &[(PC, None)]);
        assert_eq!(got.last(), Some(&Some(PC)));
        let got = run(&mut sel, &mut t, 60, &[]);
        assert_eq!(got.last(), Some(&None));
    }

    #[test]
    fn peers_outside_the_list_are_never_chosen() {
        let mut sel = MicSelector::new(SelectParams::default());
        let mut t = Instant::now();
        let stranger = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        let got = run(&mut sel, &mut t, 5, &[(stranger, Some(-10.0)), (PC, None)]);
        assert!(got.iter().all(|c| *c == Some(PC)));
    }
}
