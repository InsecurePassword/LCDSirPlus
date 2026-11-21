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

/// WO-TEL-D3-B1 (L6): the date format arrives as the app's configured slice, so this core-path
/// call (3.33/s at the 300 ms `telemetry_interval`) no longer constructs a whole default
/// `Config` (108 pub fields, 27 `String`, 2 `Vec`, `[Vec<String>; 4]`, a `BTreeMap` = the ~52
/// heap blocks the census counted) to read one field. The app owns the `Config` it passes, so the
/// value cannot drift from the live config (the D2 wholesale-replace rule). Wording note: this doc
/// deliberately avoids the constructor token itself, because the L6 census below counts that
/// token in the production region (the registered prose-token class).
pub fn read(date_format: &str) -> ClockText {
    let st = unsafe { GetLocalTime() };
    ClockText {
        date: format_date(&st, date_format),
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

    /// WO-TEL-D3-B1 (L6): the core-path clock read takes the date format from its caller, so it
    /// builds no `Config` at all. Census: the production region of this file (file start to the
    /// `#[cfg(test)]` attribute) carries ZERO `Config::default` tokens, and `read` renders the
    /// SUPPLIED pattern — proved against the host clock, so the pin is not vacuous: a `read` that
    /// ignored its argument and rendered the shipped default format fails the year leg. The four
    /// shipped format tests above stay byte-identical and exact.
    #[test]
    fn read_uses_the_supplied_date_format_and_builds_no_config() {
        let source = include_str!("clock.rs").replace("\r\n", "\n");
        let prod = &source[..source
            .find("#[cfg(test)]")
            .expect("the tests attribute opens the test module")];
        println!(
            "D3B1-L6 harness-count: Config::default tokens in the clock production region = {}",
            prod.matches("Config::default").count()
        );
        assert_eq!(
            prod.matches("Config::default").count(),
            0,
            "L6: the per-call whole-Config construction (~52 heap blocks x 3.33/s) is gone from the core path"
        );
        assert!(
            prod.contains("pub fn read(date_format: &str) -> ClockText {"),
            "L6: the format arrives as the caller's slice"
        );
        assert!(
            prod.contains("date: format_date(&st, date_format),"),
            "L6: the supplied format is the one rendered"
        );
        let st = unsafe { GetLocalTime() };
        let year = st.wYear.to_string();
        let text = read("yyyy");
        assert_eq!(
            text.date, year,
            "L6: `read` renders the SUPPLIED pattern (a default-config format cannot equal a bare year)"
        );
        assert_eq!(
            text.date_short,
            format_date(&st, "yyyy-MM-dd ddd"),
            "L6: the fixed short date keeps the shipped pattern, untouched by the parameter"
        );
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
