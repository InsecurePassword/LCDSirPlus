//! Per-logical-processor CPU utilization from Windows scheduler accounting.
//!
//! `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` returns
//! per-LP idle/kernel/user time counters (100 ns units, kernel time includes
//! idle). Utilization is the busy fraction of the delta between samples — a
//! memory read per tick, no polling I/O.
//!
//! Displayed quantity (EC-5, pinned 2026-11-13): family **(b) TOTAL-MINUS-IDLE**
//! — `busy_percent` is the one derivation, from the sampled total time
//! (`d_kernel + d_user`) minus the idle delta. The shipped LCDSirReal app
//! displays kernel+user busy-time deltas (evidence A-9 b52-21); this product
//! intentionally displays total-minus-idle. See `docs/ARCHITECTURE.md`.

#![cfg(windows)]

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;

/// WO-TEL-D3-B1 (L7): the record capacity the provider samples into. 256 records x the 48 B
/// record (the const assertion below) = 12,288 B, the resident buffer size the census measured.
const RECORDS: usize = 256;

#[repr(C)]
#[derive(Clone, Default, Debug)]
struct SystemProcessorPerformanceInformation {
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    reserved1: [i64; 2],
    reserved2: u32,
}

const _: [(); 48] = [(); std::mem::size_of::<SystemProcessorPerformanceInformation>()];

type NtQuerySystemInformationFn =
    unsafe extern "system" fn(u32, *mut core::ffi::c_void, u32, *mut u32) -> i32;

fn nt_query() -> Option<NtQuerySystemInformationFn> {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        // FARPROC is Option<unsafe extern "system" fn() -> isize>.
        GetProcAddress(ntdll, s!("NtQuerySystemInformation"))
            .map(|p| std::mem::transmute::<_, NtQuerySystemInformationFn>(p))
    }
}

fn decode_samples(
    records: &[SystemProcessorPerformanceInformation],
    returned: usize,
) -> Option<Vec<[i64; 3]>> {
    let stride = std::mem::size_of::<SystemProcessorPerformanceInformation>();
    if returned == 0
        || returned > std::mem::size_of_val(records)
        || !returned.is_multiple_of(stride)
    {
        return None;
    }
    Some(
        records[..returned / stride]
            .iter()
            .map(|record| [record.idle_time, record.kernel_time, record.user_time])
            .collect(),
    )
}

/// The ONE busy-percent derivation (EC-5, family (b) total-minus-idle).
///
/// Takes one decoded `[idle, kernel, user]` record pair (100 ns units, kernel
/// time includes idle), forms the saturating counter deltas, treats
/// `total = d_kernel + d_user` as the sampled time, and reports
/// `(total - d_idle) / total * 100` clamped to `0..=100`. `total <= 0` (no
/// elapsed time, counter reset) reports `0.0`.
///
/// Every CPU surface consumes values produced here and nowhere else: the
/// CPU_LOAD bar, CPU_LOAD_GRAPH, the `cpuload` history feed, the BOTTLENECK
/// peak input, and the CCD Cache/Frequency split (which averages these percents
/// in [`aggregate`] and adds no arithmetic of its own). A second, independent
/// derivation anywhere in that path breaks
/// `ec5_all_cpu_surfaces_share_this_one_derivation`.
fn busy_percent(current: &[i64; 3], previous: &[i64; 3]) -> f64 {
    let d_idle = current[0].saturating_sub(previous[0]);
    let d_kernel = current[1].saturating_sub(previous[1]);
    let d_user = current[2].saturating_sub(previous[2]);
    let total = d_kernel.saturating_add(d_user).max(0);
    if total <= 0 {
        return 0.0;
    }
    let busy = (total - d_idle).max(0);
    (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
}

/// Per-logical-processor busy percent (`0..=100`): one [`busy_percent`] per LP,
/// in record order. A record-count change means the topology changed, so that
/// tick reports `0.0` per LP (as does the first sample, which has no delta).
fn utilization(current: &[[i64; 3]], previous: &[[i64; 3]]) -> Vec<f64> {
    if current.len() != previous.len() {
        return vec![0.0; current.len()];
    }
    current
        .iter()
        .zip(previous)
        .map(|(cur, prev)| busy_percent(cur, prev))
        .collect()
}

#[derive(Debug, Default)]
pub struct CpuLoadProvider {
    prev: Vec<[i64; 3]>,
    /// WO-TEL-D3-B1 (L7): the resident per-LP record buffer (256 x 48 B = 12,288 B, sized once
    /// and reused by every sample), so the per-sample allocation is gone. The buffer holds the
    /// raw records ONLY: the EC-5 single-derivation rule (`busy_percent`, family (b)) is
    /// untouched, and the app-loop consumer set (CPU bar, CCD split, peak, bottleneck, soak line)
    /// keeps reading the same `utilization` output.
    buffer: Vec<SystemProcessorPerformanceInformation>,
}

impl CpuLoadProvider {
    pub fn new() -> Self {
        CpuLoadProvider::default()
    }

    /// Sample per-LP utilization (0..100). The first sample returns zeros
    /// (no delta yet); later samples return one entry per logical processor.
    pub fn update(&mut self) -> Vec<f64> {
        let Some(nt) = nt_query() else {
            return Vec::new();
        };
        if self.buffer.len() != RECORDS {
            self.buffer = vec![SystemProcessorPerformanceInformation::default(); RECORDS];
        }
        let mut returned = 0u32;
        let status = unsafe {
            nt(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                self.buffer.as_mut_ptr() as *mut core::ffi::c_void,
                std::mem::size_of_val(self.buffer.as_slice()) as u32,
                &mut returned,
            )
        };
        if status < 0 {
            return Vec::new();
        }
        let Some(current) = decode_samples(&self.buffer, returned as usize) else {
            return Vec::new();
        };
        let out = utilization(&current, &self.prev);
        self.prev = current;
        out
    }
}

/// Aggregate utilization over a logical-processor mask (kernel LP indices).
///
/// Pure mean of the per-LP percents that [`busy_percent`] produced: the loop
/// sums and counts mask members only, so the CCD Cache/Frequency split carries
/// the same family (b) quantity as the CPU bar. No time delta and no division
/// by a time total happen here.
pub fn aggregate(per_lp: &[f64], mask: u64) -> f64 {
    if mask == 0 || per_lp.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0;
    let mut count = 0usize;
    for bit in 0..64usize {
        if mask & (1u64 << bit) != 0 {
            if let Some(v) = per_lp.get(bit) {
                sum += v;
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_averages_mask_members() {
        let per_lp = vec![10.0, 20.0, 30.0, 40.0];
        assert!((aggregate(&per_lp, 0b0011) - 15.0).abs() < 1e-9);
        assert!((aggregate(&per_lp, 0b1000) - 40.0).abs() < 1e-9);
        assert_eq!(aggregate(&per_lp, 0), 0.0);
        assert_eq!(
            aggregate(&per_lp, 1 << 40),
            0.0,
            "out-of-range bits ignored"
        );
    }

    #[test]
    fn native_record_stride_preserves_ccd_average() {
        assert_eq!(
            std::mem::size_of::<SystemProcessorPerformanceInformation>(),
            48
        );
        let previous = vec![[0; 3]; 16];
        let mut records = vec![SystemProcessorPerformanceInformation::default(); 16];
        for (lp, record) in records.iter_mut().enumerate() {
            record.idle_time = if lp == 0 { 0 } else { 100 };
            record.kernel_time = 100;
            record.reserved1 = [i64::MAX - lp as i64, i64::MIN + lp as i64];
            record.reserved2 = u32::MAX - lp as u32;
        }
        let bytes = std::mem::size_of_val(records.as_slice());
        let current = decode_samples(&records, bytes).unwrap();
        let per_lp = utilization(&current, &previous);

        assert_eq!(per_lp.len(), 16);
        assert_eq!(aggregate(&per_lp, 0xffff), 6.25);
        assert_eq!(
            utilization(&[[0, i64::MAX, i64::MAX]], &[[0; 3]]),
            vec![100.0]
        );
        assert!(decode_samples(&records, bytes - 1).is_none());
        assert!(decode_samples(&records, bytes + 48).is_none());
    }

    /// Family (b) hand-check: docs name this function as the single derivation.
    #[test]
    fn ec5_docs_name_the_single_derivation_and_family() {
        const DOCS: &str = include_str!("../../docs/ARCHITECTURE.md");
        let name = concat!("providers/cpu.rs::", stringify!(busy_percent));
        assert!(DOCS.contains(name), "docs must name the derivation {name}");
        assert!(
            DOCS.contains("total-minus-idle"),
            "docs must name the (b) family 'total-minus-idle'"
        );
    }

    /// Acceptance (2): known `[idle, kernel, user]` triples -> hand-derived values.
    /// Family (b): total = d_kernel + d_user; busy = total - d_idle;
    /// percent = busy / total * 100 clamped to 0..=100; total <= 0 -> 0.0.
    #[test]
    fn ec5_synthetic_records_yield_hand_derived_percents() {
        // (current, previous, expected) with the arithmetic shown in comments.
        let cases: Vec<([i64; 3], [i64; 3], f64, &str)> = vec![
            (
                [50, 100, 50],
                [0, 0, 0],
                200.0 / 3.0,
                "d=50/100/50 total=150 busy=100",
            ),
            ([0, 100, 0], [0, 0, 0], 100.0, "total=100 busy=100"),
            ([100, 100, 0], [0, 0, 0], 0.0, "total=100 busy=0 (all idle)"),
            (
                [150, 100, 50],
                [0, 0, 0],
                0.0,
                "total=150 busy=0 (idle >= total)",
            ),
            (
                [0, 100, 25],
                [0, 0, 0],
                100.0,
                "total=125 busy=125 (never idle)",
            ),
            ([25, 100, 0], [0, 0, 0], 75.0, "total=100 busy=75"),
            (
                [60, 120, 80],
                [10, 20, 30],
                200.0 / 3.0,
                "non-zero baseline d=50/100/50 total=150 busy=100",
            ),
            (
                [0, 0, 0],
                [100, 100, 100],
                0.0,
                "counter reset: saturating deltas are all zero -> total<=0",
            ),
            (
                [0, 300, 100],
                [100, 200, 100],
                100.0,
                "d_idle=-100 -> busy=200 of total=100 -> 200% clamps to 100",
            ),
            (
                [0, i64::MAX, i64::MAX],
                [0, 0, 0],
                100.0,
                "saturating_add total=i64::MAX -> ratio 1.0",
            ),
        ];
        for (current, previous, expected, why) in cases {
            let got = utilization(&[current], &[previous]);
            assert_eq!(got.len(), 1, "one value per record: {why}");
            assert!(
                (got[0] - expected).abs() < 1e-9,
                "{why}: got {} expected {expected}",
                got[0]
            );
            assert_eq!(
                got[0],
                busy_percent(&current, &previous),
                "map == derivation"
            );
        }
    }

    /// Acceptance (1)/(7): the CPU bar mean, the graph/history value, the
    /// BOTTLENECK peak input and the CCD mask split are all the SAME derivation.
    /// A second, independent derivation in the aggregate path fails this test.
    #[test]
    fn ec5_all_cpu_surfaces_share_this_one_derivation() {
        let previous = vec![[0i64; 3]; 4];
        let current = vec![
            [50, 100, 50], // LP0 -> 200/3 = 66.66666666666666
            [0, 100, 0],   // LP1 -> 100.0
            [100, 100, 0], // LP2 -> 0.0 (all idle)
            [25, 100, 0],  // LP3 -> 75.0
        ];
        let expected = [200.0 / 3.0, 100.0, 0.0, 75.0];
        let per_lp = utilization(&current, &previous);
        assert_eq!(per_lp.len(), 4);
        for (lp, value) in per_lp.iter().zip(&expected) {
            assert!((lp - value).abs() < 1e-9, "LP percent {lp} != {value}");
        }

        // CPU_LOAD bar / CPU_LOAD_GRAPH / history / BOTTLENECK peak input mean over
        // all LPs (app.rs: sum/len, and reduce(f64::max) for the peak).
        let bar_mean = per_lp.iter().sum::<f64>() / per_lp.len() as f64;
        assert!(
            (bar_mean - (200.0 / 3.0 + 100.0 + 0.0 + 75.0) / 4.0).abs() < 1e-9,
            "CPU bar mean {bar_mean}"
        );
        assert_eq!(
            bar_mean,
            aggregate(&per_lp, 0b1111),
            "CPU bar mean must equal the full-mask split (one derivation, same order)"
        );
        assert_eq!(
            per_lp.iter().copied().reduce(f64::max),
            Some(100.0),
            "BOTTLENECK peak input is a member of the same set"
        );

        // CCD Cache/Frequency split: mean over mask members, hand-derived.
        let cache = aggregate(&per_lp, 0b0011);
        assert!(
            (cache - 250.0 / 3.0).abs() < 1e-9,
            "cache split {cache} != 250/3"
        );
        let freq = aggregate(&per_lp, 0b1100);
        assert!((freq - 37.5).abs() < 1e-9, "frequency split {freq} != 37.5");

        // Discriminating check: family (a) is the kernel+user delta SUM, which is
        // the whole sampled window here (kernel time already contains idle), so it
        // reports 100% whenever any time elapsed. An independent (a) derivation in
        // the aggregate path therefore changes the split values asserted above.
        let family_a = |record: &[i64; 3], prior: &[i64; 3]| -> f64 {
            let window = record[1]
                .saturating_sub(prior[1])
                .saturating_add(record[2].saturating_sub(prior[2]));
            if window <= 0 {
                0.0
            } else {
                100.0
            }
        };
        assert_eq!(
            family_a(&current[2], &previous[2]),
            100.0,
            "family (a) is 100% on the all-idle LP2; the shipped (b) value is 0.0"
        );
        assert_ne!(
            aggregate(&per_lp, 0b1100),
            family_a(&current[2], &previous[2]),
            "the split must not collapse to the (a) family"
        );
    }

    /// Acceptance (3): absent/invalid records produce NO data, never a 0% sample.
    #[test]
    fn ec5_absent_or_invalid_records_are_no_data_not_zero_percent() {
        let records = vec![SystemProcessorPerformanceInformation::default(); 2];
        let bytes = std::mem::size_of_val(records.as_slice());
        assert!(decode_samples(&records, 0).is_none(), "returned == 0");
        assert!(decode_samples(&records, 8).is_none(), "sub-record tail");
        assert!(decode_samples(&records, bytes + 8).is_none(), "oversize");
        assert_eq!(decode_samples(&records, bytes).unwrap().len(), 2);

        // No records -> no per-LP value at all: the snapshot pushes no reading, so
        // `renderer::canonical_number_metric` returns `Metric::invalid()` (L1352-1354).
        assert!(
            utilization(&[], &[]).is_empty(),
            "no data is not reported as a 0% sample"
        );
        assert!(
            !crate::model::Metric::invalid().valid,
            "the no-data shape is invalid, not value 0.0"
        );

        // The provider keeps no prior records before a successful query, so its
        // first `update()` cannot synthesize a delta (the hardware-gated
        // `live_provider_returns_one_bounded_sample_per_lp` covers the live shape).
        let provider = CpuLoadProvider::new();
        assert!(provider.prev.is_empty(), "fresh provider has no records");
        assert_eq!(
            aggregate(&[], 0xffff),
            0.0,
            "aggregate of no data is documented 0.0; only the split path masks it (F-EC5-a)"
        );
    }

    #[test]
    #[ignore = "requires the 32-LP Windows target host"]
    fn live_provider_returns_one_bounded_sample_per_lp() {
        let mut provider = CpuLoadProvider::new();
        let first = provider.update();
        assert_eq!(first, vec![0.0; 32]);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let second = provider.update();
        assert_eq!(second.len(), 32);
        assert!(second
            .iter()
            .all(|value| value.is_finite() && (0.0..=100.0).contains(value)));
        let cache = aggregate(&second, 0xffff);
        let frequency = aggregate(&second, 0xffff_0000);
        assert!(cache.is_finite() && (0.0..=100.0).contains(&cache));
        assert!(frequency.is_finite() && (0.0..=100.0).contains(&frequency));
        println!(
            "CPU-LIVE count={} C={cache:.2} F={frequency:.2}",
            second.len()
        );
    }

    /// WO-TEL-D3-B1 (L7) STRUCTURAL + behavioral pin: the provider samples into ONE resident
    /// record buffer, so the per-sample `vec![…; 256]` allocation (256 records x 48 B =
    /// **12,288 B per sample**, the size the census measured) is gone. The buffer is allocated
    /// at most once per provider lifetime; consecutive samples reuse it (proved by pointer
    /// identity). The EC-5 single-derivation pins stay byte-identical: the change touches the
    /// buffer ONLY, never `busy_percent`/`decode_samples`/`utilization`.
    ///
    /// Disclosed caveat (the registered alloc-harness limitation: `backends/mod.rs` `alloc_count`
    /// plus the debug-profile flake register): production has no allocation counters, so this
    /// proof is code-derived (the allocation site is guarded) and test-side (buffer identity),
    /// NOT an allocation count measured in the release suite.
    #[test]
    fn cpu_provider_reuses_one_resident_sample_buffer() {
        const RECORDS: usize = 256;
        assert_eq!(
            std::mem::size_of::<SystemProcessorPerformanceInformation>() * RECORDS,
            12_288,
            "L7 size verification: 256 records x the record size = the 12,288 B/sample the census counted"
        );
        let source = include_str!("cpu.rs").replace("\r\n", "\n");
        let struct_head = source
            .find("pub struct CpuLoadProvider {")
            .expect("the shipped provider struct exists");
        let struct_src = &source[struct_head
            ..source
                .find("impl CpuLoadProvider {")
                .expect("the shipped impl exists")];
        let impl_head = source
            .find("impl CpuLoadProvider {")
            .expect("the shipped impl exists");
        let impl_src = &source[impl_head
            ..source
                .find("/// Aggregate utilization over a logical-processor mask")
                .expect("the shipped aggregate docs close the provider impl")];
        let alloc_sites = impl_src
            .matches("vec![SystemProcessorPerformanceInformation::default(); RECORDS]")
            .count();
        println!("D3B1-L7 harness-count: record-buffer allocation sites in the provider impl = {alloc_sites}");
        assert!(
            struct_src.contains("buffer: Vec<SystemProcessorPerformanceInformation>,"),
            "L7: the record buffer is a provider field (resident state, reused across samples)"
        );
        assert_eq!(
            alloc_sites, 1,
            "L7: exactly one allocation site exists, and it must sit behind the resident-buffer guard"
        );
        assert!(
            impl_src.contains("if self.buffer.len() != RECORDS {"),
            "L7: the single allocation site is the guard that fills the resident buffer once"
        );
        assert!(
            impl_src.contains("std::mem::size_of_val(self.buffer.as_slice())"),
            "L7: the query length is the resident buffer's own byte size (48 B x records)"
        );
        assert!(
            impl_src.matches("self.buffer").count() >= 4,
            "L7: the sampling path reads and writes the resident buffer (non-vacuity of the pin above)"
        );
        let mut provider = CpuLoadProvider::new();
        let first = provider.update();
        let first_ptr = provider.buffer.as_ptr();
        let second = provider.update();
        let second_ptr = provider.buffer.as_ptr();
        println!(
            "D3B1-L7 sample shape: per_lp first={} second={} buffer_len={}",
            first.len(),
            second.len(),
            provider.buffer.len()
        );
        assert_eq!(
            first_ptr, second_ptr,
            "L7: two consecutive samples share ONE buffer allocation (the per-sample alloc is gone)"
        );
        if nt_query().is_some() {
            assert_eq!(
                provider.buffer.len(),
                RECORDS,
                "L7: on a live host the resident buffer holds the full 256-record capacity"
            );
        }
    }
}
