//! Telemetry providers: Windows-local clock, per-logical-processor CPU load,
//! memory load, and native CCD topology detection.

pub mod ccd;
pub mod clock;
pub mod cpu;
pub mod memory;
