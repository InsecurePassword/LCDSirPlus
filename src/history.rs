//! Bounded chronological series and frame-statistics helpers.
//!
//! Ported from the original Go implementation (`internal/history`). The ring
//! buffer replaces the oldest sample in O(1) once full; it never shifts the
//! complete history on every frame.
//! Stats helpers are Phase 2 surface (PresentMon 1%/0.1% lows, jitter).
#![allow(dead_code)]

use std::time::{Duration, SystemTime};

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
        if self.size > 0 && at < self.values[(self.start + self.size - 1) % self.capacity].at {
            self.start = 0;
            self.size = 0;
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

    /// Latest sample in each elapsed one-second bin, oldest to newest.
    pub fn second_bins(&self, end: SystemTime, count: usize) -> Vec<Option<f64>> {
        let mut bins = vec![None; count];
        if count == 0 {
            return bins;
        }
        let Some(start) = end.checked_sub(Duration::from_secs(count as u64)) else {
            return bins;
        };
        for sample in self.snapshot() {
            if sample.at > end {
                continue;
            }
            let Ok(offset) = sample.at.duration_since(start) else {
                continue;
            };
            let bin = (offset.as_secs() as usize).min(count - 1);
            bins[bin] = Some(sample.value);
        }
        bins
    }
}

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// Mean of the highest-valued percentile. `1000 / percentile_high(frame_times,
/// 0.01)` is the conventional 1% low frame rate.
pub fn percentile_high(values: &[f64], percentile: f64) -> f64 {
    tail_mean(values, percentile)
}

fn tail_mean(values: &[f64], percentile: f64) -> f64 {
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
    mean(&cp[cp.len() - n..])
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
    fn elapsed_second_bins_preserve_gaps_latest_value_and_reset_on_rollback() {
        let mut s = Series::new(40);
        s.add(epoch_plus(10), 1.0);
        s.add(epoch_plus(10) + std::time::Duration::from_millis(900), 2.0);
        s.add(epoch_plus(12), 3.0);
        assert_eq!(
            s.second_bins(epoch_plus(12), 4),
            vec![None, None, Some(2.0), Some(3.0)]
        );
        s.add(epoch_plus(5), 9.0);
        assert_eq!(s.snapshot().len(), 1);
        assert_eq!(s.second_bins(epoch_plus(5), 2), vec![None, Some(9.0)]);
    }

    #[test]
    fn elapsed_bins_use_exact_subsecond_boundaries_and_exclude_future() {
        let end = epoch_plus(100) + Duration::from_millis(999);
        let start = end - Duration::from_secs(30);
        let mut s = Series::new(16);
        s.add(start - Duration::from_millis(1), 1.0); // age 30.001s
        s.add(start, 2.0); // exact 30s boundary
        s.add(start + Duration::from_millis(999), 3.0);
        s.add(start + Duration::from_secs(1), 4.0);
        s.add(end, 5.0);
        s.add(end + Duration::from_millis(1), 6.0);

        let bins = s.second_bins(end, 30);
        assert_eq!(bins[0], Some(3.0), "latest sample in first bin wins");
        assert_eq!(bins[1], Some(4.0));
        assert!(bins[2..29].iter().all(Option::is_none));
        assert_eq!(bins[29], Some(5.0));
        assert!(!bins
            .iter()
            .flatten()
            .any(|value| *value == 1.0 || *value == 6.0));
    }

    #[test]
    fn elapsed_bins_are_independent_of_subsecond_sample_cadence() {
        let end = epoch_plus(60) + Duration::from_millis(125);
        let start = end - Duration::from_secs(30);
        let mut s = Series::new(16);
        for quarter in 0..8 {
            s.add(start + Duration::from_millis(quarter * 250), quarter as f64);
        }
        let bins = s.second_bins(end, 30);
        assert_eq!(bins[0], Some(3.0));
        assert_eq!(bins[1], Some(7.0));
        assert!(bins[2..].iter().all(Option::is_none));
    }

    #[test]
    fn percentile_high_matches_go() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert!((percentile_high(&v, 0.01) - 100.0).abs() < 1e-9);
        assert_eq!(percentile_high(&[], 0.01), 0.0);
    }

    #[test]
    fn jitter_is_mean_absolute_delta() {
        let v = vec![1.0, 3.0, 2.0];
        assert!((jitter(&v) - (2.0 + 1.0) / 2.0).abs() < 1e-9);
        assert_eq!(jitter(&[1.0]), 0.0);
    }
}
