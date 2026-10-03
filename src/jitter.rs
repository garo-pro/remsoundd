//! A reorder buffer keyed by a wrapping `u32` sequence.
//!
//! In-order packets pass straight through with no added delay. When one is missing, later packets
//! are held for up to `window` while it has a chance to turn up; after that it is declared lost,
//! and the caller conceals it (Opus FEC or PLC, or silence for PCM). The result is a stream with
//! no holes in it, which is what speech recognition needs, at the smallest delay that allows it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A jump this far either way is a sender that restarted, not reordering.
pub const RESTART_JUMP: u32 = 1000;

#[derive(Debug, PartialEq)]
pub enum Released<T> {
    Item(u32, T),
    /// `count` consecutive sequences starting at `first` never arrived.
    Lost {
        first: u32,
        count: u32,
    },
}

pub struct ReorderBuffer<T> {
    window: Duration,
    max_held: usize,
    next: Option<u32>,
    held: HashMap<u32, (T, Instant)>,
    pub late: u64,
    pub duplicates: u64,
    pub restarts: u64,
}

impl<T> ReorderBuffer<T> {
    pub fn new(window: Duration, max_held: usize) -> Self {
        Self {
            window,
            max_held: max_held.max(1),
            next: None,
            held: HashMap::new(),
            late: 0,
            duplicates: 0,
            restarts: 0,
        }
    }

    pub fn push(&mut self, seq: u32, item: T, now: Instant) {
        let next = *self.next.get_or_insert(seq);
        let ahead = seq.wrapping_sub(next);
        let behind = next.wrapping_sub(seq);
        if (ahead > RESTART_JUMP && ahead <= u32::MAX / 2)
            || (behind > RESTART_JUMP && behind < u32::MAX / 2)
        {
            self.held.clear();
            self.next = Some(seq);
            self.restarts += 1;
        } else if behind != 0 && behind < u32::MAX / 2 {
            self.late += 1;
            return;
        }
        if self.held.contains_key(&seq) {
            self.duplicates += 1;
            return;
        }
        self.held.insert(seq, (item, now));
    }

    /// Release everything that is ready, in order.
    pub fn pop(&mut self, now: Instant) -> Vec<Released<T>> {
        let mut out = Vec::new();
        let Some(mut next) = self.next else {
            return out;
        };
        loop {
            if let Some((item, _)) = self.held.remove(&next) {
                out.push(Released::Item(next, item));
                next = next.wrapping_add(1);
                continue;
            }
            let Some(oldest) = self.held.values().map(|(_, at)| *at).min() else {
                break;
            };
            if now.duration_since(oldest) < self.window && self.held.len() <= self.max_held {
                break;
            }
            let first_held = self
                .held
                .keys()
                .copied()
                .min_by_key(|s| s.wrapping_sub(next))
                .unwrap();
            out.push(Released::Lost {
                first: next,
                count: first_held.wrapping_sub(next),
            });
            next = first_held;
        }
        self.next = Some(next);
        out
    }

    pub fn held(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(r: Vec<Released<u32>>) -> Vec<String> {
        r.into_iter()
            .map(|x| match x {
                Released::Item(s, _) => s.to_string(),
                Released::Lost { first, count } => format!("lost {first}+{count}"),
            })
            .collect()
    }

    #[test]
    fn in_order_passes_straight_through() {
        let mut b = ReorderBuffer::new(Duration::from_millis(60), 64);
        let t = Instant::now();
        for s in 10..13 {
            b.push(s, s, t);
        }
        assert_eq!(items(b.pop(t)), ["10", "11", "12"]);
    }

    #[test]
    fn reordered_packet_is_waited_for() {
        let mut b = ReorderBuffer::new(Duration::from_millis(60), 64);
        let t = Instant::now();
        b.push(1, 1, t);
        b.push(3, 3, t);
        assert_eq!(items(b.pop(t)), ["1"]);
        b.push(2, 2, t + Duration::from_millis(10));
        assert_eq!(items(b.pop(t + Duration::from_millis(10))), ["2", "3"]);
        assert_eq!(b.late, 0);
    }

    #[test]
    fn missing_packet_is_declared_lost_after_the_window_and_late_ones_dropped() {
        let mut b = ReorderBuffer::new(Duration::from_millis(60), 64);
        let t = Instant::now();
        b.push(1, 1, t);
        b.push(4, 4, t);
        b.push(5, 5, t);
        assert_eq!(items(b.pop(t + Duration::from_millis(30))), ["1"]);
        assert_eq!(
            items(b.pop(t + Duration::from_millis(61))),
            ["lost 2+2", "4", "5"]
        );
        b.push(3, 3, t + Duration::from_millis(70));
        assert_eq!(b.late, 1);
        assert!(b.pop(t + Duration::from_millis(80)).is_empty());
    }

    #[test]
    fn too_many_held_forces_release() {
        let mut b = ReorderBuffer::new(Duration::from_secs(10), 3);
        let t = Instant::now();
        b.push(0, 0, t);
        for s in 2..6 {
            b.push(s, s, t);
        }
        assert_eq!(items(b.pop(t)), ["0", "lost 1+1", "2", "3", "4", "5"]);
    }

    #[test]
    fn duplicates_and_restarts() {
        let mut b = ReorderBuffer::new(Duration::from_millis(60), 64);
        let t = Instant::now();
        b.push(100, 1, t);
        b.push(102, 2, t);
        b.push(102, 3, t);
        assert_eq!(b.duplicates, 1);
        b.push(5000, 4, t);
        assert_eq!(b.restarts, 1);
        assert_eq!(items(b.pop(t)), ["5000"]);
        b.push(5, 5, t);
        assert_eq!(b.restarts, 2);
    }

    #[test]
    fn wraps_around_u32() {
        let mut b = ReorderBuffer::new(Duration::from_millis(60), 64);
        let t = Instant::now();
        b.push(u32::MAX, 1, t);
        b.push(0, 2, t);
        b.push(1, 3, t);
        assert_eq!(
            items(b.pop(t)),
            [u32::MAX.to_string(), "0".into(), "1".into()]
        );
    }
}
