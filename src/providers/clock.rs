//! Windows-local date/weekday/time text via Win32 national language support.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    GetDateFormatW, GetLocaleInfoEx, GetTimeFormatEx, LOCALE_SSHORTTIME, TIME_FORMAT_FLAGS,
    TIME_NOSECONDS,
};
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
        time: format_time(&st),
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

pub fn format_time(st: &SYSTEMTIME) -> String {
    let mut pattern = [0u16; 128];
    let pattern_len =
        unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_SSHORTTIME, Some(&mut pattern)) };

    format_time_from(
        st,
        (pattern_len > 1).then_some(pattern.as_slice()),
        |pattern| format_time_with_pattern(st, PCWSTR::null(), pattern),
        || format_time_ex(st, PCWSTR::null(), TIME_NOSECONDS, PCWSTR::null()),
    )
}

fn format_time_from(
    st: &SYSTEMTIME,
    pattern: Option<&[u16]>,
    format_pattern: impl FnOnce(&[u16]) -> String,
    format_fallback: impl FnOnce() -> String,
) -> String {
    if let Some(pattern) = pattern {
        let formatted = format_pattern(pattern);
        if !formatted.trim().is_empty() {
            return formatted;
        }
    }

    nonblank_or_manual(st, format_fallback())
}

fn format_time_with_pattern(st: &SYSTEMTIME, locale: PCWSTR, pattern: &[u16]) -> String {
    format_time_ex(
        st,
        locale,
        TIME_FORMAT_FLAGS(0),
        PCWSTR::from_raw(pattern.as_ptr()),
    )
}

fn format_time_ex(
    st: &SYSTEMTIME,
    locale: PCWSTR,
    flags: TIME_FORMAT_FLAGS,
    pattern: PCWSTR,
) -> String {
    let mut buffer = [0u16; 256];
    let len = unsafe { GetTimeFormatEx(locale, flags, Some(st), pattern, Some(&mut buffer)) };
    wide_to_string(&buffer, len)
}

fn nonblank_or_manual(st: &SYSTEMTIME, formatted: String) -> String {
    if formatted.trim().is_empty() {
        format!("{:02}:{:02}", st.wHour, st.wMinute)
    } else {
        formatted
    }
}

fn wide_to_string(buffer: &[u16], len: i32) -> String {
    // Win32 date/time formatters return the length INCLUDING the null.
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
    }

    #[test]
    fn explicit_short_time_patterns_render_12h_and_24h() {
        let st = SYSTEMTIME {
            wHour: 14,
            wMinute: 25,
            ..Default::default()
        };
        let locale: Vec<u16> = "en-US\0".encode_utf16().collect();
        let twelve_hour: Vec<u16> = "h:mm tt\0".encode_utf16().collect();
        let twenty_four_hour: Vec<u16> = "HH:mm\0".encode_utf16().collect();
        let locale = PCWSTR::from_raw(locale.as_ptr());

        assert_eq!(
            format_time_with_pattern(&st, locale, &twelve_hour),
            "2:25 PM"
        );
        assert_eq!(
            format_time_with_pattern(&st, locale, &twenty_four_hour),
            "14:25"
        );
    }

    #[test]
    fn short_pattern_lookup_failure_routes_to_native_fallback() {
        let st = SYSTEMTIME::default();

        assert_eq!(
            format_time_from(
                &st,
                None,
                |_| panic!("explicit formatter must not run without a pattern"),
                || "native fallback".to_owned(),
            ),
            "native fallback"
        );
    }

    #[test]
    fn explicit_and_fallback_failures_route_to_manual_time() {
        let st = SYSTEMTIME {
            wHour: 4,
            wMinute: 7,
            ..Default::default()
        };
        let pattern = [0u16];
        let calls = std::cell::RefCell::new(Vec::new());

        assert_eq!(
            format_time_from(
                &st,
                Some(&pattern),
                |_| {
                    calls.borrow_mut().push("explicit");
                    String::new()
                },
                || {
                    calls.borrow_mut().push("fallback");
                    String::new()
                },
            ),
            "04:07"
        );
        assert_eq!(*calls.borrow(), ["explicit", "fallback"]);
    }
}
