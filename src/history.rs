//! Bounded chronological series and frame-statistics helpers.
//!
//! Ported from the original Go implementation (`internal/history`). The ring
//! buffer replaces the oldest sample in O(1) once full; it never shifts the
//! complete history on every frame.
//! Stats helpers are Phase 2 surface (PresentMon 1%/0.1% lows, jitter).
#![allow(dead_code)]

use std::time::{Duration, SystemTime};

/// Model-shape census counters (WO-OPT05 A1): the `sort_by` invocations and the `to_vec()`
/// clones one `Stats::model` pays for its two high-percentile tail means. The lane's precedent
/// shape is `presentmon.rs`'s `WORKER_TICKS`/`worker_tick_count` pair: a per-THREAD map, so a
/// census measures the calling test's own model calls and not the whole process (the shipped
/// suite runs tests in parallel). One entry per test thread in the test binary, `cfg(test)`
/// only, never compiled into the shipped artifact. These are COUNTS, not time: no CPU is
/// measured and no CPU claim is made from these rows.
#[cfg(test)]
static MODEL_SHAPE_CENSUS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, (u64, u64)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn model_census_map(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, (u64, u64)>> {
    MODEL_SHAPE_CENSUS.get_or_init(std::sync::Mutex::default)
}

#[cfg(test)]
pub fn model_census_counts() -> (u64, u64) {
    let map = model_census_map();
    let map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.get(&std::thread::current().id())
        .copied()
        .unwrap_or((0, 0))
}

#[cfg(test)]
pub fn model_census_reset() {
    let map = model_census_map();
    let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.insert(std::thread::current().id(), (0, 0));
}

#[cfg(test)]
pub fn model_sort_tick() {
    let map = model_census_map();
    let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(std::thread::current().id()).or_insert((0, 0)).0 += 1;
}

#[cfg(test)]
pub fn model_clone_tick() {
    let map = model_census_map();
    let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(std::thread::current().id()).or_insert((0, 0)).1 += 1;
}

#[cfg(not(test))]
fn model_sort_tick() {}

#[cfg(not(test))]
fn model_clone_tick() {}

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
    let percentile = clamp_percentile(percentile);
    model_clone_tick();
    let mut cp = values.to_vec();
    model_sort_tick();
    cp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    tail_mean_of_sorted(&cp, percentile)
}

fn clamp_percentile(percentile: f64) -> f64 {
    if percentile <= 0.0 {
        0.01
    } else if percentile > 1.0 {
        1.0
    } else {
        percentile
    }
}

/// The tail mean of an ALREADY-ORDERED slice: the shipped `tail_mean` arithmetic from
/// `n = ceil(len * percentile)` clamped to `[1, len]`, over the highest `n` values.
/// Split out so the one ordering below and the public `percentile_high` share the exact
/// same arithmetic; nothing in it changed.
fn tail_mean_of_sorted(sorted: &[f64], percentile: f64) -> f64 {
    let n = ((sorted.len() as f64) * percentile).ceil() as usize;
    let n = n.clamp(1, sorted.len());
    mean(&sorted[sorted.len() - n..])
}

/// The 1% and 0.1% low tail means (0.01, 0.001) of one window from ONE ordering, sorting
/// the caller's buffer in place (WO-OPT05 A2.1/A2.2: the model paid two clones and two
/// full sorts of the same window for these two numbers).
///
/// Bit-identical to `percentile_high(values, 0.01)` plus `percentile_high(values, 0.001)`:
/// the comparator and the sort kind are byte-identical and STABLE on purpose. `NaN`
/// compares `Equal` to everything under `partial_cmp().unwrap_or(Equal)`, so
/// `sort_unstable_by` may permute the NaN class within a run of equal keys and land a NaN
/// inside the tail, changing a tail mean. Pinned by `tail_means_are_bit_identical_to_two
/// _percentile_high_calls`.
pub fn tail_means(values: &mut [f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    model_sort_tick();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (
        tail_mean_of_sorted(values, 0.01),
        tail_mean_of_sorted(values, 0.001),
    )
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

    /// WO-OPT05 A3: `tail_means` is BIT-IDENTICAL to the two `percentile_high` calls it
    /// replaces, on every shape that can reach a tail mean. The sort is STABLE on purpose:
    /// `NaN` compares `Equal` to everything under `partial_cmp().unwrap_or(Equal)`, so an
    /// unstable sort can permute the NaN class into the tail and change a tail mean. The
    /// There is NO fixture that discriminates the `sort_unstable_by` mutation on this
    /// toolchain: the separating shapes panic inside the SHIPPED `sort_by` first (the
    /// measured NOTE inside this test states it), so the constraint is held by
    /// construction plus the count-0 needle, and the mutation is recorded as
    /// non-discriminating, never as a passing proof (F-AUD-OPT05-2).
    #[test]
    fn tail_means_are_bit_identical_to_two_percentile_high_calls() {
        let cases: Vec<(&str, Vec<f64>)> = vec![
            ("empty", vec![]),
            ("one", vec![16.0]),
            ("duplicates", vec![16.0; 100]),
            ("nan-first", vec![f64::NAN, 16.0, 16.0]),
            ("nan-last", vec![16.0, 16.0, f64::NAN]),
            ("nan-middle", vec![16.0, f64::NAN, 16.0]),
            ("nan-only", vec![f64::NAN; 50]),
            // NOTE (measured, WO-OPT05 S3): the fixtures that could separate a STABLE from
            // an UNSTABLE ordering are distinct values with a `NaN`, which the shipped
            // comparator makes `Equal` to everything. They are not usable in a test: the debug
            // build's sort-integrity check fires inside the SHIPPED `sort_by` first
            // (`library/core/src/slice/sort/shared/smallsort.rs:854`, "user-provided comparison
            // function does not correctly implement a total order"), so the shipped and the
            // mutated path both panic and no fixture can show the difference. The constraint is
            // kept by construction (the byte-identical comparator and sort kind below, F-AUD-8
            // needle `sort_unstable_by` count 0) and the mutation is recorded as
            // non-discriminating, never as a passing proof.
            ("inf", vec![f64::INFINITY, 1.0, 2.0]),
            ("neg-inf", vec![f64::NEG_INFINITY, 1.0, 2.0]),
            (
                "inf-nan",
                vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 3.0],
            ),
            (
                "unsorted",
                (1..=50).map(|index| ((index * 7) % 50) as f64).collect(),
            ),
            (
                "descending",
                (1..=50).rev().map(|index| index as f64).collect(),
            ),
            ("nan-early-1-percent-tail", {
                let mut values = vec![16.0; 100];
                values[0] = f64::NAN;
                values
            }),
            ("nan-early-01-percent-tail", {
                let mut values = vec![16.0; 1_000];
                values[0] = f64::NAN;
                values[1] = f64::NAN;
                values[2] = f64::NAN;
                values
            }),
            ("nan-duplicate-run", {
                let mut values: Vec<f64> = (0..200)
                    .map(|index| if index % 2 == 0 { 16.0 } else { 33.0 })
                    .collect();
                values[0] = f64::NAN;
                values[1] = f64::NAN;
                values
            }),
            ("zeros", vec![0.0; 40]),
            ("subnormal-and-huge", vec![1e-300, 1e-300, 1e300]),
        ];
        for (name, values) in cases {
            let base = (
                percentile_high(&values, 0.01),
                percentile_high(&values, 0.001),
            );
            let mut scratch = values.clone();
            let changed = tail_means(&mut scratch);
            assert_eq!(
                base.0.to_bits(),
                changed.0.to_bits(),
                "{name}: the 1% tail mean bit pattern moved"
            );
            assert_eq!(
                base.1.to_bits(),
                changed.1.to_bits(),
                "{name}: the 0.1% tail mean bit pattern moved"
            );
            println!(
                "A3-HISTORY {name} len={} base=(1%={:.6},0.1%={:.6}) changed=(1%={:.6},0.1%={:.6}) BIT-EQUAL=True",
                values.len(),
                base.0,
                base.1,
                changed.0,
                changed.1
            );
        }
        // The ordering happens IN the caller's buffer: that is how the clone disappeared.
        let mut scratch = vec![3.0, 1.0, 2.0];
        let _ = tail_means(&mut scratch);
        assert_eq!(
            scratch,
            vec![1.0, 2.0, 3.0],
            "tail_means sorts the caller's scratch in place"
        );
    }
}
