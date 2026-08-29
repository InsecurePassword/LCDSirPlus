//! AC and battery state from `GetSystemPowerStatus`.

#![cfg(windows)]
#![allow(dead_code)]

use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcState {
    Offline,
    Online,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct SystemPowerSample {
    pub ac: AcState,
    pub battery_present: Option<bool>,
    pub charging: bool,
    pub battery_percent: Option<u8>,
    pub sampled_at: std::time::SystemTime,
}

pub fn sample() -> Result<SystemPowerSample, String> {
    unsafe {
        let mut raw = SYSTEM_POWER_STATUS::default();
        GetSystemPowerStatus(&mut raw).map_err(|e| format!("GetSystemPowerStatus failed: {e}"))?;
        Ok(decode(raw, std::time::SystemTime::now()))
    }
}

fn decode(raw: SYSTEM_POWER_STATUS, sampled_at: std::time::SystemTime) -> SystemPowerSample {
    let battery_present = (raw.BatteryFlag != 255).then_some(raw.BatteryFlag & 128 == 0);
    SystemPowerSample {
        ac: match raw.ACLineStatus {
            0 => AcState::Offline,
            1 => AcState::Online,
            _ => AcState::Unknown,
        },
        battery_present,
        charging: battery_present == Some(true) && raw.BatteryFlag & 8 != 0,
        battery_percent: (battery_present != Some(false) && raw.BatteryLifePercent <= 100)
            .then_some(raw.BatteryLifePercent),
        sampled_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_distinguishes_unknown_zero_and_no_battery() {
        let at = std::time::SystemTime::UNIX_EPOCH;
        let zero = decode(
            SYSTEM_POWER_STATUS {
                ACLineStatus: 0,
                BatteryLifePercent: 0,
                ..Default::default()
            },
            at,
        );
        assert_eq!(zero.ac, AcState::Offline);
        assert_eq!(zero.battery_percent, Some(0));
        let none = decode(
            SYSTEM_POWER_STATUS {
                ACLineStatus: 255,
                BatteryFlag: 128,
                BatteryLifePercent: 255,
                ..Default::default()
            },
            at,
        );
        assert_eq!(none.ac, AcState::Unknown);
        assert_eq!(none.battery_present, Some(false));
        assert_eq!(none.battery_percent, None);

        let unknown_with_percent = decode(
            SYSTEM_POWER_STATUS {
                BatteryFlag: 255,
                BatteryLifePercent: 42,
                ..Default::default()
            },
            at,
        );
        assert_eq!(unknown_with_percent.battery_present, None);
        assert_eq!(unknown_with_percent.battery_percent, Some(42));
        assert!(!unknown_with_percent.charging);

        let unknown_without_percent = decode(
            SYSTEM_POWER_STATUS {
                BatteryFlag: 255,
                BatteryLifePercent: 255,
                ..Default::default()
            },
            at,
        );
        assert_eq!(unknown_without_percent.battery_present, None);
        assert_eq!(unknown_without_percent.battery_percent, None);
    }
}
