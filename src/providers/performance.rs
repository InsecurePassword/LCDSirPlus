//! Disk throughput and approximate hard-fault pressure from PDH.

#![cfg(windows)]
#![allow(dead_code)]

use std::ptr::null;
use std::time::SystemTime;

use windows::core::PCWSTR;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE,
    PDH_FMT_DOUBLE,
};

const PATHS: [&str; 3] = [
    r"\PhysicalDisk(_Total)\Disk Read Bytes/sec",
    r"\PhysicalDisk(_Total)\Disk Write Bytes/sec",
    r"\Memory\Page Reads/sec",
];

#[derive(Clone, Copy, Debug)]
pub struct PerformanceSample {
    pub disk_read_bytes_per_sec: f64,
    pub disk_write_bytes_per_sec: f64,
    /// Approximate hard-fault pressure; this includes page reads not caused by faults.
    pub page_reads_per_sec: f64,
    pub sampled_at: SystemTime,
}

pub struct Provider {
    query: Option<Query>,
}

struct Query {
    handle: isize,
    counters: [isize; 3],
    baseline: bool,
}

impl Provider {
    pub fn new() -> Self {
        Self { query: None }
    }

    /// The first successful collection establishes a baseline and returns `Ok(None)`.
    pub fn sample(&mut self) -> Result<Option<PerformanceSample>, String> {
        if self.query.is_none() {
            self.query = Some(Query::open()?);
        }
        let result = self.query.as_mut().unwrap().sample();
        if result.is_err() {
            self.query = None;
        }
        result
    }
}

impl Default for Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl Query {
    fn open() -> Result<Self, String> {
        unsafe {
            let mut handle = 0;
            pdh(
                PdhOpenQueryW(PCWSTR(null()), 0, &mut handle),
                "PdhOpenQueryW",
            )?;
            let mut query = Self {
                handle,
                counters: [0; 3],
                baseline: false,
            };
            for (index, path) in PATHS.iter().enumerate() {
                let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                if let Err(error) = pdh(
                    PdhAddEnglishCounterW(
                        handle,
                        PCWSTR(wide.as_ptr()),
                        0,
                        &mut query.counters[index],
                    ),
                    "PdhAddEnglishCounterW",
                ) {
                    return Err(format!("{error}: {path}"));
                }
            }
            Ok(query)
        }
    }

    fn sample(&mut self) -> Result<Option<PerformanceSample>, String> {
        unsafe {
            pdh(PdhCollectQueryData(self.handle), "PdhCollectQueryData")?;
            if !self.baseline {
                self.baseline = true;
                return Ok(None);
            }
            let values = [
                formatted(self.counters[0])?,
                formatted(self.counters[1])?,
                formatted(self.counters[2])?,
            ];
            Ok(Some(PerformanceSample {
                disk_read_bytes_per_sec: values[0],
                disk_write_bytes_per_sec: values[1],
                page_reads_per_sec: values[2],
                sampled_at: SystemTime::now(),
            }))
        }
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.handle);
        }
    }
}

fn pdh(code: u32, operation: &str) -> Result<(), String> {
    (code == 0)
        .then_some(())
        .ok_or_else(|| format!("{operation} failed (0x{code:08x})"))
}

unsafe fn formatted(counter: isize) -> Result<f64, String> {
    let mut value = PDH_FMT_COUNTERVALUE::default();
    pdh(
        PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut value),
        "PdhGetFormattedCounterValue",
    )?;
    normalize(value.CStatus, value.Anonymous.doubleValue)
}

fn normalize(status: u32, value: f64) -> Result<f64, String> {
    if !matches!(status, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA) {
        return Err(format!("PDH counter status invalid (0x{status:08x})"));
    }
    if !value.is_finite() {
        return Err("PDH returned a non-finite value".into());
    }
    Ok(value.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatted_values_require_valid_status_and_finite_data() {
        assert_eq!(normalize(PDH_CSTATUS_VALID_DATA, -1.0).unwrap(), 0.0);
        assert_eq!(normalize(PDH_CSTATUS_NEW_DATA, 12.5).unwrap(), 12.5);
        assert!(normalize(0xc000_0bc6, 1.0).is_err());
        assert!(normalize(PDH_CSTATUS_VALID_DATA, f64::NAN).is_err());
    }
}
