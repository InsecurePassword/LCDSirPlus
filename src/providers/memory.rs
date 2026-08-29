//! System memory load via GlobalMemoryStatusEx.

#![cfg(windows)]

use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Clone, Copy, Debug)]
pub struct MemorySample {
    pub percent: f64,
    pub used_bytes: u64,
    pub total_bytes: u64,
    pub sampled_at: std::time::SystemTime,
}

pub fn sample() -> Result<MemorySample, String> {
    unsafe {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut status)
            .map_err(|e| format!("GlobalMemoryStatusEx failed: {e}"))?;
        Ok(MemorySample {
            percent: f64::from(status.dwMemoryLoad).clamp(0.0, 100.0),
            used_bytes: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
            total_bytes: status.ullTotalPhys,
            sampled_at: std::time::SystemTime::now(),
        })
    }
}
