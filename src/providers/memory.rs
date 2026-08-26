//! System memory load via GlobalMemoryStatusEx.

#![cfg(windows)]

use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

pub fn memory_load_percent() -> f64 {
    unsafe {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut status).is_ok() {
            f64::from(status.dwMemoryLoad).clamp(0.0, 100.0)
        } else {
            0.0
        }
    }
}
