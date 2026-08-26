//! Offline, typed diagnostics bundle with a closed privacy allowlist.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::model::Snapshot;

const MAX_BUNDLE_BYTES: usize = 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 128 * 1024;

struct Entry {
    name: &'static str,
    data: Vec<u8>,
}

struct DirectoryBoundary {
    root: PathBuf,
    pins: Vec<File>,
}

impl DirectoryBoundary {
    fn open(path: &Path) -> Result<Self, String> {
        use windows::Win32::Storage::FileSystem::GetDriveTypeW;
        use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;

        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| "diagnostic directory is unavailable")?
                .join(path)
        };
        if absolute
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err("diagnostic directory contains a parent traversal".into());
        }
        let text = absolute.to_string_lossy();
        let local = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if local.len() < 3 || local.as_bytes()[1] != b':' || local.as_bytes()[2] != b'\\' {
            return Err("diagnostic directory is not on a fixed local volume".into());
        }
        let drive: Vec<u16> = local[..3].encode_utf16().chain(Some(0)).collect();
        if unsafe { GetDriveTypeW(windows::core::PCWSTR(drive.as_ptr())) } != DRIVE_FIXED {
            return Err("diagnostic directory is not on a fixed local volume".into());
        }

        let mut pins = Vec::new();
        let mut current = PathBuf::new();
        for component in absolute.components() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            let pin = match open_directory_no_reparse(&current) {
                Ok(pin) => pin,
                Err(_) if !current.exists() => {
                    std::fs::create_dir(&current)
                        .map_err(|_| "diagnostic directory could not be created")?;
                    open_directory_no_reparse(&current)?
                }
                Err(error) => return Err(error),
            };
            pins.push(pin);
        }
        let root = std::fs::canonicalize(&absolute)
            .map_err(|_| "diagnostic directory identity is unavailable")?;
        Ok(Self { root, pins })
    }

    fn validate(&self) -> Result<(), String> {
        for pin in &self.pins {
            validate_directory_handle(pin)?;
        }
        Ok(())
    }
}

pub fn write_bundle(
    directory: &Path,
    config: &Config,
    snapshot: Option<&Snapshot>,
) -> Result<PathBuf, String> {
    let boundary = DirectoryBoundary::open(directory)?;
    let report = report(config, snapshot, &boundary.root);
    let privacy = b"LCDForge diagnostics privacy notice\n\nIncluded: application version, build architecture, validated feature states, bounded cadence values, fixed G13 contract, closed provider-health states when supplied, and sizes/counts for known rotated log files.\nExcluded: raw logs, configuration files, credentials, Discord identifiers or content, window titles, process or filesystem paths, network targets and addresses, environment variables, command lines, registry values, serial numbers, arbitrary directory listings, and device paths.\nCollection is local and offline. No HID, provider, Discord, network, or hung-action work is started. Stable identifier hashes are not used. Review the bundle before sharing it.\n".to_vec();
    let manifest = format!(
        "LCDForge diagnostics manifest v1\nprivacy.txt {} {}\nreport.txt {} {}\nmanifest.txt self-excluded\n",
        privacy.len(),
        crate::sha256::sha256_hex(&privacy),
        report.len(),
        crate::sha256::sha256_hex(report.as_bytes())
    )
    .into_bytes();
    let entries = [
        Entry {
            name: "privacy.txt",
            data: privacy,
        },
        Entry {
            name: "report.txt",
            data: report.into_bytes(),
        },
        Entry {
            name: "manifest.txt",
            data: manifest,
        },
    ];
    write_atomic(&boundary, &entries)
}

fn report(config: &Config, snapshot: Option<&Snapshot>, log_dir: &Path) -> String {
    fn known<'a>(value: &'a str, allowed: &[&str]) -> &'a str {
        if allowed.contains(&value) {
            value
        } else {
            "invalid"
        }
    }
    fn enabled(value: bool) -> &'static str {
        if value {
            "enabled"
        } else {
            "disabled"
        }
    }
    fn provider(snapshot: Option<&Snapshot>, name: &str) -> &'static str {
        match snapshot.and_then(|snapshot| snapshot.providers.get(name)) {
            Some(true) => "healthy",
            Some(false) => "degraded",
            None => "not-collected",
        }
    }

    let mut output = format!(
        "LCDForge diagnostics report v1\napp_version={}\nbuild_arch={}\nos_family={}\ncollection_mode=offline\nsafe_mode={}\npreview_mode={}\npreview_scale={}\nlogitech_backend={}\nlogitech_orientation={}\nlogitech_invert={}\nlhm_mode={}\ngpu_provider={}\npresentmon={}\nheadset={}\ncontroller={}\nnetwork_probe={}\nnetwork_probe_method={}\naudio={}\ndiscord={}\nhang_detection={}\ntelemetry_interval_ms={}\nrender_interval_ms={}\nlog_max_bytes={}\nlog_backups={}\ng13_geometry=160x43\ng13_vid=046d\ng13_pid=c21c\ng13_usage=ff00:0000\nprovider_cpu={}\nprovider_memory={}\nprovider_gpu={}\nprovider_lhm={}\nprovider_presentmon={}\nprovider_headset={}\nprovider_controller={}\nprovider_network={}\nprovider_audio={}\nprovider_discord={}\nprovider_hang={}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS,
        enabled(config.safe_mode),
        known(&config.preview_mode, &["auto", "always", "never"]),
        config.preview_scale.clamp(1, 8),
        known(&config.logitech_backend, &["auto", "hid", "virtual"]),
        known(&config.logitech_orientation, &["normal", "flip_x", "flip_y", "rotate_180"]),
        enabled(config.logitech_invert),
        known(&config.lhm_mode, &["auto", "on", "off"]),
        known(&config.gpu_provider, &["auto", "nvapi", "adlx", "off"]),
        enabled(config.presentmon_enabled),
        enabled(config.headset_enabled),
        enabled(config.controller_enabled),
        enabled(config.network_probe_enabled),
        known(&config.network_probe_method, &["auto", "icmp", "tcp"]),
        enabled(config.audio_enabled),
        enabled(config.discord_enabled),
        enabled(config.hang_enabled),
        config.telemetry_interval.as_millis().min(u64::MAX as u128),
        config.render_interval.as_millis().min(u64::MAX as u128),
        config.log_max_bytes.min(MAX_BUNDLE_BYTES as u64 * 64),
        config.log_backups.clamp(0, 32),
        provider(snapshot, "cpu"),
        provider(snapshot, "memory"),
        provider(snapshot, "gpu"),
        provider(snapshot, "lhm"),
        provider(snapshot, "presentmon"),
        provider(snapshot, "headset"),
        provider(snapshot, "controller"),
        provider(snapshot, "network"),
        provider(snapshot, "audio"),
        provider(snapshot, "discord"),
        provider(snapshot, "hang"),
    );
    let mut count = 0u32;
    let mut bytes = 0u64;
    let backups = config.log_backups.clamp(0, 32);
    for index in 0..=backups {
        let path = if index == 0 {
            log_dir.join("lcdforge.log")
        } else {
            log_dir.join(format!("lcdforge.log.{index}"))
        };
        if let Ok(file) = open_regular_read(&path) {
            if let Ok(metadata) = file.metadata() {
                count += 1;
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    output.push_str(&format!(
        "known_log_files={}\nknown_log_bytes={}\nstable_identifier_hashing=not-used\n",
        count,
        bytes.min(MAX_BUNDLE_BYTES as u64 * 64)
    ));
    output
}

fn write_atomic(boundary: &DirectoryBoundary, entries: &[Entry]) -> Result<PathBuf, String> {
    let nonce = random_nonce()?;
    let temporary = boundary
        .root
        .join(format!(".LCDForge-Diagnostics-{nonce}.tmp"));
    let final_path = boundary
        .root
        .join(format!("LCDForge-Diagnostics-{nonce}.zip"));
    if final_path.exists() {
        return Err("diagnostic output already exists".into());
    }
    let mut owns_temporary = false;
    let result = (|| {
        boundary.validate()?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| "diagnostic temporary file could not be created")?;
        owns_temporary = true;
        validate_regular_handle(&file)?;
        write_zip(&mut file, entries)?;
        file.sync_all()
            .map_err(|_| "diagnostic bundle could not be synchronized")?;
        validate_regular_handle(&file)?;
        boundary.validate()?;
        std::fs::rename(&temporary, &final_path)
            .map_err(|_| "diagnostic bundle could not be published")?;
        Ok(final_path.clone())
    })();
    if result.is_err() && owns_temporary {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn write_zip(output: &mut impl Write, entries: &[Entry]) -> Result<(), String> {
    let names: Vec<_> = entries.iter().map(|entry| entry.name).collect();
    if names != ["privacy.txt", "report.txt", "manifest.txt"] {
        return Err("diagnostic bundle entry schema is invalid".into());
    }
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        if !valid_entry_name(entry.name) || entry.data.len() > MAX_ENTRY_BYTES {
            return Err("diagnostic bundle entry is invalid or oversized".into());
        }
        let name = entry.name.as_bytes();
        let size = entry.data.len() as u32;
        let crc = crc32(&entry.data);
        let offset = archive.len() as u32;
        push_u32(&mut archive, 0x0403_4b50);
        push_u16(&mut archive, 20);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0x21);
        push_u32(&mut archive, crc);
        push_u32(&mut archive, size);
        push_u32(&mut archive, size);
        push_u16(&mut archive, name.len() as u16);
        push_u16(&mut archive, 0);
        archive.extend_from_slice(name);
        archive.extend_from_slice(&entry.data);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0x21);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let central_offset = archive.len() as u32;
    let central_size = central.len() as u32;
    archive.extend_from_slice(&central);
    push_u32(&mut archive, 0x0605_4b50);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, entries.len() as u16);
    push_u16(&mut archive, entries.len() as u16);
    push_u32(&mut archive, central_size);
    push_u32(&mut archive, central_offset);
    push_u16(&mut archive, 0);
    if archive.len() > MAX_BUNDLE_BYTES {
        return Err("diagnostic bundle exceeds its size cap".into());
    }
    output
        .write_all(&archive)
        .map_err(|_| "diagnostic bundle could not be written".to_string())
}

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains("..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.')
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn random_nonce() -> Result<String, String> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut bytes = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            &mut bytes,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status.0 < 0 {
        return Err("diagnostic nonce generation failed".into());
    }
    Ok(crate::sha256::hex(&bytes))
}

fn open_directory_no_reparse(path: &Path) -> Result<File, String> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|_| "diagnostic directory could not be pinned")?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    validate_directory_handle(&file)?;
    Ok(file)
}

fn validate_directory_handle(file: &File) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast()),
            &mut info,
        )
    }
    .map_err(|_| "diagnostic directory identity is unavailable")?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err("diagnostic directory boundary is not trusted".into());
    }
    Ok(())
}

fn open_regular_read(path: &Path) -> Result<File, String> {
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|_| "diagnostic file could not be opened")?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    validate_regular_handle(&file)?;
    Ok(file)
}

fn validate_regular_handle(file: &File) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast()),
            &mut info,
        )
    }
    .map_err(|_| "diagnostic file identity is unavailable")?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0
        || info.nNumberOfLinks != 1
    {
        return Err("diagnostic file must be regular, non-reparse, and single-linked".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lcdforge-diagnostics-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    fn stored_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut entries = Vec::new();
        let mut offset = 0;
        while bytes.get(offset..offset + 4) == Some(&0x0403_4b50u32.to_le_bytes()) {
            let crc = u32::from_le_bytes(bytes[offset + 14..offset + 18].try_into().unwrap());
            let size =
                u32::from_le_bytes(bytes[offset + 18..offset + 22].try_into().unwrap()) as usize;
            let name_len =
                u16::from_le_bytes(bytes[offset + 26..offset + 28].try_into().unwrap()) as usize;
            let extra_len =
                u16::from_le_bytes(bytes[offset + 28..offset + 30].try_into().unwrap()) as usize;
            let data_offset = offset + 30 + name_len + extra_len;
            let name =
                String::from_utf8(bytes[offset + 30..offset + 30 + name_len].to_vec()).unwrap();
            let data = bytes[data_offset..data_offset + size].to_vec();
            assert_eq!(crc32(&data), crc);
            entries.push((name, data));
            offset = data_offset + size;
        }
        entries
    }

    #[test]
    fn bundle_is_bounded_valid_and_excludes_adversarial_private_data() {
        let secret = "PRIVATE-SENTINEL-user@example.test-C:\\Users\\Secret-203.0.113.9";
        let dir = temp_dir("privacy");
        std::fs::write(dir.join("lcdforge.log"), format!("error: {secret}")).unwrap();
        let config = Config {
            path: secret.into(),
            logitech_friendly_name: secret.into(),
            lhm_url: secret.into(),
            presentmon_path: secret.into(),
            presentmon_process_name: secret.into(),
            network_probe_target: secret.into(),
            discord_client_id: secret.into(),
            discord_redirect_uri: secret.into(),
            preview_mode: secret.into(),
            ..Default::default()
        };
        let mut snapshot = Snapshot {
            date_text: secret.into(),
            game: crate::model::GameStats {
                process_name: secret.into(),
                game_name: secret.into(),
                ..Default::default()
            },
            discord: crate::model::DiscordState {
                channel_id: secret.into(),
                channel_name: secret.into(),
                error: secret.into(),
                ..Default::default()
            },
            alerts: vec![crate::model::Alert {
                title: secret.into(),
                detail: secret.into(),
                ..Default::default()
            }],
            hung: vec![crate::model::HungTarget {
                image_path: secret.into(),
                process_name: secret.into(),
                title: secret.into(),
                ..Default::default()
            }],
            readings: crate::model::ReadingsSnapshot {
                metrics: vec![crate::model::Reading {
                    hardware_id: secret.into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        snapshot.providers.insert(secret.into(), true);

        let path = write_bundle(&dir, &config, Some(&snapshot)).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() <= MAX_BUNDLE_BYTES);
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
        let entries = stored_entries(&bytes);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.0.as_str())
                .collect::<Vec<_>>(),
            ["privacy.txt", "report.txt", "manifest.txt"]
        );
        for (_, data) in &entries {
            assert!(!String::from_utf8_lossy(data).contains(secret));
            assert!(data.len() <= MAX_ENTRY_BYTES);
        }
        let manifest = String::from_utf8(entries[2].1.clone()).unwrap();
        assert!(manifest.contains(&crate::sha256::sha256_hex(&entries[0].1)));
        assert!(manifest.contains(&crate::sha256::sha256_hex(&entries[1].1)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn zip_rejects_unsafe_names_and_oversized_members() {
        let mut output = Vec::new();
        assert!(write_zip(
            &mut output,
            &[Entry {
                name: "../escape",
                data: Vec::new(),
            }]
        )
        .is_err());
        assert!(output.is_empty());
        assert!(write_zip(
            &mut output,
            &[Entry {
                name: "report.txt",
                data: vec![0; MAX_ENTRY_BYTES + 1],
            }]
        )
        .is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn failed_atomic_write_leaves_no_partial_artifact() {
        let dir = temp_dir("atomic-failure");
        let boundary = DirectoryBoundary::open(&dir).unwrap();
        let invalid = [Entry {
            name: "report.txt",
            data: vec![0; MAX_ENTRY_BYTES + 1],
        }];
        assert!(write_atomic(&boundary, &invalid).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        drop(boundary);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn regular_file_validation_rejects_hard_links() {
        let dir = temp_dir("hardlink");
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::write(&first, b"sentinel").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        let file = File::open(&first).unwrap();
        assert!(validate_regular_handle(&file).is_err());
        assert_eq!(std::fs::read(&second).unwrap(), b"sentinel");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_boundary_rejects_reparse_output() {
        let dir = temp_dir("reparse");
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::create_dir(&target).unwrap();
        if std::os::windows::fs::symlink_dir(&target, &link).is_ok() {
            assert!(DirectoryBoundary::open(&link).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
