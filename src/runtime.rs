//! Windows runtime ownership and per-user startup registration.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CommitTransaction, CreateTransaction, RollbackTransaction, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyTransactedW, RegDeleteValueW, RegOpenKeyTransactedW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
    REG_OPEN_CREATE_OPTIONS, REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

const INSTANCE_NAME: &str = "Local\\LCDSirPlus.Runtime";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "LCDSirPlus";
const MAX_RUN_VALUE_BYTES: u32 = 8192;
const INSTALL_MARKER: &str = "lcdsirplus.layout";
const INSTALL_MARKER_CONTENT: &[u8] = b"installed-v1";
const DEFAULT_CONFIG: &str = "lcdsirplus.txt";
const INSTALLED_TEMPLATE: &str = "lcdsirplus.default.txt";
static SEED_NONCE: AtomicU64 = AtomicU64::new(0);

pub struct ResolvedConfig {
    pub path: PathBuf,
    pub installed: bool,
}

pub fn resolve_config(explicit: Option<PathBuf>) -> Result<ResolvedConfig, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("current executable path unavailable: {error}"))?;
    resolve_config_with(explicit, &executable, local_app_data)
}

fn resolve_config_with<F>(
    explicit: Option<PathBuf>,
    executable: &Path,
    local_app_data: F,
) -> Result<ResolvedConfig, String>
where
    F: FnOnce() -> Result<PathBuf, String>,
{
    let executable_dir = executable
        .parent()
        .ok_or("current executable has no parent directory")?;
    let marker = executable_dir.join(INSTALL_MARKER);
    let installed = match std::fs::symlink_metadata(&marker) {
        Ok(_) => {
            let bytes = read_regular_non_reparse(
                &marker,
                "installed layout marker",
                INSTALL_MARKER_CONTENT.len(),
            )?;
            if bytes != INSTALL_MARKER_CONTENT {
                return Err(format!(
                    "installed layout marker {} must contain exactly installed-v1",
                    marker.display()
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "installed layout marker {} could not be inspected: {error}",
                marker.display()
            ))
        }
    };

    if let Some(path) = explicit {
        return Ok(ResolvedConfig { path, installed });
    }
    if !installed {
        return Ok(ResolvedConfig {
            path: executable_dir.join(DEFAULT_CONFIG),
            installed: false,
        });
    }

    let config_dir = local_app_data()?.join("LCDSirPlus").join("Config");
    let path = config_dir.join(DEFAULT_CONFIG);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            return Ok(ResolvedConfig {
                path,
                installed: true,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "installed user configuration {} could not be inspected: {error}",
                path.display()
            ))
        }
    }

    let template = executable_dir.join(INSTALLED_TEMPLATE);
    let bytes = read_regular_non_reparse(
        &template,
        "installed configuration template",
        crate::config::MAX_CONFIG_AGGREGATE_BYTES,
    )?;
    crate::parser::parse_standalone(&bytes).map_err(|error| {
        format!(
            "installed configuration template {} is invalid: {error}",
            template.display()
        )
    })?;
    std::fs::create_dir_all(&config_dir).map_err(|error| {
        format!(
            "installed configuration directory {} could not be created: {error}",
            config_dir.display()
        )
    })?;
    seed_config(&path, &bytes)?;
    Ok(ResolvedConfig {
        path,
        installed: true,
    })
}

fn seed_config(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let nonce = SEED_NONCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".{DEFAULT_CONFIG}.seed-{}-{nonce}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            format!(
                "installed user configuration temporary file {} could not be created: {error}",
                temporary.display()
            )
        })?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        std::fs::remove_file(&temporary).ok();
        return Err(format!(
            "installed user configuration {} could not be written: {error}",
            path.display()
        ));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        std::fs::remove_file(&temporary).ok();
        return Err(format!(
            "installed user configuration {} could not be synchronized: {error}",
            path.display()
        ));
    }
    drop(file);
    if let Err(error) = move_no_replace(&temporary, path) {
        std::fs::remove_file(&temporary).ok();
        if std::fs::symlink_metadata(path).is_ok() {
            return Ok(());
        }
        return Err(format!(
            "installed user configuration {} could not be published: {error}",
            path.display()
        ));
    }
    Ok(())
}

fn move_no_replace(from: &Path, to: &Path) -> windows::core::Result<()> {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
}

fn local_app_data() -> Result<PathBuf, String> {
    let raw =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, HANDLE::default()) }
            .map_err(|error| format!("LocalAppData known folder unavailable: {error}"))?;
    let value = unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(|error| format!("LocalAppData known folder path is invalid: {error}"));
    unsafe { CoTaskMemFree(Some(raw.as_ptr().cast())) };
    value
}

fn read_regular_non_reparse(path: &Path, label: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{label} {} could not be inspected: {error}", path.display()))?;
    validate_regular_attributes(metadata.is_file(), metadata.file_attributes(), path, label)?;
    validate_file_size(metadata.len(), max_bytes, path, label)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(|error| format!("{label} {} could not be opened: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("{label} {} could not be inspected: {error}", path.display()))?;
    validate_regular_attributes(metadata.is_file(), metadata.file_attributes(), path, label)?;
    validate_file_size(metadata.len(), max_bytes, path, label)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{label} {} could not be read: {error}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "{label} {} exceeds byte limit of {max_bytes}",
            path.display(),
        ));
    }
    Ok(bytes)
}

fn validate_regular_attributes(
    is_file: bool,
    attributes: u32,
    path: &Path,
    label: &str,
) -> Result<(), String> {
    if !is_file || attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(format!(
            "{label} {} must be a regular non-reparse file",
            path.display()
        ));
    }
    Ok(())
}

fn validate_file_size(
    length: u64,
    max_bytes: usize,
    path: &Path,
    label: &str,
) -> Result<(), String> {
    if length > max_bytes as u64 {
        return Err(format!(
            "{label} {} exceeds byte limit of {max_bytes}",
            path.display()
        ));
    }
    Ok(())
}

pub struct InstanceGuard(HANDLE);

impl InstanceGuard {
    pub fn acquire() -> Result<Option<Self>, String> {
        Self::acquire_named(INSTANCE_NAME)
    }

    fn acquire_named(name: &str) -> Result<Option<Self>, String> {
        let name = wide(name);
        unsafe {
            windows::Win32::Foundation::SetLastError(ERROR_SUCCESS);
            let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()))
                .map_err(|_| "create single-instance mutex failed")?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                Ok(None)
            } else {
                Ok(Some(Self(handle)))
            }
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn canonical_executable() -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(
        std::env::current_exe().map_err(|_| "current executable path unavailable")?,
    )
    .map_err(|_| "canonical executable path unavailable")?;
    Ok(path
        .strip_prefix(r"\\?\")
        .ok()
        .filter(|rest| {
            rest.to_str()
                .is_some_and(|text| text.as_bytes().get(1) == Some(&b':'))
        })
        .unwrap_or(&path)
        .to_path_buf())
}

pub fn startup_command(executable: &Path) -> Result<String, String> {
    let text = executable
        .to_str()
        .filter(|text| !text.is_empty() && !text.contains('"'))
        .ok_or("startup executable path is not representable")?;
    Ok(format!("\"{text}\""))
}

fn startup_value_owned(value: &str, executable: &Path) -> bool {
    let Ok(command) = startup_command(executable) else {
        return false;
    };
    value.eq_ignore_ascii_case(&command)
}

pub fn sync_startup(enabled: bool, executable: &Path) -> Result<(), String> {
    let command = startup_command(executable)?;
    let key_name = wide(RUN_KEY);
    let value_name = wide(RUN_VALUE);
    unsafe {
        let transaction = CreateTransaction(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            0,
            0,
            5000,
            None,
        )
        .map_err(|_| "create startup registry transaction failed")?;
        let mut key = HKEY::default();
        let status = if enabled {
            RegCreateKeyTransactedW(
                HKEY_CURRENT_USER,
                PCWSTR(key_name.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPEN_CREATE_OPTIONS(0),
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut key,
                None,
                transaction,
                None,
            )
        } else {
            RegOpenKeyTransactedW(
                HKEY_CURRENT_USER,
                PCWSTR(key_name.as_ptr()),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut key,
                transaction,
                None,
            )
        };
        if !enabled && status == ERROR_FILE_NOT_FOUND {
            let _ = RollbackTransaction(transaction);
            let _ = CloseHandle(transaction);
            return Ok(());
        }
        if status.is_err() {
            let _ = RollbackTransaction(transaction);
            let _ = CloseHandle(transaction);
            return Err("open transactional HKCU Run key failed".into());
        }

        let result = (|| {
            let current = read_value(key, &value_name)?;
            if enabled {
                if current
                    .as_deref()
                    .is_some_and(|value| !startup_value_owned(value, executable))
                {
                    return Err(
                        "refusing to replace foreign HKCU Run value named LCDSirPlus".into(),
                    );
                }
                let encoded: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
                let bytes = std::slice::from_raw_parts(
                    encoded.as_ptr() as *const u8,
                    encoded.len() * std::mem::size_of::<u16>(),
                );
                RegSetValueExW(key, PCWSTR(value_name.as_ptr()), 0, REG_SZ, Some(bytes))
                    .ok()
                    .map_err(|_| "write HKCU Run value failed")?;
            } else if current
                .as_deref()
                .is_some_and(|value| startup_value_owned(value, executable))
            {
                let deleted = RegDeleteValueW(key, PCWSTR(value_name.as_ptr()));
                if deleted != ERROR_FILE_NOT_FOUND {
                    deleted
                        .ok()
                        .map_err(|_| "delete owned HKCU Run value failed")?;
                }
            }
            Ok(())
        })();
        let _ = RegCloseKey(key);
        if result.is_ok() {
            if CommitTransaction(transaction).is_err() {
                let _ = CloseHandle(transaction);
                return Err("commit startup registry transaction failed".into());
            }
        } else {
            let _ = RollbackTransaction(transaction);
        }
        let _ = CloseHandle(transaction);
        result
    }
}

unsafe fn read_value(key: HKEY, name: &[u16]) -> Result<Option<String>, String> {
    let mut value_type = Default::default();
    let mut size = 0u32;
    let status = RegQueryValueExW(
        key,
        PCWSTR(name.as_ptr()),
        None,
        Some(&mut value_type),
        None,
        Some(&mut size),
    );
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status.ok().map_err(|_| "query HKCU Run value failed")?;
    if value_type != REG_SZ || size == 0 || size > MAX_RUN_VALUE_BYTES || !size.is_multiple_of(2) {
        return Ok(Some(String::new()));
    }
    let mut bytes = vec![0u8; size as usize];
    RegQueryValueExW(
        key,
        PCWSTR(name.as_ptr()),
        None,
        Some(&mut value_type),
        Some(bytes.as_mut_ptr()),
        Some(&mut size),
    )
    .ok()
    .map_err(|_| "read HKCU Run value failed")?;
    let mut units: Vec<u16> = bytes[..size as usize]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    Ok(Some(String::from_utf16_lossy(&units)))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lcdsirplus-runtime-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn installed_executable(root: &Path) -> PathBuf {
        std::fs::write(root.join(INSTALL_MARKER), INSTALL_MARKER_CONTENT).unwrap();
        root.join("LCDSirPlus.exe")
    }

    #[test]
    fn explicit_config_wins_without_seeding_in_installed_layout() {
        let root = temp_dir("explicit");
        let executable = installed_executable(&root);
        let explicit = root.join("elsewhere.txt");
        let resolved = resolve_config_with(Some(explicit.clone()), &executable, || {
            panic!("explicit configuration must not query LocalAppData")
        })
        .unwrap();
        assert_eq!(resolved.path, explicit);
        assert!(resolved.installed);
        assert!(!root.join(INSTALLED_TEMPLATE).exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn missing_marker_is_portable_and_uses_adjacent_config() {
        let root = temp_dir("portable");
        let executable = root.join("LCDSirPlus.exe");
        let resolved = resolve_config_with(None, &executable, || {
            panic!("portable configuration must not query LocalAppData")
        })
        .unwrap();
        assert_eq!(resolved.path, root.join(DEFAULT_CONFIG));
        assert!(!resolved.installed);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn installed_default_seeds_exact_template_once() {
        let root = temp_dir("seed");
        let executable = installed_executable(&root);
        let template = b"preview_mode never\r\n# exact bytes\r\n";
        std::fs::write(root.join(INSTALLED_TEMPLATE), template).unwrap();
        let local = root.join("local");

        let resolved = resolve_config_with(None, &executable, || Ok(local.clone())).unwrap();
        assert!(resolved.installed);
        assert_eq!(
            resolved.path,
            local.join("LCDSirPlus").join("Config").join(DEFAULT_CONFIG)
        );
        assert_eq!(std::fs::read(&resolved.path).unwrap(), template);
        assert!(crate::app::validate_config(&resolved.path).ok);

        std::fs::write(&resolved.path, b"user-owned").unwrap();
        std::fs::write(root.join(INSTALLED_TEMPLATE), b"replacement").unwrap();
        let again = resolve_config_with(None, &executable, || Ok(local)).unwrap();
        assert_eq!(again.path, resolved.path);
        assert_eq!(std::fs::read(&again.path).unwrap(), b"user-owned");
        seed_config(&again.path, b"racing seed").unwrap();
        assert_eq!(std::fs::read(&again.path).unwrap(), b"user-owned");
        assert!(std::fs::read_dir(again.path.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".seed-")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn concurrent_first_seed_publishes_one_complete_candidate_without_residue() {
        let root = temp_dir("concurrent-seed");
        let destination = root.join(DEFAULT_CONFIG);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let candidates = [b"preview_scale 2\n".to_vec(), b"preview_scale 3\n".to_vec()];
        let threads: Vec<_> = candidates
            .iter()
            .cloned()
            .map(|candidate| {
                let barrier = barrier.clone();
                let destination = destination.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    seed_config(&destination, &candidate)
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        let published = std::fs::read(&destination).unwrap();
        assert!(candidates.contains(&published));
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".seed-")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn installed_layout_rejects_malformed_marker_and_missing_template() {
        let malformed = temp_dir("malformed-marker");
        std::fs::write(malformed.join(INSTALL_MARKER), b"installed-v1\r\n").unwrap();
        let error = resolve_config_with(None, &malformed.join("LCDSirPlus.exe"), || {
            Ok(malformed.join("local"))
        })
        .err()
        .unwrap();
        assert!(error.contains("exceeds byte limit"));
        std::fs::write(malformed.join(INSTALL_MARKER), b"installed-v").unwrap();
        let error = resolve_config_with(None, &malformed.join("LCDSirPlus.exe"), || {
            Ok(malformed.join("local"))
        })
        .err()
        .unwrap();
        assert!(error.contains("must contain exactly installed-v1"));

        let missing = temp_dir("missing-template");
        let executable = installed_executable(&missing);
        let error = resolve_config_with(None, &executable, || Ok(missing.join("local")))
            .err()
            .unwrap();
        assert!(error.contains("installed configuration template"));
        std::fs::remove_dir_all(malformed).ok();
        std::fs::remove_dir_all(missing).ok();
    }

    #[test]
    fn installed_template_is_bounded_standalone_and_valid_before_destination_creation() {
        for (name, contents, expected) in [
            (
                "malformed-template",
                b"preview_mode bogus\n".as_slice(),
                "is invalid",
            ),
            (
                "included-template",
                b"include local.txt\n".as_slice(),
                "does not permit include",
            ),
        ] {
            let root = temp_dir(name);
            let executable = installed_executable(&root);
            std::fs::write(root.join(INSTALLED_TEMPLATE), contents).unwrap();
            let local = root.join("local");
            let error = resolve_config_with(None, &executable, || Ok(local.clone()))
                .err()
                .unwrap();
            assert!(error.contains(expected), "{error}");
            assert!(!local.exists());
            std::fs::remove_dir_all(root).ok();
        }

        let root = temp_dir("oversized-template");
        let executable = installed_executable(&root);
        let template = std::fs::File::create(root.join(INSTALLED_TEMPLATE)).unwrap();
        template
            .set_len(crate::config::MAX_CONFIG_AGGREGATE_BYTES as u64 + 1)
            .unwrap();
        let local = root.join("local");
        let error = resolve_config_with(None, &executable, || Ok(local.clone()))
            .err()
            .unwrap();
        assert!(error.contains("exceeds byte limit"));
        assert!(!local.exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn installed_layout_rejects_non_regular_marker_and_template() {
        let marker_dir = temp_dir("marker-dir");
        std::fs::create_dir(marker_dir.join(INSTALL_MARKER)).unwrap();
        let error = resolve_config_with(None, &marker_dir.join("LCDSirPlus.exe"), || {
            Ok(marker_dir.join("local"))
        })
        .err()
        .unwrap();
        assert!(error.contains("regular non-reparse file"));

        let template_dir = temp_dir("template-dir");
        let executable = installed_executable(&template_dir);
        std::fs::create_dir(template_dir.join(INSTALLED_TEMPLATE)).unwrap();
        let error = resolve_config_with(None, &executable, || Ok(template_dir.join("local")))
            .err()
            .unwrap();
        assert!(error.contains("regular non-reparse file"));
        std::fs::remove_dir_all(marker_dir).ok();
        std::fs::remove_dir_all(template_dir).ok();
    }

    #[test]
    fn installed_layout_rejects_file_reparse_marker_and_template() {
        use std::os::windows::fs::symlink_file;

        let marker_root = temp_dir("marker-reparse");
        let marker_target = marker_root.join("marker-target");
        std::fs::write(&marker_target, INSTALL_MARKER_CONTENT).unwrap();
        match symlink_file(&marker_target, marker_root.join(INSTALL_MARKER)) {
            Ok(()) => {
                let error = resolve_config_with(None, &marker_root.join("LCDSirPlus.exe"), || {
                    Ok(marker_root.join("local"))
                })
                .err()
                .unwrap();
                assert!(error.contains("regular non-reparse file"));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                assert!(validate_regular_attributes(
                    true,
                    FILE_ATTRIBUTE_REPARSE_POINT.0,
                    Path::new(INSTALL_MARKER),
                    "installed layout marker",
                )
                .is_err());
            }
            Err(error) => panic!("file symlink setup failed unexpectedly: {error}"),
        }

        let template_root = temp_dir("template-reparse");
        let executable = installed_executable(&template_root);
        let template_target = template_root.join("template-target");
        std::fs::write(&template_target, b"preview_scale 2\n").unwrap();
        match symlink_file(&template_target, template_root.join(INSTALLED_TEMPLATE)) {
            Ok(()) => {
                let error =
                    resolve_config_with(None, &executable, || Ok(template_root.join("local")))
                        .err()
                        .unwrap();
                assert!(error.contains("regular non-reparse file"));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                assert!(validate_regular_attributes(
                    true,
                    FILE_ATTRIBUTE_REPARSE_POINT.0,
                    Path::new(INSTALLED_TEMPLATE),
                    "installed configuration template",
                )
                .is_err());
            }
            Err(error) => panic!("file symlink setup failed unexpectedly: {error}"),
        }
        std::fs::remove_dir_all(marker_root).ok();
        std::fs::remove_dir_all(template_root).ok();
    }

    #[test]
    fn regular_attribute_validation_rejects_mocked_handle_reparse_flag() {
        let error = validate_regular_attributes(
            true,
            FILE_ATTRIBUTE_REPARSE_POINT.0,
            Path::new("mocked-file"),
            "mocked handle",
        )
        .unwrap_err();
        assert!(error.contains("regular non-reparse file"));
    }

    #[test]
    fn installed_default_reports_injected_local_app_data_failure() {
        let root = temp_dir("known-folder");
        let executable = installed_executable(&root);
        let error =
            resolve_config_with(
                None,
                &executable,
                || Err("LocalAppData test failure".into()),
            )
            .err()
            .unwrap();
        assert_eq!(error, "LocalAppData test failure");
        assert!(!root.join("LCDSirPlus").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn startup_command_quotes_canonical_path_and_ownership_is_exact() {
        let path = Path::new(r"C:\Program Files\LCDSirPlus\LCDSirPlus.exe");
        assert_eq!(
            startup_command(path).unwrap(),
            r#""C:\Program Files\LCDSirPlus\LCDSirPlus.exe""#
        );
        assert!(startup_value_owned(
            r#""c:\program files\lcdsirplus\LCDSIRPLUS.EXE""#,
            path
        ));
        assert!(!startup_value_owned(r#""C:\Other\LCDSirPlus.exe""#, path));
        assert!(!startup_value_owned(
            r#"C:\Program Files\LCDSirPlus\LCDSirPlus.exe"#,
            path
        ));
    }

    #[test]
    fn named_mutex_rejects_second_owner_and_releases() {
        let name = format!("Local\\LCDSirPlus.Runtime.Test.{}", std::process::id());
        let first = InstanceGuard::acquire_named(&name)
            .unwrap()
            .expect("first owner");
        assert!(InstanceGuard::acquire_named(&name).unwrap().is_none());
        drop(first);
        assert!(InstanceGuard::acquire_named(&name).unwrap().is_some());
    }
}
