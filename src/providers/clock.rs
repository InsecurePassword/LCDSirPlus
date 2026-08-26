//! Windows-local date/weekday/time text via Win32 national language support.

#![cfg(windows)]

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{GetDateFormatW, GetTimeFormatW};
use windows::Win32::System::SystemInformation::GetLocalTime;

const LOCALE_USER_DEFAULT: u32 = 0x0400;

pub struct ClockText {
    pub date: String,
    pub date_short: String,
    pub time: String,
}

pub fn read() -> ClockText {
    let defaults = crate::config::Config::default();
    let st = unsafe { GetLocalTime() };
    ClockText {
        date: format_date(&st, &defaults.date_format),
        date_short: format_date(&st, "yyyy-MM-dd ddd"),
        time: format_time(&st, &defaults.time_format),
    }
}

pub fn format_date(st: &SYSTEMTIME, format: &str) -> String {
    let wide: Vec<u16> = format.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0u16; 256];
    let len = unsafe {
        GetDateFormatW(
            LOCALE_USER_DEFAULT,
            0,
            Some(st),
            windows::core::PCWSTR::from_raw(wide.as_ptr()),
            Some(&mut buffer),
        )
    };
    wide_to_string(&buffer, len)
}

pub fn format_time(st: &SYSTEMTIME, format: &str) -> String {
    let wide: Vec<u16> = format.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0u16; 256];
    let len = unsafe {
        GetTimeFormatW(
            LOCALE_USER_DEFAULT,
            0,
            Some(st),
            windows::core::PCWSTR::from_raw(wide.as_ptr()),
            Some(&mut buffer),
        )
    };
    wide_to_string(&buffer, len)
}

fn wide_to_string(buffer: &[u16], len: i32) -> String {
    // GetDateFormatW/GetTimeFormatW return the length INCLUDING the null.
    if len <= 1 {
        return String::new();
    }
    let len = ((len - 1) as usize).min(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_date_format_renders() {
        let st = SYSTEMTIME {
            wYear: 2026,
            wMonth: 8,
            wDayOfWeek: 6, // Saturday
            wDay: 8,
            wHour: 10,
            wMinute: 44,
            wSecond: 14,
            wMilliseconds: 0,
        };
        assert_eq!(format_date(&st, "yyyy-MM-dd dddd"), "2026-08-08 Saturday");
        assert_eq!(format_date(&st, "yyyy-MM-dd ddd"), "2026-08-08 Sat");
        assert_eq!(format_time(&st, "HH:mm:ss"), "10:44:14");
    }
}
