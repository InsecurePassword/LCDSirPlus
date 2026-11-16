//! ABI v1 (WO-TEL-D3-C S2): one exported symbol, one size-checked function table, POD payloads.
//!
//! The block between the `ABI-V1-BLOCK-START` / `ABI-V1-BLOCK-END` markers is byte-identical in
//! the host module `src/providers/discord.rs`; the pair is pinned by
//! `discord_abi_blocks_stay_byte_identical_across_the_boundary` in both crates, so the layouts,
//! codes and table shape cannot drift. No Rust type, `String`, `Vec`, reference, slice pair,
//! allocator handle, `dyn` or panic crosses the boundary: the root profile is `panic = "abort"`,
//! so the component is total by construction (every entry validates its `size`/`len`/`capacity`
//! and returns a status code; nothing indexes without a length check).

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

/// The single export (contract S2): the table pointer, or null on any handshake refusal.
#[no_mangle]
pub extern "system" fn lcsd_get_table(
    magic: u32,
    version: u32,
    table_size: usize,
) -> *const std::ffi::c_void {
    if magic != LCSD_MAGIC || version != LCSD_VERSION {
        return std::ptr::null();
    }
    if table_size < std::mem::size_of::<LcsdV1>() {
        return std::ptr::null();
    }
    &crate::engine::TABLE as *const LcsdV1 as *const std::ffi::c_void
}
