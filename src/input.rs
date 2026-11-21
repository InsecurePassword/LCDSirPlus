//! Button input events and edge-debounce tracking.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// EC-7: per-transition monotonic sequence stamp; never reset, and it saturates
/// at `u64::MAX` (the wrap policy: at saturation the last stamp repeats).
static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

/// Stamp taken at the emission site, so a stamp identifies one physical
/// transition; `0` is the unstamped value events built without this helper carry.
fn next_stamp() -> u64 {
    NEXT_STAMP
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |stamp| {
            stamp.checked_add(1)
        })
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Physical button index 0..4.
    pub index: usize,
    pub down: bool,
    pub backward: bool,
    /// Canceled ends an in-progress press without treating it as a deliberate
    /// release. Device loss and input-source shutdown must use this flag.
    pub canceled: bool,
    pub at: Instant,
    pub source: &'static str,
    /// EC-7: per-transition monotonic stamp (0 = unstamped), so a consumer can
    /// tell a re-emitted transition of the same physical press from a new press.
    pub stamp: u64,
}

/// Edge detector with per-button debounce, ported from the Go
/// `buttonTracker`. A raw change must hold stable for `debounce` before it
/// becomes an emitted edge; disconnect emits canceled releases.
#[derive(Debug, Default)]
pub struct ButtonTracker {
    initialized: bool,
    raw: [bool; 4],
    stable: [bool; 4],
    emitted: [bool; 4],
    changed: [Option<Instant>; 4],
}

impl ButtonTracker {
    pub fn observe(
        &mut self,
        now: Instant,
        values: [bool; 4],
        debounce: Duration,
        source: &'static str,
    ) -> Vec<Event> {
        if !self.initialized {
            self.initialized = true;
            self.raw = values;
            self.stable = values;
            self.changed = [Some(now); 4];
            return Vec::new();
        }
        let mut events = Vec::new();
        for (i, &value) in values.iter().enumerate() {
            if value != self.raw[i] {
                self.raw[i] = values[i];
                self.changed[i] = Some(now);
                continue;
            }
            if self.stable[i] != self.raw[i]
                && now.duration_since(self.changed[i].unwrap_or(now)) >= debounce
            {
                self.stable[i] = self.raw[i];
                if self.stable[i] {
                    self.emitted[i] = true;
                    events.push(Event {
                        index: i,
                        down: true,
                        backward: false,
                        canceled: false,
                        at: now,
                        source,
                        stamp: next_stamp(),
                    });
                } else if self.emitted[i] {
                    self.emitted[i] = false;
                    events.push(Event {
                        index: i,
                        down: false,
                        backward: false,
                        canceled: false,
                        at: now,
                        source,
                        stamp: next_stamp(),
                    });
                }
            }
        }
        events
    }

    pub fn disconnect(&mut self, now: Instant, source: &'static str) -> Vec<Event> {
        let mut events = Vec::new();
        for (i, down) in self.emitted.iter().enumerate() {
            if *down {
                events.push(Event {
                    index: i,
                    down: false,
                    backward: false,
                    canceled: true,
                    at: now,
                    source,
                    stamp: next_stamp(),
                });
            }
        }
        *self = ButtonTracker::default();
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64, millis: u64) -> Instant {
        // Instant::now() is monotonic; tests use relative arithmetic only.
        Instant::now() - Duration::new(1000 - secs, (millis * 1_000_000) as u32)
    }

    #[test]
    fn first_observation_emits_nothing() {
        let mut b = ButtonTracker::default();
        assert!(b
            .observe(t(0, 0), [true; 4], Duration::from_millis(40), "test")
            .is_empty());
    }

    #[test]
    fn stable_press_emits_down_then_release() {
        let mut b = ButtonTracker::default();
        b.observe(t(0, 0), [false; 4], Duration::from_millis(40), "test");
        assert!(
            b.observe(
                t(1, 0),
                [true, false, false, false],
                Duration::from_millis(40),
                "test"
            )
            .is_empty(),
            "raw change only"
        );
        let events = b.observe(
            t(2, 0),
            [true, false, false, false],
            Duration::from_millis(40),
            "test",
        );
        assert_eq!(events.len(), 1);
        assert!(events[0].down && events[0].index == 0);
        assert!(b
            .observe(t(3, 0), [false; 4], Duration::from_millis(40), "test")
            .is_empty());
        let events = b.observe(t(4, 0), [false; 4], Duration::from_millis(40), "test");
        assert_eq!(events.len(), 1);
        assert!(!events[0].down && !events[0].canceled);
    }

    #[test]
    fn bounce_shorter_than_debounce_is_ignored() {
        let mut b = ButtonTracker::default();
        b.observe(t(0, 0), [false; 4], Duration::from_millis(40), "test");
        b.observe(t(1, 0), [true; 4], Duration::from_millis(40), "test");
        // Bounce back within debounce window.
        b.observe(t(1, 10), [false; 4], Duration::from_millis(40), "test");
        let events = b.observe(t(1, 20), [false; 4], Duration::from_millis(40), "test");
        assert!(events.is_empty(), "bounce must not emit");
    }

    #[test]
    fn disconnect_emits_canceled_releases() {
        let mut b = ButtonTracker::default();
        b.observe(t(0, 0), [false; 4], Duration::from_millis(40), "test");
        b.observe(
            t(1, 0),
            [false, false, true, true],
            Duration::from_millis(40),
            "test",
        );
        b.observe(
            t(2, 0),
            [false, false, true, true],
            Duration::from_millis(40),
            "test",
        );
        let events = b.disconnect(t(3, 0), "test");
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| !e.down && e.canceled));
        assert!(events.iter().all(|e| e.source == "test"));
    }

    #[test]
    fn ec7_every_emitted_transition_carries_a_unique_non_decreasing_stamp() {
        let debounce = Duration::from_millis(40);
        let mut b = ButtonTracker::default();
        let mut stamps: Vec<u64> = Vec::new();
        for (second, states) in [
            (0u64, [false, false, false, false]),
            (1, [true, false, false, false]),
            (2, [true, false, false, false]),
            (3, [true, true, false, false]),
            (4, [true, true, false, false]),
            (5, [false, true, false, false]),
            (6, [false, true, false, false]),
        ]
        .into_iter()
        {
            stamps.extend(
                b.observe(t(second, 0), states, debounce, "test")
                    .into_iter()
                    .map(|event| event.stamp),
            );
        }
        stamps.extend(
            b.disconnect(t(7, 0), "test")
                .into_iter()
                .map(|event| event.stamp),
        );
        assert_eq!(
            stamps.len(),
            4,
            "down(0), down(1), release(0), canceled release(1)"
        );
        assert!(
            stamps.iter().all(|&stamp| stamp != 0),
            "an emitted transition is never the unstamped value"
        );
        assert!(
            stamps.windows(2).all(|pair| pair[1] > pair[0]),
            "stamps are unique and non-decreasing: {stamps:?}"
        );
    }
}
