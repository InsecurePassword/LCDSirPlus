//! Telemetry providers: Windows-local clock, per-logical-processor CPU load,
//! memory load, and native CCD topology detection.

pub mod audio;
pub mod ccd;
pub mod clock;
pub mod cpu;
pub mod discord;
pub mod gpu;
pub mod hang;
pub mod headset;
pub mod lhm;
pub mod memory;
pub mod netif;
pub mod presentmon;
pub mod xinput;
