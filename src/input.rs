//! Button input events and edge-debounce tracking.

use std::time::{Duration, Instant};

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
}
