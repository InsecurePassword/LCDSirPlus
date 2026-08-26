//! Leveled logging to stdout with size-capped, rotated file output.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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
    file: Mutex<Option<RotatingFile>>,
}

struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    backups: i32,
}

impl RotatingFile {
    fn open(path: PathBuf, max_bytes: u64, backups: i32) -> io::Result<Self> {
        if existing_len(&path) > max_bytes {
            rotate(&path, backups)?;
        }
        let size = existing_len(&path);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            size,
            max_bytes,
            backups,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log line exceeds cap",
            ));
        }
        if self.size.saturating_add(bytes.len() as u64) > self.max_bytes {
            self.file.take();
            if let Err(error) = rotate(&self.path, self.backups) {
                self.file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .ok();
                return Err(error);
            }
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
            self.size = 0;
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "log file closed"))?
            .write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}

static LOGGER: std::sync::OnceLock<Logger> = std::sync::OnceLock::new();

/// Initialize the global logger. File output is optional; startup failures are
/// reported to the caller rather than silently discarding logs.
pub fn init(level: Level, file: Option<(PathBuf, u64, i32)>) -> Result<(), String> {
    let slot = file
        .map(|(path, max, backups)| RotatingFile::open(path, max, backups))
        .transpose()
        .map_err(|_| "log file could not be opened".to_string())?;
    LOGGER
        .set(Logger {
            level,
            file: Mutex::new(slot),
        })
        .map_err(|_| "logger is already initialized".to_string())
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
    let message: String = message.chars().take(4096).collect();
    let line = format!("{} [{}] {}\n", chrono_friendly_now(), level.name(), message);
    print!("{}", line);
    let _ = std::io::stdout().flush();
    let mut guard = logger
        .file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.as_mut() {
        if file.write(line.as_bytes()).is_err() {
            // A runtime filesystem failure drops this line and disables file
            // output; stdout logging remains available.
            *guard = None;
        }
    }
}

fn rotate(path: &Path, backups: i32) -> io::Result<()> {
    // lcdforge.log -> lcdforge.log.1 -> ... -> lcdforge.log.N (dropped)
    let oldest = path.with_extension(format!("log.{}", backups));
    if oldest.exists() {
        std::fs::remove_file(oldest)?;
    }
    for i in (1..backups).rev() {
        let from = path.with_extension(format!("log.{}", i));
        let to = path.with_extension(format!("log.{}", i + 1));
        if from.exists() {
            std::fs::rename(from, to)?;
        }
    }
    if path.exists() {
        std::fs::rename(path, path.with_extension("log.1"))?;
    }
    Ok(())
}

fn existing_len(path: &Path) -> u64 {
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
    use std::sync::{Arc, Mutex};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lcdforge-logging-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

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

    #[test]
    fn rotation_is_bounded_and_preserves_complete_lines() {
        let dir = temp_dir("rotation");
        let path = dir.join("lcdforge.log");
        let mut file = RotatingFile::open(path.clone(), 12, 2).unwrap();
        for line in [b"one\n".as_slice(), b"two\n", b"three\n", b"four\n"] {
            file.write(line).unwrap();
        }
        drop(file);
        assert!(std::fs::metadata(&path).unwrap().len() <= 12);
        assert!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len()
                <= 12
        );
        assert!(!path.with_extension("log.3").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_writes_remain_serialized_and_bounded() {
        let dir = temp_dir("concurrent");
        let path = dir.join("lcdforge.log");
        let file = Arc::new(Mutex::new(
            RotatingFile::open(path.clone(), 128, 2).unwrap(),
        ));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let file = Arc::clone(&file);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        file.lock().unwrap().write(b"complete-line\n").unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        drop(file);
        for suffix in ["log", "log.1", "log.2"] {
            let candidate = if suffix == "log" {
                path.clone()
            } else {
                path.with_extension(suffix)
            };
            if candidate.exists() {
                let bytes = std::fs::read(candidate).unwrap();
                assert!(bytes.len() <= 128);
                assert!(bytes.ends_with(b"\n"));
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn open_and_rotation_fail_without_panicking_or_overwriting() {
        let dir = temp_dir("failure");
        let parent_file = dir.join("not-a-directory");
        std::fs::write(&parent_file, b"sentinel").unwrap();
        assert!(RotatingFile::open(parent_file.join("lcdforge.log"), 8, 1).is_err());

        let path = dir.join("lcdforge.log");
        std::fs::write(&path, b"12345678").unwrap();
        std::fs::create_dir(path.with_extension("log.1")).unwrap();
        assert!(RotatingFile::open(path.clone(), 4, 1).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"12345678");
        let _ = std::fs::remove_dir_all(dir);
    }
}
