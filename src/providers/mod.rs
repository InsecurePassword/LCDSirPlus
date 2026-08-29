//! Telemetry providers: Windows-local clock, per-logical-processor CPU load,
//! memory load, and native CCD topology detection.

pub mod audio;
pub mod ccd;
pub mod clock;
pub mod connections;
pub mod cpu;
pub mod discord;
pub mod gpu;
pub mod hang;
pub mod headset;
pub mod hwinfo;
pub mod lhm;
pub mod memory;
pub mod netif;
pub mod network;
pub mod performance;
pub mod presentmon;
pub mod system_power;
pub mod xinput;
