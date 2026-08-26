//! Windows runtime ownership and per-user startup registration.

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CommitTransaction, CreateTransaction, RollbackTransaction,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyTransactedW, RegDeleteValueW, RegOpenKeyTransactedW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
    REG_OPEN_CREATE_OPTIONS, REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;

const INSTANCE_NAME: &str = "Local\\LCDSirPlus.Runtime";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "LCDSirPlus";
const MAX_RUN_VALUE_BYTES: u32 = 8192;

pub struct InstanceGuard(HANDLE);

impl InstanceGuard {
    pub fn acquire() -> Result<Option<Self>, String> {
        let name = wide(INSTANCE_NAME);
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
        let first = InstanceGuard::acquire().unwrap().expect("first owner");
        assert!(InstanceGuard::acquire().unwrap().is_none());
        drop(first);
        assert!(InstanceGuard::acquire().unwrap().is_some());
    }
}
