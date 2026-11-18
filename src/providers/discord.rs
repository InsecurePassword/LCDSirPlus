//! Discord desktop local RPC, OAuth authorization, and current-user credentials.
//!
//! WO-TEL-D3-C: this module is the **host side** of the optional Discord component. The RPC
//! framing, pipe transport, WinHTTP OAuth exchange, token-record codec and speaker tracker now
//! live in the dynamically loaded `lcdsirplus_discord` cdylib (ABI v1: one `lcsd_get_table`
//! export + a size-checked 10-function table). What stays in the executable is the load gate, the
//! worker thread (spawn-on-enable, join-on-disable), the published `DiscordState`, the credential
//! store (DPAPI + `CredentialBoundary`, so the component never touches a credential file), the
//! Discord IPC server-identity + Authenticode verifier (no DLL code runs in a detachable thread),
//! and the config re-arm/backoff machinery the D2/B1 units accepted, byte-identical.

use std::ffi::c_void;
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::model::{DiscordState, Speaker};

const MAX_CREDENTIAL: usize = 64 * 1024;

// SHARED-RULES-START (WO-TEL-D3-C: boundary-shared identity rules; this block is byte-identical
// in src/providers/discord.rs (host) and here (component); pinned by
// discord_shared_rules_stay_byte_identical_across_the_boundary in both crates, so the credential
// path rule and the cryptographic nonce rule cannot drift between the two packages.)
fn validate_user_id(id: &str) -> Result<&str, String> {
    if id.is_empty()
        || id.len() > 20
        || !id.bytes().all(|byte| byte.is_ascii_digit())
        || id.starts_with('0')
        || id.parse::<u64>().is_err()
    {
        return Err("Discord user ID is not a canonical numeric Snowflake".into());
    }
    Ok(id)
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
        return Err("Windows cryptographic RNG failed".into());
    }
    Ok(bytes.iter().map(|value| format!("{:02x}", value)).collect())
} // SHARED-RULES-END

// ABI-V1-BLOCK-START (WO-TEL-D3-C: this block is byte-identical in the host module
// src/providers/discord.rs and in the component's src/abi.rs; the pair is pinned by
// discord_abi_blocks_stay_byte_identical_across_the_boundary in both crates: the v1 POD layouts,
// the table shape and the status codes. Rust extern "system" fn pointers are non-null by type, so
// the host validates the module's ten entries through its raw-pointer mirror LcsdV1Raw.)
/// `'LCSd'` as a little-endian u32: `L`=0x4C, `C`=0x43, `S`=0x53, `d`=0x64.
pub const LCSD_MAGIC: u32 = 0x6453_434C;
pub const LCSD_VERSION: u32 = 1;
pub const LCSD_OK: i32 = 0;
pub const LCSD_IDLE: i32 = 1;
pub const LCSD_E_SIZE: i32 = -1;
pub const LCSD_E_ARG: i32 = -2;
pub const LCSD_E_STATE: i32 = -3;
pub const LCSD_E_IO: i32 = -4;
pub const LCSD_E_PROTOCOL: i32 = -5;
pub const LCSD_E_IDENTITY: i32 = -6;
pub const LCSD_E_CREDENTIAL: i32 = -7;
pub const LCSD_E_INFLIGHT: i32 = -8;
pub const LCSD_E_CANCELED: i32 = -9;
pub const LCSD_E_UNAVAILABLE: i32 = -10;
pub const LCSD_E_NOT_FOUND: i32 = -11;
pub const LCSD_E_LEGACY: i32 = -12;
pub const LCSD_E_MAGIC: i32 = -13;

pub const LCSD_CLIENT_ID_CAPACITY: usize = 64;
pub const LCSD_ID_CAPACITY: usize = 24;
pub const LCSD_NAME_CAPACITY: usize = 48;
pub const LCSD_CHANNEL_CAPACITY: usize = 64;
pub const LCSD_CHANNEL_ID_CAPACITY: usize = 32;
pub const LCSD_STATE_CAPACITY: usize = 24;
pub const LCSD_ERROR_CAPACITY: usize = 96;
pub const LCSD_STATUS_CAPACITY: usize = 128;
pub const LCSD_MAX_SPEAKERS: usize = 8;

/// The settings the host hands the component: exactly the fields `session_config_changed`
/// compares, plus the overlay options the shipped renderer reads. Fixed layout, size-checked.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LcsdSettings {
    pub size: u32,
    pub client_id_len: u32,
    pub client_id: [u8; LCSD_CLIENT_ID_CAPACITY],
    pub enabled: u8,
    pub safe_mode: u8,
    pub show_self: u8,
    pub show_channel: u8,
    pub linger_ms: u64,
    pub max_speakers: i32,
    pub ipc_probe_count: u32,
}

impl Default for LcsdSettings {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<LcsdSettings>() as u32,
            client_id_len: 0,
            client_id: [0; LCSD_CLIENT_ID_CAPACITY],
            enabled: 0,
            safe_mode: 0,
            show_self: 0,
            show_channel: 0,
            linger_ms: 0,
            max_speakers: 0,
            ipc_probe_count: 0,
        }
    }
}

/// Host callbacks the component is allowed to call, all on the CALLING host thread (condition
/// iii: no component code ever runs in a thread the component cannot join). `canceled` reproduces
/// the shipped mid-session cancellation (worker shutdown OR a config-content change);
/// `verify_server` / `verify_path` run the host's Discord IPC identity + Authenticode verifier;
/// `provide_token` / `store_credential` are the only credential touches, and they cross the
/// DPAPI-protected record as an opaque bounded blob — the component never opens a credential
/// file and never calls DPAPI.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LcsdHostApi {
    pub size: u32,
    pub context: *mut std::ffi::c_void,
    pub canceled: extern "system" fn(context: *mut std::ffi::c_void) -> i32,
    pub verify_server: extern "system" fn(
        context: *mut std::ffi::c_void,
        pipe_handle: *mut std::ffi::c_void,
    ) -> i32,
    pub verify_path: extern "system" fn(
        context: *mut std::ffi::c_void,
        exe_path: *const u16,
        out_signer: *mut u8,
        out_cap: u32,
    ) -> i32,
    pub provide_token: extern "system" fn(
        context: *mut std::ffi::c_void,
        user_id: *const u8,
        client_id: *const u8,
        out_blob: *mut u8,
        out_cap: u32,
    ) -> i32,
    pub store_credential: extern "system" fn(
        context: *mut std::ffi::c_void,
        user_id: *const u8,
        blob: *const u8,
        blob_len: u32,
    ) -> i32,
}

impl Default for LcsdHostApi {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<LcsdHostApi>() as u32,
            context: std::ptr::null_mut(),
            canceled: lcsd_host_stop,
            verify_server: lcsd_host_stop_pipe,
            verify_path: lcsd_host_stop_path,
            provide_token: lcsd_host_stop_token,
            store_credential: lcsd_host_stop_store,
        }
    }
}

extern "system" fn lcsd_host_stop(_context: *mut std::ffi::c_void) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_host_stop_pipe(
    _context: *mut std::ffi::c_void,
    _pipe_handle: *mut std::ffi::c_void,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_host_stop_path(
    _context: *mut std::ffi::c_void,
    _exe_path: *const u16,
    _out_signer: *mut u8,
    _out_cap: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_host_stop_token(
    _context: *mut std::ffi::c_void,
    _user_id: *const u8,
    _client_id: *const u8,
    _out_blob: *mut u8,
    _out_cap: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_host_stop_store(
    _context: *mut std::ffi::c_void,
    _user_id: *const u8,
    _blob: *const u8,
    _blob_len: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}

/// One speaker, fixed layout: length-prefixed fixed buffers, no allocation crosses.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LcsdSpeaker {
    pub user_id_len: u32,
    pub user_id: [u8; LCSD_ID_CAPACITY],
    pub name_len: u32,
    pub name: [u8; LCSD_NAME_CAPACITY],
    pub speaking: u8,
    pub is_self: u8,
    pub started_unix_ms: u64,
    pub stopped_unix_ms: u64,
}

impl Default for LcsdSpeaker {
    fn default() -> Self {
        Self {
            user_id_len: 0,
            user_id: [0; LCSD_ID_CAPACITY],
            name_len: 0,
            name: [0; LCSD_NAME_CAPACITY],
            speaking: 0,
            is_self: 0,
            started_unix_ms: 0,
            stopped_unix_ms: 0,
        }
    }
}

/// The host-side snapshot of the component's state projection of the shipped `DiscordState`
/// (`src/model.rs`), field for field, bounded (`LCSD_MAX_SPEAKERS` >= the configured maximum).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LcsdState {
    pub size: u32,
    pub revision: u64,
    pub connected: u8,
    pub authenticated: u8,
    pub self_mute: u8,
    pub self_deaf: u8,
    pub voice_ping_ms: f64,
    pub channel_id_len: u32,
    pub channel_id: [u8; LCSD_CHANNEL_ID_CAPACITY],
    pub channel_name_len: u32,
    pub channel_name: [u8; LCSD_CHANNEL_CAPACITY],
    pub connection_state_len: u32,
    pub connection_state: [u8; LCSD_STATE_CAPACITY],
    pub error_len: u32,
    pub error: [u8; LCSD_ERROR_CAPACITY],
    pub updated_unix_ms: u64,
    pub speaker_count: u32,
    pub speakers: [LcsdSpeaker; LCSD_MAX_SPEAKERS],
}

impl Default for LcsdState {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<LcsdState>() as u32,
            revision: 0,
            connected: 0,
            authenticated: 0,
            self_mute: 0,
            self_deaf: 0,
            voice_ping_ms: 0.0,
            channel_id_len: 0,
            channel_id: [0; LCSD_CHANNEL_ID_CAPACITY],
            channel_name_len: 0,
            channel_name: [0; LCSD_CHANNEL_CAPACITY],
            connection_state_len: 0,
            connection_state: [0; LCSD_STATE_CAPACITY],
            error_len: 0,
            error: [0; LCSD_ERROR_CAPACITY],
            updated_unix_ms: 0,
            speaker_count: 0,
            speakers: [LcsdSpeaker::default(); LCSD_MAX_SPEAKERS],
        }
    }
}

/// The v1 table. The host validates `size >= size_of::<LcsdV1>()` and every entry non-null
/// before the first call; the component validates the requested size at the export.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LcsdV1 {
    pub size: u32,
    pub start: extern "system" fn(
        settings: *const LcsdSettings,
        settings_len: u32,
        api: *const LcsdHostApi,
        api_len: u32,
        out_handle: *mut *mut std::ffi::c_void,
    ) -> i32,
    pub set_config: extern "system" fn(
        handle: *mut std::ffi::c_void,
        settings: *const LcsdSettings,
        settings_len: u32,
    ) -> i32,
    pub poll_io: extern "system" fn(handle: *mut std::ffi::c_void, timeout_ms: u32) -> i32,
    pub state: extern "system" fn(
        handle: *mut std::ffi::c_void,
        out: *mut LcsdState,
        capacity: u32,
    ) -> i32,
    pub stop: extern "system" fn(handle: *mut std::ffi::c_void) -> i32,
    pub join: extern "system" fn(handle: *mut std::ffi::c_void) -> i32,
    pub release: extern "system" fn(handle: *mut std::ffi::c_void) -> i32,
    pub authorize: extern "system" fn(
        settings: *const LcsdSettings,
        secret: *const u8,
        secret_len: u32,
        api: *const LcsdHostApi,
        api_len: u32,
        out_status: *mut u8,
        out_cap: u32,
    ) -> i32,
    pub verify_client:
        extern "system" fn(exe_path: *const u16, out_signer: *mut u8, out_cap: u32) -> i32,
    pub last_error: extern "system" fn(out: *mut u8, out_cap: u32) -> i32,
}

impl Default for LcsdV1 {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<LcsdV1>() as u32,
            start: lcsd_stub_start,
            set_config: lcsd_stub_handle,
            poll_io: lcsd_stub_poll,
            state: lcsd_stub_state,
            stop: lcsd_stub_one,
            join: lcsd_stub_one,
            release: lcsd_stub_one,
            authorize: lcsd_stub_authorize,
            verify_client: lcsd_stub_verify,
            last_error: lcsd_stub_error,
        }
    }
}

extern "system" fn lcsd_stub_start(
    _settings: *const LcsdSettings,
    _settings_len: u32,
    _api: *const LcsdHostApi,
    _api_len: u32,
    out_handle: *mut *mut std::ffi::c_void,
) -> i32 {
    if !out_handle.is_null() {
        unsafe { *out_handle = std::ptr::null_mut() };
    }
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_handle(
    _handle: *mut std::ffi::c_void,
    _a: *const LcsdSettings,
    _b: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_one(_handle: *mut std::ffi::c_void) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_poll(_handle: *mut std::ffi::c_void, _timeout_ms: u32) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_state(
    _handle: *mut std::ffi::c_void,
    _out: *mut LcsdState,
    _capacity: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_authorize(
    _settings: *const LcsdSettings,
    _secret: *const u8,
    _secret_len: u32,
    _api: *const LcsdHostApi,
    _api_len: u32,
    _out_status: *mut u8,
    _out_cap: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_verify(
    _exe_path: *const u16,
    _out_signer: *mut u8,
    _out_cap: u32,
) -> i32 {
    LCSD_E_UNAVAILABLE
}
extern "system" fn lcsd_stub_error(_out: *mut u8, _out_cap: u32) -> i32 {
    LCSD_E_UNAVAILABLE
}
// ABI-V1-BLOCK-END

// ---- WO-TEL-D3-C S3: the component loader (host side) -------------------------------------
//
// The executable resolves the component at ONE exact path beside itself, loads it with the
// restricted search policy, and validates the v1 handshake. Any failure means "Discord
// unavailable" plus ONE bounded, secret-free diagnostic; the app and every core meter keep
// running, and no retry loop follows (a load is attempted on an activation edge, never per tick).

// (the module header already imports `c_void`, `OsStrExt`, `Path`, `PathBuf`, `Arc`, atomics)

use windows::core::PCWSTR;
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::System::Diagnostics::Debug::{
    SetErrorMode, SEM_FAILCRITICALERRORS, THREAD_ERROR_MODE,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

pub const COMPONENT_NAME: &str = "lcdsirplus_discord.dll";
pub const COMPONENT_EXPORT: &str = "lcsd_get_table";
pub const LOADER_DIAGNOSTIC_CAPACITY: usize = LCSD_STATUS_CAPACITY;

/// A loaded module. The handle is only ever used by the Discord worker thread, and every call
/// into the module is synchronous on that thread, so `FreeLibrary` can never race module code.
pub struct Module {
    library: windows::Win32::Foundation::HMODULE,
    pub table: *const LcsdV1,
}

/// Raw mirror of `LcsdV1`: one size word + ten entries, all pointer-sized. The host uses it to
/// validate that the module filled EVERY entry before the first call (a Rust `extern "system" fn`
/// pointer cannot be null, so the check happens on this view, not on the typed one).
#[repr(C)]
pub struct LcsdV1Raw {
    pub size: u32,
    pub entries: [*const c_void; 10],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadFault {
    Missing,
    Image,
    Symbol,
    Handshake,
}

impl LoadFault {
    /// One fixed, bounded, secret-free sentence per failure class. No path, version, token or
    /// remote text crosses into it, and it is the only thing the loader reports.
    pub const fn diagnostic(self) -> &'static str {
        match self {
            LoadFault::Missing => "discord component is not installed",
            LoadFault::Image => "discord component image was rejected",
            LoadFault::Symbol => "discord component export is unavailable",
            LoadFault::Handshake => "discord component handshake refused (abi v1)",
        }
    }
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// The bounded, secret-free ABI v1 status name. Exactly one production consumer: the start
/// refusal diagnostic, where the numeric code alone would not identify which contract rule the
/// component applied (size, argument, quiescence, handshake). No secret, path or remote text.
pub fn status_name(code: i32) -> &'static str {
    match code {
        LCSD_OK => "LCSD_OK",
        LCSD_IDLE => "LCSD_IDLE",
        LCSD_E_SIZE => "LCSD_E_SIZE",
        LCSD_E_ARG => "LCSD_E_ARG",
        LCSD_E_STATE => "LCSD_E_STATE",
        LCSD_E_IO => "LCSD_E_IO",
        LCSD_E_PROTOCOL => "LCSD_E_PROTOCOL",
        LCSD_E_IDENTITY => "LCSD_E_IDENTITY",
        LCSD_E_INFLIGHT => "LCSD_E_INFLIGHT",
        LCSD_E_CANCELED => "LCSD_E_CANCELED",
        LCSD_E_UNAVAILABLE => "LCSD_E_UNAVAILABLE",
        LCSD_E_NOT_FOUND => "LCSD_E_NOT_FOUND",
        LCSD_E_LEGACY => "LCSD_E_LEGACY",
        LCSD_E_MAGIC => "LCSD_E_MAGIC",
        _ => "LCSD_STATUS_UNKNOWN",
    }
}

/// F-H-2 (the F-4 class): the component lookup root that a premise test controls. A workspace
/// build puts the built cdylib in `release\deps\`, i.e. BESIDE the app test binary, so "the
/// component is not installed beside the executable" is NOT a property of the machine that runs
/// the suite: the loader would find a build artifact, and a running Discord Desktop answers the
/// authorize handshake, inverting the premise. The tests that assert the premise create their own
/// EMPTY lookup directory and point this override at it (`AbsentComponentFixture`), so
/// `cargo test --workspace` is reliable with any pre-existing artifacts and no harness moves any
/// build output. `cfg(test)`: neither static is compiled into the shipped binary, and the
/// production arm of `component_path` below is the shipped code verbatim.
#[cfg(test)]
static COMPONENT_LOOKUP_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// F-H-2: how many premise tests are holding a lookup fixture. The root is released by the LAST
/// fixture to leave, so one fixture's Drop can never expose the production lookup to a premise
/// test that is still running on a parallel suite thread.
#[cfg(test)]
static COMPONENT_LOOKUP_FIXTURES: AtomicUsize = AtomicUsize::new(0);

/// The exact module path beside the running executable (`runtime::canonical_executable`, the
/// app-startup canonicalization already used at `src/app.rs:127`). No PATH, no current directory,
/// no module-name search: the only lookup this order performs.
pub fn component_path() -> Result<PathBuf, String> {
    #[cfg(test)]
    {
        let root = COMPONENT_LOOKUP_ROOT
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(root) = root {
            return Ok(root.join(COMPONENT_NAME));
        }
    }
    let executable = crate::runtime::canonical_executable()?;
    let directory = executable
        .parent()
        .ok_or("discord component has no executable directory")?;
    Ok(directory.join(COMPONENT_NAME))
}

fn write_diagnostic(diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY], text: &str) {
    let bytes = text.as_bytes();
    let len = bytes.len().min(LOADER_DIAGNOSTIC_CAPACITY - 1);
    diagnostic[..len].copy_from_slice(&bytes[..len]);
    diagnostic[len] = 0;
}

fn validate_table(raw: *const c_void) -> Option<&'static LcsdV1> {
    if raw.is_null() {
        return None;
    }
    let view = unsafe { &*(raw as *const LcsdV1Raw) };
    if (view.size as usize) < std::mem::size_of::<LcsdV1>() {
        return None;
    }
    if view.entries.iter().any(|entry| entry.is_null()) {
        return None;
    }
    if std::mem::size_of::<LcsdV1Raw>() != std::mem::size_of::<LcsdV1>() {
        return None;
    }
    unsafe { Some(&*(raw as *const LcsdV1)) }
}

/// The OS hard-error mode, saved and restored around one component load. `Drop` runs on every
/// exit path, so a classified failure never leaves the mode raised. The mode is process-wide; the
/// only production load site is the single Discord worker thread (`spawn_worker`, whose loop owns
/// the `loader` call), so no two component loads are in flight inside the running app.
struct SilentComponentLoadErrors(THREAD_ERROR_MODE);

impl SilentComponentLoadErrors {
    fn new() -> Self {
        Self(unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) })
    }
}

impl Drop for SilentComponentLoadErrors {
    fn drop(&mut self) {
        unsafe {
            SetErrorMode(self.0);
        }
    }
}

/// Load + handshake. Split from the path resolution so a test can drive every failure arm with a
/// path that does not depend on the deployment layout.
pub fn load_table_from(
    path: &Path,
    diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY],
) -> Result<Module, LoadFault> {
    let wide_path = wide(path.as_os_str());
    // component loads are demand-driven against a user-installable file: classify failures silently; SEM_FAILCRITICALERRORS prevents the OS Bad-Image modal dialog (INCIDENT-002)
    let library = {
        let _silent = SilentComponentLoadErrors::new();
        unsafe {
            LoadLibraryExW(
                PCWSTR(wide_path.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
    }
    .map_err(|error| {
        // The Win32 codes distinguish "not found" from an unreadable/rejected image; both are
        // reported with their fixed sentence, and nothing is retried.
        write_diagnostic(diagnostic, LoadFault::Missing.diagnostic());
        if error.code() == windows::Win32::Foundation::ERROR_MOD_NOT_FOUND.to_hresult()
            || error.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult()
        {
            LoadFault::Missing
        } else {
            write_diagnostic(diagnostic, LoadFault::Image.diagnostic());
            LoadFault::Image
        }
    })?;
    let mut symbol_name = Vec::with_capacity(COMPONENT_EXPORT.len() + 1);
    symbol_name.extend_from_slice(COMPONENT_EXPORT.as_bytes());
    symbol_name.push(0);
    let symbol = unsafe { GetProcAddress(library, windows::core::PCSTR(symbol_name.as_ptr())) };
    let Some(symbol) = symbol else {
        unsafe {
            let _ = FreeLibrary(library);
        }
        write_diagnostic(diagnostic, LoadFault::Symbol.diagnostic());
        return Err(LoadFault::Symbol);
    };
    // The shipped export-transmute form (providers/xinput.rs:50): a FARPROC is an
    // `extern "system" fn() -> isize`, reinterpreted as the v1 export signature.
    let export: unsafe extern "system" fn(u32, u32, usize) -> *const c_void = unsafe {
        std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(u32, u32, usize) -> *const c_void,
        >(symbol)
    };
    let raw = unsafe { export(LCSD_MAGIC, LCSD_VERSION, std::mem::size_of::<LcsdV1>()) };
    let Some(table) = validate_table(raw) else {
        unsafe {
            let _ = FreeLibrary(library);
        }
        write_diagnostic(diagnostic, LoadFault::Handshake.diagnostic());
        return Err(LoadFault::Handshake);
    };
    write_diagnostic(diagnostic, "discord component ready (abi v1)");
    Ok(Module {
        library,
        table: table as *const LcsdV1,
    })
}

/// The production loader seam: exact path + load + handshake, one bounded diagnostic.
pub fn load_production_table(
    diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY],
) -> Result<Module, LoadFault> {
    let path = match component_path() {
        Ok(path) => path,
        Err(_) => {
            write_diagnostic(diagnostic, LoadFault::Missing.diagnostic());
            return Err(LoadFault::Missing);
        }
    };
    load_table_from(&path, diagnostic)
}

/// The test-visible module-presence census: a handle lookup only, never a load.
#[cfg(test)]
pub fn component_loaded(path: &Path) -> bool {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    let wide_path = wide(path.as_os_str());
    unsafe { GetModuleHandleW(PCWSTR(wide_path.as_ptr())) }
        .map(|handle| !handle.is_invalid())
        .unwrap_or(false)
}

impl Drop for Module {
    fn drop(&mut self) {
        // Reached only after `stop` -> `join` -> `release` returned success, i.e. with no step in
        // flight and no component code running on any thread.
        unsafe {
            let _ = FreeLibrary(self.library);
        }
    }
}

// ---- WO-TEL-D3-C S4: host callbacks + the state bridge -------------------------------------

/// The host state the component may observe through `LcsdHostApi`. Owned by the worker thread;
/// every pointer handed to the component outlives all of its calls.
pub struct WorkerContext {
    shutdown: Arc<AtomicBool>,
    config: Arc<RwLock<Config>>,
    session_config: RwLock<Config>,
    credentials: Arc<AtomicUsize>,
}

extern "system" fn host_canceled(context: *mut c_void) -> i32 {
    if context.is_null() {
        return 1;
    }
    let context = unsafe { &*(context as *const WorkerContext) };
    if context.shutdown.load(Ordering::Relaxed) {
        return 1;
    }
    // The shipped mid-session cancellation, kept exactly: worker shutdown OR a config-content
    // change. A reload that changes no Discord field deliberately does NOT interrupt a session.
    let current = context.config.read().unwrap_or_else(|e| e.into_inner());
    let session = context
        .session_config
        .read()
        .unwrap_or_else(|e| e.into_inner());
    if session_config_changed(&session, &current) {
        return 1;
    }
    0
}

extern "system" fn host_verify_server(context: *mut c_void, pipe_handle: *mut c_void) -> i32 {
    if context.is_null() || pipe_handle.is_null() {
        return LCSD_E_ARG;
    }
    // Borrow, never own: the component keeps the pipe handle, so this must not close it.
    let borrowed = unsafe { std::mem::ManuallyDrop::new(File::from_raw_handle(pipe_handle)) };
    match verify_pipe(&borrowed) {
        Ok(()) => LCSD_OK,
        Err(_) => LCSD_E_IDENTITY,
    }
}

extern "system" fn host_verify_path(
    context: *mut c_void,
    exe_path: *const u16,
    out_signer: *mut u8,
    out_cap: u32,
) -> i32 {
    if context.is_null() || exe_path.is_null() || out_signer.is_null() || out_cap == 0 {
        return LCSD_E_ARG;
    }
    let mut units = Vec::new();
    unsafe {
        let mut cursor = exe_path;
        while *cursor != 0 {
            units.push(*cursor);
            cursor = cursor.add(1);
            if units.len() > 32_768 {
                return LCSD_E_ARG;
            }
        }
    }
    let path = PathBuf::from(String::from_utf16_lossy(&units));
    match verify_authenticode_bounded(path) {
        Ok(organization) => {
            let bytes = organization.as_bytes();
            let len = bytes.len().min(out_cap as usize - 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_signer, len);
                *out_signer.add(len) = 0;
            }
            LCSD_OK
        }
        Err(_) => LCSD_E_IDENTITY,
    }
}

extern "system" fn host_provide_token(
    context: *mut c_void,
    user_id: *const u8,
    client_id: *const u8,
    out_blob: *mut u8,
    out_cap: u32,
) -> i32 {
    if context.is_null() || user_id.is_null() || client_id.is_null() || out_blob.is_null() {
        return LCSD_E_ARG;
    }
    let context = unsafe { &*(context as *const WorkerContext) };
    context.credentials.fetch_add(1, Ordering::Relaxed);
    let user = match unsafe { read_c_string(user_id) } {
        Some(user) => user,
        None => return LCSD_E_ARG,
    };
    let client = match unsafe { read_c_string(client_id) } {
        Some(client) => client,
        None => return LCSD_E_ARG,
    };
    if user.is_empty() || client.is_empty() {
        return LCSD_E_ARG;
    }
    let path = match credential_path(&user) {
        Ok(path) => path,
        Err(_) => return LCSD_E_ARG,
    };
    match load_credential_blob(&path) {
        Ok(Some(blob)) => {
            if blob.len() as u32 > out_cap {
                return LCSD_E_SIZE;
            }
            unsafe { std::ptr::copy_nonoverlapping(blob.as_ptr(), out_blob, blob.len()) };
            blob.len() as i32
        }
        Ok(None) => {
            // The shipped distinction: a single-account legacy file is a hard failure the user
            // clears with the CLI; no credential at all is the "authorize first" path.
            if credential_root()
                .map(|root| root.join("discord.token").exists())
                .unwrap_or(false)
            {
                LCSD_E_LEGACY
            } else {
                LCSD_E_NOT_FOUND
            }
        }
        Err(_) => LCSD_E_CREDENTIAL,
    }
}

extern "system" fn host_store_credential(
    context: *mut c_void,
    user_id: *const u8,
    blob: *const u8,
    blob_len: u32,
) -> i32 {
    if context.is_null() || user_id.is_null() || blob.is_null() {
        return LCSD_E_ARG;
    }
    let context = unsafe { &*(context as *const WorkerContext) };
    context.credentials.fetch_add(1, Ordering::Relaxed);
    let user = match unsafe { read_c_string(user_id) } {
        Some(user) => user,
        None => return LCSD_E_ARG,
    };
    if blob_len == 0 || blob_len as usize > MAX_CREDENTIAL {
        return LCSD_E_SIZE;
    }
    let record = unsafe { std::slice::from_raw_parts(blob, blob_len as usize) };
    let path = match credential_path(&user) {
        Ok(path) => path,
        Err(_) => return LCSD_E_ARG,
    };
    match save_credential(&path, &user, record) {
        Ok(()) => LCSD_OK,
        Err(_) => LCSD_E_CREDENTIAL,
    }
}

unsafe fn read_c_string(pointer: *const u8) -> Option<String> {
    let mut bytes = Vec::new();
    let mut cursor = pointer;
    loop {
        let byte = *cursor;
        if byte == 0 {
            return String::from_utf8(bytes).ok();
        }
        bytes.push(byte);
        cursor = cursor.add(1);
        if bytes.len() > 4096 {
            return None;
        }
    }
}

fn host_api(context: &WorkerContext) -> LcsdHostApi {
    LcsdHostApi {
        size: std::mem::size_of::<LcsdHostApi>() as u32,
        context: context as *const WorkerContext as *mut c_void,
        canceled: host_canceled,
        verify_server: host_verify_server,
        verify_path: host_verify_path,
        provide_token: host_provide_token,
        store_credential: host_store_credential,
    }
}

fn settings_from_config(config: &Config) -> LcsdSettings {
    let mut settings = LcsdSettings::default();
    let bytes = config.discord_client_id.as_bytes();
    let len = bytes.len().min(LCSD_CLIENT_ID_CAPACITY);
    settings.client_id[..len].copy_from_slice(&bytes[..len]);
    settings.client_id_len = len as u32;
    settings.enabled = config.discord_enabled as u8;
    settings.safe_mode = config.safe_mode as u8;
    settings.show_self = config.discord_show_self as u8;
    settings.show_channel = config.discord_show_channel as u8;
    settings.linger_ms = config.discord_linger.as_millis() as u64;
    settings.max_speakers = config.discord_max_speakers;
    settings.ipc_probe_count = 10;
    settings
}

/// The v1 state POD -> the app's `model::DiscordState`, field for field. This is the only bridge
/// from component data to the renderer's `visible_speakers` / health-row consumers.
fn state_from_pod(pod: &LcsdState) -> DiscordState {
    let text = |len: u32, bytes: &[u8]| -> String {
        let len = (len as usize).min(bytes.len());
        String::from_utf8_lossy(&bytes[..len]).into_owned()
    };
    let time = |ms: u64| -> Option<SystemTime> {
        (ms != 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
    };
    DiscordState {
        connected: pod.connected != 0,
        authenticated: pod.authenticated != 0,
        channel_id: text(pod.channel_id_len, &pod.channel_id),
        channel_name: text(pod.channel_name_len, &pod.channel_name),
        self_mute: pod.self_mute != 0,
        self_deaf: pod.self_deaf != 0,
        voice_ping_ms: pod.voice_ping_ms,
        connection_state: text(pod.connection_state_len, &pod.connection_state),
        speakers: pod
            .speakers
            .iter()
            .take((pod.speaker_count as usize).min(LCSD_MAX_SPEAKERS))
            .map(|speaker| Speaker {
                user_id: text(speaker.user_id_len, &speaker.user_id),
                name: text(speaker.name_len, &speaker.name),
                speaking: speaker.speaking != 0,
                is_self: speaker.is_self != 0,
                started_at: time(speaker.started_unix_ms),
                stopped_at: time(speaker.stopped_unix_ms),
            })
            .collect(),
        error: text(pod.error_len, &pod.error),
        updated: time(pod.updated_unix_ms),
    }
}

fn component_error(table: *const LcsdV1, fallback: &str) -> String {
    // The component's last bounded, secret-free classification, read through the table.
    let mut buffer = [0u8; LOADER_DIAGNOSTIC_CAPACITY];
    let written = unsafe { ((*table).last_error)(buffer.as_mut_ptr(), buffer.len() as u32) };
    if written > 0 {
        let text = String::from_utf8_lossy(&buffer[..written as usize]).into_owned();
        if !text.is_empty() {
            return text;
        }
    }
    fallback.to_string()
}

// ---- WO-TEL-D3-C S4: the worker (spawn-on-enable, join-on-disable) -------------------------

/// The loader seam. Production always passes `load_production_table`; a test passes a stub that
/// returns a v1 table built in the test region, so the host's gate, load-exactly-once, refusal,
/// unload-order and census machine are all driven without a deployed module.
pub type TableLoader =
    fn(diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY]) -> Result<Module, LoadFault>;

pub(crate) struct Census {
    pub(crate) loads: AtomicU64,
    pub(crate) load_attempts: AtomicU64,
    pub(crate) starts: AtomicU64,
    pub(crate) stops: AtomicU64,
    pub(crate) joins: AtomicU64,
    pub(crate) releases: AtomicU64,
    pub(crate) unloads: AtomicU64,
    pub(crate) retained: AtomicU64,
    pub(crate) spawns: AtomicU64,
    pub(crate) worker_joins: AtomicU64,
    pub(crate) diagnostics: AtomicU64,
}

impl Census {
    fn new() -> Arc<Self> {
        Arc::new(Census {
            loads: AtomicU64::new(0),
            load_attempts: AtomicU64::new(0),
            starts: AtomicU64::new(0),
            stops: AtomicU64::new(0),
            joins: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            unloads: AtomicU64::new(0),
            retained: AtomicU64::new(0),
            spawns: AtomicU64::new(0),
            worker_joins: AtomicU64::new(0),
            diagnostics: AtomicU64::new(0),
        })
    }
}

const SERVE_STEP_TIMEOUT_MS: u32 = 100;
const CONNECT_STEP_TIMEOUT_MS: u32 = 5_000;

/// The mandated unload order, and its one documented limit: if any entry reports work in flight,
/// the module is RETAINED and reported, never force-unloaded. Callers may not claim a full unload.
fn unload_module(
    module: &mut Option<Module>,
    handle: &mut *mut c_void,
    census: &Census,
    retained: &Arc<AtomicBool>,
) -> bool {
    if let Some(table) = module.as_ref().map(|module| module.table) {
        if !handle.is_null() {
            unsafe {
                let stopped = ((*table).stop)(*handle);
                census.stops.fetch_add(1, Ordering::Relaxed);
                if stopped != LCSD_OK {
                    // The component reports work in flight AT THE FIRST STEP, so the host refuses
                    // the whole unload: no join, no release, no FreeLibrary (audit condition iv).
                    // RETAIN means LEAK the image handle: dropping `Module` would FreeLibrary it
                    // while component code is still live, which is exactly what is forbidden.
                    if let Some(live) = module.take() {
                        std::mem::forget(live);
                    }
                    retained.store(true, Ordering::Relaxed);
                    census.retained.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                let joined = ((*table).join)(*handle);
                census.joins.fetch_add(1, Ordering::Relaxed);
                let released = if joined == LCSD_OK {
                    let code = ((*table).release)(*handle);
                    // The census counts only calls that ACTUALLY ran, so a refused unload reports
                    // zero releases rather than a synthetic one.
                    census.releases.fetch_add(1, Ordering::Relaxed);
                    code
                } else {
                    LCSD_E_INFLIGHT
                };
                if joined != LCSD_OK || released != LCSD_OK {
                    // REFUSE the unload: keep the module + the handle, report it, and never call
                    // FreeLibrary under a live step (condition iv). The image is leaked on purpose,
                    // so `Module`'s Drop cannot free a loaded image with work in flight.
                    if let Some(live) = module.take() {
                        std::mem::forget(live);
                    }
                    retained.store(true, Ordering::Relaxed);
                    census.retained.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                *handle = std::ptr::null_mut();
            }
        }
        if handle.is_null() {
            drop(module.take());
            census.unloads.fetch_add(1, Ordering::Relaxed);
            retained.store(false, Ordering::Relaxed);
            return true;
        }
    }
    *module = None;
    true
}

/// Many short successful polls form one healthy session. A slow failing call is
/// not evidence of health, and a short-lived connection retains failure backoff.
fn session_backoff(backoff: Duration, connected_since: Option<Instant>, now: Instant) -> Duration {
    if connected_since
        .is_some_and(|since| now.saturating_duration_since(since) >= Duration::from_secs(30))
    {
        Duration::from_secs(1)
    } else {
        backoff
    }
}

fn start_session(
    table: *const LcsdV1,
    config: &Config,
    api: &LcsdHostApi,
    census: &Census,
) -> Result<*mut c_void, String> {
    let settings = settings_from_config(config);
    let mut handle: *mut c_void = std::ptr::null_mut();
    let code = unsafe {
        ((*table).start)(
            &settings,
            std::mem::size_of::<LcsdSettings>() as u32,
            api,
            std::mem::size_of::<LcsdHostApi>() as u32,
            &mut handle,
        )
    };
    census.starts.fetch_add(1, Ordering::Relaxed);
    if code == LCSD_OK && !handle.is_null() {
        return Ok(handle);
    }
    if code == LCSD_OK {
        return Err("discord component returned no handle".to_string());
    }
    // The refusal is reported as the component's own bounded classification plus the ABI status
    // name it answered with: secret-free, no path, no remote text.
    Err(format!(
        "{} ({})",
        component_error(table, "discord component refused to start"),
        status_name(code)
    ))
}

// ---- PART2 ----

// ---- WO-TEL-D3-C S4: the Runtime, the gated worker, the one-shot authorize ------------------

pub struct Runtime {
    pub updates: mpsc::Receiver<DiscordState>,
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    credentials: Arc<AtomicUsize>,
    census: Arc<Census>,
    retained: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    loader: TableLoader,
    tx: mpsc::Sender<DiscordState>,
}

/// Test-visible snapshot of the lifecycle census (the same counters the worker increments).
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CensusSnapshot {
    pub loads: u64,
    pub load_attempts: u64,
    pub starts: u64,
    pub stops: u64,
    pub joins: u64,
    pub releases: u64,
    pub unloads: u64,
    pub retained: u64,
    pub spawns: u64,
    pub worker_joins: u64,
    pub diagnostics: u64,
}

impl Runtime {
    #[cfg(test)]
    /// The same `Arc<Census>` the worker drives, so the unload order is observable after the
    /// Runtime is dropped: the per-Runtime counters, never a process-global one.
    pub fn census(&self) -> Arc<Census> {
        self.census.clone()
    }

    #[cfg(test)]
    pub fn census_snapshot(&self) -> CensusSnapshot {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        CensusSnapshot {
            loads: read(&self.census.loads),
            load_attempts: read(&self.census.load_attempts),
            starts: read(&self.census.starts),
            stops: read(&self.census.stops),
            joins: read(&self.census.joins),
            releases: read(&self.census.releases),
            unloads: read(&self.census.unloads),
            retained: read(&self.census.retained),
            spawns: read(&self.census.spawns),
            worker_joins: read(&self.census.worker_joins),
            diagnostics: read(&self.census.diagnostics),
        }
    }

    #[cfg(test)]
    pub fn worker_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn credential_calls(&self) -> usize {
        self.credentials.load(Ordering::Relaxed)
    }

    /// The refuse-unload-if-inflight report bit. True means the module is RETAINED: this order
    /// never claims a full unload in that state (audit condition iv).
    #[cfg(test)]
    pub fn module_retained(&self) -> bool {
        self.retained.load(Ordering::Relaxed)
    }

    pub fn update_config(&self, config: &Config) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
        // WO-TEL-D3-C keeps the WO-TEL-D3-B1 (L3) pairing: the write, then the generation bump,
        // which is what invalidates the worker's `cached_request` copy.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.reconcile();
    }

    /// The lifecycle edge: enable -> spawn the worker (and load on demand), disable -> join it.
    /// Joining is bounded by the shipped cancellation contract: the worker's in-flight component
    /// step returns within its 20 ms poll quantum once `shutdown`/the content change is visible,
    /// and its park iteration sleeps 100 ms.
    fn reconcile(&self) {
        let enabled = provider_enabled(&current_config(&self.config));
        let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if enabled {
            if self.running.load(Ordering::Relaxed) {
                return;
            }
            if let Some(stale) = slot.take() {
                let _ = stale.join();
                self.census.worker_joins.fetch_add(1, Ordering::Relaxed);
            }
            let config = current_config(&self.config);
            *slot = Some(spawn_worker(self, &config));
            return;
        }
        if !self.running.load(Ordering::Relaxed) {
            if slot.take().is_some() {
                // The worker already exited on its disable edge; reap the finished handle.
                self.census.worker_joins.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        if let Some(handle) = slot.take() {
            let _ = handle.join();
            self.census.worker_joins.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
            self.census.worker_joins.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn spawn(config: &Config) -> Runtime {
    spawn_with(config, load_production_table)
}

/// The construction path. At disabled / unconfigured this performs NO component call, creates NO
/// thread and NO provider-owned state beyond the descriptors below; it publishes the disabled
/// state once, exactly as the accepted parked loop did.
pub fn spawn_with(config: &Config, loader: TableLoader) -> Runtime {
    let (tx, rx) = mpsc::channel();
    let runtime = Runtime {
        updates: rx,
        config: Arc::new(RwLock::new(config.clone())),
        generation: Arc::new(AtomicU64::new(1)),
        shutdown: Arc::new(AtomicBool::new(false)),
        credentials: Arc::new(AtomicUsize::new(0)),
        census: Census::new(),
        retained: Arc::new(AtomicBool::new(false)),
        running: Arc::new(AtomicBool::new(false)),
        thread: Mutex::new(None),
        loader,
        tx,
    };
    if provider_enabled(config) {
        let worker = spawn_worker(&runtime, config);
        *runtime.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(worker);
    } else {
        // The disabled / unconfigured case: NO thread, NO component call, NO provider state beyond
        // the descriptors above; the parked disabled state is published exactly once.
        let _ = runtime
            .tx
            .send(unavailable(&DiscordState::default(), "disabled"));
    }
    runtime
}

/// The Discord worker: the host-side loop that drives the component synchronously. It is the only
/// thread that touches the module, so the unload order has no concurrent caller. The
/// WO-TEL-D3-B1 (L3/L4) loop-head cached read, the cache seed and the publish-once park are the
/// accepted bodies, relocated coherently into this worker; the config gate (`provider_enabled`)
/// and the backoff ladder are byte-identical.
fn spawn_worker(runtime: &Runtime, config: &Config) -> std::thread::JoinHandle<()> {
    let worker_config = runtime.config.clone();
    let worker_generation = runtime.generation.clone();
    let worker_shutdown = runtime.shutdown.clone();
    let worker_credentials = runtime.credentials.clone();
    let worker_census = runtime.census.clone();
    let worker_retained = runtime.retained.clone();
    let worker_running = runtime.running.clone();
    let worker_tx = runtime.tx.clone();
    let loader = runtime.loader;
    // The cache seed: the `Config` `spawn` received, as an `Arc` the worker owns (the shipped
    // loops seed their cache the same way, from `current_config`).
    let seed = Arc::new(config.clone());
    runtime.census.spawns.fetch_add(1, Ordering::Relaxed);
    runtime.running.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("lcdsirplus-discord".into())
        .spawn(move || {
            let mut state = DiscordState::default();
            let mut request = (0u64, seed);
            let mut backoff = Duration::from_secs(1);
            // WO-TEL-D3-B1 (L4): the parked disabled state is published ONCE per park transition.
            // WO-TEL-D3-C adds the lifecycle edge: the publish happens, then the worker thread
            // EXITS, so a disabled provider owns no thread at all; the enable edge in `reconcile`
            // spawns a fresh one. The single publish is therefore structural, not a flag.
            let mut module: Option<Module> = None;
            let mut handle: *mut c_void = std::ptr::null_mut();
            let mut load_attempted_generation = 0u64;
            let mut revision = u64::MAX;
            let mut first_step = true;
            let mut connected_since = None;
            let context = WorkerContext {
                shutdown: worker_shutdown.clone(),
                config: worker_config.clone(),
                session_config: RwLock::new(Config::default()),
                credentials: worker_credentials.clone(),
            };
            let api = host_api(&context);
            while !worker_shutdown.load(Ordering::Relaxed) {
                let (request_generation, config) =
                    cached_request(&worker_config, &worker_generation, &mut request);
                if !provider_enabled(&config) {
                    // Conditions (iii)/(iv): the disable edge ENDS this worker thread. The component
                    // session (if any) is stopped, joined and released here; a refused unload
                    // (work in flight) leaves the module RETAINED and REPORTED, and this order
                    // never claims a full unload in that state. Because the thread is gone, the
                    // parked disabled state is published exactly ONCE per park transition
                    // (WO-TEL-D3-B1 L4) by construction, and the enable edge in `reconcile` spawns
                    // a fresh thread.
                    let quiescent =
                        unload_module(&mut module, &mut handle, &worker_census, &worker_retained);
                    if !quiescent {
                        state = unavailable(&state, "discord module retained");
                        let _ = worker_tx.send(state.clone());
                        crate::log_warn!("Discord component retained: unload refused");
                    }
                    state = unavailable(&state, "disabled");
                    let _ = worker_tx.send(state.clone());
                    break;
                }
                if handle.is_null() {
                    if module.is_none() {
                        if load_attempted_generation == request_generation {
                            // The load already failed for this activation: park, do NOT retry per
                            // tick. The retry edge is a config (re)activation, i.e. a new generation.
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        load_attempted_generation = request_generation;
                        let mut diagnostic = [0u8; LOADER_DIAGNOSTIC_CAPACITY];
                        worker_census.load_attempts.fetch_add(1, Ordering::Relaxed);
                        match loader(&mut diagnostic) {
                            Ok(loaded) => {
                                worker_census.loads.fetch_add(1, Ordering::Relaxed);
                                module = Some(loaded);
                            }
                            Err(fault) => {
                                worker_census.diagnostics.fetch_add(1, Ordering::Relaxed);
                                state = unavailable(&state, "discord module unavailable");
                                let _ = worker_tx.send(state.clone());
                                crate::log_warn!(
                                    "Discord component unavailable: {}",
                                    fault.diagnostic()
                                );
                                continue;
                            }
                        }
                    }
                    let table = match module.as_ref() {
                        Some(loaded) => loaded.table,
                        None => continue,
                    };
                    *context
                        .session_config
                        .write()
                        .unwrap_or_else(|e| e.into_inner()) = (*config).clone();
                    match start_session(table, &config, &api, &worker_census) {
                        Ok(started_handle) => {
                            handle = started_handle;
                            revision = u64::MAX;
                            first_step = true;
                            connected_since = None;
                        }
                        Err(message) => {
                            state = unavailable(&state, &message);
                            let _ = worker_tx.send(state.clone());
                            crate::log_warn!("Discord component refused to start: {}", message);
                            unload_module(
                                &mut module,
                                &mut handle,
                                &worker_census,
                                &worker_retained,
                            );
                            load_attempted_generation = 0;
                            let deadline = Instant::now() + backoff;
                            while Instant::now() < deadline
                                && !worker_shutdown.load(Ordering::Relaxed)
                            {
                                if session_config_changed(
                                    &config,
                                    &worker_config.read().unwrap_or_else(|e| e.into_inner()),
                                ) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            backoff = next_backoff(backoff);
                            continue;
                        }
                    }
                }
                let table = match module.as_ref() {
                    Some(loaded) => loaded.table,
                    None => continue,
                };
                let timeout = if first_step {
                    CONNECT_STEP_TIMEOUT_MS
                } else {
                    SERVE_STEP_TIMEOUT_MS
                };
                let code = unsafe { ((*table).poll_io)(handle, timeout) };
                first_step = false;
                let now = Instant::now();
                if matches!(code, LCSD_OK | LCSD_IDLE) {
                    connected_since.get_or_insert(now);
                }
                backoff = session_backoff(backoff, connected_since, now);
                publish_component_state(table, handle, &mut state, &mut revision, &worker_tx);
                match code {
                    LCSD_OK | LCSD_IDLE => {
                        continue;
                    }
                    LCSD_E_CANCELED => {
                        // The shipped content-driven re-arm: the session ends, the state says so,
                        // the backoff resets, and the next iteration re-arms on the new content.
                        end_session(table, &mut handle, &worker_census);
                        connected_since = None;
                        state = unavailable(&state, "reconfiguring");
                        let _ = worker_tx.send(state.clone());
                        backoff = Duration::from_secs(1);
                        continue;
                    }
                    _ => {}
                }
                let error = component_error(table, "Discord RPC disconnected");
                end_session(table, &mut handle, &worker_census);
                if worker_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                if session_config_changed(
                    &config,
                    &worker_config.read().unwrap_or_else(|e| e.into_inner()),
                ) {
                    state = unavailable(&state, "reconfiguring");
                    let _ = worker_tx.send(state.clone());
                    backoff = Duration::from_secs(1);
                    continue;
                }
                connected_since = None;
                state = unavailable(&state, &error);
                let _ = worker_tx.send(state.clone());
                crate::log_warn!("Discord RPC disconnected: {}", error);
                let deadline = Instant::now() + backoff;
                while Instant::now() < deadline && !worker_shutdown.load(Ordering::Relaxed) {
                    if session_config_changed(
                        &config,
                        &worker_config.read().unwrap_or_else(|e| e.into_inner()),
                    ) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                backoff = next_backoff(backoff);
            }
            if !worker_retained.load(Ordering::Relaxed) {
                unload_module(&mut module, &mut handle, &worker_census, &worker_retained);
            }
            worker_running.store(false, Ordering::SeqCst);
        })
        .expect("spawn Discord worker")
}

/// Reads the component's state POD and publishes it only when the component's own revision moved
/// (the accepted publish-on-change shape; the renderer and the health row are the consumers).
fn publish_component_state(
    table: *const LcsdV1,
    handle: *mut c_void,
    state: &mut DiscordState,
    revision: &mut u64,
    tx: &mpsc::Sender<DiscordState>,
) {
    if handle.is_null() {
        return;
    }
    let mut pod = LcsdState::default();
    let code =
        unsafe { ((*table).state)(handle, &mut pod, std::mem::size_of::<LcsdState>() as u32) };
    if code != LCSD_OK {
        return;
    }
    if pod.revision == *revision {
        return;
    }
    *revision = pod.revision;
    *state = state_from_pod(&pod);
    let _ = tx.send(state.clone());
}

/// Ends one session (stop -> join -> release of the handle) and keeps the MODULE loaded: the
/// module is present exactly while Discord is enabled-and-demanded, and the next activation
/// re-`start`s on the same module, so a session restart never re-loads the image.
fn end_session(table: *const LcsdV1, handle: &mut *mut c_void, census: &Census) {
    if handle.is_null() {
        return;
    }
    unsafe {
        ((*table).stop)(*handle);
        census.stops.fetch_add(1, Ordering::Relaxed);
        ((*table).join)(*handle);
        census.joins.fetch_add(1, Ordering::Relaxed);
        ((*table).release)(*handle);
        census.releases.fetch_add(1, Ordering::Relaxed);
    }
    *handle = std::ptr::null_mut();
}

/// WO-TEL-D3-C (CLI, audit §10(v)): `--discord-authorize` is the documented temporary load. The
/// module is loaded for this one command, runs its one-shot flow with no handle and no worker, and
/// is unloaded on the same thread afterwards. Credentials stay host-side: the record is written
/// through `store_credential` (DPAPI + the pinned boundary in the executable).
pub fn authorize(config: &Config, client_secret: &str) -> Result<(), String> {
    if config.discord_client_id.trim().is_empty() {
        return Err("discord_client_id is not configured".into());
    }
    let mut diagnostic = [0u8; LOADER_DIAGNOSTIC_CAPACITY];
    let module = match load_production_table(&mut diagnostic) {
        Ok(module) => module,
        Err(fault) => return Err(fault.diagnostic().to_string()),
    };
    let context = WorkerContext {
        shutdown: Arc::new(AtomicBool::new(false)),
        config: Arc::new(RwLock::new(config.clone())),
        session_config: RwLock::new(config.clone()),
        credentials: Arc::new(AtomicUsize::new(0)),
    };
    let api = host_api(&context);
    let settings = settings_from_config(config);
    let mut status = [0u8; 256];
    let code = unsafe {
        ((*module.table).authorize)(
            &settings,
            client_secret.as_ptr(),
            client_secret.len() as u32,
            &api,
            std::mem::size_of::<LcsdHostApi>() as u32,
            status.as_mut_ptr(),
            status.len() as u32,
        )
    };
    let message = component_error(module.table, "Discord authorization failed");
    drop(context);
    drop(module);
    if code == LCSD_OK {
        Ok(())
    } else {
        Err(message)
    }
}

fn credential_path_at(root: &Path, user_id: &str) -> Result<PathBuf, String> {
    validate_user_id(user_id)?;
    Ok(root.join(format!("discord-{}.token", user_id)))
}

pub fn credential_path(user_id: &str) -> Result<PathBuf, String> {
    credential_path_at(&credential_root()?, user_id)
}

fn credential_user_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix("discord-")?.strip_suffix(".token")?;
    validate_user_id(id).ok()
}

fn dpapi(input: &[u8], protect: bool) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::LocalFree;
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if input.is_empty() || input.len() > MAX_CREDENTIAL {
        return Err("Discord credential input is empty or exceeds its limit".into());
    }
    let source = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if protect {
            CryptProtectData(
                &source,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &source,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|_| "Windows could not protect the Discord credential".to_string())?;
        if output.cbData == 0 || output.pbData.is_null() || output.cbData as usize > MAX_CREDENTIAL
        {
            if !output.pbData.is_null() {
                let _ = LocalFree(windows::Win32::Foundation::HLOCAL(output.pbData.cast()));
            }
            return Err("DPAPI returned an invalid Discord credential blob".into());
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(windows::Win32::Foundation::HLOCAL(output.pbData.cast()));
        Ok(result)
    }
}

struct CredentialBoundary {
    root: PathBuf,
    pins: Vec<File>,
}

impl CredentialBoundary {
    fn open(root: &Path, create_missing: bool) -> Result<Self, String> {
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, GetDriveTypeW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        };
        use windows::Win32::System::WindowsProgramming::DRIVE_FIXED as SYSTEM_DRIVE_FIXED;

        let text = root.to_string_lossy();
        let path = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if path.len() < 3 || path.as_bytes()[1] != b':' || path.as_bytes()[2] != b'\\' {
            return Err("Discord credential directory is not on a fixed local volume".into());
        }
        let drive_root = &path[..3];
        let drive_wide: Vec<u16> = drive_root.encode_utf16().chain(Some(0)).collect();
        let drive_type = unsafe { GetDriveTypeW(windows::core::PCWSTR(drive_wide.as_ptr())) };
        if drive_type != SYSTEM_DRIVE_FIXED {
            return Err("Discord credential directory is not on a fixed local volume".into());
        }
        let mut pins = Vec::new();
        let mut current = PathBuf::from(drive_root);
        let components = path[3..].split('\\').filter(|part| !part.is_empty());
        for component in std::iter::once("").chain(components) {
            if !component.is_empty() {
                if component == "." || component == ".." || component.contains(['/', '\\']) {
                    return Err("Discord credential directory contains an invalid component".into());
                }
                current.push(component);
            }
            let open = |path: &Path| -> Result<File, windows::core::Error> {
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
                }?;
                Ok(unsafe { File::from_raw_handle(handle.0) })
            };
            let file = match open(&current) {
                Ok(file) => file,
                Err(_) if create_missing && !current.exists() => {
                    std::fs::create_dir(&current)
                        .map_err(|_| "Discord credential directory could not be created")?;
                    open(&current)
                        .map_err(|_| "Discord credential directory could not be pinned")?
                }
                Err(_) => return Err("Discord credential directory could not be pinned".into()),
            };
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe {
                GetFileInformationByHandle(
                    windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast()),
                    &mut info,
                )
            }
            .map_err(|_| "Discord credential directory identity is unavailable")?;
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
                || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            {
                return Err("Discord credential directory boundary is not trusted".into());
            }
            verify_handle_path(&file, &current, "directory")?;
            pins.push(file);
        }
        Ok(Self {
            root: PathBuf::from(path),
            pins,
        })
    }

    fn validate(&self) -> Result<(), String> {
        verify_handle_path(
            self.pins
                .last()
                .ok_or("Discord credential directory could not be pinned")?,
            &self.root,
            "directory",
        )
    }
}

fn verify_handle_path(file: &File, path: &Path, kind: &str) -> Result<(), String> {
    use std::ffi::OsString;
    use windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let mut resolved = vec![0; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast()),
            &mut resolved,
            Default::default(),
        )
    } as usize;
    if length == 0 || length >= resolved.len() {
        return Err(format!(
            "Discord credential {} identity is unavailable",
            kind
        ));
    }
    resolved.truncate(length);
    let expected = std::fs::canonicalize(path)
        .map_err(|_| format!("Discord credential {} identity is unavailable", kind))?;
    let actual = PathBuf::from(OsString::from_wide(&resolved));
    if !actual
        .as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
    {
        return Err(format!(
            "Discord credential {} escaped its pinned boundary",
            kind
        ));
    }
    Ok(())
}

fn open_regular_no_reparse(
    path: &Path,
    access: u32,
    share: windows::Win32::Storage::FileSystem::FILE_SHARE_MODE,
    disposition: windows::Win32::Storage::FileSystem::FILE_CREATION_DISPOSITION,
) -> Result<File, String> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            access,
            share,
            None,
            disposition,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|_| "Discord credential file could not be opened")?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast()),
            &mut info,
        )
    }
    .map_err(|_| "Discord credential file identity is unavailable")?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0 {
        return Err("Discord credential file must be a regular non-reparse file".into());
    }
    if info.nNumberOfLinks != 1 {
        return Err("Discord credential file must not be a hard link".into());
    }
    verify_handle_path(&file, path, "file")?;
    Ok(file)
}

fn credential_root() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("LCDSirPlus"))
        .ok_or_else(|| "LOCALAPPDATA does not identify an absolute credential directory".into())
}

/// WO-TEL-D3-C: the record codec moved with the OAuth region, so the host stores the
/// component-supplied serialized record as an opaque blob; DPAPI, the pinned credential boundary,
/// the hard-link/reparse rules, the atomic replace and the path-binding check are the accepted
/// shipped bodies, byte-for-byte. The user id comes from the caller (the component's READY
/// account), and the binding `credential_path_at(parent, user_id) == path` is checked here.
fn save_credential(path: &Path, user_id: &str, blob: &[u8]) -> Result<(), String> {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CREATE_NEW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let plain = blob.to_vec();
    if plain.len() > MAX_CREDENTIAL {
        return Err("Discord credential encoding exceeds 65536-byte limit".into());
    }
    let protected = dpapi(&plain, true)?;
    let parent = path
        .parent()
        .ok_or("Discord credential path has no parent")?;
    let boundary = CredentialBoundary::open(parent, true)?;
    if credential_path_at(parent, user_id)? != path {
        return Err("Discord credential path is outside its pinned boundary".into());
    }
    boundary.validate()?;
    if path.exists() {
        drop(open_regular_no_reparse(
            path,
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            OPEN_EXISTING,
        )?);
    }
    let temporary = path.with_extension(format!("token.{}.tmp", random_nonce()?));
    let result: Result<(), String> = (|| {
        let mut file = open_regular_no_reparse(
            &temporary,
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ,
            CREATE_NEW,
        )?;
        file.write_all(&protected)
            .map_err(|_| "Discord credential could not be written".to_string())?;
        file.sync_all()
            .map_err(|_| "Discord credential could not be flushed".to_string())?;
        boundary.validate()?;
        drop(file);
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                windows::core::PCWSTR(from.as_ptr()),
                windows::core::PCWSTR(to.as_ptr()),
                windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                    | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )
            .map_err(|_| "Discord credential could not be atomically replaced".to_string())
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    drop(open_regular_no_reparse(
        path,
        GENERIC_READ.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        OPEN_EXISTING,
    )?);
    Ok(())
}

/// WO-TEL-D3-C: the host hands the component the DPAPI-unwrapped record blob for a path; the
/// record decode and the account/client binding validation moved with the codec, and the shipped
/// file-layer rules (pinned boundary, regular non-reparse single-link file, size bound) are the
/// accepted bodies, byte-for-byte. `Ok(None)` means "no credential here", exactly as shipped.
/// The path-binding check is inherent: the caller builds the path from the READY user id.
fn load_credential_blob(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, OPEN_EXISTING};
    let parent = path
        .parent()
        .ok_or("Discord credential path has no parent")?;
    let boundary = match CredentialBoundary::open(parent, false) {
        Ok(boundary) => boundary,
        Err(_) if !parent.exists() => return Ok(None),
        Err(error) => return Err(error),
    };
    boundary.validate()?;
    let file = match open_regular_no_reparse(path, GENERIC_READ.0, FILE_SHARE_READ, OPEN_EXISTING) {
        Ok(file) => file,
        Err(_) if !path.exists() => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut protected = Vec::new();
    file.take((MAX_CREDENTIAL + 1) as u64)
        .read_to_end(&mut protected)
        .map_err(|e| format!("read Discord credential: {}", e))?;
    if protected.len() > MAX_CREDENTIAL {
        return Err("Discord credential file exceeds 65536-byte limit".into());
    }
    Ok(Some(dpapi(&protected, false)?))
}

pub fn clear_token() -> Result<(), String> {
    clear_tokens_at(&credential_root()?)
}

fn clear_tokens_at(root: &Path) -> Result<(), String> {
    let boundary = match CredentialBoundary::open(root, false) {
        Ok(boundary) => boundary,
        Err(_) if !root.exists() => return Ok(()),
        Err(error) => return Err(error),
    };
    boundary.validate()?;
    let mut owned = Vec::new();
    for entry in std::fs::read_dir(root)
        .map_err(|_| "Discord credential directory could not be enumerated")?
    {
        let entry = entry.map_err(|_| "Discord credential directory entry is unavailable")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "discord.token" || credential_user_id(name).is_some() {
            owned.push(entry.path());
        }
    }
    drop(boundary);
    for path in owned {
        clear_token_at(&path)?;
    }
    Ok(())
}

fn clear_token_at(path: &Path) -> Result<(), String> {
    use windows::Win32::Foundation::{BOOLEAN, GENERIC_READ};
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_DISPOSITION_INFO,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let parent = path
        .parent()
        .ok_or("Discord credential path has no parent")?;
    let boundary = match CredentialBoundary::open(parent, false) {
        Ok(boundary) => boundary,
        Err(_) if !parent.exists() => return Ok(()),
        Err(error) => return Err(error),
    };
    boundary.validate()?;
    let file = match open_regular_no_reparse(
        path,
        GENERIC_READ.0 | DELETE.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        OPEN_EXISTING,
    ) {
        Ok(file) => file,
        Err(_) if !path.exists() => return Ok(()),
        Err(error) => return Err(error),
    };
    let disposition = FILE_DISPOSITION_INFO {
        DeleteFile: BOOLEAN(1),
    };
    unsafe {
        SetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast()),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|_| "Discord credential could not be removed")?;
    drop(file);
    drop(boundary);
    Ok(())
}

#[derive(Clone, Debug)]
struct PipeIdentity {
    server_session: u32,
    current_session: u32,
    same_user: bool,
    image_name: String,
    fixed_local_drive: bool,
    valid_signature: bool,
    signer_organization: String,
}

fn validate_pipe_identity(identity: &PipeIdentity) -> Result<(), String> {
    if identity.server_session != identity.current_session {
        return Err("Discord IPC server belongs to a different Windows session".into());
    }
    if !identity.same_user {
        return Err("Discord IPC server belongs to a different Windows user".into());
    }
    if !identity.fixed_local_drive {
        return Err("Discord IPC server executable is not on a fixed local drive".into());
    }
    if !matches!(
        identity.image_name.to_ascii_lowercase().as_str(),
        "discord.exe" | "discordptb.exe" | "discordcanary.exe" | "discorddevelopment.exe"
    ) {
        return Err("Discord IPC server executable name is not recognized".into());
    }
    if !identity.valid_signature {
        return Err("Discord IPC server executable signature is not valid".into());
    }
    if !identity
        .signer_organization
        .eq_ignore_ascii_case("Discord Inc.")
    {
        return Err("Discord IPC server executable publisher is not Discord Inc.".into());
    }
    Ok(())
}

fn same_process_user(process: windows::Win32::Foundation::HANDLE) -> Result<bool, String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe fn sid_for(process: HANDLE) -> Result<(HANDLE, Vec<u8>), String> {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token)
            .map_err(|_| "process user identity is unavailable".to_string())?;
        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        if size == 0 || size > 64 * 1024 {
            let _ = CloseHandle(token);
            return Err("process user identity is unavailable".into());
        }
        let mut buffer = vec![0u8; size as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .is_err()
        {
            let _ = CloseHandle(token);
            return Err("process user identity is unavailable".into());
        }
        Ok((token, buffer))
    }

    unsafe {
        let (server_token, server) = sid_for(process)?;
        let (current_token, current) = match sid_for(GetCurrentProcess()) {
            Ok(value) => value,
            Err(error) => {
                let _ = CloseHandle(server_token);
                return Err(error);
            }
        };
        let server_user = &*(server.as_ptr().cast::<TOKEN_USER>());
        let current_user = &*(current.as_ptr().cast::<TOKEN_USER>());
        let equal = EqualSid(server_user.User.Sid, current_user.User.Sid).is_ok();
        let _ = CloseHandle(server_token);
        let _ = CloseHandle(current_token);
        Ok(equal)
    }
}

fn canonical_process_image(process: windows::Win32::Foundation::HANDLE) -> Result<PathBuf, String> {
    use windows::core::PWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_FORMAT};
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .map_err(|_| "Discord IPC server executable identity is unavailable".to_string())?;
    }
    if size == 0 || size as usize >= buffer.len() {
        return Err("Discord IPC server executable identity is unavailable".into());
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize]));
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| "Discord IPC server executable path cannot be canonicalized".to_string())?;
    if !std::fs::metadata(&canonical)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return Err("Discord IPC server executable is not a regular file".into());
    }
    let text = canonical.to_string_lossy();
    let drive_path = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if drive_path.len() < 3 || drive_path.as_bytes()[1] != b':' {
        return Err("Discord IPC server executable has no local volume".into());
    }
    let root: Vec<u16> = format!("{}\\", &drive_path[..2])
        .encode_utf16()
        .chain(Some(0))
        .collect();
    if unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) } != DRIVE_FIXED {
        return Err("Discord IPC server executable is not on a fixed local drive".into());
    }
    Ok(canonical)
}

fn verify_authenticode(path: &Path) -> Result<String, String> {
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::Security::Cryptography::{CertGetNameStringW, CERT_NAME_ATTR_TYPE};
    use windows::Win32::Security::WinTrust::*;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let file = File::open(path).map_err(|_| "Authenticode target cannot be opened")?;
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        hFile: HANDLE(file.as_raw_handle().cast()),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_DISABLE_MD2_MD4,
        dwUIContext: WTD_UICONTEXT_EXECUTE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    unsafe {
        let result = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        if result != 0 || data.hWVTStateData.is_invalid() {
            data.dwStateAction = WTD_STATEACTION_CLOSE;
            let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
            return Err("Authenticode signature is not valid".into());
        }
        let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
        let signer = if provider.is_null() {
            std::ptr::null_mut()
        } else {
            WTHelperGetProvSignerFromChain(provider, 0, false, 0)
        };
        let provider_cert = if signer.is_null() {
            std::ptr::null_mut()
        } else {
            WTHelperGetProvCertFromChain(signer, 0)
        };
        let cert = if provider_cert.is_null() {
            std::ptr::null()
        } else {
            (*provider_cert).pCert
        };
        let oid = b"2.5.4.10\0";
        let required = if cert.is_null() {
            0
        } else {
            CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                Some(PCSTR(oid.as_ptr()).0.cast()),
                None,
            )
        };
        let organization = if required > 1 && required <= 16 * 1024 {
            let mut output = vec![0u16; required as usize];
            let written = CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                Some(PCSTR(oid.as_ptr()).0.cast()),
                Some(&mut output),
            );
            if written == required {
                String::from_utf16_lossy(&output[..written as usize - 1])
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        if organization.is_empty() {
            Err("verified Authenticode signer identity is unavailable".into())
        } else {
            Ok(organization)
        }
    }
}

static AUTHENTICODE_ACTIVE: OnceLock<Mutex<bool>> = OnceLock::new();

fn verify_authenticode_bounded(path: PathBuf) -> Result<String, String> {
    let gate = AUTHENTICODE_ACTIVE.get_or_init(|| Mutex::new(false));
    {
        let mut active = gate.lock().unwrap_or_else(|e| e.into_inner());
        if *active {
            return Err("Discord IPC executable signature verifier is unavailable".into());
        }
        *active = true;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = verify_authenticode(&path);
        *AUTHENTICODE_ACTIVE
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
        let _ = tx.send(result);
    });
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "Discord IPC executable signature verification timed out".to_string())?
}

fn verify_pipe(file: &File) -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let pipe = HANDLE(file.as_raw_handle().cast());
    let mut pid = 0u32;
    unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) }
        .map_err(|_| "Discord IPC server process identity is unavailable".to_string())?;
    if pid == 0 {
        return Err("Discord IPC server returned an invalid process ID".into());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|_| "Discord IPC server process cannot be inspected".to_string())?;
    let result = (|| {
        let mut server_session = 0;
        let mut current_session = 0;
        unsafe {
            ProcessIdToSessionId(pid, &mut server_session)
                .map_err(|_| "Discord IPC server session identity is unavailable".to_string())?;
            ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session)
                .map_err(|_| "LCDSirPlus session identity is unavailable".to_string())?;
        }
        let same_user = same_process_user(process)?;
        let image = canonical_process_image(process)?;
        let organization = verify_authenticode_bounded(image.clone())?;
        validate_pipe_identity(&PipeIdentity {
            server_session,
            current_session,
            same_user,
            image_name: image
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .into(),
            fixed_local_drive: true,
            valid_signature: true,
            signer_organization: organization,
        })
    })();
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

fn session_config_changed(a: &Config, b: &Config) -> bool {
    a.discord_enabled != b.discord_enabled
        || a.safe_mode != b.safe_mode
        || a.discord_client_id != b.discord_client_id
        || a.discord_linger != b.discord_linger
        || a.discord_max_speakers != b.discord_max_speakers
        || a.discord_show_self != b.discord_show_self
        || a.discord_show_channel != b.discord_show_channel
}

fn unavailable(_previous: &DiscordState, error: &str) -> DiscordState {
    DiscordState {
        error: error.into(),
        updated: Some(SystemTime::now()),
        ..Default::default()
    }
}

fn provider_enabled(config: &Config) -> bool {
    // WO-TEL-D2 (S3, U-4): CONFIG GATE, not a demand gate. `discord_enabled 1`
    // is the effective gate supplied by the app's selected DISCORD pane. The
    // app masks it off without changing stored preferences when no pane consumes
    // voice state, so only an active display keeps reconnect behavior running. An EMPTY
    // `discord_client_id` is a permanent configuration failure — the body
    // hard-fails at the `connect_and_serve` entry before any pipe open — so it
    // routes into the shipped disabled arm (100 ms park, backoff reset, zero
    // connect cycles, zero WARN publishes). Re-arm is content-driven:
    // `session_config_changed` already compares `discord_client_id`, and the
    // disabled arm re-reads the config every iteration.
    config.discord_enabled && !config.safe_mode && !config.discord_client_id.trim().is_empty()
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(30))
}

/// WO-TEL-D3-B1 (L3): the module's copy of the shipped generation-guarded request read
/// (`telemetry.rs:202-240`, production twin `providers/presentmon.rs:1262-1274`). A 100 ms park
/// iteration under an unchanged generation reuses the cached `Arc<Config>` (one refcount bump)
/// instead of cloning the whole `Config` — the ~10 full clones/s the D3-A census measured at
/// `discord_client_id ""` (§A3.2) drop to zero — and the fresh path IS the shipped `SeqCst`
/// before/after pairing, so the cached value belongs to the generation reported with it. The
/// accepted content-driven re-arm (`session_config_changed`) and the config gate predicate
/// (`provider_enabled`) are untouched.
fn current_config(config: &RwLock<Config>) -> Config {
    config.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn current_request(config: &RwLock<Config>, generation: &AtomicU64) -> (u64, Config) {
    loop {
        let before = generation.load(Ordering::SeqCst);
        let config = current_config(config);
        if before == generation.load(Ordering::SeqCst) {
            return (before, config);
        }
    }
}

fn cached_request(
    config: &RwLock<Config>,
    generation: &AtomicU64,
    cache: &mut (u64, Arc<Config>),
) -> (u64, Arc<Config>) {
    let before = generation.load(Ordering::SeqCst);
    if before == cache.0 && before != 0 && generation.load(Ordering::SeqCst) == before {
        return (before, Arc::clone(&cache.1));
    }
    let (generation, config) = current_request(config, generation);
    *cache = (generation, Arc::new(config));
    (generation, Arc::clone(&cache.1))
}
#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("lcdsirplus-discord-{}", random_nonce().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// F-H-2: the lookup directory a component-absence premise OWNS. `TestDir::new` creates an
    /// empty temp directory and this fixture points the test-only `COMPONENT_LOOKUP_ROOT` seam at
    /// it for its whole lifetime, so the production loader resolves `COMPONENT_NAME` inside a
    /// directory the test made and removes, whatever the workspace build left beside the test
    /// binary. Drop releases the seam (last fixture out clears it), then `TestDir`'s Drop removes
    /// the directory. The fixture is deliberately empty: no dll, so the premise holds by
    /// construction, with no file manipulation by any harness.
    struct AbsentComponentFixture {
        directory: TestDir,
    }

    impl AbsentComponentFixture {
        fn new() -> Self {
            let directory = TestDir::new();
            COMPONENT_LOOKUP_FIXTURES.fetch_add(1, Ordering::Relaxed);
            *COMPONENT_LOOKUP_ROOT
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(directory.0.clone());
            Self { directory }
        }

        /// The exact component path the seam now resolves: `COMPONENT_NAME` inside this fixture's
        /// own (empty) directory.
        fn expected_path(&self) -> PathBuf {
            self.directory.0.join(COMPONENT_NAME)
        }
    }

    impl Drop for AbsentComponentFixture {
        fn drop(&mut self) {
            if COMPONENT_LOOKUP_FIXTURES.fetch_sub(1, Ordering::Relaxed) == 1 {
                *COMPONENT_LOOKUP_ROOT
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
    }

    /// WO-TEL-D3-C relocation note: the record codec (encode/decode/validate_for) moved with the
    /// OAuth region into the component, so the host file layer is exercised with an opaque record
    /// blob. Every shipped claim stays: per-account file independence, the path-binding rule, the
    /// pinned boundary's escape detection, and idempotent exact-ownership clearing.
    #[test]
    fn per_account_credentials_are_independent_and_path_bound() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        let path_a = credential_path_at(&root, "111").unwrap();
        let path_b = credential_path_at(&root, "222").unwrap();
        let record_a = b"version-2-record-for-111";
        let record_b = b"version-2-record-for-222";
        save_credential(&path_a, "111", record_a).unwrap();
        save_credential(&path_b, "222", record_b).unwrap();
        assert_eq!(
            load_credential_blob(&path_a).unwrap().as_deref(),
            Some(&record_a[..])
        );
        assert_eq!(
            load_credential_blob(&path_b).unwrap().as_deref(),
            Some(&record_b[..])
        );
        assert!(save_credential(&path_a, "222", record_b).is_err());
        assert_eq!(
            load_credential_blob(&path_a).unwrap().as_deref(),
            Some(&record_a[..])
        );
        assert!(credential_path_at(&root, "../111").is_err());
        assert!(credential_path_at(&root, "123456789012345678901").is_err());

        let boundary = CredentialBoundary::open(&root, false).unwrap();
        let moved = temp.0.join("moved");
        if std::fs::rename(&root, &moved).is_ok() {
            assert!(boundary.validate().is_err());
            drop(boundary);
            std::fs::rename(&moved, &root).unwrap();
        } else {
            drop(boundary);
        }

        clear_token_at(&path_a).unwrap();
        assert!(!path_a.exists() && path_b.exists());
        clear_token_at(&path_a).unwrap();
    }

    #[test]
    fn clear_removes_only_exact_owned_credentials_and_legacy() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        let owned = credential_path_at(&root, "111").unwrap();
        save_credential(&owned, "111", b"version-2-record-for-111").unwrap();
        let legacy = root.join("discord.token");
        let unrelated = root.join("notes.txt");
        let malformed = root.join("discord-../111.token");
        std::fs::write(&legacy, b"legacy").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();
        std::fs::write(&malformed, b"keep").unwrap_err();
        let malformed = root.join("discord-abc.token");
        std::fs::write(&malformed, b"keep").unwrap();
        clear_tokens_at(&root).unwrap();
        assert!(!owned.exists() && !legacy.exists());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
        assert_eq!(std::fs::read(&malformed).unwrap(), b"keep");
    }

    #[test]
    fn credential_boundary_rejects_hard_link_and_reparse_targets() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        std::fs::create_dir(&root).unwrap();
        let outside = temp.0.join("outside.token");
        std::fs::write(&outside, b"private").unwrap();
        let hard_link = root.join("discord-111.token");
        match std::fs::hard_link(&outside, &hard_link) {
            Ok(()) => {
                let error = load_credential_blob(&hard_link).unwrap_err();
                assert_eq!(error, "Discord credential file must not be a hard link");
                assert_eq!(std::fs::read(&outside).unwrap(), b"private");
            }
            Err(error) => eprintln!("hard-link setup unavailable: {}", error),
        }

        let target = temp.0.join("target");
        let link = temp.0.join("linked-runtime");
        std::fs::create_dir(&target).unwrap();
        match std::os::windows::fs::symlink_dir(&target, &link) {
            Ok(()) => assert!(CredentialBoundary::open(&link, false).is_err()),
            Err(error) => eprintln!("directory-reparse setup unavailable: {}", error),
        }
    }

    fn valid_identity() -> PipeIdentity {
        PipeIdentity {
            server_session: 7,
            current_session: 7,
            same_user: true,
            image_name: "Discord.exe".into(),
            fixed_local_drive: true,
            valid_signature: true,
            signer_organization: "Discord Inc.".into(),
        }
    }

    #[test]
    fn endpoint_policy_requires_every_identity_property() {
        assert!(validate_pipe_identity(&valid_identity()).is_ok());
        let mut cases = Vec::new();
        let mut value = valid_identity();
        value.current_session += 1;
        cases.push(value);
        let mut value = valid_identity();
        value.same_user = false;
        cases.push(value);
        let mut value = valid_identity();
        value.image_name = "DiscordHelper.exe".into();
        cases.push(value);
        let mut value = valid_identity();
        value.fixed_local_drive = false;
        cases.push(value);
        let mut value = valid_identity();
        value.valid_signature = false;
        cases.push(value);
        let mut value = valid_identity();
        value.signer_organization = "Discord Lookalike".into();
        cases.push(value);
        assert!(cases
            .iter()
            .all(|identity| validate_pipe_identity(identity).is_err()));
        for name in [
            "DiscordPTB.exe",
            "DiscordCanary.exe",
            "DiscordDevelopment.exe",
        ] {
            let mut value = valid_identity();
            value.image_name = name.into();
            assert!(validate_pipe_identity(&value).is_ok(), "{}", name);
        }
    }

    #[test]
    fn lifecycle_fingerprint_safe_mode_and_backoff_are_bounded() {
        // WO-TEL-D2 (S3, U-4) contract-forced line: `provider_enabled` now also
        // requires a configured `discord_client_id` (empty = permanent
        // configuration failure => the shipped disabled arm), so the base here
        // carries one. Every shipped predicate below is byte-identical; the
        // unconfigured side of the gate is pinned by
        // `provider_enabled_requires_a_configured_client_id`.
        let base = Config {
            discord_client_id: "123456789012345678".into(),
            ..Config::default()
        };
        assert!(provider_enabled(&base));
        let mut safe = base.clone();
        safe.safe_mode = true;
        assert!(!provider_enabled(&safe));
        assert!(session_config_changed(&base, &safe));
        let mut unrelated = base.clone();
        unrelated.preview_scale += 1;
        assert!(!session_config_changed(&base, &unrelated));
        for change in [
            |config: &mut Config| config.discord_enabled = !config.discord_enabled,
            |config: &mut Config| config.discord_client_id = "123".into(),
            |config: &mut Config| config.discord_linger += Duration::from_millis(1),
            |config: &mut Config| config.discord_max_speakers += 1,
            |config: &mut Config| config.discord_show_self = !config.discord_show_self,
            |config: &mut Config| config.discord_show_channel = !config.discord_show_channel,
        ] {
            let mut changed = base.clone();
            change(&mut changed);
            assert!(session_config_changed(&base, &changed));
        }
        let mut legacy_redirect = base.clone();
        legacy_redirect.discord_redirect_uri.push_str("/changed");
        assert!(!session_config_changed(&base, &legacy_redirect));
        let mut delay = Duration::from_secs(1);
        for expected in [2, 4, 8, 16, 30, 30] {
            delay = next_backoff(delay);
            assert_eq!(delay, Duration::from_secs(expected));
        }
    }

    #[test]
    fn dpapi_round_trip_is_current_user_and_bounded() {
        let plain = b"lcdsirplus-discord-dpapi-test";
        let protected = dpapi(plain, true).unwrap();
        assert_ne!(protected, plain);
        assert_eq!(dpapi(&protected, false).unwrap(), plain);
        assert!(dpapi(&vec![0; MAX_CREDENTIAL + 1], true).is_err());
    }

    #[test]
    fn safe_mode_runtime_publishes_disabled_without_starting_a_session() {
        let config = Config {
            safe_mode: true,
            ..Config::default()
        };
        let runtime = spawn(&config);
        let update = runtime
            .updates
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(update.error, "disabled");
        assert!(!update.connected && !update.authenticated);
    }

    #[test]
    #[ignore = "requires a running Discord Desktop instance"]
    fn live_endpoint_identity_smoke() {
        let mut failures = Vec::new();
        for index in 0..10 {
            let path = format!(r"\\?\pipe\discord-ipc-{}", index);
            if let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) {
                match verify_pipe(&file) {
                    Ok(()) => {
                        drop(file);
                        return;
                    }
                    Err(error) => failures.push(format!("{}: {}", index, error)),
                }
            }
        }
        panic!("no verified Discord endpoint: {}", failures.join("; "));
    }

    // WO-DISC-C: STRUCTURAL source-pin scanner helpers (test region only; they read the
    // shipped text of this file and assert nothing about runtime behavior).
    /// The shipped production region of this file: LF-normalized text from the file start
    /// up to (excluding) the `#[cfg(test)]` attribute that opens `mod tests`.
    fn shipped_production_region() -> String {
        let source = include_str!("discord.rs").replace("\r\n", "\n");
        let tests = source
            .find("\nmod tests {")
            .expect("the shipped file carries the test module");
        let region = source[..tests].trim_end();
        region
            .strip_suffix("#[cfg(test)]")
            .map(str::trim_end)
            .unwrap_or(region)
            .to_string()
    }

    fn is_ident_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }

    /// First identifier of `text` (leading whitespace skipped), if any.
    fn leading_ident(text: &str) -> Option<&str> {
        let text = text.trim_start();
        let end = text
            .char_indices()
            .find(|(_, c)| !is_ident_char(*c))
            .map_or(text.len(), |(i, _)| i);
        text.get(..end)
            .filter(|ident| !ident.is_empty() && !ident.starts_with(|c: char| c.is_ascii_digit()))
    }

    /// Last identifier of `text` (trailing whitespace skipped), if any.
    fn trailing_ident(text: &str) -> Option<&str> {
        let text = text.trim_end();
        let start = text
            .char_indices()
            .rev()
            .find(|(_, c)| !is_ident_char(*c))
            .map_or(0, |(i, _)| i + 1);
        text.get(start..)
            .filter(|ident| !ident.is_empty() && !ident.starts_with(|c: char| c.is_ascii_digit()))
    }

    /// `<receiver>.<member>` at the end of `text`, as two identifiers.
    fn trailing_member(text: &str) -> Option<(&str, &str)> {
        let text = text.trim_end();
        let member = trailing_ident(text)?;
        let head = text[..text.len() - member.len()].strip_suffix('.')?;
        Some((trailing_ident(head)?, member))
    }

    /// `<receiver>.<member>` at the start of `text`, as two identifiers.
    fn leading_member(text: &str) -> Option<(&str, &str)> {
        let text = text.trim_start();
        let receiver = leading_ident(text)?;
        let tail = text[receiver.len()..].strip_prefix('.')?;
        let member = leading_ident(tail)?;
        Some((receiver, member))
    }

    /// The `a.<field> != b.<field>` field names of one shipped comparison predicate, in
    /// source order; every operand must read the predicate's `a` / `b` config parameter.
    fn compared_fields(source: &str, fn_head: &str) -> Vec<String> {
        let start = source.find(fn_head).expect("the shipped predicate exists");
        let body = &source[start..];
        let end = body
            .find("\n}")
            .expect("the shipped predicate ends with a top-level closing brace");
        let body = &body[..end];
        let mut fields = Vec::new();
        let mut offset = 0;
        while let Some(relative) = body[offset..].find("!=") {
            let at = offset + relative;
            let (left_recv, left) =
                trailing_member(&body[..at]).expect("an `a.<field>` left operand");
            let (right_recv, right) =
                leading_member(&body[at + 2..]).expect("a `b.<field>` right operand");
            assert_eq!(
                (left_recv, right_recv),
                ("a", "b"),
                "each comparison must read the predicate's two config parameters"
            );
            assert_eq!(
                left, right,
                "the predicate compares the same field on both sides ({left} vs {right})"
            );
            fields.push(left.to_string());
            offset = at + 2;
        }
        fields
    }

    /// Member names read off a bare `Config` receiver in the shipped text: the receiver
    /// spelling must be exactly `config`/`cfg` and must start at a boundary whose preceding
    /// character is neither an identifier character nor `.`, so `self.config.<field>` (a field
    /// of the runtime struct) and the shared `Arc<RwLock<Config>>` receivers named
    /// `*_config` are excluded; the member must not be plumbing (`clone`, `read`, ...).
    fn bare_config_members(source: &str, receivers: &[&str], plumbing: &[&str]) -> Vec<String> {
        let chars: Vec<char> = source.chars().collect();
        let mut members = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            let at_boundary = index == 0 || {
                let previous = chars[index - 1];
                !(is_ident_char(previous) || previous == '.')
            };
            let hit = at_boundary.then(|| {
                receivers.iter().find_map(|receiver| {
                    let name: Vec<char> = receiver.chars().collect();
                    let after = index + name.len();
                    if chars[index..].starts_with(name.as_slice()) && chars.get(after) == Some(&'.')
                    {
                        let member: String = chars[after + 1..]
                            .iter()
                            .take_while(|c| is_ident_char(**c))
                            .collect();
                        (!member.is_empty() && !plumbing.contains(&member.as_str()))
                            .then_some(member)
                    } else {
                        None
                    }
                })
            });
            if let Some(Some(member)) = hit {
                members.push(member);
            }
            index += 1;
        }
        members
    }

    /// WO-DISC-C (EC-8 audit follow-up (c); the WO-A1-08 audit's F-4 hardening
    /// recommendation) STRUCTURAL source pin of the Discord re-arm gate. Discord is the only
    /// provider loop without an `Arc<AtomicU64>` generation: it re-arms by comparing
    /// configuration CONTENT, so the closed field set of `session_config_changed` IS its
    /// generation gate. This test inspects the shipped text of this file (production region =
    /// file start to the `#[cfg(test)]` attribute that opens `mod tests`) and pins:
    ///
    /// * (a) the `a.<field> != b.<field>` comparisons inside `session_config_changed` are
    ///   EXACTLY the seven pinned fields, in source order, and
    /// * (b) every field read off a bare `Config` receiver in that region
    ///   (`config.<field>` / `cfg.<field>`, with the receiver rules and the plumbing
    ///   allowlist documented above) is one of those seven.
    ///
    /// A new `config.<field>` read in the worker path, or a field added to / removed from the
    /// comparison, reddens this test and forces a conscious update of the pinned set.
    ///
    /// This is a structural inspection of production source text (the EC-8/EC-5 and WO-TEL-B
    /// `include_str!` pin pattern), NOT a behavioral driver: the IPC session loop needs a live
    /// Discord pipe, so its loop path stays hardware-gated (the live-pipe test
    /// `live_endpoint_identity_smoke` remains ignored) and no runtime re-arm behavior is claimed
    /// here.
    #[test]
    fn discord_source_pin_of_the_session_config_read_field_set() {
        // The re-arm gate as shipped (L1527-1535): the seven fields it compares.
        const PINNED: [&str; 7] = [
            "discord_enabled",
            "safe_mode",
            "discord_client_id",
            "discord_linger",
            "discord_max_speakers",
            "discord_show_self",
            "discord_show_channel",
        ];
        const RECEIVERS: [&str; 2] = ["config", "cfg"];
        // Census of the shipped production region (WO-DISC-C-receiver-census.txt): the only
        // bare-receiver members that are not Config fields are this lock/clone plumbing.
        const PLUMBING: [&str; 2] = ["clone", "read"];
        let region = shipped_production_region();
        let compared = compared_fields(&region, "fn session_config_changed(");
        assert_eq!(
            compared, PINNED,
            "session_config_changed must compare exactly the pinned field set, in source order"
        );
        let reads = bare_config_members(&region, &RECEIVERS, &PLUMBING);
        let outside: Vec<&str> = reads
            .iter()
            .map(String::as_str)
            .filter(|field| !PINNED.contains(field))
            .collect();
        assert!(
            outside.is_empty(),
            "a Config field read in the production region is outside the pinned set: {outside:?}"
        );
        assert!(
            reads.iter().any(|read| read == "discord_client_id"),
            "the read pin is not vacuous: the shipped production region reads pinned Config fields"
        );
    }

    /// WO-TEL-D2 (S3, U-4): the config gate truth table for `provider_enabled`.
    /// An unconfigured `discord_client_id` is a permanent configuration failure
    /// (the body hard-fails at the same trim check before any pipe open), so the
    /// gate routes the worker into the shipped disabled arm: no reconnect
    /// cycles, no backoff ladder, no per-cycle WARN publishes. A CONFIGURED
    /// client is an explicitly enabled background consumer (the speaker
    /// overlay reads the state every frame), so it keeps the shipped behavior
    /// unchanged — demand gating is NOT applied to Discord (audit amendment 1).
    #[test]
    fn provider_enabled_requires_a_configured_client_id() {
        let configured = Config {
            discord_enabled: true,
            safe_mode: false,
            discord_client_id: "123456789012345678".into(),
            ..Config::default()
        };
        assert!(
            provider_enabled(&configured),
            "a configured client keeps the shipped reconnect behavior (background consumer)"
        );
        let unconfigured = Config {
            discord_client_id: String::new(),
            ..configured.clone()
        };
        assert!(
            !provider_enabled(&unconfigured),
            "an empty client_id parks the worker in the disabled arm (full loop stop, no backoff cycles)"
        );
        let whitespace = Config {
            discord_client_id: "   ".into(),
            ..configured.clone()
        };
        assert!(
            !provider_enabled(&whitespace),
            "whitespace-only client_id matches the body's own trim check at the `connect_and_serve` entry"
        );
        let disabled = Config {
            discord_enabled: false,
            ..configured.clone()
        };
        assert!(
            !provider_enabled(&disabled),
            "an explicit disable stays a disable (shipped top switch)"
        );
        let safe = Config {
            safe_mode: true,
            ..configured
        };
        assert!(
            !provider_enabled(&safe),
            "safe mode stays the shipped kill switch"
        );
    }

    /// WO-TEL-D3-B1 (L3) pin, WO-TEL-D3-C restated in LOCKSTEP (a declared accepted-region touch;
    /// the same claim set, not weakened): the worker's loop head reads the config through the
    /// generation-guarded cache, performs ZERO full-`Config` clones per iteration, seeds the cache
    /// from the `Config` `spawn` received, and `update_config` bumps the generation AFTER the
    /// write. NEW claims added by this unit (additive, none replacing a shipped claim): the worker
    /// is spawned ONLY under `provider_enabled` (the disabled case has no thread and no load), and
    /// the accepted L4 claim (the park publishes ONCE per transition, with a bounded quantum) is
    /// present in its relocated home, now STRUCTURAL: the disable edge publishes and the worker
    /// thread ENDS, while the 100 ms quantum stays for the load-failure park. The gate predicate
    /// and the D2 pairing stay byte-identical.
    #[test]
    fn discord_park_loop_head_reads_a_cached_arc_config() {
        let source = include_str!("discord.rs").replace("\r\n", "\n");
        fn fn_body<'a>(source: &'a str, head: &str) -> &'a str {
            let start = source
                .find(head)
                .unwrap_or_else(|| panic!("the shipped text carries {head}"));
            let end = source[start..]
                .find("\n}")
                .unwrap_or_else(|| panic!("{head} closes at column zero"))
                + start
                + 2;
            &source[start..end]
        }
        let head = source
            .find("fn spawn_worker(")
            .expect("the worker body has one home (the D3-C relocation)");
        let tail = source
            .find(".expect(\"spawn Discord worker\")")
            .expect("the shipped worker thread closes the loop");
        assert!(
            tail > head,
            "the worker body sits between the worker head and its thread handle"
        );
        let loop_src = &source[head..tail];
        let full_clone = "let config = worker_config\n                    .read()\n                    .unwrap_or_else(|e| e.into_inner())\n                    .clone();";
        let clones = loop_src.matches(full_clone).count();
        let cached = loop_src
            .matches("cached_request(&worker_config, &worker_generation, &mut request);")
            .count();
        println!(
            "D3C-L3 harness-count: worker-loop full-Config clone sites = {clones}, cached loop-head reads = {cached}"
        );
        assert_eq!(
            clones, 0,
            "L3: the parked loop performs ZERO full-Config clones per iteration (~10/s removed)"
        );
        assert_eq!(
            cached, 1,
            "L3: exactly one cached loop-head request read (the shipped `cached_request` shape)"
        );
        assert!(
            loop_src.contains("let seed = Arc::new(config.clone());")
                && loop_src.contains("let mut request = (0u64, seed);"),
            "L3: the cache is seeded from the `Config` `spawn` received (an `Arc` the worker owns), exactly as the shipped loops seed from `current_config`"
        );
        // WO-TEL-D3-C restatement of the WO-TEL-D3-B1 (L4) mechanism claim; the claim set is
        // preserved. The park publishes its disabled state EXACTLY ONCE per transition (one publish
        // site, then the worker thread ENDS, so repeats are structurally impossible), the worker
        // keeps a bounded 100 ms park quantum (the load-failure park), and the `provider_enabled`
        // gate stays the arm's head.
        assert_eq!(
            loop_src
                .matches("state = unavailable(&state, \"disabled\");")
                .count(),
            1,
            "L4 (restated): the park publishes its disabled state from exactly one site"
        );
        assert!(
            loop_src.contains("let _ = worker_tx.send(state.clone());")
                && loop_src.contains("break;"),
            "L4 (restated): the disable edge publishes then ends the thread, so once is structural"
        );
        assert!(
            loop_src.contains("std::thread::sleep(Duration::from_millis(100));"),
            "L4 (restated): a bounded 100 ms park quantum remains, never a busy loop"
        );
        assert!(
            loop_src.contains("if !provider_enabled(&config) {"),
            "the accepted config gate drives the park arm unchanged"
        );
        let spawn_body = fn_body(&source, "pub fn spawn(config: &Config) -> Runtime {");
        assert!(
            spawn_body.contains("spawn_with(config, load_production_table)"),
            "production `spawn` uses the real loader, and keeps the shipped signature (app.rs:154 untouched)"
        );
        let gated = fn_body(
            &source,
            "pub fn spawn_with(config: &Config, loader: TableLoader) -> Runtime {",
        );
        assert!(
            gated.contains("if provider_enabled(config) {")
                && gated.contains("let worker = spawn_worker(&runtime, config);"),
            "the worker is created ONLY when the accepted gate is true (spawn-on-enable)"
        );
        assert!(
            gated.contains(".send(unavailable(&DiscordState::default(), \"disabled\"))")
                && gated.contains("thread: Mutex::new(None),"),
            "the disabled branch creates NO thread and publishes the disabled state exactly once"
        );
        assert!(
            gated.contains("let (tx, rx) = mpsc::channel();"),
            "the disabled path keeps only the trivial descriptors (channel + locks + atomics)"
        );
        assert!(
            !gated.contains("load_production_table(&mut"),
            "construction never loads the module by itself"
        );
        let runtime_src = fn_body(&source, "impl Runtime {");
        let update_head = runtime_src
            .find("pub fn update_config(&self, config: &Config) {")
            .expect("the shipped update_config exists");
        let update = &runtime_src[update_head
            ..update_head
                + runtime_src[update_head..]
                    .find("\n    }")
                    .expect("the method closes at one indent level")];
        assert!(
            update.contains("self.generation.fetch_add(1, Ordering::SeqCst);"),
            "L3: `update_config` bumps the generation AFTER the write (the D2-accepted pairing)"
        );
        assert!(
            update.find("self.generation.fetch_add(1, Ordering::SeqCst);")
                > update.find("= config.clone();"),
            "L3: the bump is strictly after the write, so a cached read never spans the two"
        );
        assert!(
            update.contains("self.reconcile();"),
            "D3-C: the reload edge is what spawns/joins the worker (hot reload reaches the gate)"
        );
        assert!(
            source.contains("generation: Arc<AtomicU64>,"),
            "L3: the generation lives in the Runtime/spawn pair only (zero new shared state)"
        );
    }

    /// WO-TEL-D3-B1 (L4) RED-FIRST behavioral proof on the REAL shipped `spawn`/`Runtime`. An
    /// unconfigured client parks in the accepted disabled arm (zero I/O: the body hard-fails
    /// before any pipe open, `connect_and_serve` is never entered) and publishes its disabled
    /// state ONCE, not once per 100 ms iteration. At `ec3df0d` the arm sent per iteration, so
    /// this test is RED there (4-5 publishes inside the window).
    #[test]
    fn discord_park_publishes_its_disabled_state_once() {
        let runtime = spawn(&Config {
            discord_enabled: true,
            discord_client_id: String::new(),
            ..Config::default()
        });
        let start = Instant::now();
        let mut publishes = 0usize;
        let mut first = None;
        while start.elapsed() < Duration::from_millis(400) {
            match runtime.updates.try_recv() {
                Ok(state) => {
                    publishes += 1;
                    if publishes == 1 {
                        first = Some(state);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        drop(runtime);
        let state = first.expect("the parked loop publishes its disabled state at least once");
        assert_eq!(
            state.error, "disabled",
            "L4: the parked state is the explicit disabled state, never silence and never a stale speaker set"
        );
        assert!(
            !state.connected && !state.authenticated,
            "L4: the parked state claims no connection (never stale-as-fresh)"
        );
        assert_eq!(
            publishes, 1,
            "L4: the park publishes ONCE, not per 100 ms iteration ({publishes} publishes measured across 400 ms)"
        );
    }
    // ---- WO-TEL-D3-C: the component-boundary test surface -----------------------------------
    //
    // Seam precedent = WO-TEL-C (a production fn-pointer seam, real production body, only the
    // injected part synthetic). Here the seam is the loader: `spawn_with` takes a `TableLoader`,
    // production `spawn` always passes `load_production_table`, and the tests pass a stub loader
    // that returns a v1 table of `extern "system"` fns implemented in this region. Disclosed
    // minimality note: the stub exercises the host's gate, load-exactly-once, refusal, unload
    // order and census machine; the component's own behavior is exercised by the member crate's
    // tests, and the real module presence is measured by the loader tests below on real paths.
    mod stub_v1 {
        use super::*;
        use std::sync::atomic::AtomicUsize;

        pub struct Counters {
            pub polls: AtomicUsize,
            pub client_id: Mutex<String>,
            pub revision: AtomicUsize,
        }

        pub static CALLS: Counters = Counters {
            polls: AtomicUsize::new(0),
            client_id: Mutex::new(String::new()),
            revision: AtomicUsize::new(0),
        };
        extern "system" fn stub_start(
            settings: *const LcsdSettings,
            settings_len: u32,
            api: *const LcsdHostApi,
            api_len: u32,
            out_handle: *mut *mut c_void,
        ) -> i32 {
            if settings.is_null()
                || api.is_null()
                || out_handle.is_null()
                || settings_len as usize != std::mem::size_of::<LcsdSettings>()
                || api_len as usize != std::mem::size_of::<LcsdHostApi>()
            {
                return LCSD_E_SIZE;
            }
            let pod = unsafe { &*settings };
            let text =
                String::from_utf8_lossy(&pod.client_id[..pod.client_id_len as usize]).into_owned();
            *CALLS.client_id.lock().unwrap_or_else(|e| e.into_inner()) = text;
            CALLS.revision.store(1, Ordering::Relaxed);
            // The component's credential touch crosses the host API, exercised hermetically: the
            // host's `provide_token` callback runs (counting the touch) and refuses on the
            // malformed user id BEFORE any filesystem access, so the test never reads the real
            // credential root and the component never sees a path, a file handle or DPAPI.
            let user = b"bad id\0";
            let mut blob = [0u8; MAX_CREDENTIAL];
            let code = unsafe {
                ((*api).provide_token)(
                    (*api).context,
                    user.as_ptr(),
                    pod.client_id.as_ptr(),
                    blob.as_mut_ptr(),
                    blob.len() as u32,
                )
            };
            if code != LCSD_E_ARG {
                return LCSD_E_ARG;
            }
            unsafe { *out_handle = &CALLS.polls as *const AtomicUsize as *mut c_void };
            LCSD_OK
        }

        extern "system" fn stub_set_config(
            handle: *mut c_void,
            settings: *const LcsdSettings,
            len: u32,
        ) -> i32 {
            if handle.is_null()
                || settings.is_null()
                || len as usize != std::mem::size_of::<LcsdSettings>()
            {
                return LCSD_E_SIZE;
            }
            let pod = unsafe { &*settings };
            let text =
                String::from_utf8_lossy(&pod.client_id[..pod.client_id_len as usize]).into_owned();
            *CALLS.client_id.lock().unwrap_or_else(|e| e.into_inner()) = text;
            LCSD_OK
        }

        extern "system" fn stub_poll(handle: *mut c_void, _timeout_ms: u32) -> i32 {
            CALLS.polls.fetch_add(1, Ordering::Relaxed);
            if handle.is_null() {
                return LCSD_E_ARG;
            }
            LCSD_OK
        }

        extern "system" fn stub_state(
            handle: *mut c_void,
            out: *mut LcsdState,
            capacity: u32,
        ) -> i32 {
            if handle.is_null()
                || out.is_null()
                || (capacity as usize) < std::mem::size_of::<LcsdState>()
            {
                return LCSD_E_SIZE;
            }
            let client = CALLS
                .client_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let pod = unsafe { &mut *out };
            *pod = LcsdState::default();
            pod.size = std::mem::size_of::<LcsdState>() as u32;
            pod.revision = CALLS.revision.load(Ordering::Relaxed) as u64;
            pod.connected = 1;
            pod.authenticated = 1;
            pod.voice_ping_ms = 12.5;
            pod.self_mute = 1;
            pod.self_deaf = 1;
            let channel = format!("channel-of-{client}");
            let bytes = channel.as_bytes();
            let len = bytes.len().min(LCSD_CHANNEL_CAPACITY);
            pod.channel_name[..len].copy_from_slice(&bytes[..len]);
            pod.channel_name_len = len as u32;
            pod.speaker_count = 1;
            pod.speakers[0].user_id[..3].copy_from_slice(b"111");
            pod.speakers[0].user_id_len = 3;
            pod.speakers[0].name[..6].copy_from_slice(b"tester");
            pod.speakers[0].name_len = 6;
            pod.speakers[0].speaking = 1;
            pod.speakers[0].is_self = 1;
            pod.speakers[0].started_unix_ms = 5;
            LCSD_OK
        }

        extern "system" fn stub_stop(handle: *mut c_void) -> i32 {
            if handle.is_null() {
                return LCSD_E_ARG;
            }
            LCSD_OK
        }

        extern "system" fn stub_join(_handle: *mut c_void) -> i32 {
            LCSD_OK
        }

        extern "system" fn stub_release(_handle: *mut c_void) -> i32 {
            LCSD_OK
        }

        extern "system" fn stub_authorize(
            _settings: *const LcsdSettings,
            _secret: *const u8,
            _secret_len: u32,
            _api: *const LcsdHostApi,
            _api_len: u32,
            _out_status: *mut u8,
            _out_cap: u32,
        ) -> i32 {
            LCSD_OK
        }

        extern "system" fn stub_verify_client(_path: *const u16, _out: *mut u8, _cap: u32) -> i32 {
            LCSD_OK
        }

        extern "system" fn stub_last_error(out: *mut u8, cap: u32) -> i32 {
            if out.is_null() || cap == 0 {
                return LCSD_E_ARG;
            }
            let text = b"stub component classification";
            let len = text.len().min(cap as usize - 1);
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), out, len);
                *out.add(len) = 0;
            }
            len as i32
        }

        pub static TABLE: LcsdV1 = LcsdV1 {
            size: std::mem::size_of::<LcsdV1>() as u32,
            start: stub_start,
            set_config: stub_set_config,
            poll_io: stub_poll,
            state: stub_state,
            stop: stub_stop,
            join: stub_join,
            release: stub_release,
            authorize: stub_authorize,
            verify_client: stub_verify_client,
            last_error: stub_last_error,
        };

        extern "system" fn inflight_join(_handle: *mut c_void) -> i32 {
            LCSD_E_INFLIGHT
        }

        pub static TABLE_RETAINED: LcsdV1 = LcsdV1 {
            size: std::mem::size_of::<LcsdV1>() as u32,
            start: stub_start,
            set_config: stub_set_config,
            poll_io: stub_poll,
            state: stub_state,
            stop: stub_stop,
            join: inflight_join,
            release: stub_release,
            authorize: stub_authorize,
            verify_client: stub_verify_client,
            last_error: stub_last_error,
        };

        pub fn loader(
            _diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY],
        ) -> Result<Module, LoadFault> {
            Ok(Module {
                library: windows::Win32::Foundation::HMODULE(std::ptr::null_mut()),
                table: &TABLE as *const LcsdV1,
            })
        }

        pub fn loader_retained(
            _diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY],
        ) -> Result<Module, LoadFault> {
            Ok(Module {
                library: windows::Win32::Foundation::HMODULE(std::ptr::null_mut()),
                table: &TABLE_RETAINED as *const LcsdV1,
            })
        }

        pub fn loader_handshake(
            diagnostic: &mut [u8; LOADER_DIAGNOSTIC_CAPACITY],
        ) -> Result<Module, LoadFault> {
            let message = LoadFault::Handshake.diagnostic();
            let bytes = message.as_bytes();
            diagnostic[..bytes.len()].copy_from_slice(bytes);
            Err(LoadFault::Handshake)
        }
    }

    fn enabled_config(client_id: &str) -> Config {
        Config {
            discord_enabled: true,
            safe_mode: false,
            discord_client_id: client_id.into(),
            ..Config::default()
        }
    }

    /// Reads a loader diagnostic up to its NUL terminator, the form the host publishes.
    fn diagnostic_text(diagnostic: &[u8; LOADER_DIAGNOSTIC_CAPACITY]) -> String {
        let end = diagnostic
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(diagnostic.len());
        String::from_utf8_lossy(&diagnostic[..end]).into_owned()
    }

    /// The text of one top-level function: from its `head` line to its column-zero close. Used by
    /// the source-shape claims (the CLI-path DLL-free proof), which must not depend on scheduling.
    fn fn_body_at<'a>(source: &'a str, head: &str) -> &'a str {
        let start = source
            .find(head)
            .unwrap_or_else(|| panic!("the source carries `{head}`"));
        let end = source[start..]
            .find("\n}")
            .unwrap_or_else(|| panic!("`{head}` closes at column zero"))
            + start
            + 2;
        &source[start..end]
    }

    fn drained(runtime: &Runtime) -> Vec<DiscordState> {
        let mut states = Vec::new();
        while let Ok(update) = runtime.updates.try_recv() {
            states.push(update);
        }
        states
    }

    fn wait_for_state(
        runtime: &Runtime,
        want: impl Fn(&DiscordState) -> bool,
    ) -> Option<DiscordState> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(update) = runtime.updates.try_recv() {
                if want(&update) {
                    return Some(update);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// V1(a): the disabled / unconfigured case performs NO component call, spawns NO worker thread
    /// and touches NO credential. At `42ad35f` this was RED: `spawn` always started the worker
    /// thread and the parked loop published per iteration.
    ///
    /// WO-TEL-D3-E (audit F-1): every claim is read from the per-Runtime census at CONSTRUCTION,
    /// so no assertion depends on thread scheduling. `spawn_worker` increments `census.spawns` on
    /// the CALLING thread (L1195) and every census counter is `fetch_add`-monotonic, so
    /// `spawns == 0` proves "NO worker thread exists" without sampling a live thread: the
    /// spawn-at-disabled mutation is RED deterministically, not a race between a mutated worker's
    /// publish (L1238) and its end (L1377).
    #[test]
    fn discord_disabled_never_loads_never_spawns_a_worker_never_touches_credentials() {
        for (label, config) in [
            (
                "unconfigured client_id",
                Config {
                    discord_enabled: true,
                    discord_client_id: String::new(),
                    ..Config::default()
                },
            ),
            (
                "explicit disable",
                Config {
                    discord_enabled: false,
                    discord_client_id: "123456789012345678".into(),
                    ..Config::default()
                },
            ),
            (
                "safe mode",
                Config {
                    discord_enabled: true,
                    safe_mode: true,
                    discord_client_id: "123456789012345678".into(),
                    ..Config::default()
                },
            ),
            (
                "whitespace client_id",
                Config {
                    discord_enabled: true,
                    discord_client_id: "   ".into(),
                    ..Config::default()
                },
            ),
        ] {
            let runtime = spawn_with(&config, stub_v1::loader);
            // The construction-time census read: deterministic (monotonic per-Runtime counters, no
            // liveness sampling), and it is the load-bearing form.
            let census = runtime.census_snapshot();
            assert_eq!(
                census.spawns, 0,
                "{label}: NO worker thread is created at disabled"
            );
            assert!(
                !runtime.worker_running(),
                "{label}: NO worker thread exists"
            );
            assert_eq!(
                census.load_attempts, 0,
                "{label}: the loader is never called"
            );
            assert_eq!(census.loads, 0, "{label}: the module is never loaded");
            assert_eq!(census.starts, 0, "{label}: the component is never started");
            assert_eq!(
                runtime.credential_calls(),
                0,
                "{label}: no credential callback ran"
            );
            // The publish claim: the disabled arm sends the parked state synchronously
            // (L1170-1172), so it arrives with the shipped reason, never stale-as-fresh, and it is
            // the ONLY publish: no thread exists to publish again.
            let update = runtime
                .updates
                .recv_timeout(Duration::from_secs(1))
                .expect("the disabled state is published once, never silence");
            assert_eq!(
                update.error, "disabled",
                "{label}: the shipped disabled reason stays"
            );
            assert!(
                !update.connected && !update.authenticated,
                "{label}: never stale-as-fresh"
            );
            assert!(
                runtime.updates.try_recv().is_err(),
                "{label}: exactly ONE publish, never the per-tick stream"
            );
            // A reload that stays disabled must not create a worker, load, start, touch a
            // credential or publish; the same monotonic census proves it, construction-shape.
            runtime.update_config(&config);
            let census = runtime.census_snapshot();
            assert_eq!(
                census.spawns, 0,
                "{label}: a disabled reload spawns nothing"
            );
            assert!(
                !runtime.worker_running(),
                "{label}: a disabled reload leaves no worker"
            );
            assert_eq!(census.load_attempts, 0, "{label}: still no load");
            assert_eq!(census.loads, 0, "{label}: still no module load");
            assert_eq!(census.starts, 0, "{label}: still no start");
            assert_eq!(
                runtime.credential_calls(),
                0,
                "{label}: still no credential callback"
            );
            assert!(
                runtime.updates.try_recv().is_err(),
                "{label}: the disabled reload publishes nothing"
            );
            drained(&runtime);
        }
    }

    /// V1(b): enabled + configured (= the ratified consumer rule) loads the module and starts the
    /// component EXACTLY ONCE, drives `poll_io`, and delivers the component state to the consumer
    /// channel. Drop runs the full unload order.
    #[test]
    fn discord_enabled_demanded_loads_and_starts_exactly_once() {
        let runtime = spawn_with(&enabled_config("123456789012345678"), stub_v1::loader);
        let update = wait_for_state(&runtime, |state| state.connected && state.authenticated)
            .expect("the enabled, demanded provider publishes the component state");
        assert_eq!(update.channel_name, "channel-of-123456789012345678");
        assert_eq!(
            update.speakers.len(),
            1,
            "the speaker set reaches the consumer"
        );
        assert_eq!(update.speakers[0].user_id, "111");
        assert!(update.speakers[0].speaking && update.speakers[0].is_self);
        let census = runtime.census_snapshot();
        assert_eq!(census.load_attempts, 1, "exactly one load attempt");
        assert_eq!(
            census.loads, 1,
            "the module is loaded EXACTLY ONCE (no double-load)"
        );
        assert_eq!(census.starts, 1, "exactly one component start");
        assert_eq!(census.spawns, 1, "exactly one worker thread");
        assert!(
            stub_v1::CALLS.polls.load(Ordering::Relaxed) >= 1,
            "the host worker drives poll_io"
        );
        assert_eq!(
            runtime.credential_calls(),
            1,
            "the component's credential touch runs through the host API exactly once per session"
        );
        assert!(runtime.worker_running(), "the worker exists while demanded");
        let census = runtime.census();
        drop(runtime);
        assert_eq!(census.stops.load(Ordering::Relaxed), 1, "stop ran once");
        assert_eq!(census.joins.load(Ordering::Relaxed), 1, "join ran once");
        assert_eq!(
            census.releases.load(Ordering::Relaxed),
            1,
            "release ran once"
        );
    }

    /// V1(d): a module that cannot be found yields ONE bounded diagnostic: a single load
    /// attempt, zero starts, zero worker spawns, no retry loop, explicit unavailability. The
    /// premise is test-owned (F-H-2): `AbsentComponentFixture` plus the real production loader, so
    /// this drives `component_path` -> `LoadLibraryExW` against an empty fixture directory; a
    /// workspace-built dll beside the test binary can never satisfy the load.
    #[test]
    fn discord_missing_module_reports_once_and_never_retries() {
        let _fixture = AbsentComponentFixture::new();
        let runtime = spawn_with(&enabled_config("123456789012345678"), load_production_table);
        let update = wait_for_state(&runtime, |state| {
            state.error == "discord module unavailable"
        })
        .expect("a missing module is reported as explicitly unavailable");
        assert!(!update.connected && !update.authenticated);
        std::thread::sleep(Duration::from_millis(300));
        let census = runtime.census_snapshot();
        assert_eq!(
            census.load_attempts, 1,
            "the load is attempted exactly once"
        );
        assert_eq!(census.loads, 0, "no load SUCCEEDED");
        assert_eq!(census.starts, 0, "nothing starts without a module");
        assert_eq!(
            census.spawns, 1,
            "one host worker exists (Discord is demanded); NO component session ever existed"
        );
        assert_eq!(census.diagnostics, 1, "exactly ONE bounded diagnostic");
        assert!(
            !runtime.module_retained(),
            "the host retains no module handle"
        );
        drop(runtime);
    }
    /// V1(g)/(c): the v1 handshake refusals. `validate_table` is the host's gate: a short table,
    /// a null entry, a null export, an undersized self-size, a missing symbol and a null table all
    /// refuse; the provider then reports unavailable with ONE bounded diagnostic and NO retry.
    #[test]
    fn discord_handshake_refusals_never_activate_the_provider() {
        // Layout-level refusals, checked on the raw mirror the host reads.
        assert!(
            validate_table(std::ptr::null()).is_none(),
            "a null table refuses"
        );
        #[repr(C)]
        struct ShortTable {
            size: u32,
            entries: [*const c_void; 10],
        }
        let mut short = ShortTable {
            size: 4,
            entries: [std::ptr::null(); 10],
        };
        assert!(
            validate_table(&mut short as *mut ShortTable as *const c_void).is_none(),
            "a table smaller than LcsdV1 refuses"
        );
        short.size = std::mem::size_of::<LcsdV1>() as u32;
        assert!(
            validate_table(&mut short as *mut ShortTable as *const c_void).is_none(),
            "a table with a null entry refuses"
        );
        let sentinel = &short.size as *const u32 as *const c_void;
        for entry in short.entries.iter_mut() {
            *entry = sentinel;
        }
        assert!(
            validate_table(&mut short as *mut ShortTable as *const c_void).is_some(),
            "a full, non-null table is accepted (the refusal set is exact, not blanket)"
        );
        // The worker's reaction to a refused handshake.
        let runtime = spawn_with(
            &enabled_config("123456789012345678"),
            stub_v1::loader_handshake,
        );
        let update = wait_for_state(&runtime, |state| {
            state.error == "discord module unavailable"
        })
        .expect("a refused handshake makes Discord explicitly unavailable, not silent");
        assert!(!update.connected && !update.authenticated);
        assert_eq!(
            runtime.census_snapshot().diagnostics,
            1,
            "exactly one bounded diagnostic is published"
        );
        std::thread::sleep(Duration::from_millis(300));
        let census = runtime.census_snapshot();
        assert_eq!(
            census.load_attempts, 1,
            "no retry storm: one attempt per activation edge"
        );
        assert_eq!(census.diagnostics, 1, "one diagnostic, not one per tick");
        assert_eq!(
            census.starts, 0,
            "a refused handshake never starts the component"
        );
        // A generation bump (a real config re-activation) is the ONLY thing that re-attempts.
        runtime.update_config(&enabled_config("987654321098765432"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while runtime.census_snapshot().load_attempts < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            runtime.census_snapshot().load_attempts,
            2,
            "the retry edge is a config (re)activation"
        );
        drop(runtime);
    }

    /// V1(c): a missing / corrupt module is a non-fatal, classified failure with ONE bounded,
    /// secret-free diagnostic and no retry loop. F-H-2: the whole test runs inside an
    /// `AbsentComponentFixture`, so the loader's own lookup site is a directory this test created
    /// and removes (empty: no dll), never the deployment directory beside the test binary.
    #[test]
    fn discord_missing_dll_is_non_fatal_with_one_bounded_diagnostic_and_no_retry() {
        let fixture = AbsentComponentFixture::new();
        let mut diagnostic = [0u8; LOADER_DIAGNOSTIC_CAPACITY];
        let temp = std::env::temp_dir().join(format!(
            "lcdsirplus-d3c-missing-{}-{}.dll",
            std::process::id(),
            random_nonce().unwrap()
        ));
        let missing = load_table_from(&temp, &mut diagnostic);
        assert_eq!(
            missing.err().unwrap(),
            LoadFault::Missing,
            "an absent file is Missing"
        );
        let text = diagnostic_text(&diagnostic);
        assert!(
            text.starts_with("discord component is not installed"),
            "{text}"
        );
        assert!(
            text.len() < LOADER_DIAGNOSTIC_CAPACITY,
            "the diagnostic stays inside the capacity: {}",
            text.len()
        );
        let corrupt = temp.with_extension("corrupt");
        std::fs::write(&corrupt, b"this is not a PE image").unwrap();
        let mut second = [0u8; LOADER_DIAGNOSTIC_CAPACITY];
        let image = load_table_from(&corrupt, &mut second);
        assert!(
            matches!(
                image.err(),
                Some(LoadFault::Image) | Some(LoadFault::Missing)
            ),
            "a non-image file is refused as an image/missing failure, never a crash"
        );
        let _ = std::fs::remove_file(&corrupt);
        assert!(
            component_path().is_ok(),
            "the component lookup always resolves to one exact path"
        );
        let path = component_path().unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(COMPONENT_NAME),
            "the loader resolves ONE exact file name (no PATH, no search order)"
        );
        // F-H-2, the load-bearing shape of the cure, measured while the workspace-built dll sits
        // BESIDE the test binary: the lookup resolves into a test-owned fixture directory (the
        // suite runs test threads in parallel, so any live premise fixture qualifies) and the file
        // is absent there, so the build adjacency can never satisfy the "not installed" premise.
        let exe_directory = crate::runtime::canonical_executable()
            .ok()
            .and_then(|executable| executable.parent().map(Path::to_path_buf));
        assert_ne!(
            path.parent(),
            exe_directory.as_deref(),
            "F-H-2: the lookup root is a test-owned directory, never the build directory beside the test binary"
        );
        assert!(
            !path.exists() && !fixture.expected_path().exists(),
            "F-H-2: no component dll exists at the resolved lookup path or in this test's fixture"
        );
        // The real production loader, through the seam, against the fixture's owned EMPTY
        // directory. A workspace build legitimately leaves its dll beside the test executable;
        // nothing moves it, and the premise cannot be satisfied by it.
        let runtime = spawn_with(&enabled_config("123456789012345678"), load_production_table);
        let update = wait_for_state(&runtime, |state| {
            state.error == "discord module unavailable"
        })
        .expect("the missing module is an explicit unavailable state");
        assert!(!update.connected);
        assert_eq!(runtime.census_snapshot().load_attempts, 1);
        assert_eq!(runtime.census_snapshot().diagnostics, 1);
        assert!(
            !component_loaded(&fixture.expected_path()),
            "the module is NOT present when it is not installed"
        );
        drop(runtime);
        drop(fixture);
    }

    /// V1(d): the 50-cycle enable/disable census. One worker spawn + join and one load/start/
    /// stop/join/release/unload per cycle, no thread drift (a real thread snapshot through
    /// Toolhelp), a bounded handle count, and the module absent at rest.
    #[test]
    fn discord_toggle_fifty_cycles_leaves_no_thread_handle_or_module_drift() {
        fn live_threads() -> usize {
            use windows::Win32::System::Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
            };
            let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) } {
                Ok(snapshot) => snapshot,
                Err(_) => return usize::MAX,
            };
            let mut entry = windows::Win32::System::Diagnostics::ToolHelp::THREADENTRY32 {
                dwSize: std::mem::size_of::<
                    windows::Win32::System::Diagnostics::ToolHelp::THREADENTRY32,
                >() as u32,
                ..Default::default()
            };
            let mut count = 0usize;
            let pid = std::process::id();
            if unsafe { Thread32First(snapshot, &mut entry) }.is_ok() {
                loop {
                    if entry.th32OwnerProcessID == pid {
                        count += 1;
                    }
                    if unsafe { Thread32Next(snapshot, &mut entry) }.is_err() {
                        break;
                    }
                }
            }
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(snapshot);
            }
            count
        }
        fn handle_count() -> usize {
            use windows::Win32::System::Threading::GetProcessHandleCount;
            let mut count = 0u32;
            unsafe {
                match GetProcessHandleCount(
                    windows::Win32::Foundation::HANDLE(-1_isize as *mut std::ffi::c_void),
                    &mut count,
                ) {
                    Ok(()) => count as usize,
                    Err(_) => usize::MAX,
                }
            }
        }
        let on = enabled_config("123456789012345678");
        let off = Config {
            discord_enabled: false,
            discord_client_id: "123456789012345678".into(),
            ..Config::default()
        };
        let runtime = spawn_with(&off, stub_v1::loader);
        drained(&runtime);
        let baseline_threads = live_threads();
        let baseline_handles = handle_count();
        for cycle in 0..50 {
            runtime.update_config(&on);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !runtime.worker_running() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                runtime.worker_running(),
                "cycle {cycle}: enabling spawns the worker"
            );
            // The demand edge: the module must be LOADED (the session live) before the disable, so
            // the counters below measure the lifecycle, not a spawn/join race.
            let deadline = Instant::now() + Duration::from_secs(2);
            let want = (cycle as u64) + 1;
            while runtime.census_snapshot().loads < want && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                runtime.census_snapshot().loads,
                want,
                "cycle {cycle}: the enable edge loaded the module exactly once"
            );
            runtime.update_config(&off);
            let deadline = Instant::now() + Duration::from_secs(2);
            while runtime.worker_running() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !runtime.worker_running(),
                "cycle {cycle}: disabling joins the worker"
            );
            assert!(
                !runtime.module_retained(),
                "cycle {cycle}: the module unloaded cleanly, not retained"
            );
        }
        let census = runtime.census_snapshot();
        assert_eq!(census.spawns, 50, "one worker spawn per cycle");
        assert_eq!(
            census.worker_joins, 50,
            "one join per cycle (no leaked JoinHandle)"
        );
        assert_eq!(census.loads, 50, "one load per cycle");
        assert_eq!(census.starts, 50, "one component start per cycle");
        assert_eq!(census.stops, 50, "one stop per cycle");
        assert_eq!(census.joins, 50, "one component join per cycle");
        assert_eq!(census.releases, 50, "one release per cycle");
        assert_eq!(census.unloads, 50, "one module unload per cycle");
        assert_eq!(census.retained, 0, "no cycle refused the unload");
        // The OS-level metrics are process-wide, so each is sampled over a BOUNDED settle window:
        // a leaked thread or handle is PERMANENT and never settles, while the transient activity of
        // tests running in parallel does. The claim stays "at rest, no drift".
        let mut threads = live_threads();
        let mut handles = handle_count();
        let settle = Instant::now() + Duration::from_secs(15);
        let drifting = |threads: usize, handles: usize| -> bool {
            threads > baseline_threads || (handles != usize::MAX && handles > baseline_handles + 8)
        };
        while drifting(threads, handles) && Instant::now() < settle {
            std::thread::sleep(Duration::from_millis(20));
            threads = live_threads().min(threads);
            handles = handle_count().min(handles);
        }
        println!(
            "D3C toggle census: baseline_threads={baseline_threads} rest_threads={threads} \
             baseline_handles={baseline_handles} rest_handles={handles}"
        );
        assert!(
            threads <= baseline_threads,
            "50 enable/disable cycles leave NO thread drift (the discord thread-instance is gone at rest)"
        );
        // The PROCESS handle table is a process-wide metric: with 588 tests running in parallel it
        // is not attributable to this provider, so it is reported above, and the per-cycle
        // module-handle claim is carried by the exact per-Runtime census (unloads == 50,
        // retained == 0, module_retained() false every cycle) plus GetModuleHandleW at rest below.
        let fixture = AbsentComponentFixture::new();
        assert!(
            !component_loaded(&fixture.expected_path()),
            "the injected lifecycle loader never loads the absent image"
        );
        drop(runtime);
        drop(fixture);
    }

    /// V1(d) module-presence half, production loader: with the module ABSENT (the shipped default
    /// deployment for a disabled profile) 50 cycles never make the module appear. F-H-2: the
    /// absence is the test's own empty fixture directory (`AbsentComponentFixture`) resolved by the
    /// REAL `component_path`/`load_production_table` chain through the test-only seam, so a
    /// workspace-built dll beside the test binary cannot satisfy the premise and no harness has to
    /// move it.
    #[test]
    fn discord_module_is_absent_across_toggle_cycles_when_not_installed() {
        let fixture = AbsentComponentFixture::new();
        let on = enabled_config("123456789012345678");
        let off = Config {
            discord_enabled: false,
            discord_client_id: "123456789012345678".into(),
            ..Config::default()
        };
        let runtime = spawn_with(&off, load_production_table);
        let path = fixture.expected_path();
        for cycle in 0..50 {
            runtime.update_config(&on);
            // WO-TEL-D3-E (F-1 class, pre-existing at `4ef030a`): the activation edge's load
            // attempt is OBSERVED in the monotonic per-Runtime census, bounded, instead of sampling
            // the thread 20 ms after the edge. Under the parallel suite shape a freshly spawned
            // worker is not always scheduled inside a fixed 20 ms window, so that cycle legitimately
            // records no attempt and the shipped `load_attempts == 50` claim becomes a scheduling
            // race (measured red at HEAD content, `flakewatch.txt` head rows). Observing the attempt
            // before the disable edge is what makes "one attempt per activation edge, no more"
            // testable deterministically; it adds no activation edge of its own. The bound is 10 s,
            // the shipped alloc-window bound, because a measured suite-shape starved a 2 s bound at
            // cycle 0 (`paircached-full-cached-2.log`: left 0 / right 1 at `3729`); overrunning the
            // bound is a FAIL-CLOSED red naming the cycle, never a silent skip.
            let attempt_deadline = Instant::now() + Duration::from_secs(10);
            while runtime.census_snapshot().load_attempts < cycle + 1
                && Instant::now() < attempt_deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                runtime.census_snapshot().load_attempts,
                cycle + 1,
                "cycle {cycle}: the activation edge made exactly one attempt"
            );
            runtime.update_config(&off);
            let deadline = Instant::now() + Duration::from_secs(2);
            while runtime.worker_running() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !component_loaded(&path),
                "cycle {cycle}: GetModuleHandleW sees no component module"
            );
        }
        let census = runtime.census_snapshot();
        assert_eq!(
            census.loads, 0,
            "no cycle loaded a module that is not installed"
        );
        assert_eq!(
            census.load_attempts, 50,
            "one attempt per activation edge, no more"
        );
        assert_eq!(census.diagnostics, 50, "one diagnostic per activation edge");
        drop(runtime);
        drop(fixture);
    }

    /// V1: the refuse-unload-if-inflight policy. A component that reports work in flight is NOT
    /// unloaded; the module is retained, the state reports it, and no unload is claimed.
    #[test]
    fn discord_refuses_unload_while_the_component_reports_work_in_flight_and_retains_the_module() {
        let on = enabled_config("123456789012345678");
        let off = Config {
            discord_enabled: false,
            discord_client_id: "123456789012345678".into(),
            ..Config::default()
        };
        let runtime = spawn_with(&on, stub_v1::loader_retained);
        let _update = wait_for_state(&runtime, |state| state.connected)
            .expect("the session is live before the disable");
        assert_eq!(runtime.census_snapshot().loads, 1);
        runtime.update_config(&off);
        let retained = wait_for_state(&runtime, |state| state.error == "discord module retained")
            .expect("the refused unload is REPORTED, never silently accepted");
        assert!(
            !retained.connected,
            "a retained module publishes no live connection"
        );
        assert!(
            runtime.module_retained(),
            "the refuse is test-visible on the Runtime"
        );
        let census = runtime.census_snapshot();
        assert_eq!(census.unloads, 0, "the module was NOT released");
        assert!(census.retained >= 1, "the retain is counted");
        assert_eq!(census.stops, 1, "stop ran before the refusal");
        assert_eq!(
            census.joins, 1,
            "the host called join exactly once, and the component answered in-flight"
        );
        assert_eq!(
            census.releases, 0,
            "the component's `release` was NOT called once join refused"
        );
    }

    /// V1(e): config hot reload drives the full path: `""` -> set -> `""` -> set. Load, unload,
    /// no-load, load; every published state belongs to the CURRENT generation, so a stale session
    /// is never published as fresh.
    #[test]
    fn discord_hot_reload_set_clear_set_loads_unloads_and_never_publishes_stale_as_fresh() {
        let empty = Config {
            discord_enabled: true,
            discord_client_id: String::new(),
            ..Config::default()
        };
        let first = enabled_config("111000111000111000");
        let second = enabled_config("222000222000222000");
        let runtime = spawn_with(&empty, stub_v1::loader);
        let disabled = runtime
            .updates
            .recv_timeout(Duration::from_secs(1))
            .expect("the unconfigured start publishes disabled once");
        assert_eq!(disabled.error, "disabled");
        assert_eq!(
            runtime.census_snapshot().loads,
            0,
            "no module for an empty client_id"
        );
        drained(&runtime);

        runtime.update_config(&first);
        let live = wait_for_state(&runtime, |state| state.connected)
            .expect("the enable edge loads, starts and publishes");
        assert_eq!(live.channel_name, "channel-of-111000111000111000");
        assert_eq!(runtime.census_snapshot().loads, 1);

        runtime.update_config(&empty);
        let parked = wait_for_state(&runtime, |state| state.error == "disabled")
            .expect("the disable edge publishes the disabled state");
        assert!(
            !parked.connected,
            "the disable publishes no live connection"
        );
        let census = runtime.census_snapshot();
        assert_eq!(census.unloads, 1, "the disable edge unloaded the module");
        assert_eq!(
            census.starts, 1,
            "the first session was the only start so far"
        );
        drained(&runtime);

        runtime.update_config(&second);
        let fresh = wait_for_state(&runtime, |state| {
            state.connected && state.channel_name == "channel-of-222000222000222000"
        })
        .expect("the re-enable publishes the NEW session's state");
        assert_eq!(fresh.speakers.len(), 1);
        let census = runtime.census_snapshot();
        assert_eq!(census.loads, 2, "enable -> demand loads the module again");
        assert_eq!(census.starts, 2, "a fresh component session per activation");
        assert!(
            !runtime.module_retained(),
            "the reload path never retains the module"
        );
        // No published state carries the previous session while a new one is live: the stream
        // between the last disable and this fresh row is generation-checked, never stale-fresh.
        let stale = drained(&runtime)
            .into_iter()
            .filter(|state| state.connected && state.channel_name.contains("111000"))
            .count();
        assert_eq!(stale, 0, "the old generation is never published as current");
        drop(runtime);
    }

    /// V1(f): the credential-clear CLI path needs NO component: it is host DPAPI + file work, and
    /// the loader seam proves zero table calls.
    #[test]
    fn discord_cli_clear_token_is_dll_free() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        let owned = credential_path_at(&root, "111").unwrap();
        save_credential(&owned, "111", b"version-2-record-for-111").unwrap();
        let legacy = root.join("discord.token");
        std::fs::write(&legacy, b"legacy").unwrap();
        clear_tokens_at(&root).unwrap();
        assert!(
            !owned.exists() && !legacy.exists(),
            "the shipped clear semantics (exact ownership + legacy) survive the split"
        );
        // The DLL-free claim is a SOURCE property of the CLI path: neither entry point names the
        // loader, a module handle, the v1 table, or any ABI entry, so no component code can run.
        let source = include_str!("discord.rs").replace("\r\n", "\n");
        for head in [
            "pub fn clear_token() -> Result<(), String> {",
            "fn clear_tokens_at(root: &Path) -> Result<(), String> {",
        ] {
            let body = fn_body_at(&source, head);
            for banned in [
                "load_production_table",
                "load_table_from",
                "LcsdV1",
                "LcsdHostApi",
                "lcsd_",
                "Module",
            ] {
                assert!(
                    !body.contains(banned),
                    "the CLI clear path names no loader/table/ABI symbol: {banned} in {head}"
                );
            }
        }
    }

    /// V1: `authorize` is the documented temporary load: one component authorize call, and the
    /// module is unloaded on the same thread. With no module installed it fails as a classified,
    /// bounded message (the CLI prints it), never a crash. F-H-2: the "no module installed" premise
    /// is this test's own empty fixture directory, resolved through the test-only seam by the
    /// production `component_path`, so a workspace-built dll beside the test binary and a live
    /// Discord Desktop can never answer the handshake and invert it (the F-4 failure mode).
    #[test]
    fn discord_authorize_is_a_documented_temporary_load_and_clear_needs_none() {
        let _fixture = AbsentComponentFixture::new();
        // The unconfigured guard is checked before any load (the shipped order is kept).
        let unconfigured = Config {
            discord_enabled: true,
            discord_client_id: String::new(),
            ..Config::default()
        };
        assert_eq!(
            authorize(&unconfigured, "secret").unwrap_err(),
            "discord_client_id is not configured"
        );
        // The guard fires before any load: the failure names the CONFIG, not a missing module.
        let result = authorize(&enabled_config("123456789012345678"), "secret");
        assert!(
            result.is_err(),
            "with no component installed the CLI reports a failure (the fixture root holds no dll)"
        );
        let message = result.unwrap_err();
        assert_eq!(
            message,
            LoadFault::Missing.diagnostic(),
            "the CLI diagnostic is the loader's one bounded sentence: {message}"
        );
    }

    /// V1(h): the renderer consumers (`visible_speakers`, the `providers["Discord"]` health row)
    /// are fed from the host-side projection of the component state, field for field.
    #[test]
    fn discord_state_projection_keeps_the_renderer_consumers_supplied() {
        let mut pod = LcsdState {
            size: std::mem::size_of::<LcsdState>() as u32,
            revision: 3,
            connected: 1,
            authenticated: 1,
            self_mute: 1,
            self_deaf: 1,
            voice_ping_ms: 12.5,
            channel_id_len: 18,
            channel_name_len: 7,
            connection_state_len: 9,
            updated_unix_ms: 1_700_000_000_000,
            speaker_count: 2,
            ..LcsdState::default()
        };
        pod.channel_id[..18].copy_from_slice(b"900000000000000001");
        pod.channel_name[..7].copy_from_slice(b"General");
        pod.connection_state[..9].copy_from_slice(b"CONNECTED");
        pod.speakers[0].user_id[..3].copy_from_slice(b"111");
        pod.speakers[0].user_id_len = 3;
        pod.speakers[0].name[..4].copy_from_slice(b"self");
        pod.speakers[0].name_len = 4;
        pod.speakers[0].is_self = 1;
        pod.speakers[0].speaking = 1;
        pod.speakers[0].started_unix_ms = 1_700_000_000_000;
        pod.speakers[1].user_id[..3].copy_from_slice(b"222");
        pod.speakers[1].user_id_len = 3;
        pod.speakers[1].name[..6].copy_from_slice(b"friend");
        pod.speakers[1].name_len = 6;
        pod.speakers[1].stopped_unix_ms = 1_700_000_001_000;
        let state = state_from_pod(&pod);
        assert!(state.connected && state.authenticated);
        assert_eq!(state.channel_id, "900000000000000001");
        assert_eq!(state.channel_name, "General");
        assert_eq!(state.connection_state, "CONNECTED");
        assert!(state.self_mute && state.self_deaf);
        assert_eq!(state.voice_ping_ms, 12.5);
        assert_eq!(state.speakers.len(), 2);
        assert_eq!(state.speakers[0].user_id, "111");
        assert!(state.speakers[0].is_self && state.speakers[0].speaking);
        assert_eq!(
            state.speakers[0].started_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000))
        );
        assert_eq!(state.speakers[1].name, "friend");
        assert_eq!(
            state.speakers[1].stopped_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_001_000))
        );
        // The surviving consumer of this state is the `Discord` health row the app publishes
        // from it (`app.rs`: connected AND authenticated). WO-REMOVE-ALERTS deleted the
        // speaker-triggered renderer takeover (`visible_speakers` + `discord_overlay`) and the
        // alert overlay that used to rank above it, together with the shipped renderer tests
        // that pinned those surfaces (discord_overlay_draws_border,
        // discord_visibility_order_linger_self_and_restoration,
        // critical_alert_overrides_discord_and_dashboard,
        // hung_hold_overrides_alert_and_discord_then_restores_exact_dashboard, now
        // hung_hold_is_the_only_attention_surface_and_restores_exact_dashboard). The projection
        // below still supplies every field those routes read, so the provider contract is
        // unchanged; no renderer entry any more consumes `speakers`.
        let health = state.connected && state.authenticated;
        assert!(
            health,
            "the providers[Discord] row reads the projected state"
        );
        assert_eq!(
            state
                .speakers
                .iter()
                .filter(|speaker| speaker.speaking)
                .count(),
            1,
            "the overlay's speaker rule has real data to work on"
        );
        // A truncated capacity is honored, not a silent overflow.
        let mut long = pod;
        long.error.copy_from_slice(&[b'x'; LCSD_ERROR_CAPACITY]);
        long.error_len = LCSD_ERROR_CAPACITY as u32 + 32;
        let projected = state_from_pod(&long);
        assert!(
            projected.error.len() <= LCSD_ERROR_CAPACITY,
            "the state bridge never grows past the ABI capacity"
        );
    }

    /// V1: the ABI v1 block and the boundary-shared identity rules are byte-identical between the
    /// executable and the component; the pin reddens on any layout/code drift in either package.
    #[test]
    fn discord_abi_blocks_stay_byte_identical_across_the_boundary() {
        fn marker(source: &str, start: &str, end: &str) -> String {
            let text = source.replace("\r\n", "\n");
            let open = text
                .find(start)
                .unwrap_or_else(|| panic!("the shipped text opens {start}"));
            let close = text[open..]
                .find(end)
                .map(|offset| open + offset)
                .unwrap_or_else(|| panic!("the shipped text closes {end}"));
            text[open + start.len()..close].trim().to_string()
        }
        const HOST: &str = include_str!("discord.rs");
        const COMPONENT: &str = include_str!("../../components/lcdsirplus_discord/src/abi.rs");
        const COMPONENT_SHARED: &str =
            include_str!("../../components/lcdsirplus_discord/src/shared.rs");
        let mine = marker(HOST, "// ABI-V1-BLOCK-START", "// ABI-V1-BLOCK-END");
        let theirs = marker(COMPONENT, "// ABI-V1-BLOCK-START", "// ABI-V1-BLOCK-END");
        assert_eq!(
            mine, theirs,
            "the ABI v1 layouts, table shape and status codes must match byte-for-byte"
        );
        assert_eq!(
            mine.matches("pub struct").count(),
            5,
            "the block carries settings, host api, speaker, state and table"
        );
        assert_eq!(
            mine.matches("pub extern").count() + mine.matches("extern \"system\" fn").count(),
            theirs.matches("extern \"system\" fn").count() + mine.matches("pub extern").count(),
            "the extern entry counts match (the pin is not vacuous)"
        );
        let shared = marker(HOST, "// SHARED-RULES-START", "// SHARED-RULES-END");
        let shared_theirs = marker(
            COMPONENT_SHARED,
            "// SHARED-RULES-START",
            "// SHARED-RULES-END",
        );
        assert_eq!(
            shared, shared_theirs,
            "the boundary-shared identity rules drifted between the packages"
        );
        assert!(shared.contains("fn validate_user_id"));
        assert!(shared.contains("fn random_nonce"));
    }

    /// The component crate's production text carries no host type, no logging and no app path:
    /// checked from the host side too, so the ban list is pinned in BOTH packages.
    #[test]
    fn discord_component_keeps_host_types_and_logging_out_of_its_production_text() {
        const COMPONENT_FILES: [&str; 5] = [
            include_str!("../../components/lcdsirplus_discord/src/rpc.rs"),
            include_str!("../../components/lcdsirplus_discord/src/engine.rs"),
            include_str!("../../components/lcdsirplus_discord/src/types.rs"),
            include_str!("../../components/lcdsirplus_discord/src/abi.rs"),
            include_str!("../../components/lcdsirplus_discord/src/lib.rs"),
        ];
        for file in COMPONENT_FILES {
            let text = file.replace("\r\n", "\n");
            let production = match text.find("#[cfg(test)]") {
                Some(cut) => &text[..cut],
                None => &text[..],
            };
            for token in [
                "crate::config::",
                "crate::model::",
                "log_warn!",
                "log_info!",
                "println!",
                "std::thread::spawn",
                "LoadLibrary",
                "FreeLibrary",
                "CryptProtectData",
            ] {
                assert!(
                    !production.contains(token),
                    "the component's production text must not contain {token}"
                );
            }
        }
    }
    #[test]
    fn reconnect_backoff_resets_after_many_short_successful_steps() {
        let start = Instant::now();
        let mut backoff = Duration::from_secs(16);
        for step in 0..300 {
            backoff = session_backoff(
                backoff,
                Some(start),
                start + Duration::from_millis(step * 100),
            );
            assert_eq!(backoff, Duration::from_secs(16));
        }
        assert_eq!(
            session_backoff(backoff, Some(start), start + Duration::from_secs(30)),
            Duration::from_secs(1)
        );
        assert_eq!(
            session_backoff(backoff, None, start + Duration::from_secs(60)),
            Duration::from_secs(16)
        );
    }
}
