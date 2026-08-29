//! Leveled logging to stdout with size-capped, rotated file output.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    next_slot: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RotationFault {
    None,
    Prepare,
    Publish,
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl RotatingFile {
    fn open(path: PathBuf, max_bytes: u64, backups: i32) -> io::Result<Self> {
        normalize_set(&path, max_bytes, backups)?;
        let size = existing_len(&path)?;
        let next_slot = next_backup_slot(&path, backups)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            size,
            max_bytes,
            backups,
            next_slot,
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
            if let Err(error) = self.rotate(RotationFault::None) {
                self.file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .ok();
                return Err(error);
            }
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "log file closed"))?
            .write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self, fault: RotationFault) -> io::Result<()> {
        self.file.take();
        let backup = backup_path(&self.path, self.next_slot);
        rotate_to(&self.path, &backup, self.max_bytes, fault)?;
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?,
        );
        self.size = 0;
        self.next_slot = self.next_slot % self.backups + 1;
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
        // Drop only this line; retaining the sink lets a transient filesystem
        // failure recover on the next independent log call without recursion.
        let _ = file.write(line.as_bytes());
    }
}

fn normalize_set(path: &Path, max_bytes: u64, backups: i32) -> io::Result<()> {
    let retained: Vec<_> = std::iter::once(path.to_path_buf())
        .chain((1..=backups).map(|index| backup_path(path, index)))
        .collect();
    let mut oversized = Vec::new();
    for candidate in &retained {
        match std::fs::metadata(candidate) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "log path is not a regular file",
                ));
            }
            Ok(metadata) if metadata.len() > max_bytes => {
                oversized.push(candidate.clone());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for candidate in oversized {
        let temporary = prepare_tail(&candidate, max_bytes)?;
        if let Err(error) = replace_file(&temporary, &candidate) {
            let _ = std::fs::remove_file(temporary);
            return Err(error);
        }
    }
    for index in backups + 1..=20 {
        let extra = backup_path(path, index);
        match std::fs::remove_file(extra) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn rotate_to(
    current: &Path,
    backup: &Path,
    max_bytes: u64,
    fault: RotationFault,
) -> io::Result<()> {
    if fault == RotationFault::Prepare {
        return Err(io::Error::other("injected rotation preparation failure"));
    }
    let temporary = prepare_tail(current, max_bytes)?;
    if fault == RotationFault::Publish {
        let _ = std::fs::remove_file(temporary);
        return Err(io::Error::other("injected rotation publication failure"));
    }
    if let Err(error) = replace_file(&temporary, backup) {
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn prepare_tail(source: &Path, max_bytes: u64) -> io::Result<PathBuf> {
    let temporary = source.with_extension(format!(
        "log.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        let mut input = File::open(source)?;
        let length = input.metadata()?.len();
        input.seek(SeekFrom::Start(length.saturating_sub(max_bytes)))?;
        io::copy(&mut input.take(max_bytes), &mut output)?;
        output.sync_all()
    })();
    if let Err(error) = result {
        drop(output);
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    drop(output);
    Ok(temporary)
}

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            windows::core::PCWSTR(from.as_ptr()),
            windows::core::PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

fn next_backup_slot(path: &Path, backups: i32) -> io::Result<i32> {
    let mut oldest = None;
    for index in 1..=backups {
        match std::fs::metadata(backup_path(path, index)) {
            Ok(metadata) => {
                let modified = metadata.modified()?;
                if oldest.is_none_or(|(_, time)| modified < time) {
                    oldest = Some((index, modified));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(index),
            Err(error) => return Err(error),
        }
    }
    Ok(oldest.map_or(1, |(index, _)| index))
}

fn backup_path(path: &Path, index: i32) -> PathBuf {
    path.with_extension(format!("log.{index}"))
}

fn existing_len(path: &Path) -> io::Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
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
            "lcdsirplus-logging-{name}-{}-{}",
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
        let path = dir.join("lcdsirplus.log");
        let mut file = RotatingFile::open(path.clone(), 12, 2).unwrap();
        for line in [b"one\n".as_slice(), b"two\n", b"three\n", b"four\n"] {
            file.write(line).unwrap();
        }
        file.write(b"x\n").unwrap();
        drop(file);
        assert!(std::fs::metadata(&path).unwrap().len() <= 12);
        assert!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len()
                <= 12
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"x\n");
        assert_eq!(
            std::fs::read(path.with_extension("log.1")).unwrap(),
            b"one\ntwo\n"
        );
        assert_eq!(
            std::fs::read(path.with_extension("log.2")).unwrap(),
            b"three\nfour\n"
        );
        assert!(!path.with_extension("log.3").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_writes_remain_serialized_and_bounded() {
        let dir = temp_dir("concurrent");
        let path = dir.join("lcdsirplus.log");
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
    fn initialization_caps_existing_files_and_removes_excess_backups() {
        let dir = temp_dir("initial-cap");
        let path = dir.join("lcdsirplus.log");
        std::fs::write(&path, b"0123456789ABCDEFGHIJ").unwrap();
        std::fs::write(path.with_extension("log.1"), b"abcdefghijklmnop").unwrap();
        std::fs::write(path.with_extension("log.2"), b"ABCDEFGHIJKLMNOP").unwrap();
        std::fs::write(path.with_extension("log.3"), b"retired").unwrap();
        drop(RotatingFile::open(path.clone(), 10, 2).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"ABCDEFGHIJ");
        assert_eq!(
            std::fs::read(path.with_extension("log.1")).unwrap(),
            b"ghijklmnop"
        );
        assert_eq!(
            std::fs::read(path.with_extension("log.2")).unwrap(),
            b"GHIJKLMNOP"
        );
        assert!(!path.with_extension("log.3").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotation_failures_preserve_the_bounded_set_and_clean_preparation() {
        for fault in [RotationFault::Prepare, RotationFault::Publish] {
            let dir = temp_dir(if fault == RotationFault::Prepare {
                "prepare-failure"
            } else {
                "publish-failure"
            });
            let path = dir.join("lcdsirplus.log");
            std::fs::write(&path, b"current\n").unwrap();
            std::fs::write(path.with_extension("log.1"), b"backup\n").unwrap();
            let mut file = RotatingFile::open(path.clone(), 8, 1).unwrap();
            assert!(file.rotate(fault).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"current\n");
            assert_eq!(
                std::fs::read(path.with_extension("log.1")).unwrap(),
                b"backup\n"
            );
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn transient_rotation_failure_recovers_on_the_next_write() {
        let dir = temp_dir("rotation-retry");
        let path = dir.join("lcdsirplus.log");
        let backup = path.with_extension("log.1");
        let mut file = RotatingFile::open(path.clone(), 8, 1).unwrap();
        file.write(b"current\n").unwrap();
        std::fs::create_dir(&backup).unwrap();
        assert!(file.write(b"next\n").is_err());
        assert!(file.file.is_some());
        std::fs::remove_dir(&backup).unwrap();
        file.write(b"next\n").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"next\n");
        assert_eq!(std::fs::read(&backup).unwrap(), b"current\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_log_paths_fail_without_panicking_or_overwriting() {
        let dir = temp_dir("failure");
        let parent_file = dir.join("not-a-directory");
        std::fs::write(&parent_file, b"sentinel").unwrap();
        assert!(RotatingFile::open(parent_file.join("lcdsirplus.log"), 8, 1).is_err());

        let path = dir.join("lcdsirplus.log");
        std::fs::write(&path, b"12345678").unwrap();
        std::fs::create_dir(path.with_extension("log.1")).unwrap();
        assert!(RotatingFile::open(path.clone(), 4, 1).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"12345678");
        let _ = std::fs::remove_dir_all(dir);
    }
}
