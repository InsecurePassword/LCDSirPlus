//! The component's lifecycle core (WO-TEL-D3-C S2/S4): the v1 function table, the engine state
//! machine the host worker drives synchronously, and the one-shot authorize flow.
//!
//! Ownership: `start` allocates the `Handle` (Rust-owned, in this module's allocator); the host
//! holds only an opaque `*mut c_void`. `release` is the only entry that frees it, and it refuses
//! (`LCSD_E_INFLIGHT`) while a session or a token exchange is live, so the host can never free
//! what the component still uses. Every entry point runs on the CALLER's thread; the component
//! creates no thread, so the host's module unload after `stop`/`join`/`release` has no component
//! code to outlive (audit condition iii). `busy` makes a call that arrives while a step is in
//! flight refuse with `LCSD_E_INFLIGHT` rather than race.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::abi::{
    LcsdHostApi, LcsdSettings, LcsdState, LcsdV1, LCSD_E_ARG, LCSD_E_CANCELED, LCSD_E_IDENTITY,
    LCSD_E_INFLIGHT, LCSD_E_IO, LCSD_E_PROTOCOL, LCSD_E_SIZE, LCSD_E_STATE, LCSD_E_UNAVAILABLE,
    LCSD_IDLE, LCSD_OK, LCSD_STATUS_CAPACITY,
};
use crate::rpc::{
    authorization_code_body, authorize_rpc, close_error, decode_payload, exchange_token,
    json_string, new_rpc, normalized_client_secret, open_pipe, persist_rotated_refresh,
    pipe_command, provide_record, refresh_token_if_needed, require_same_ready_user,
    runtime_handshake, unix_now, validate_authentication, TokenExchangeOwner, TokenRecord,
    OP_CLOSE, OP_FRAME, OP_PING, OP_PONG,
};
use crate::types::{project_state, settings_to_config, Config, DiscordState};

pub(crate) const CONNECT_STEP_TIMEOUT_MS: u32 = 5_000;

pub(crate) struct Engine {
    api: LcsdHostApi,
    settings: Config,
    session: Option<crate::rpc::Rpc>,
    published: LcsdState,
    token_owner: TokenExchangeOwner,
}

struct Handle {
    engine: Engine,
    busy: AtomicBool,
    stopped: bool,
}

static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// The host callbacks the component may call. The pointers are host statics whose code and
/// context outlive every component call (the host retains the module until after `release`), so
/// the POD is safe to share between the host's threads; the wrapper is where that invariant is
/// stated, and it is the only `unsafe impl` in this crate.
struct HostApiSlot(LcsdHostApi);
unsafe impl Send for HostApiSlot {}
unsafe impl Sync for HostApiSlot {}

static HOST_API: OnceLock<Mutex<Option<HostApiSlot>>> = OnceLock::new();

fn set_last_error(text: &str) {
    let mut slot = LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    let bounded: String = text.chars().take(LCSD_STATUS_CAPACITY - 1).collect();
    slot.clear();
    slot.push_str(&bounded);
}

fn api_slot() -> &'static Mutex<Option<HostApiSlot>> {
    HOST_API.get_or_init(|| Mutex::new(None))
}

fn remember_api(api: &LcsdHostApi) {
    *api_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(HostApiSlot(*api));
}

/// The component's host-side verifier delegate: `verify_client` is the ABI v1 entry, the
/// Authenticode work is host code, and it runs on the host's calling thread.
fn with_api<T>(run: impl FnOnce(&LcsdHostApi) -> T) -> Option<T> {
    let guard = api_slot().lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|slot| run(&slot.0))
}

fn canceled(api: &LcsdHostApi) -> bool {
    (api.canceled)(api.context) != 0
}

impl Engine {
    fn new(api: LcsdHostApi, settings: Config) -> Self {
        Self {
            api,
            settings,
            session: None,
            published: LcsdState::default(),
            token_owner: TokenExchangeOwner::default(),
        }
    }

    fn sync(&mut self, rpc: &mut crate::rpc::Rpc) {
        if rpc.published.revision != self.published.revision {
            self.published = rpc.published;
        }
    }

    fn fail(&mut self, rpc: Option<crate::rpc::Rpc>, error: String) -> i32 {
        set_last_error(&error);
        drop(rpc);
        self.session = None;
        let failed = DiscordState {
            error,
            updated: Some(std::time::SystemTime::now()),
            ..Default::default()
        };
        let mut output = LcsdState::default();
        project_state(&failed, self.published.revision + 1, &mut output);
        self.published = output;
        if canceled(&self.api) {
            LCSD_E_CANCELED
        } else {
            LCSD_E_IO
        }
    }

    /// The shipped `connect_and_serve` prefix, run as one bounded step: pipe discovery + server
    /// verification (host), READY handshake, credential read (host), refresh + rotation persist
    /// (host store), re-handshake on rotation, AUTHENTICATE + identity binding, CURRENT_USER_UPDATE
    /// subscription and the first channel refresh.
    fn connect(&mut self) -> Result<crate::rpc::Rpc, String> {
        let settings = self.settings.clone();
        if settings.discord_client_id.trim().is_empty() {
            return Err("discord_client_id is not configured".into());
        }
        let (file, _) = open_pipe(&self.api)?;
        let mut rpc = new_rpc(file, self.api);
        let ready_id = runtime_handshake(&mut rpc, &settings.discord_client_id)?;
        let credential = match provide_record(&self.api, &ready_id, &settings.discord_client_id)? {
            Some(record) => record,
            None => return Err(
                "Discord current account is not authorized; run LCDSirPlus.exe --discord-authorize"
                    .into(),
            ),
        };
        let (credential, refreshed) =
            refresh_token_if_needed(&settings, credential, &self.token_owner)?;
        persist_rotated_refresh(&self.api, &credential, refreshed)?;
        if refreshed {
            // OAuth rotation re-hands-hakes on a fresh pipe, bound to the same READY account.
            drop(rpc);
            let (file, _) = open_pipe(&self.api)?;
            rpc = new_rpc(file, self.api);
            let fresh_ready_id = runtime_handshake(&mut rpc, &settings.discord_client_id)?;
            require_same_ready_user(&ready_id, &fresh_ready_id, &credential)?;
        }
        rpc.ready_user_id = ready_id.clone();
        rpc.tracker.self_id = ready_id.clone();
        let args = format!(
            "{{\"access_token\":{}}}",
            json_string(&credential.access_token)
        );
        let authenticated = rpc.command("AUTHENTICATE", "", Some(&args))?;
        validate_authentication(
            &authenticated.data,
            &ready_id,
            &credential,
            &settings.discord_client_id,
        )?;
        rpc.command("SUBSCRIBE", "CURRENT_USER_UPDATE", None)?;
        rpc.current_user_subscribed = true;
        if let Err(error) = rpc.refresh_channel() {
            if rpc.account_changed {
                return Err(error);
            }
            // The shipped path logs this WARN host-side; the component keeps the message as the
            // last error and the host reads it through `last_error`.
            set_last_error(&error);
        }
        self.sync(&mut rpc);
        Ok(rpc)
    }

    /// One shipped serve-loop iteration: read one frame, answer a PING, close on CLOSE, decode and
    /// apply one event, refresh the channel when the voice channel moved, publish.
    fn serve_step(rpc: &mut crate::rpc::Rpc, timeout_ms: u32) -> Result<bool, String> {
        let api = rpc.api;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1) as u64);
        if !rpc.file.data_available(&|| canceled(&api), deadline)? {
            return Ok(false);
        }
        let (opcode, data) = rpc.read()?;
        match opcode {
            OP_PING => rpc.write(OP_PONG, &data)?,
            OP_CLOSE => return Err(close_error(&data)),
            OP_FRAME => {
                let payload = match decode_payload(&data) {
                    Ok(payload) => payload,
                    Err(_) => return Ok(true),
                };
                if crate::rpc::payload_error(&payload).is_err() || payload.event.is_empty() {
                    return Ok(true);
                }
                let changed = rpc.handle_event(&payload.event, &payload.data)?;
                if changed {
                    if let Err(error) = rpc.refresh_channel() {
                        if rpc.account_changed {
                            return Err(error);
                        }
                    }
                }
                rpc.publish();
            }
            _ => {}
        }
        Ok(true)
    }

    fn poll_io(&mut self, timeout_ms: u32) -> i32 {
        if canceled(&self.api) {
            return LCSD_E_CANCELED;
        }
        if self.session.is_none() {
            return match self.connect() {
                Ok(rpc) => {
                    self.session = Some(rpc);
                    LCSD_OK
                }
                Err(error) => self.fail(None, error),
            };
        }
        let mut rpc = match self.session.take() {
            Some(rpc) => rpc,
            None => return LCSD_E_STATE,
        };
        match Self::serve_step(&mut rpc, timeout_ms) {
            Ok(progress) => {
                self.sync(&mut rpc);
                self.session = Some(rpc);
                if progress {
                    LCSD_OK
                } else {
                    LCSD_IDLE
                }
            }
            Err(error) => self.fail(Some(rpc), error),
        }
    }

    fn stop(&mut self) {
        self.session = None;
        self.token_owner.reset();
    }

    fn inflight(&self) -> bool {
        self.session.is_some() || self.token_owner.active()
    }
}

// ------------------------------------------------------------------ ABI v1 entries

extern "system" fn start(
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
        || unsafe { (*settings).size < std::mem::size_of::<LcsdSettings>() as u32 }
        || unsafe { (*api).size < std::mem::size_of::<LcsdHostApi>() as u32 }
    {
        set_last_error("discord component: start received an undersized or missing block");
        return LCSD_E_SIZE;
    }
    // Rust `extern "system" fn` pointers are non-null by type, so a missing host entry is
    // unrepresentable once the block has been read. The host validates the component's table the
    // other way round, through the raw-pointer mirror `LcsdV1Raw` (host loader).
    let api = unsafe { *api };
    let config = match settings_to_config(unsafe { &*settings }) {
        Some(config) => config,
        None => {
            set_last_error("discord component: settings payload is out of range");
            return LCSD_E_ARG;
        }
    };
    remember_api(&api);
    let handle = Box::new(Handle {
        engine: Engine::new(api, config),
        busy: AtomicBool::new(false),
        stopped: false,
    });
    unsafe { *out_handle = Box::into_raw(handle) as *mut c_void };
    LCSD_OK
}

/// Borrows the handle for one entry, refusing when a step is already in flight on this handle.
fn begin(handle: *mut c_void) -> Option<&'static mut Handle> {
    if handle.is_null() {
        return None;
    }
    let borrowed = unsafe { &mut *(handle as *mut Handle) };
    if borrowed.busy.swap(true, Ordering::Acquire) {
        return None;
    }
    Some(borrowed)
}

fn end(handle: &mut Handle) {
    handle.busy.store(false, Ordering::Release);
}

extern "system" fn set_config(handle: *mut c_void, settings: *const LcsdSettings, len: u32) -> i32 {
    let Some(borrowed) = begin(handle) else {
        set_last_error("discord component: set_config on a busy or null handle");
        return if handle.is_null() {
            LCSD_E_ARG
        } else {
            LCSD_E_INFLIGHT
        };
    };
    if settings.is_null()
        || len as usize != std::mem::size_of::<LcsdSettings>()
        || unsafe { (*settings).size < std::mem::size_of::<LcsdSettings>() as u32 }
    {
        end(borrowed);
        set_last_error("discord component: set_config settings block is undersized");
        return LCSD_E_SIZE;
    }
    let code = match settings_to_config(unsafe { &*settings }) {
        Some(config) => {
            borrowed.engine.settings = config;
            LCSD_OK
        }
        None => {
            set_last_error("discord component: set_config payload is out of range");
            LCSD_E_ARG
        }
    };
    end(borrowed);
    code
}

extern "system" fn poll_io(handle: *mut c_void, timeout_ms: u32) -> i32 {
    let Some(borrowed) = begin(handle) else {
        set_last_error("discord component: poll_io on a busy or null handle");
        return if handle.is_null() {
            LCSD_E_ARG
        } else {
            LCSD_E_INFLIGHT
        };
    };
    if borrowed.stopped {
        end(borrowed);
        return LCSD_E_STATE;
    }
    let timeout = if timeout_ms == 0 {
        CONNECT_STEP_TIMEOUT_MS
    } else {
        timeout_ms
    };
    let code = borrowed.engine.poll_io(timeout);
    end(borrowed);
    code
}

extern "system" fn state(handle: *mut c_void, out: *mut LcsdState, capacity: u32) -> i32 {
    let Some(borrowed) = begin(handle) else {
        set_last_error("discord component: state on a busy or null handle");
        return if handle.is_null() {
            LCSD_E_ARG
        } else {
            LCSD_E_INFLIGHT
        };
    };
    if out.is_null() {
        end(borrowed);
        set_last_error("discord component: state buffer is missing");
        return LCSD_E_ARG;
    }
    if (capacity as usize) < std::mem::size_of::<LcsdState>()
        || unsafe { (*out).size < std::mem::size_of::<LcsdState>() as u32 }
    {
        end(borrowed);
        set_last_error("discord component: state buffer is undersized");
        return LCSD_E_SIZE;
    }
    unsafe { *out = borrowed.engine.published };
    end(borrowed);
    LCSD_OK
}

extern "system" fn stop(handle: *mut c_void) -> i32 {
    let Some(borrowed) = begin(handle) else {
        set_last_error("discord component: stop on a busy handle");
        return if handle.is_null() {
            LCSD_E_ARG
        } else {
            LCSD_E_INFLIGHT
        };
    };
    borrowed.engine.stop();
    borrowed.stopped = true;
    end(borrowed);
    LCSD_OK
}

extern "system" fn join(handle: *mut c_void) -> i32 {
    let Some(borrowed) = begin(handle) else {
        set_last_error("discord component: join refused (a step is in flight)");
        return if handle.is_null() {
            LCSD_E_ARG
        } else {
            LCSD_E_INFLIGHT
        };
    };
    let code = if borrowed.engine.inflight() {
        set_last_error("discord component: join refused while the session is live");
        LCSD_E_INFLIGHT
    } else {
        LCSD_OK
    };
    end(borrowed);
    code
}

extern "system" fn release(handle: *mut c_void) -> i32 {
    if handle.is_null() {
        return LCSD_E_ARG;
    }
    let owned = unsafe { Box::from_raw(handle as *mut Handle) };
    if owned.busy.load(Ordering::Acquire) || owned.engine.inflight() {
        // Refuse to destroy a live engine: hand the ownership back to the caller.
        // Refuse to destroy a live engine: ownership stays with the caller (the host retains the
        // module + the handle, and the pointer it holds stays valid).
        std::mem::forget(owned);
        set_last_error("discord component: release refused while work is in flight");
        return LCSD_E_INFLIGHT;
    }
    drop(owned);
    LCSD_OK
}

extern "system" fn authorize(
    settings: *const LcsdSettings,
    secret: *const u8,
    secret_len: u32,
    api: *const LcsdHostApi,
    api_len: u32,
    out_status: *mut u8,
    out_cap: u32,
) -> i32 {
    let write_status = |text: &str| {
        if out_status.is_null() || out_cap == 0 {
            return;
        }
        let bytes = text.as_bytes();
        let len = bytes.len().min(out_cap as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_status, len);
            *out_status.add(len) = 0;
        }
    };
    if settings.is_null()
        || api.is_null()
        || secret.is_null()
        || settings_len_mismatch(settings, api, api_len)
    {
        write_status("discord component: authorize received an undersized block");
        return LCSD_E_SIZE;
    }
    let api = unsafe { *api };
    remember_api(&api);
    let config = match settings_to_config(unsafe { &*settings }) {
        Some(config) => config,
        None => {
            write_status("discord component: authorize payload is out of range");
            return LCSD_E_ARG;
        }
    };
    if secret_len == 0 || secret_len > LCSD_STATUS_CAPACITY as u32 * 8 {
        write_status("discord component: authorize secret length is out of range");
        return LCSD_E_ARG;
    }
    let secret_text =
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(secret, secret_len as usize) })
            .into_owned();
    let result = authorize_flow(&api, &config, &secret_text);
    match result {
        Ok(()) => LCSD_OK,
        Err(error) => {
            set_last_error(&error);
            write_status(&error);
            LCSD_E_PROTOCOL
        }
    }
}

fn settings_len_mismatch(
    settings: *const LcsdSettings,
    api: *const LcsdHostApi,
    api_len: u32,
) -> bool {
    unsafe {
        (*settings).size < std::mem::size_of::<LcsdSettings>() as u32
            || api_len as usize != std::mem::size_of::<LcsdHostApi>()
            || (*api).size < std::mem::size_of::<LcsdHostApi>() as u32
    }
}

/// The shipped one-shot authorization flow, on the CLI's thread, with no handle and no worker:
/// pipe discovery + host server verification, the redirectless AUTHORIZE handshake, the code ->
/// token exchange over WinHTTP, a second verified pipe open with a same-account re-handshake,
/// AUTHENTICATE + CURRENT_USER_UPDATE subscription, and the record persist through the host's
/// `store_credential` (DPAPI stays in the executable).
pub(crate) fn authorize_flow(
    api: &LcsdHostApi,
    config: &Config,
    secret: &str,
) -> Result<(), String> {
    if config.discord_client_id.trim().is_empty() {
        return Err("discord_client_id is not configured".into());
    }
    let client_secret = normalized_client_secret(secret).ok_or(
        "LCDSIRPLUS_DISCORD_CLIENT_SECRET must be exposed temporarily only for Discord authorization and cleared immediately afterward",
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut file, _) = open_pipe(api)?;
    let (ready_id, code) = authorize_rpc(&mut file, config, deadline)?;
    let token_owner = TokenExchangeOwner::default();
    let response = exchange_token(
        &token_owner,
        &authorization_code_body(&config.discord_client_id, &code, client_secret),
        deadline,
    )?;
    let record = TokenRecord::from_response(
        &ready_id,
        &config.discord_client_id,
        Some(client_secret),
        response,
        unix_now(),
    )?;
    drop(file);

    let (mut file, _) = open_pipe(api)?;
    let fresh_ready_id = crate::rpc::authorization_handshake(&mut file, config, deadline)?;
    require_same_ready_user(&ready_id, &fresh_ready_id, &record)?;
    let args = format!("{{\"access_token\":{}}}", json_string(&record.access_token));
    let authenticated = pipe_command(
        &mut file,
        "AUTHENTICATE",
        "",
        Some(&args),
        &fresh_ready_id,
        deadline,
    )?;
    crate::rpc::validate_authentication(
        &authenticated.data,
        &fresh_ready_id,
        &record,
        &config.discord_client_id,
    )?;
    pipe_command(
        &mut file,
        "SUBSCRIBE",
        "CURRENT_USER_UPDATE",
        None,
        &fresh_ready_id,
        deadline,
    )?;
    persist_rotated_refresh(api, &record, true)
}

extern "system" fn verify_client(exe_path: *const u16, out_signer: *mut u8, out_cap: u32) -> i32 {
    if exe_path.is_null() || out_cap == 0 {
        return LCSD_E_ARG;
    }
    match with_api(|api| (api.verify_path)(api.context, exe_path, out_signer, out_cap)) {
        Some(code) if code == LCSD_OK => LCSD_OK,
        Some(code) if code == LCSD_E_SIZE => LCSD_E_SIZE,
        Some(_) => LCSD_E_IDENTITY,
        None => {
            set_last_error("discord component: no host verifier is registered");
            LCSD_E_UNAVAILABLE
        }
    }
}

extern "system" fn last_error(out: *mut u8, out_cap: u32) -> i32 {
    if out.is_null() || out_cap == 0 {
        return LCSD_E_ARG;
    }
    let text = LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let bytes = text.as_bytes();
    let len = bytes.len().min(out_cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, len);
        *out.add(len) = 0;
    }
    len as i32
}

pub(crate) static TABLE: LcsdV1 = LcsdV1 {
    size: std::mem::size_of::<LcsdV1>() as u32,
    start,
    set_config,
    poll_io,
    state,
    stop,
    join,
    release,
    authorize,
    verify_client,
    last_error,
};
