//! Leveled logging to stdout with size-capped, rotated file output.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

struct Logger {
    level: Level,
    file: Mutex<Option<(PathBuf, File, u64)>>,
    max_bytes: u64,
    backups: i32,
}

static LOGGER: std::sync::OnceLock<Logger> = std::sync::OnceLock::new();

/// Initialize the global logger. File output is optional (diagnostic dir);
/// rotation fires when the file exceeds `max_bytes`, keeping `backups`.
pub fn init(level: Level, file: Option<(PathBuf, u64, i32)>) {
    let (max_bytes, backups) = file
        .as_ref()
        .map(|(_, max, backups)| (*max, *backups))
        .unwrap_or((2 * 1024 * 1024, 3));
    let slot = file.and_then(|(path, _, _)| {
        let size = existing_len(&path);
        let handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some((path, handle, size))
    });
    let _ = LOGGER.set(Logger {
        level,
        file: Mutex::new(slot),
        max_bytes,
        backups,
    });
}

pub fn log(level: Level, message: &str) {
    let Some(logger) = LOGGER.get() else {
        if level >= Level::Info {
            println!("[{}] {}", level.name(), message);
        }
        return;
    };
    if level < logger.level {
        return;
    }
    let line = format!("{} [{}] {}\n", chrono_friendly_now(), level.name(), message);
    print!("{}", line);
    let _ = std::io::stdout().flush();
    let mut guard = logger.file.lock().unwrap();
    if let Some((path, file, size)) = guard.as_mut() {
        let _ = file.write_all(line.as_bytes());
        *size += line.len() as u64;
        if *size > logger.max_bytes {
            rotate(path, logger.backups);
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.as_path())
            {
                Ok(new_file) => {
                    *file = new_file;
                    *size = existing_len(path);
                }
                Err(_) => {
                    // Disable file logging rather than crash the app.
                    *guard = None;
                }
            }
        }
    }
}

fn rotate(path: &PathBuf, backups: i32) {
    // lcdforge.log -> lcdforge.log.1 -> ... -> lcdforge.log.N (dropped)
    for i in (1..backups).rev() {
        let from = path.with_extension(format!("log.{}", i));
        let to = path.with_extension(format!("log.{}", i + 1));
        if from.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    let first = path.with_extension("log.1");
    let _ = std::fs::rename(path, &first);
}

fn existing_len(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn chrono_friendly_now() -> String {
    // UTC timestamp without external crates.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = secs / 86400;
    let (y, mo, d) = civil_from_days(days as i64);
    let (h, mi, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, mi, s, millis
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logging::log($crate::logging::Level::Info, &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logging::log($crate::logging::Level::Warn, &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logging::log($crate::logging::Level::Error, &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => { $crate::logging::log($crate::logging::Level::Debug, &format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 2026-08-08 is day 20673 since epoch.
        assert_eq!(civil_from_days(20673), (2026, 8, 8));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
    }

    #[test]
    fn level_parsing() {
        assert_eq!(Level::parse("info"), Some(Level::Info));
        assert_eq!(Level::parse("bogus"), None);
        assert!(Level::Debug < Level::Error);
    }
}
