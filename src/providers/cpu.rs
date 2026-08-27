//! Per-logical-processor CPU utilization from Windows scheduler accounting.
//!
//! `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` returns
//! per-LP idle/kernel/user time counters (100 ns units, kernel time includes
//! idle). Utilization is the busy fraction of the delta between samples — a
//! memory read per tick, no polling I/O.

#![cfg(windows)]

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;

#[repr(C)]
#[derive(Clone, Default)]
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

fn utilization(current: &[[i64; 3]], previous: &[[i64; 3]]) -> Vec<f64> {
    if current.len() != previous.len() {
        return vec![0.0; current.len()];
    }
    current
        .iter()
        .zip(previous)
        .map(|(cur, prev)| {
            let d_idle = cur[0].saturating_sub(prev[0]);
            let d_kernel = cur[1].saturating_sub(prev[1]);
            let d_user = cur[2].saturating_sub(prev[2]);
            let total = d_kernel.saturating_add(d_user).max(0);
            if total <= 0 {
                return 0.0;
            }
            let busy = (total - d_idle).max(0);
            (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct CpuLoadProvider {
    prev: Vec<[i64; 3]>,
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
        let mut buffer = vec![SystemProcessorPerformanceInformation::default(); 256];
        let mut returned = 0u32;
        let status = unsafe {
            nt(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                std::mem::size_of_val(buffer.as_slice()) as u32,
                &mut returned,
            )
        };
        if status < 0 {
            return Vec::new();
        }
        let Some(current) = decode_samples(&buffer, returned as usize) else {
            return Vec::new();
        };
        let out = utilization(&current, &self.prev);
        self.prev = current;
        out
    }
}

/// Aggregate utilization over a logical-processor mask (kernel LP indices).
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
}
