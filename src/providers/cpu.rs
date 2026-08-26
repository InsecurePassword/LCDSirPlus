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
const ENTRY_SIZE: usize = 24; // three LARGE_INTEGERs: idle, kernel, user

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
        let mut buffer = vec![[0i64; 3]; 256];
        let mut returned = 0u32;
        let status = unsafe {
            nt(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                (buffer.len() * ENTRY_SIZE) as u32,
                &mut returned,
            )
        };
        if status < 0 || returned == 0 {
            return Vec::new();
        }
        let count = (returned as usize / ENTRY_SIZE).min(buffer.len());
        let current: Vec<[i64; 3]> = buffer[..count].to_vec();
        let mut out = Vec::with_capacity(count);
        if self.prev.len() == count {
            for (cur, prev) in current.iter().zip(self.prev.iter()) {
                let d_idle = cur[0].saturating_sub(prev[0]);
                let d_kernel = cur[1].saturating_sub(prev[1]);
                let d_user = cur[2].saturating_sub(prev[2]);
                let total = (d_kernel + d_user).max(0);
                if total <= 0 {
                    out.push(0.0);
                    continue;
                }
                let busy = (total - d_idle).max(0);
                out.push((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0));
            }
        } else {
            out = vec![0.0; count];
        }
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
}
