//! Bounded chronological series and frame-statistics helpers.
//!
//! Ported from the LCDForge Go implementation (`internal/history`). The ring
//! buffer replaces the oldest sample in O(1) once full; it never shifts the
//! complete history on every frame.
//! Stats helpers are Phase 2 surface (PresentMon 1%/0.1% lows, jitter).
#![allow(dead_code)]

use std::time::SystemTime;

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub at: SystemTime,
    pub value: f64,
}

#[derive(Debug)]
pub struct Series {
    capacity: usize,
    values: Vec<Sample>,
    start: usize,
    size: usize,
}

impl Series {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Series {
            capacity,
            values: vec![
                Sample {
                    at: SystemTime::UNIX_EPOCH,
                    value: 0.0
                };
                capacity
            ],
            start: 0,
            size: 0,
        }
    }

    pub fn add(&mut self, at: SystemTime, value: f64) {
        if !value.is_finite() {
            return;
        }
        if self.size < self.capacity {
            let index = (self.start + self.size) % self.capacity;
            self.values[index] = Sample { at, value };
            self.size += 1;
            return;
        }
        self.values[self.start] = Sample { at, value };
        self.start = (self.start + 1) % self.capacity;
    }

    pub fn snapshot(&self) -> Vec<Sample> {
        (0..self.size)
            .map(|i| self.values[(self.start + i) % self.capacity])
            .collect()
    }

    pub fn values_since(&self, cutoff: SystemTime) -> Vec<f64> {
        (0..self.size)
            .map(|i| self.values[(self.start + i) % self.capacity])
            .filter(|s| s.at >= cutoff)
            .map(|s| s.value)
            .collect()
    }
}

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// Mean of the lowest-valued percentile (adverse tail for FPS-like values).
pub fn percentile_low(values: &[f64], percentile: f64) -> f64 {
    tail_mean(values, percentile, true)
}

/// Mean of the highest-valued percentile. `1000 / percentile_high(frame_times,
/// 0.01)` is the conventional 1% low frame rate.
pub fn percentile_high(values: &[f64], percentile: f64) -> f64 {
    tail_mean(values, percentile, false)
}

fn tail_mean(values: &[f64], percentile: f64, low: bool) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let percentile = if percentile <= 0.0 {
        0.01
    } else if percentile > 1.0 {
        1.0
    } else {
        percentile
    };
    let mut cp = values.to_vec();
    cp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = ((cp.len() as f64) * percentile).ceil() as usize;
    let n = n.clamp(1, cp.len());
    if low {
        mean(&cp[..n])
    } else {
        mean(&cp[cp.len() - n..])
    }
}

pub fn jitter(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let sum: f64 = values.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    sum / (values.len() - 1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn epoch_plus(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn ring_buffer_wraps_in_order() {
        let mut s = Series::new(3);
        for i in 0..5u64 {
            s.add(epoch_plus(i), i as f64);
        }
        let vals: Vec<f64> = s.snapshot().iter().map(|v| v.value).collect();
        assert_eq!(vals, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn values_since_filters_by_time() {
        let mut s = Series::new(4);
        s.add(epoch_plus(1), 1.0);
        s.add(epoch_plus(2), 2.0);
        s.add(epoch_plus(3), 3.0);
        assert_eq!(s.values_since(epoch_plus(2)), vec![2.0, 3.0]);
    }

    #[test]
    fn percentile_low_matches_go() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert!((percentile_low(&v, 0.01) - 1.0).abs() < 1e-9);
        assert!((percentile_high(&v, 0.01) - 100.0).abs() < 1e-9);
        assert!(percentile_low(&v, 0.1) < percentile_high(&v, 0.1));
        assert_eq!(percentile_low(&[], 0.01), 0.0);
    }

    #[test]
    fn jitter_is_mean_absolute_delta() {
        let v = vec![1.0, 3.0, 2.0];
        assert!((jitter(&v) - (2.0 + 1.0) / 2.0).abs() < 1e-9);
        assert_eq!(jitter(&[1.0]), 0.0);
    }
}
