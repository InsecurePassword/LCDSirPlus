//! XInput controller battery via dynamically loaded system DLLs (no import
//! dependency).

#![cfg(windows)]

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use std::sync::OnceLock;

const BATTERY_TYPE_DISCONNECTED: u8 = 0x00;
const BATTERY_TYPE_WIRED: u8 = 0x01;
const BATTERY_LEVEL_EMPTY: u8 = 0x00;

#[derive(Clone, Copy, Debug, Default)]
pub struct XInputControllerBattery {
    pub wired: bool,
    pub percent: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XInputBatteryInformation {
    battery_type: u8,
    battery_level: u8,
}

type XInputGetBatteryInformationFn = unsafe extern "system" fn(
    user_index: u32,
    device_type: u8,
    battery: *mut XInputBatteryInformation,
) -> u32;

const BATTERY_DEVICE_TYPE_GAMEPAD: u8 = 0x00;

fn xinput_battery_fn() -> Option<XInputGetBatteryInformationFn> {
    static FUNCTION: OnceLock<Option<XInputGetBatteryInformationFn>> = OnceLock::new();
    *FUNCTION.get_or_init(|| unsafe {
        for dll in [
            w!("xinput1_4.dll"),
            w!("xinput1_3.dll"),
            w!("xinput9_1_0.dll"),
        ] {
            let Ok(module) = LoadLibraryW(dll) else {
                continue;
            };
            if let Some(p) = GetProcAddress(module, s!("XInputGetBatteryInformation")) {
                return Some(std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    XInputGetBatteryInformationFn,
                >(p));
            }
        }
        None
    })
}

/// Query one controller (0..4). `Ok(None)` = not connected.
pub fn query_index(index: u32) -> Result<Option<XInputControllerBattery>, String> {
    let Some(f) = xinput_battery_fn() else {
        return Err("XInputGetBatteryInformation is unavailable".into());
    };
    let mut battery = XInputBatteryInformation::default();
    let status = unsafe { f(index, BATTERY_DEVICE_TYPE_GAMEPAD, &mut battery) };
    if status != 0 {
        // ERROR_NOT_CONNECTED (1163) and ERROR_DEVICE_NOT_CONNECTED are the
        // documented "no controller" outcomes.
        return Ok(None);
    }
    if battery.battery_type == BATTERY_TYPE_DISCONNECTED {
        return Ok(None);
    }
    if battery.battery_type == BATTERY_TYPE_WIRED {
        return Ok(Some(XInputControllerBattery {
            wired: true,
            percent: 0,
        }));
    }
    // Levels: Empty, Low, Medium, Full -> 0/33/66/100.
    let percent = match battery.battery_level {
        BATTERY_LEVEL_EMPTY => 0,
        0x01 => 33,
        0x02 => 66,
        _ => 100,
    };
    Ok(Some(XInputControllerBattery {
        wired: false,
        percent,
    }))
}

/// First connected controller, or the explicit index when >= 0.
pub fn query(preferred: i32) -> Result<Option<(u32, XInputControllerBattery)>, String> {
    if preferred >= 0 {
        return query_index(preferred as u32).map(|b| b.map(|b| (preferred as u32, b)));
    }
    for index in 0..4u32 {
        if let Some(b) = query_index(index)? {
            return Ok(Some((index, b)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_does_not_panic_without_controller() {
        // On machines with no controller this returns Ok(None); with one it
        // returns battery state. Either way it must not error.
        let result = query(-1);
        assert!(result.is_ok(), "xinput query failed: {:?}", result.err());
    }
}
