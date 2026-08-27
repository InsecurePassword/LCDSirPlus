//! Trusted Logitech LCD SDK transport. Native calls are confined to one
//! owner thread and every request owns its buffers so a timed-out call can be
//! quarantined without retaining borrowed Rust data.

#![cfg(windows)]

use std::collections::HashSet;
use std::ffi::{c_void, CString, OsStr};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use windows::core::{PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, VS_FIXEDFILEINFO,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};

const LCD_TYPE_MONO: i32 = 1;
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const PIXELS: usize = crate::model::WIDTH * crate::model::HEIGHT;
const EXPORTS: [&str; 6] = [
    "LogiLcdInit",
    "LogiLcdIsConnected",
    "LogiLcdIsButtonPressed",
    "LogiLcdMonoSetBackground",
    "LogiLcdUpdate",
    "LogiLcdShutdown",
];
static CIRCUIT_OPEN: AtomicBool = AtomicBool::new(false);

pub fn circuit_open() -> bool {
    CIRCUIT_OPEN.load(Ordering::Acquire)
}

#[derive(Clone, Debug)]
pub struct LCore {
    pub path: PathBuf,
    pub pid: u32,
}

pub fn lcore() -> Result<Option<LCore>, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("LCore discovery: {e}"))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if let Err(error) = Process32FirstW(snapshot, &mut entry) {
            let _ = CloseHandle(snapshot);
            return Err(format!("enumerate LCore.exe processes: {error}"));
        }
        let mut found = Vec::new();
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case("LCore.exe") {
                found.push(entry.th32ProcessID);
            }
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    let _ = CloseHandle(snapshot);
                    return Err(format!("enumerate LCore.exe processes: {error}"));
                }
                break;
            }
        }
        let _ = CloseHandle(snapshot);
        if found.len() > 1 {
            return Err("multiple LCore.exe processes make LCD ownership ambiguous".into());
        }
        let Some(pid) = found.first().copied() else {
            return Ok(None);
        };
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("open LCore.exe process {pid}: {e}"))?;
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result.map_err(|e| format!("query LCore.exe path: {e}"))?;
        let path = fs::canonicalize(PathBuf::from(String::from_utf16_lossy(
            &buffer[..length as usize],
        )))
        .map_err(|e| format!("canonicalize LCore.exe: {e}"))?;
        Ok(Some(LCore { path, pid }))
    }
}

pub fn discover_trusted_dll(owner: &LCore) -> Result<PathBuf, String> {
    if !owner
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n.eq_ignore_ascii_case("LCore.exe"))
    {
        return Err("trusted SDK discovery requires the running LCore.exe path".into());
    }
    let install = owner
        .path
        .parent()
        .ok_or("LCore.exe has no install directory")?;
    if !install
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n.eq_ignore_ascii_case("Logitech Gaming Software"))
    {
        return Err(format!(
            "LCore.exe is outside the canonical Logitech Gaming Software directory: {}",
            owner.path.display()
        ));
    }
    let expected = install.join(r"SDK\LCD\x64\LogitechLcd.dll");
    validate_candidate(&owner.path, &expected)
}

fn validate_candidate(lcore: &Path, candidate: &Path) -> Result<PathBuf, String> {
    validate_candidate_with(lcore, candidate, &WindowsTrust)
}

trait TrustProvider {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String>;
    fn roots(&self) -> Result<Vec<PathBuf>, String>;
    fn require_immutable(&self, path: &Path, roots: &[PathBuf]) -> Result<(), String>;
    fn signer(&self, path: &Path) -> Result<String, String>;
    fn version(&self, path: &Path) -> Result<[u16; 4], String>;
    fn metadata(&self, path: &Path) -> Result<(String, String), String>;
    fn pe(&self, path: &Path) -> Result<(), String>;
}

struct WindowsTrust;

impl TrustProvider for WindowsTrust {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
        fs::canonicalize(path)
            .map_err(|e| format!("canonical Logitech LCD SDK DLL is unavailable: {e}"))
    }
    fn roots(&self) -> Result<Vec<PathBuf>, String> {
        program_files_roots()
    }
    fn require_immutable(&self, path: &Path, roots: &[PathBuf]) -> Result<(), String> {
        reject_reparse_and_writable_chain(path, roots)
    }
    fn signer(&self, path: &Path) -> Result<String, String> {
        crate::providers::discord::verify_authenticode(path)
    }
    fn version(&self, path: &Path) -> Result<[u16; 4], String> {
        file_version(path)
    }
    fn metadata(&self, path: &Path) -> Result<(String, String), String> {
        Ok((
            version_string(path, "CompanyName")?,
            version_string(path, "ProductName")?,
        ))
    }
    fn pe(&self, path: &Path) -> Result<(), String> {
        validate_pe(path)
    }
}

fn validate_candidate_with(
    lcore: &Path,
    candidate: &Path,
    trust: &dyn TrustProvider,
) -> Result<PathBuf, String> {
    let roots = trust.roots()?;
    if !roots.iter().any(|root| path_within(candidate, root)) {
        return Err(format!(
            "SDK candidate is lexically outside Program Files: {}",
            candidate.display()
        ));
    }
    trust.require_immutable(candidate, &roots)?;
    let dll = trust.canonicalize(candidate)?;
    if !dll
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n.eq_ignore_ascii_case("LogitechLcd.dll"))
    {
        return Err("SDK candidate has the wrong filename".into());
    }
    if !roots.iter().any(|root| path_within(&dll, root)) {
        return Err(format!(
            "SDK candidate is outside Program Files: {}",
            dll.display()
        ));
    }
    trust.require_immutable(&dll, &roots)?;
    let signer = trust.signer(&dll)?;
    if !signer.to_ascii_lowercase().contains("logitech") {
        return Err(format!("SDK signer is not Logitech: {signer}"));
    }
    let lcore_signer = trust.signer(lcore)?;
    if !lcore_signer.to_ascii_lowercase().contains("logitech") {
        return Err(format!("LCore signer is not Logitech: {lcore_signer}"));
    }
    let exe_version = trust.version(lcore)?;
    let dll_version = trust.version(&dll)?;
    if exe_version[..2] != dll_version[..2] {
        return Err(format!(
            "LCore/SDK product versions are incompatible: {}.{} vs {}.{}",
            exe_version[0], exe_version[1], dll_version[0], dll_version[1]
        ));
    }
    for path in [lcore, dll.as_path()] {
        let (company, product) = trust.metadata(path)?;
        if !company.to_ascii_lowercase().contains("logitech")
            || !product.to_ascii_lowercase().contains("logitech")
        {
            return Err(format!(
                "Logitech product metadata is incompatible for {}: company={company:?} product={product:?}",
                path.display()
            ));
        }
    }
    trust.pe(&dll)?;
    Ok(dll)
}

fn program_files_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for folder in [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86] {
        let raw = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, HANDLE::default()) }
            .map_err(|e| format!("resolve authoritative Program Files root: {e}"))?;
        let value = unsafe { raw.to_string() }
            .map_err(|e| format!("decode authoritative Program Files root: {e}"));
        unsafe { CoTaskMemFree(Some(raw.as_ptr().cast())) };
        let root = fs::canonicalize(value?)
            .map_err(|e| format!("canonicalize authoritative Program Files root: {e}"))?;
        let text = root.to_string_lossy();
        let local = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if local.len() < 3 || local.as_bytes()[1] != b':' {
            return Err(format!(
                "Program Files root is not a local drive path: {text}"
            ));
        }
        let drive: Vec<u16> = format!("{}\\", &local[..2])
            .encode_utf16()
            .chain(Some(0))
            .collect();
        if unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) } != DRIVE_FIXED {
            return Err(format!(
                "Program Files root is not on a fixed drive: {text}"
            ));
        }
        if !roots.iter().any(|known| known == &root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        Err("authoritative Program Files roots are unavailable".into())
    } else {
        Ok(roots)
    }
}

fn path_within(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy().to_ascii_lowercase();
    let root = root
        .to_string_lossy()
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    path == root || path.starts_with(&(root + "\\"))
}

fn reject_reparse_and_writable_chain(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let root = roots
        .iter()
        .find(|root| path_within(path, root))
        .ok_or("SDK path escaped Program Files")?;
    let mut current = root.clone();
    let root_metadata = fs::symlink_metadata(&current)
        .map_err(|e| format!("inspect trusted root {}: {e}", current.display()))?;
    if root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || can_open_for_write(&current, true)?
    {
        return Err(format!(
            "Program Files root is not immutable: {}",
            current.display()
        ));
    }
    for component in path.components().skip(root.components().count()) {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|e| format!("inspect trusted path {}: {e}", current.display()))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(format!(
                "trusted SDK path contains a reparse point: {}",
                current.display()
            ));
        }
        if can_open_for_write(&current, metadata.is_dir())? {
            return Err(format!(
                "current token can modify trusted SDK path: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

fn can_open_for_write(path: &Path, directory: bool) -> Result<bool, String> {
    let wide = wide(path);
    let flags = if directory {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        Default::default()
    };
    match unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0),
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    } {
        Ok(handle) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            Ok(true)
        }
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => Ok(false),
        Err(error) => Err(format!("check write access to {}: {error}", path.display())),
    }
}

fn file_version(path: &Path) -> Result<[u16; 4], String> {
    let wide = wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return Err(format!(
                "version metadata is unavailable: {}",
                path.display()
            ));
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), 0, size, data.as_mut_ptr().cast())
            .map_err(|e| format!("read version metadata {}: {e}", path.display()))?;
        let query = [b'\\' as u16, 0];
        let mut value: *mut c_void = std::ptr::null_mut();
        let mut length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut value,
            &mut length,
        )
        .as_bool()
        {
            return Err(format!("query version metadata failed: {}", path.display()));
        }
        if value.is_null() || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32 {
            return Err(format!("invalid version metadata: {}", path.display()));
        }
        let info = &*(value.cast::<VS_FIXEDFILEINFO>());
        Ok([
            (info.dwFileVersionMS >> 16) as u16,
            info.dwFileVersionMS as u16,
            (info.dwFileVersionLS >> 16) as u16,
            info.dwFileVersionLS as u16,
        ])
    }
}

fn version_string(path: &Path, key: &str) -> Result<String, String> {
    let wide_path = wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide_path.as_ptr()), None);
        if size == 0 {
            return Err(format!(
                "version metadata is unavailable: {}",
                path.display()
            ));
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(
            PCWSTR(wide_path.as_ptr()),
            0,
            size,
            data.as_mut_ptr().cast(),
        )
        .map_err(|e| format!("read version metadata {}: {e}", path.display()))?;
        let translation_query: Vec<u16> = "\\VarFileInfo\\Translation"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut translation: *mut c_void = std::ptr::null_mut();
        let mut translation_length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(translation_query.as_ptr()),
            &mut translation,
            &mut translation_length,
        )
        .as_bool()
            || translation.is_null()
            || translation_length < 4
        {
            return Err(format!(
                "version translation is unavailable: {}",
                path.display()
            ));
        }
        let language = *(translation.cast::<u16>());
        let codepage = *(translation.cast::<u16>().add(1));
        let query: Vec<u16> = format!("\\StringFileInfo\\{language:04x}{codepage:04x}\\{key}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut value: *mut c_void = std::ptr::null_mut();
        let mut length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut value,
            &mut length,
        )
        .as_bool()
            || value.is_null()
            || length <= 1
            || length > 32768
        {
            return Err(format!("{key} metadata is unavailable: {}", path.display()));
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            value.cast::<u16>(),
            length as usize - 1,
        )))
    }
}

fn validate_pe(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("read SDK PE: {e}"))?;
    let u16_at = |at: usize| -> Option<u16> {
        Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
    };
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(0..2) != Some(b"MZ") {
        return Err("SDK DLL has no DOS header".into());
    }
    let pe = u32_at(0x3c).ok_or("truncated SDK DOS header")? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0") || u16_at(pe + 4) != Some(0x8664) {
        return Err("SDK DLL is not an AMD64 PE image".into());
    }
    let sections = u16_at(pe + 6).ok_or("truncated SDK PE header")? as usize;
    let optional_size = u16_at(pe + 20).ok_or("truncated SDK PE header")? as usize;
    let optional = pe + 24;
    if u16_at(optional) != Some(0x20b) {
        return Err("SDK DLL is not PE32+".into());
    }
    let export_rva = u32_at(optional + 112).ok_or("SDK export directory is absent")?;
    let section_table = optional + optional_size;
    let rva_offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|index| {
            let at = section_table + index * 40;
            let virtual_size = u32_at(at + 8)?;
            let virtual_address = u32_at(at + 12)?;
            let raw_size = u32_at(at + 16)?;
            let raw = u32_at(at + 20)?;
            let span = virtual_size.max(raw_size);
            (rva >= virtual_address && rva < virtual_address.checked_add(span)?)
                .then_some((raw + rva - virtual_address) as usize)
        })
    };
    let export = rva_offset(export_rva).ok_or("SDK export directory is invalid")?;
    let count = u32_at(export + 24).ok_or("truncated SDK export directory")? as usize;
    if count > 65536 {
        return Err("SDK export table is unreasonably large".into());
    }
    let names = rva_offset(u32_at(export + 32).ok_or("truncated SDK export directory")?)
        .ok_or("SDK export name table is invalid")?;
    let mut actual = HashSet::new();
    for index in 0..count {
        let name_rva = u32_at(names + index * 4).ok_or("truncated SDK export name table")?;
        let start = rva_offset(name_rva).ok_or("invalid SDK export name")?;
        let end = bytes[start..]
            .iter()
            .position(|b| *b == 0)
            .map(|n| start + n)
            .ok_or("unterminated SDK export name")?;
        if end - start <= 256 {
            actual.insert(String::from_utf8_lossy(&bytes[start..end]).into_owned());
        }
    }
    for required in EXPORTS {
        if !actual.contains(required) {
            return Err(format!("SDK DLL is missing export {required}"));
        }
    }
    Ok(())
}

type Init = unsafe extern "system" fn(*const u16, i32) -> u8;
type IsConnected = unsafe extern "system" fn(i32) -> u8;
type IsButton = unsafe extern "system" fn(i32) -> u8;
type SetBackground = unsafe extern "system" fn(*const u8) -> u8;
type Update = unsafe extern "system" fn();
type Shutdown = unsafe extern "system" fn();

trait Runtime {
    fn execute(&mut self, request: &Request) -> Reply;
    fn shutdown(&mut self);
}

struct Native {
    library: windows::Win32::Foundation::HMODULE,
    connected: IsConnected,
    button: IsButton,
    background: SetBackground,
    update: Update,
    shutdown_fn: Shutdown,
}

impl Native {
    unsafe fn load(path: &Path, friendly_name: &str) -> Result<Self, String> {
        let path_wide = wide(path);
        let library = LoadLibraryExW(
            PCWSTR(path_wide.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .map_err(|e| format!("secure LoadLibraryExW {}: {e}", path.display()))?;
        macro_rules! proc {
            ($name:literal, $ty:ty) => {{
                let name = CString::new($name).unwrap();
                let Some(address) = GetProcAddress(library, PCSTR(name.as_ptr().cast())) else {
                    let _ = FreeLibrary(library);
                    return Err(format!("SDK DLL lost required export {}", $name));
                };
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address)
            }};
        }
        let init: Init = proc!("LogiLcdInit", Init);
        let native = Native {
            library,
            connected: proc!("LogiLcdIsConnected", IsConnected),
            button: proc!("LogiLcdIsButtonPressed", IsButton),
            background: proc!("LogiLcdMonoSetBackground", SetBackground),
            update: proc!("LogiLcdUpdate", Update),
            shutdown_fn: proc!("LogiLcdShutdown", Shutdown),
        };
        let friendly: Vec<u16> = friendly_name.encode_utf16().chain(Some(0)).collect();
        if init(friendly.as_ptr(), LCD_TYPE_MONO) == 0 {
            let _ = FreeLibrary(native.library);
            return Err("LogiLcdInit returned false".into());
        }
        Ok(native)
    }
}

impl Runtime for Native {
    fn execute(&mut self, request: &Request) -> Reply {
        unsafe {
            match request {
                Request::Submit(frame) => {
                    if frame.len() != PIXELS || (self.background)(frame.as_ptr()) == 0 {
                        return Reply::Error("LogiLcdMonoSetBackground returned false".into());
                    }
                    (self.update)();
                    Reply::Unit
                }
                Request::Poll => {
                    (self.update)();
                    Reply::Poll {
                        connected: (self.connected)(LCD_TYPE_MONO) != 0,
                        buttons: [1, 2, 4, 8].map(|mask| (self.button)(mask) != 0),
                    }
                }
                Request::Close { blank } => {
                    if *blank {
                        let pixels = [0u8; PIXELS];
                        if (self.background)(pixels.as_ptr()) == 0 {
                            return Reply::Error(
                                "LogiLcdMonoSetBackground blank returned false".into(),
                            );
                        }
                        (self.update)();
                    }
                    Reply::Unit
                }
            }
        }
    }

    fn shutdown(&mut self) {
        unsafe {
            (self.shutdown_fn)();
            if !self.library.is_invalid() {
                let _ = FreeLibrary(self.library);
                self.library = Default::default();
            }
        }
    }
}

enum Request {
    Submit(Vec<u8>),
    Poll,
    Close { blank: bool },
}

enum Reply {
    Unit,
    Poll { connected: bool, buttons: [bool; 4] },
    Error(String),
}

struct Envelope {
    request: Request,
    reply: SyncSender<Reply>,
}

pub struct SdkDevice {
    requests: Option<SyncSender<Envelope>>,
}

impl SdkDevice {
    pub fn open(owner: &LCore, friendly_name: &str) -> Result<Self, String> {
        if CIRCUIT_OPEN.load(Ordering::Acquire) {
            return Err("Logitech SDK circuit is open after an unreturning native owner".into());
        }
        let path = discover_trusted_dll(owner)?;
        let expected_owner = owner.clone();
        let friendly_name = friendly_name.to_string();
        let (requests, receiver) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("lcdsirplus-sdk-owner".into())
            .spawn(move || unsafe {
                match lcore() {
                    Ok(Some(current))
                        if current.pid == expected_owner.pid
                            && current.path == expected_owner.path => {}
                    Ok(_) => {
                        let _ =
                            ready_tx.send(Err("LCore.exe identity changed before SDK load".into()));
                        return;
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                }
                match Native::load(&path, &friendly_name) {
                    Ok(native) => {
                        let _ = ready_tx.send(Ok(()));
                        owner_loop(Box::new(native), receiver);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|e| format!("spawn Logitech SDK owner: {e}"))?;
        match ready_rx.recv_timeout(CALL_TIMEOUT) {
            Ok(Ok(())) => {
                let device = Self {
                    requests: Some(requests),
                };
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match device.poll() {
                        Ok((true, _)) => return Ok(device),
                        Ok((false, _)) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(750));
                        }
                        Ok((false, _)) => {
                            return Err(
                                "Logitech SDK monochrome LCD did not connect within 5 seconds"
                                    .into(),
                            )
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                CIRCUIT_OPEN.store(true, Ordering::Release);
                Err(
                    "Logitech SDK initialization timed out; owner quarantined and circuit opened"
                        .into(),
                )
            }
        }
    }

    fn call(&self, request: Request) -> Result<Reply, String> {
        self.call_timeout(request, CALL_TIMEOUT)
    }

    fn call_timeout(&self, request: Request, timeout: Duration) -> Result<Reply, String> {
        let requests = self
            .requests
            .as_ref()
            .ok_or("Logitech SDK device is closed")?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout;
        let mut envelope = Envelope {
            request,
            reply: reply_tx,
        };
        loop {
            match requests.try_send(envelope) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    envelope = returned;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TrySendError::Full(_)) => {
                    CIRCUIT_OPEN.store(true, Ordering::Release);
                    return Err("Logitech SDK request dispatch timed out; circuit opened".into());
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err("Logitech SDK owner disconnected".into());
                }
            }
        }
        reply_rx.recv_timeout(timeout).map_err(|_| {
            CIRCUIT_OPEN.store(true, Ordering::Release);
            "Logitech SDK native call timed out; owner quarantined and circuit opened".to_string()
        })
    }

    pub fn submit(&self, pixels: Vec<u8>) -> Result<(), String> {
        match self.call(Request::Submit(pixels))? {
            Reply::Unit => Ok(()),
            Reply::Error(error) => Err(error),
            Reply::Poll { .. } => Err("unexpected Logitech SDK submit reply".into()),
        }
    }

    pub fn poll(&self) -> Result<(bool, [bool; 4]), String> {
        match self.call(Request::Poll)? {
            Reply::Poll { connected, buttons } => Ok((connected, buttons)),
            Reply::Error(error) => Err(error),
            Reply::Unit => Err("unexpected Logitech SDK poll reply".into()),
        }
    }

    pub fn close(&mut self, blank: bool) -> Result<(), String> {
        let reply = self.call(Request::Close { blank });
        self.requests.take();
        match reply? {
            Reply::Unit => Ok(()),
            Reply::Error(error) => Err(error),
            Reply::Poll { .. } => Err("unexpected Logitech SDK close reply".into()),
        }
    }
}

impl Drop for SdkDevice {
    fn drop(&mut self) {
        if self.requests.is_some() {
            let _ = self.close(true);
        }
    }
}

fn owner_loop(mut runtime: Box<dyn Runtime>, requests: Receiver<Envelope>) {
    let mut shut_down = false;
    while let Ok(envelope) = requests.recv() {
        let close = matches!(envelope.request, Request::Close { .. });
        let reply = runtime.execute(&envelope.request);
        if close {
            runtime.shutdown();
            shut_down = true;
            let _ = envelope.reply.send(reply);
            break;
        }
        let _ = envelope.reply.send(reply);
    }
    if !shut_down {
        runtime.shutdown();
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Fake(Arc<Mutex<Vec<&'static str>>>);
    impl Runtime for Fake {
        fn execute(&mut self, request: &Request) -> Reply {
            self.0.lock().unwrap().push(match request {
                Request::Submit(_) => "submit",
                Request::Poll => "poll",
                Request::Close { .. } => "close",
            });
            match request {
                Request::Submit(frame) if frame.is_empty() => Reply::Error("injected".into()),
                Request::Poll => Reply::Poll {
                    connected: true,
                    buttons: [true, false, false, true],
                },
                _ => Reply::Unit,
            }
        }
        fn shutdown(&mut self) {
            self.0.lock().unwrap().push("shutdown");
        }
    }

    struct Slow;
    impl Runtime for Slow {
        fn execute(&mut self, _request: &Request) -> Reply {
            std::thread::sleep(Duration::from_millis(30));
            Reply::Unit
        }
        fn shutdown(&mut self) {}
    }

    struct FakeTrust {
        signer: &'static str,
        immutable: bool,
        pe: bool,
        dll_version: [u16; 4],
    }

    impl Default for FakeTrust {
        fn default() -> Self {
            Self {
                signer: "Logitech Inc.",
                immutable: true,
                pe: true,
                dll_version: [9, 4, 0, 0],
            }
        }
    }

    impl TrustProvider for FakeTrust {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(path.to_path_buf())
        }
        fn roots(&self) -> Result<Vec<PathBuf>, String> {
            Ok(vec![PathBuf::from(r"C:\Program Files")])
        }
        fn require_immutable(&self, _path: &Path, _roots: &[PathBuf]) -> Result<(), String> {
            self.immutable
                .then_some(())
                .ok_or_else(|| "writable path".into())
        }
        fn signer(&self, _path: &Path) -> Result<String, String> {
            Ok(self.signer.into())
        }
        fn version(&self, path: &Path) -> Result<[u16; 4], String> {
            Ok(if path.extension().is_some_and(|value| value == "dll") {
                self.dll_version
            } else {
                [9, 4, 12, 0]
            })
        }
        fn metadata(&self, _path: &Path) -> Result<(String, String), String> {
            Ok(("Logitech Inc.".into(), "Logitech Gaming Software".into()))
        }
        fn pe(&self, _path: &Path) -> Result<(), String> {
            self.pe.then_some(()).ok_or_else(|| "bad PE".into())
        }
    }

    #[test]
    fn owner_serializes_submit_poll_close_and_shutdown() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel(1);
        let thread_calls = calls.clone();
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(thread_calls)), rx));
        for request in [
            Request::Submit(vec![0; PIXELS]),
            Request::Poll,
            Request::Close { blank: true },
        ] {
            let (reply, receive) = mpsc::sync_channel(1);
            tx.send(Envelope { request, reply }).unwrap();
            receive.recv().unwrap();
        }
        thread.join().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["submit", "poll", "close", "shutdown"]
        );
    }

    #[test]
    fn owner_propagates_native_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel(1);
        let thread_calls = calls.clone();
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(thread_calls)), rx));
        let (reply, receive) = mpsc::sync_channel(1);
        tx.send(Envelope {
            request: Request::Submit(Vec::new()),
            reply,
        })
        .unwrap();
        assert!(matches!(receive.recv().unwrap(), Reply::Error(error) if error == "injected"));
        drop(tx);
        thread.join().unwrap();
    }

    #[test]
    fn path_boundary_is_component_aware() {
        assert!(path_within(
            Path::new(r"C:\Program Files\Logitech\x"),
            Path::new(r"C:\Program Files")
        ));
        assert!(!path_within(
            Path::new(r"C:\Program FilesX\Logitech"),
            Path::new(r"C:\Program Files")
        ));
    }

    #[test]
    fn trust_pipeline_accepts_only_complete_fake_evidence() {
        let exe = Path::new(r"C:\Program Files\Logitech Gaming Software\LCore.exe");
        let dll =
            Path::new(r"C:\Program Files\Logitech Gaming Software\SDK\LCD\x64\LogitechLcd.dll");
        assert_eq!(
            validate_candidate_with(exe, dll, &FakeTrust::default()).unwrap(),
            dll
        );

        let mut trust = FakeTrust {
            signer: "Other Corp",
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            immutable: false,
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            pe: false,
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            dll_version: [8, 0, 0, 0],
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
    }

    #[test]
    fn unreturning_owner_opens_the_circuit_without_borrowed_data() {
        CIRCUIT_OPEN.store(false, Ordering::Release);
        let (requests, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || owner_loop(Box::new(Slow), receiver));
        let mut device = SdkDevice {
            requests: Some(requests),
        };
        let error = device
            .call_timeout(Request::Submit(vec![0; PIXELS]), Duration::from_millis(1))
            .err()
            .unwrap();
        assert!(error.contains("timed out") && circuit_open());
        device.requests.take();
        thread.join().unwrap();
        CIRCUIT_OPEN.store(false, Ordering::Release);
    }
}
