//! Discord desktop local RPC, OAuth authorization, and current-user credentials.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::json::Json;
use crate::model::{DiscordState, Speaker};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_TOKEN_RESPONSE: usize = 1024 * 1024;
const MAX_CREDENTIAL: usize = 64 * 1024;
const PIPE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
const REQUIRED_SCOPES: [&str; 3] = ["rpc", "identify", "rpc.voice.read"];

#[derive(Clone, Debug)]
struct Payload {
    event: String,
    nonce: String,
    data: Json,
}

impl Default for Payload {
    fn default() -> Self {
        Self {
            event: String::new(),
            nonce: String::new(),
            data: Json::Null,
        }
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
fn write_packet<W: Write>(writer: &mut W, opcode: u32, body: &[u8]) -> Result<(), String> {
    if body.len() > MAX_FRAME {
        return Err(format!("Discord frame exceeds {} bytes", MAX_FRAME));
    }
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&opcode.to_le_bytes());
    header[4..].copy_from_slice(&(body.len() as u32).to_le_bytes());
    writer.write_all(&header).map_err(|e| e.to_string())?;
    writer.write_all(body).map_err(|e| e.to_string())
}

#[cfg(test)]
fn write_pong<W: Write>(writer: &mut W, body: &[u8]) -> Result<(), String> {
    write_packet(writer, OP_PONG, body)
}

#[cfg(test)]
fn read_packet<R: Read>(reader: &mut R) -> Result<(u32, Vec<u8>), String> {
    let mut header = [0u8; 8];
    reader.read_exact(&mut header).map_err(|e| e.to_string())?;
    let opcode = u32::from_le_bytes(header[..4].try_into().unwrap());
    let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
    if length > MAX_FRAME {
        return Err(format!("Discord frame length {} exceeds limit", length));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok((opcode, body))
}

fn decode_payload(body: &[u8]) -> Result<Payload, String> {
    let text = std::str::from_utf8(body).map_err(|_| "Discord RPC payload is malformed")?;
    let value = Json::parse(text).map_err(|_| "Discord RPC payload is malformed")?;
    Ok(Payload {
        event: value
            .get("evt")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string(),
        nonce: value
            .get("nonce")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string(),
        data: value.get("data").cloned().unwrap_or(Json::Null),
    })
}

fn payload_error(payload: &Payload) -> Result<(), String> {
    if payload.event != "ERROR" {
        return Ok(());
    }
    let code = payload
        .data
        .get("code")
        .and_then(Json::as_f64)
        .ok_or("Discord RPC error payload is malformed")?;
    let code = code as i64;
    let detail = (code == 5000).then(|| {
        classify_oauth_error(
            payload
                .data
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or(""),
        )
    });
    Err(match detail {
        Some(detail) => format!("Discord RPC error code {code}: {detail}"),
        None => format!("Discord RPC error code {code}"),
    })
}

fn classify_oauth_error(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("redirect_uri") || message.contains("redirect uri") {
        "RPC OAuth redirect URI rejected"
    } else if message.contains("scope") {
        "RPC OAuth scope rejected"
    } else if message.contains("client secret")
        || message.contains("client_secret")
        || message.contains("client id")
        || message.contains("client_id")
        || message.contains("invalid client")
    {
        "RPC OAuth client credentials rejected"
    } else if message.contains("application tester")
        || message.contains("app tester")
        || message.contains("not a tester")
    {
        "RPC application tester access rejected"
    } else {
        "OAuth2 error"
    }
}

fn close_error(body: &[u8]) -> String {
    let code = std::str::from_utf8(body)
        .ok()
        .and_then(|text| Json::parse(text).ok())
        .and_then(|value| value.get("code").and_then(Json::as_f64))
        .map(|value| value as i64);
    match code {
        Some(code) => format!("Discord closed RPC code {}", code),
        None => "Discord closed RPC with malformed close payload".into(),
    }
}

#[derive(Default)]
struct Tracker {
    state: DiscordState,
    self_id: String,
}

impl Tracker {
    fn set_channel(&mut self, data: &Json, now: SystemTime) -> Result<(), String> {
        if matches!(data, Json::Null) {
            self.state.channel_id.clear();
            self.state.channel_name.clear();
            self.state.speakers.clear();
            self.state.updated = Some(now);
            return Ok(());
        }
        self.state.channel_id = field(data, "id");
        self.state.channel_name = field(data, "name");
        self.state.speakers.clear();
        for voice in data
            .get("voice_states")
            .and_then(Json::as_arr)
            .unwrap_or_default()
        {
            let speaker = self.speaker_from_voice(voice);
            if speaker.user_id.is_empty() {
                continue;
            }
            if speaker.is_self {
                self.update_self_flags(voice);
            }
            self.state.speakers.push(speaker);
        }
        self.state.updated = Some(now);
        Ok(())
    }

    fn handle(&mut self, event: &str, data: &Json, now: SystemTime) -> Result<bool, String> {
        let mut channel_changed = false;
        match event {
            "SPEAKING_START" | "SPEAKING_STOP" => {
                let id = field(data, "user_id");
                if id.is_empty() {
                    return Err("Discord speaking event omitted user_id".into());
                }
                let index = self.speaker_index_or_insert(&id);
                let speaker = &mut self.state.speakers[index];
                if event == "SPEAKING_START" {
                    speaker.speaking = true;
                    speaker.started_at = Some(now);
                    speaker.stopped_at = None;
                } else {
                    speaker.speaking = false;
                    speaker.stopped_at = Some(now);
                }
            }
            "VOICE_STATE_CREATE" | "VOICE_STATE_UPDATE" => {
                let replacement = self.speaker_from_voice(data);
                if replacement.user_id.is_empty() {
                    return Err("Discord voice state omitted user id".into());
                }
                let index = self.speaker_index_or_insert(&replacement.user_id);
                let existing = &mut self.state.speakers[index];
                existing.name = replacement.name;
                existing.is_self = replacement.is_self;
                if existing.is_self {
                    self.update_self_flags(data);
                }
            }
            "VOICE_STATE_DELETE" => {
                let id = user_id(data);
                self.state.speakers.retain(|speaker| speaker.user_id != id);
            }
            "VOICE_CHANNEL_SELECT" => channel_changed = true,
            "VOICE_SETTINGS_UPDATE" => {
                self.state.self_mute = bool_field(data, "mute");
                self.state.self_deaf = bool_field(data, "deaf");
            }
            "VOICE_CONNECTION_STATUS" => {
                self.state.connection_state = field(data, "state");
                self.state.voice_ping_ms = data
                    .get("average_ping")
                    .and_then(Json::as_f64)
                    .unwrap_or_default();
            }
            _ => return Ok(false),
        }
        self.state.updated = Some(now);
        Ok(channel_changed)
    }

    fn speaker_from_voice(&self, voice: &Json) -> Speaker {
        let user = voice.get("user").unwrap_or(&Json::Null);
        let id = field(user, "id");
        Speaker {
            user_id: id.clone(),
            name: display_name(voice, user, &id),
            is_self: id == self.self_id,
            ..Default::default()
        }
    }

    fn speaker_index_or_insert(&mut self, id: &str) -> usize {
        if let Some(index) = self
            .state
            .speakers
            .iter()
            .position(|speaker| speaker.user_id == id)
        {
            return index;
        }
        self.state.speakers.push(Speaker {
            user_id: id.into(),
            name: format!("USER {}", tail(id, 4)),
            is_self: id == self.self_id,
            ..Default::default()
        });
        self.state.speakers.len() - 1
    }

    fn update_self_flags(&mut self, voice: &Json) {
        let flags = voice.get("voice_state").unwrap_or(&Json::Null);
        self.state.self_mute = bool_field(flags, "self_mute");
        self.state.self_deaf = bool_field(flags, "self_deaf");
    }
}

fn field(value: &Json, key: &str) -> String {
    value
        .get(key)
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string()
}

fn bool_field(value: &Json, key: &str) -> bool {
    value.get(key).and_then(Json::as_bool).unwrap_or(false)
}

fn user_id(value: &Json) -> String {
    value
        .get("user")
        .map(|user| field(user, "id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| field(value, "user_id"))
}

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

fn ready_user_id(data: &Json) -> Result<String, String> {
    let id = data
        .get("user")
        .map(|user| field(user, "id"))
        .unwrap_or_default();
    validate_user_id(&id)?;
    Ok(id)
}

fn has_required_scopes<'a>(scopes: impl Iterator<Item = &'a str> + Clone) -> bool {
    REQUIRED_SCOPES
        .iter()
        .all(|required| scopes.clone().any(|scope| scope == *required))
}

fn scope_string_is_valid(scope: &str) -> bool {
    has_required_scopes(scope.split_ascii_whitespace())
}

fn authenticated_user_id(data: &Json) -> Result<String, String> {
    let id = data
        .get("user")
        .map(|user| field(user, "id"))
        .unwrap_or_default();
    validate_user_id(&id)?;
    let scopes = data
        .get("scopes")
        .and_then(Json::as_arr)
        .ok_or("Discord authentication response omitted scopes")?;
    if scopes.iter().any(|scope| scope.as_str().is_none())
        || !has_required_scopes(scopes.iter().filter_map(Json::as_str))
    {
        return Err("Discord authentication response omitted required scopes".into());
    }
    Ok(id)
}

fn validate_authentication(
    data: &Json,
    ready_id: &str,
    credential: &TokenRecord,
    client_id: &str,
) -> Result<(), String> {
    credential.validate_for(ready_id, client_id)?;
    let authenticated_id = authenticated_user_id(data)?;
    if authenticated_id != ready_id || authenticated_id != credential.user_id {
        return Err(
            "Discord authenticated account does not match the current READY account".into(),
        );
    }
    Ok(())
}

fn current_user_changed(event: &str, data: &Json, ready_id: &str) -> Result<bool, String> {
    if event != "CURRENT_USER_UPDATE" {
        return Ok(false);
    }
    let id = data
        .get("user")
        .map(|user| field(user, "id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| field(data, "id"));
    validate_user_id(&id)?;
    Ok(id != ready_id)
}

fn disconnected_account_state() -> DiscordState {
    DiscordState {
        error: "Discord current account changed; reconnecting".into(),
        updated: Some(SystemTime::now()),
        ..Default::default()
    }
}

fn display_name(voice: &Json, user: &Json, id: &str) -> String {
    [
        field(voice, "nick"),
        field(user, "global_name"),
        field(user, "username"),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or_else(|| format!("USER {}", tail(id, 4)))
}

fn tail(value: &str, count: usize) -> &str {
    let start = value
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map(|(index, _)| index)
        .unwrap_or(0);
    &value[start..]
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TokenRecord {
    version: u32,
    user_id: String,
    client_id: String,
    client_secret: String,
    access_token: String,
    refresh_token: String,
    token_type: String,
    scope: String,
    expires_at: u64,
    updated_at: u64,
}

#[derive(Clone, Debug, Default)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    scope: String,
    expires_in: u64,
}

impl TokenRecord {
    fn from_response(
        user_id: &str,
        client_id: &str,
        client_secret: Option<&str>,
        response: TokenResponse,
        now: u64,
    ) -> Result<Self, String> {
        validate_user_id(user_id)?;
        if response.access_token.trim().is_empty() {
            return Err("Discord token response contained no access_token".into());
        }
        if !scope_string_is_valid(&response.scope) {
            return Err("Discord token response omitted required scopes".into());
        }
        Ok(Self {
            version: 2,
            user_id: user_id.into(),
            client_id: client_id.into(),
            client_secret: client_secret.unwrap_or_default().into(),
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            token_type: response.token_type,
            scope: response.scope,
            expires_at: now.saturating_add(response.expires_in),
            updated_at: now,
        })
    }

    fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at != 0 && now.saturating_add(24 * 60 * 60) >= self.expires_at
    }

    fn encode(&self) -> Vec<u8> {
        format!(
            "{{\"version\":{},\"userId\":{},\"clientId\":{},\"clientSecret\":{},\"accessToken\":{},\"refreshToken\":{},\"tokenType\":{},\"scope\":{},\"expiresAt\":{},\"updatedAt\":{}}}",
            self.version,
            json_string(&self.user_id),
            json_string(&self.client_id),
            json_string(&self.client_secret),
            json_string(&self.access_token),
            json_string(&self.refresh_token),
            json_string(&self.token_type),
            json_string(&self.scope),
            self.expires_at,
            self.updated_at
        )
        .into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "decode Discord credential failed")?;
        let value = Json::parse(text).map_err(|_| "decode Discord credential failed")?;
        let record = Self {
            version: value
                .get("version")
                .and_then(Json::as_f64)
                .unwrap_or_default() as u32,
            user_id: field(&value, "userId"),
            client_id: field(&value, "clientId"),
            client_secret: field(&value, "clientSecret").trim().into(),
            access_token: field(&value, "accessToken"),
            refresh_token: field(&value, "refreshToken"),
            token_type: field(&value, "tokenType"),
            scope: field(&value, "scope"),
            expires_at: value
                .get("expiresAt")
                .and_then(Json::as_f64)
                .unwrap_or_default() as u64,
            updated_at: value
                .get("updatedAt")
                .and_then(Json::as_f64)
                .unwrap_or_default() as u64,
        };
        if record.version != 2 {
            return Err(format!(
                "unsupported Discord credential version {}",
                record.version
            ));
        }
        if record.access_token.trim().is_empty() {
            return Err("Discord credential contains no access token".into());
        }
        validate_user_id(&record.user_id)?;
        if !scope_string_is_valid(&record.scope) {
            return Err("Discord credential omits required scopes".into());
        }
        Ok(record)
    }

    fn validate_for(&self, user_id: &str, client_id: &str) -> Result<(), String> {
        validate_user_id(user_id)?;
        if self.user_id != user_id {
            return Err("stored Discord credential belongs to another account".into());
        }
        if self.client_id != client_id {
            return Err("stored Discord credential belongs to another client ID".into());
        }
        if !scope_string_is_valid(&self.scope) {
            return Err("stored Discord credential omits required scopes".into());
        }
        Ok(())
    }
}

fn parse_token_response(body: &[u8]) -> Result<TokenResponse, String> {
    let text = std::str::from_utf8(body).map_err(|_| "Discord token response is malformed")?;
    let value = Json::parse(text).map_err(|_| "Discord token response is malformed")?;
    Ok(TokenResponse {
        access_token: field(&value, "access_token"),
        refresh_token: field(&value, "refresh_token"),
        token_type: field(&value, "token_type"),
        scope: field(&value, "scope"),
        expires_in: value
            .get("expires_in")
            .and_then(Json::as_f64)
            .unwrap_or_default() as u64,
    })
}

fn retain_if_empty(value: &mut String, current: &str) {
    if value.is_empty() {
        *value = current.into();
    }
}

fn append_token_body(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if body.len() + chunk.len() > MAX_TOKEN_RESPONSE {
        return Err("Discord token response exceeded 1 MiB".into());
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn form_encode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                form_component_encode(key),
                form_component_encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn form_component_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'*' | b'-' | b'.' | b'_') {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn normalized_client_secret(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

// Platform and runtime implementation follows the pure protocol/state core.

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

fn save_token(path: &Path, record: &TokenRecord) -> Result<(), String> {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CREATE_NEW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let plain = record.encode();
    if plain.len() > MAX_CREDENTIAL {
        return Err("Discord credential encoding exceeds 65536-byte limit".into());
    }
    let protected = dpapi(&plain, true)?;
    let parent = path
        .parent()
        .ok_or("Discord credential path has no parent")?;
    let boundary = CredentialBoundary::open(parent, true)?;
    if credential_path_at(parent, &record.user_id)? != path {
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

fn load_token(path: &Path, user_id: &str, client_id: &str) -> Result<Option<TokenRecord>, String> {
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
    if credential_path_at(parent, user_id)? != path {
        return Err("Discord credential path is outside its pinned boundary".into());
    }
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
    let record = TokenRecord::decode(&dpapi(&protected, false)?)?;
    record.validate_for(user_id, client_id)?;
    Ok(Some(record))
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

struct WinHttpHandle(*mut std::ffi::c_void);
impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn remaining_timeout_ms(deadline: Instant) -> Result<i32, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Discord token exchange timed out".into());
    }
    Ok(remaining.as_millis().clamp(1, i32::MAX as u128) as i32)
}

fn exchange_token_native(form: &str, deadline: Instant) -> Result<TokenResponse, String> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Networking::WinHttp::*;
    unsafe {
        let session = WinHttpHandle(WinHttpOpen(
            w!("LCDSirPlus/0.3"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            WINHTTP_FLAG_SECURE_DEFAULTS,
        ));
        if session.0.is_null() {
            return Err("Discord token transport could not start".into());
        }
        let timeout = remaining_timeout_ms(deadline)?;
        WinHttpSetTimeouts(session.0, timeout, timeout, timeout, timeout)
            .map_err(|_| "Discord token transport timeout setup failed".to_string())?;
        let connection = WinHttpHandle(WinHttpConnect(session.0, w!("discord.com"), 443, 0));
        if connection.0.is_null() {
            return Err("Discord token transport could not connect".into());
        }
        let request = WinHttpHandle(WinHttpOpenRequest(
            connection.0,
            w!("POST"),
            w!("/api/oauth2/token"),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("Discord token request could not be created".into());
        }
        let headers: Vec<u16> = "Content-Type: application/x-www-form-urlencoded\r\n"
            .encode_utf16()
            .collect();
        let timeout = remaining_timeout_ms(deadline)?;
        WinHttpSetTimeouts(request.0, timeout, timeout, timeout, timeout)
            .map_err(|_| "Discord token request timeout setup failed".to_string())?;
        WinHttpSendRequest(
            request.0,
            Some(&headers),
            Some(form.as_ptr().cast()),
            form.len() as u32,
            form.len() as u32,
            0,
        )
        .map_err(|_| "Discord token request failed".to_string())?;
        let timeout = remaining_timeout_ms(deadline)?;
        WinHttpSetTimeouts(request.0, timeout, timeout, timeout, timeout)
            .map_err(|_| "Discord token response timeout setup failed".to_string())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|_| "Discord token response failed".to_string())?;
        let mut status = 0u32;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut status_size,
            &mut index,
        )
        .map_err(|_| "Discord token response status is unavailable".to_string())?;
        let mut body = Vec::new();
        loop {
            let timeout = remaining_timeout_ms(deadline)?;
            WinHttpSetTimeouts(request.0, timeout, timeout, timeout, timeout)
                .map_err(|_| "Discord token read timeout setup failed".to_string())?;
            let mut chunk = [0u8; 8192];
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            )
            .map_err(|_| "Discord token response read failed".to_string())?;
            if read == 0 {
                break;
            }
            append_token_body(&mut body, &chunk[..read as usize])?;
        }
        if status / 100 != 2 {
            return Err(format!("Discord token exchange failed: HTTP {}", status));
        }
        parse_token_response(&body)
    }
}

#[derive(Default)]
struct TokenExchangeOwner {
    active: Cell<bool>,
}

impl TokenExchangeOwner {
    fn run<T>(&self, operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        if self.active.replace(true) {
            return Err("Discord token exchange is already active".into());
        }
        struct Reset<'a>(&'a Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.active);
        operation()
    }
}

fn exchange_token(
    owner: &TokenExchangeOwner,
    form: &str,
    deadline: Instant,
) -> Result<TokenResponse, String> {
    owner.run(|| exchange_token_native(form, deadline))
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

fn open_pipe() -> Result<(Pipe, String), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
    let mut rejected = false;
    let mut last = None;
    for index in 0..10 {
        let path = format!(r"\\?\pipe\discord-ipc-{}", index);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(&path)
        {
            Ok(file) => match verify_pipe(&file) {
                Ok(()) => return Ok((Pipe::new(file)?, path)),
                Err(_) => {
                    rejected = true;
                    drop(file);
                }
            },
            Err(error) => last = Some(error),
        }
    }
    if rejected {
        Err("no verified Discord IPC server found".into())
    } else {
        Err(format!(
            "no Discord IPC pipe found: {}",
            last.map(|e| e.kind().to_string())
                .unwrap_or_else(|| "not found".into())
        ))
    }
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

fn transfer_all<F, C>(data: &[u8], canceled: &C, mut transfer: F) -> Result<(), String>
where
    F: FnMut(&[u8]) -> Result<usize, String>,
    C: Fn() -> bool,
{
    let mut offset = 0;
    while offset < data.len() {
        if canceled() {
            return Err("Discord session canceled".into());
        }
        let transferred = transfer(&data[offset..])?;
        if transferred == 0 || transferred > data.len() - offset {
            return Err("Discord IPC write made no valid progress".into());
        }
        offset += transferred;
    }
    Ok(())
}

struct Pipe {
    file: File,
    event: windows::Win32::Foundation::HANDLE,
}

impl Pipe {
    fn new(file: File) -> Result<Self, String> {
        let event =
            unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) }
                .map_err(|_| "Discord IPC completion event could not be created")?;
        Ok(Self { file, event })
    }

    fn handle(&self) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(self.file.as_raw_handle().cast())
    }

    #[allow(clippy::field_reassign_with_default)]
    fn overlapped(&self) -> windows::Win32::System::IO::OVERLAPPED {
        let mut overlapped = windows::Win32::System::IO::OVERLAPPED::default();
        overlapped.hEvent = self.event;
        overlapped
    }

    fn cancel_and_drain(&self, overlapped: &windows::Win32::System::IO::OVERLAPPED) {
        unsafe {
            let _ = windows::Win32::System::IO::CancelIoEx(self.handle(), Some(overlapped));
            let mut transferred = 0;
            let _ = windows::Win32::System::IO::GetOverlappedResult(
                self.handle(),
                overlapped,
                &mut transferred,
                true,
            );
        }
    }

    fn completed(
        &self,
        overlapped: &windows::Win32::System::IO::OVERLAPPED,
        operation: &str,
    ) -> Result<usize, String> {
        let mut transferred = 0;
        unsafe {
            windows::Win32::System::IO::GetOverlappedResult(
                self.handle(),
                overlapped,
                &mut transferred,
                false,
            )
        }
        .map_err(|_| format!("Discord IPC {} failed", operation))?;
        Ok(transferred as usize)
    }

    fn wait<C: Fn() -> bool>(
        &self,
        overlapped: &windows::Win32::System::IO::OVERLAPPED,
        canceled: &C,
        deadline: Option<Instant>,
        operation: &str,
    ) -> Result<usize, String> {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        loop {
            if canceled() {
                self.cancel_and_drain(overlapped);
                return Err("Discord session canceled".into());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                self.cancel_and_drain(overlapped);
                return Err(format!("Discord IPC {} timed out", operation));
            }
            match unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.event, 20) }
            {
                WAIT_OBJECT_0 => {
                    return self.completed(overlapped, operation);
                }
                WAIT_TIMEOUT => {}
                _ => {
                    self.cancel_and_drain(overlapped);
                    return Err(format!("Discord IPC {} wait failed", operation));
                }
            }
        }
    }

    fn write_some<C: Fn() -> bool>(
        &mut self,
        data: &[u8],
        canceled: &C,
        deadline: Instant,
    ) -> Result<usize, String> {
        use windows::Win32::Foundation::ERROR_IO_PENDING;
        unsafe {
            windows::Win32::System::Threading::ResetEvent(self.event)
                .map_err(|_| "Discord IPC write event reset failed".to_string())?;
        }
        let mut overlapped = self.overlapped();
        let result = unsafe {
            windows::Win32::Storage::FileSystem::WriteFile(
                self.handle(),
                Some(data),
                None,
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => self.completed(&overlapped, "write"),
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                self.wait(&overlapped, canceled, Some(deadline), "write")
            }
            Err(_) => Err("Discord IPC write failed".into()),
        }
    }

    fn write_packet<C: Fn() -> bool>(
        &mut self,
        opcode: u32,
        body: &[u8],
        canceled: &C,
    ) -> Result<(), String> {
        if body.len() > MAX_FRAME {
            return Err(format!("Discord frame exceeds {} bytes", MAX_FRAME));
        }
        let mut packet = Vec::with_capacity(body.len() + 8);
        packet.extend_from_slice(&opcode.to_le_bytes());
        packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
        packet.extend_from_slice(body);
        let deadline = Instant::now() + PIPE_WRITE_TIMEOUT;
        transfer_all(&packet, canceled, |remaining| {
            self.write_some(remaining, canceled, deadline)
        })
    }

    fn read_some<C: Fn() -> bool>(
        &mut self,
        data: &mut [u8],
        canceled: &C,
    ) -> Result<usize, String> {
        use windows::Win32::Foundation::ERROR_IO_PENDING;
        unsafe {
            windows::Win32::System::Threading::ResetEvent(self.event)
                .map_err(|_| "Discord IPC read event reset failed".to_string())?;
        }
        let mut overlapped = self.overlapped();
        let result = unsafe {
            windows::Win32::Storage::FileSystem::ReadFile(
                self.handle(),
                Some(data),
                None,
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => self.completed(&overlapped, "read"),
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                self.wait(&overlapped, canceled, None, "read")
            }
            Err(_) => Err("Discord IPC read failed".into()),
        }
    }

    fn read_exact<C: Fn() -> bool>(&mut self, data: &mut [u8], canceled: &C) -> Result<(), String> {
        let mut offset = 0;
        while offset < data.len() {
            let read = self.read_some(&mut data[offset..], canceled)?;
            if read == 0 || read > data.len() - offset {
                return Err("Discord IPC closed".into());
            }
            offset += read;
        }
        Ok(())
    }

    fn read_packet<C: Fn() -> bool>(&mut self, canceled: &C) -> Result<(u32, Vec<u8>), String> {
        let mut header = [0u8; 8];
        self.read_exact(&mut header, canceled)?;
        let opcode = u32::from_le_bytes(header[..4].try_into().unwrap());
        let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        if length > MAX_FRAME {
            return Err(format!("Discord frame length {} exceeds limit", length));
        }
        let mut body = vec![0u8; length];
        self.read_exact(&mut body, canceled)?;
        Ok((opcode, body))
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::IO::CancelIoEx(self.handle(), None);
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

fn refresh_token_if_needed(
    config: &Config,
    mut current: TokenRecord,
    token_owner: &TokenExchangeOwner,
) -> Result<(TokenRecord, bool), String> {
    if !current.needs_refresh(unix_now()) {
        return Ok((current, false));
    }
    if current.refresh_token.is_empty() {
        return Err(
            "access token is near expiration and no refresh token is stored; authorize again"
                .into(),
        );
    }
    let client_secret = normalized_client_secret(&current.client_secret).map(str::to_owned);
    let mut fields = vec![
        ("client_id", config.discord_client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", current.refresh_token.as_str()),
    ];
    if let Some(client_secret) = client_secret.as_deref() {
        fields.push(("client_secret", client_secret));
    }
    let mut response = exchange_token(
        token_owner,
        &form_encode(&fields),
        Instant::now() + TOKEN_TIMEOUT,
    )?;
    retain_if_empty(&mut response.refresh_token, &current.refresh_token);
    retain_if_empty(&mut response.scope, &current.scope);
    current = TokenRecord::from_response(
        &current.user_id,
        &current.client_id,
        client_secret.as_deref(),
        response,
        unix_now(),
    )?;
    Ok((current, true))
}

fn persist_rotated_refresh(
    path: &Path,
    credential: &TokenRecord,
    refreshed: bool,
) -> Result<(), String> {
    if refreshed {
        // OAuth refresh-token rotation may invalidate the old record before IPC validation.
        save_token(path, credential)?;
    }
    Ok(())
}

fn require_same_ready_user(
    initial: &str,
    fresh: &str,
    credential: &TokenRecord,
) -> Result<(), String> {
    validate_user_id(initial)?;
    validate_user_id(fresh)?;
    if initial != fresh || fresh != credential.user_id {
        return Err("Discord current account changed before authentication".into());
    }
    Ok(())
}

fn require_current_user_subscription(subscribed: bool) -> Result<(), String> {
    if subscribed {
        Ok(())
    } else {
        Err("Discord current-account monitor is not established".into())
    }
}

struct Rpc<'a> {
    file: Pipe,
    tracker: Tracker,
    updates: &'a mpsc::Sender<DiscordState>,
    latest: &'a Mutex<DiscordState>,
    config: &'a Arc<RwLock<Config>>,
    shutdown: &'a AtomicBool,
    session_config: Config,
    ready_user_id: String,
    nonce: u64,
    account_changed: bool,
    current_user_subscribed: bool,
    pending_refresh: bool,
    global_subscribed: bool,
    channel_subscribed: String,
}

impl Rpc<'_> {
    fn clear_session(&mut self) {
        self.account_changed = true;
        let state = disconnected_account_state();
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
        let _ = self.updates.send(state);
    }

    fn handle_event(&mut self, event: &str, data: &Json) -> Result<bool, String> {
        match current_user_changed(event, data, &self.ready_user_id) {
            Ok(true) => {
                self.clear_session();
                return Err("Discord current account changed".into());
            }
            Err(error) => {
                self.clear_session();
                return Err(error);
            }
            Ok(false) => {}
        }
        self.tracker.handle(event, data, SystemTime::now())
    }

    fn read(&mut self) -> Result<(u32, Vec<u8>), String> {
        let shutdown = self.shutdown;
        let config = self.config;
        let session = self.session_config.clone();
        self.file.read_packet(&|| {
            shutdown.load(Ordering::Relaxed)
                || session_config_changed(
                    &session,
                    &config.read().unwrap_or_else(|e| e.into_inner()),
                )
        })
    }

    fn write(&mut self, opcode: u32, body: &[u8]) -> Result<(), String> {
        let shutdown = self.shutdown;
        let config = self.config;
        let session = self.session_config.clone();
        self.file.write_packet(opcode, body, &|| {
            shutdown.load(Ordering::Relaxed)
                || session_config_changed(
                    &session,
                    &config.read().unwrap_or_else(|e| e.into_inner()),
                )
        })
    }

    fn command(
        &mut self,
        command: &str,
        event: &str,
        args: Option<&str>,
    ) -> Result<Payload, String> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        let mut body = format!(
            "{{\"cmd\":{},\"nonce\":{}",
            json_string(command),
            json_string(&nonce)
        );
        if !event.is_empty() {
            body.push_str(&format!(",\"evt\":{}", json_string(event)));
        }
        if let Some(args) = args {
            body.push_str(",\"args\":");
            body.push_str(args);
        }
        body.push('}');
        self.write(OP_FRAME, body.as_bytes())?;
        loop {
            let (opcode, data) = self.read()?;
            match opcode {
                OP_PING => self.write(OP_PONG, &data)?,
                OP_CLOSE => return Err(close_error(&data)),
                OP_FRAME => {
                    let payload = match decode_payload(&data) {
                        Ok(payload) => payload,
                        Err(_) => continue,
                    };
                    if !payload.event.is_empty() && payload.nonce != nonce {
                        let changed = self.handle_event(&payload.event, &payload.data)?;
                        self.pending_refresh |= changed;
                        if self.current_user_subscribed {
                            self.publish();
                        }
                        continue;
                    }
                    if payload.nonce != nonce {
                        continue;
                    }
                    payload_error(&payload)?;
                    return Ok(payload);
                }
                _ => {}
            }
        }
    }

    fn refresh_channel(&mut self) -> Result<(), String> {
        const CHANNEL_EVENTS: [&str; 5] = [
            "VOICE_STATE_CREATE",
            "VOICE_STATE_UPDATE",
            "VOICE_STATE_DELETE",
            "SPEAKING_START",
            "SPEAKING_STOP",
        ];
        require_current_user_subscription(self.current_user_subscribed)?;
        for _ in 0..3 {
            self.pending_refresh = false;
            let response = self.command("GET_SELECTED_VOICE_CHANNEL", "", None)?;
            self.tracker
                .set_channel(&response.data, SystemTime::now())?;
            if !self.global_subscribed {
                for event in [
                    "VOICE_CHANNEL_SELECT",
                    "VOICE_SETTINGS_UPDATE",
                    "VOICE_CONNECTION_STATUS",
                ] {
                    self.command("SUBSCRIBE", event, None)?;
                }
                self.global_subscribed = true;
            }
            let selected = self.tracker.state.channel_id.clone();
            if !self.channel_subscribed.is_empty() && self.channel_subscribed != selected {
                let old = self.channel_subscribed.clone();
                for event in CHANNEL_EVENTS {
                    let args = format!("{{\"channel_id\":{}}}", json_string(&old));
                    let _ = self.command("UNSUBSCRIBE", event, Some(&args));
                }
                self.channel_subscribed.clear();
            }
            if !selected.is_empty() && self.channel_subscribed != selected {
                for event in CHANNEL_EVENTS {
                    let args = format!("{{\"channel_id\":{}}}", json_string(&selected));
                    self.command("SUBSCRIBE", event, Some(&args))?;
                }
                self.channel_subscribed = selected;
            }
            self.publish();
            if !self.pending_refresh {
                return Ok(());
            }
        }
        Err("Discord voice channel changed repeatedly while subscriptions were refreshed".into())
    }

    fn publish(&self) {
        let mut state = self.tracker.state.clone();
        state.connected = true;
        state.authenticated = true;
        state.error.clear();
        state.updated = Some(SystemTime::now());
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
        let _ = self.updates.send(state);
    }
}

fn new_rpc<'a>(
    file: Pipe,
    config: Config,
    shared: &'a Arc<RwLock<Config>>,
    shutdown: &'a AtomicBool,
    updates: &'a mpsc::Sender<DiscordState>,
    latest: &'a Mutex<DiscordState>,
) -> Rpc<'a> {
    Rpc {
        file,
        tracker: Tracker::default(),
        updates,
        latest,
        config: shared,
        shutdown,
        session_config: config,
        ready_user_id: String::new(),
        nonce: 0,
        account_changed: false,
        current_user_subscribed: false,
        pending_refresh: false,
        global_subscribed: false,
        channel_subscribed: String::new(),
    }
}

fn runtime_handshake(rpc: &mut Rpc<'_>, client_id: &str) -> Result<String, String> {
    let handshake = format!("{{\"v\":1,\"client_id\":{}}}", json_string(client_id));
    rpc.write(OP_HANDSHAKE, handshake.as_bytes())?;
    let (opcode, body) = rpc.read()?;
    if opcode != OP_FRAME {
        return Err(format!("Discord handshake returned opcode {}", opcode));
    }
    let ready = decode_payload(&body)?;
    payload_error(&ready)?;
    if ready.event != "READY" {
        return Err("Discord handshake did not return READY".into());
    }
    ready_user_id(&ready.data)
}

fn connect_and_serve(
    config: Config,
    shared: &Arc<RwLock<Config>>,
    shutdown: &AtomicBool,
    updates: &mpsc::Sender<DiscordState>,
    latest: &Mutex<DiscordState>,
    token_owner: &TokenExchangeOwner,
) -> Result<(), String> {
    if config.discord_client_id.trim().is_empty() {
        return Err("discord_client_id is not configured".into());
    }
    let (file, _) = open_pipe()?;
    let mut rpc = new_rpc(file, config.clone(), shared, shutdown, updates, latest);
    let ready_id = runtime_handshake(&mut rpc, &config.discord_client_id)?;
    let path = credential_path(&ready_id)?;
    let credential = match load_token(&path, &ready_id, &config.discord_client_id)? {
        Some(record) => record,
        None if credential_root()?.join("discord.token").exists() => {
            return Err("legacy Discord credential is unsupported; run --discord-clear-token, then authorize the current account".into())
        }
        None => return Err("Discord current account is not authorized; run LCDSirPlus.exe --discord-authorize".into()),
    };
    let (credential, refreshed) = refresh_token_if_needed(&config, credential, token_owner)?;
    persist_rotated_refresh(&path, &credential, refreshed)?;
    if refreshed {
        drop(rpc);
        let (file, _) = open_pipe()?;
        rpc = new_rpc(file, config.clone(), shared, shutdown, updates, latest);
        let fresh_ready_id = runtime_handshake(&mut rpc, &config.discord_client_id)?;
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
        &config.discord_client_id,
    )?;
    rpc.command("SUBSCRIBE", "CURRENT_USER_UPDATE", None)?;
    rpc.current_user_subscribed = true;
    if let Err(error) = rpc.refresh_channel() {
        if rpc.account_changed {
            return Err(error);
        }
        crate::log_warn!("Discord initial channel query failed: {}", error);
    }
    loop {
        let (opcode, data) = rpc.read()?;
        match opcode {
            OP_PING => rpc.write(OP_PONG, &data)?,
            OP_CLOSE => return Err(close_error(&data)),
            OP_FRAME => {
                let payload = match decode_payload(&data) {
                    Ok(payload) => payload,
                    Err(_) => continue,
                };
                if payload_error(&payload).is_err() || payload.event.is_empty() {
                    continue;
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
    }
}

fn unavailable(_previous: &DiscordState, error: &str) -> DiscordState {
    DiscordState {
        error: error.into(),
        updated: Some(SystemTime::now()),
        ..Default::default()
    }
}

fn provider_enabled(config: &Config) -> bool {
    config.discord_enabled && !config.safe_mode
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(30))
}

pub struct Runtime {
    pub updates: mpsc::Receiver<DiscordState>,
    config: Arc<RwLock<Config>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Runtime {
    pub fn update_config(&self, config: &Config) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn(config: &Config) -> Runtime {
    let shared = Arc::new(RwLock::new(config.clone()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker_config = shared.clone();
    let worker_shutdown = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name("lcdsirplus-discord".into())
        .spawn(move || {
            let mut state = DiscordState::default();
            let latest = Mutex::new(DiscordState::default());
            let token_owner = TokenExchangeOwner::default();
            let mut backoff = Duration::from_secs(1);
            while !worker_shutdown.load(Ordering::Relaxed) {
                let config = worker_config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if !provider_enabled(&config) {
                    state = unavailable(&state, "disabled");
                    let _ = tx.send(state.clone());
                    std::thread::sleep(Duration::from_millis(100));
                    backoff = Duration::from_secs(1);
                    continue;
                }
                let started = Instant::now();
                let result = connect_and_serve(
                    config.clone(),
                    &worker_config,
                    &worker_shutdown,
                    &tx,
                    &latest,
                    &token_owner,
                );
                if worker_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let changed = session_config_changed(
                    &config,
                    &worker_config.read().unwrap_or_else(|e| e.into_inner()),
                );
                if changed {
                    state = unavailable(&state, "reconfiguring");
                    let _ = tx.send(state.clone());
                    backoff = Duration::from_secs(1);
                    continue;
                }
                if started.elapsed() >= Duration::from_secs(30) {
                    backoff = Duration::from_secs(1);
                }
                let error = result
                    .err()
                    .unwrap_or_else(|| "Discord RPC disconnected".into());
                state = unavailable(&latest.lock().unwrap_or_else(|e| e.into_inner()), &error);
                let _ = tx.send(state.clone());
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
        })
        .expect("spawn Discord worker");
    Runtime {
        updates: rx,
        config: shared,
        shutdown,
        thread: Some(thread),
    }
}

fn pipe_command(
    file: &mut Pipe,
    command: &str,
    event: &str,
    args: Option<&str>,
    ready_id: &str,
    deadline: Instant,
) -> Result<Payload, String> {
    let canceled = || Instant::now() >= deadline;
    let nonce = random_nonce()?;
    let mut body = format!(
        "{{\"cmd\":{},\"nonce\":{}",
        json_string(command),
        json_string(&nonce)
    );
    if !event.is_empty() {
        body.push_str(&format!(",\"evt\":{}", json_string(event)));
    }
    if let Some(args) = args {
        body.push_str(",\"args\":");
        body.push_str(args);
    }
    body.push('}');
    file.write_packet(OP_FRAME, body.as_bytes(), &canceled)?;
    loop {
        let (opcode, body) = file.read_packet(&canceled)?;
        match opcode {
            OP_PING => file.write_packet(OP_PONG, &body, &canceled)?,
            OP_CLOSE => return Err(close_error(&body)),
            OP_FRAME => {
                let payload = match decode_payload(&body) {
                    Ok(payload) => payload,
                    Err(_) => continue,
                };
                if payload.event == "CURRENT_USER_UPDATE" {
                    if current_user_changed(&payload.event, &payload.data, ready_id)? {
                        return Err("Discord current account changed during authorization".into());
                    }
                    continue;
                }
                if payload.nonce != nonce {
                    continue;
                }
                payload_error(&payload)?;
                return Ok(payload);
            }
            _ => {}
        }
    }
}

fn authorization_handshake(
    file: &mut Pipe,
    config: &Config,
    deadline: Instant,
) -> Result<String, String> {
    let canceled = || Instant::now() >= deadline;
    let handshake = format!(
        "{{\"v\":1,\"client_id\":{}}}",
        json_string(&config.discord_client_id)
    );
    file.write_packet(OP_HANDSHAKE, handshake.as_bytes(), &canceled)?;
    let (opcode, body) = file.read_packet(&canceled)?;
    if opcode != OP_FRAME {
        return Err(format!(
            "Discord authorization handshake returned opcode {}",
            opcode
        ));
    }
    let ready = decode_payload(&body)?;
    payload_error(&ready)?;
    if ready.event != "READY" {
        return Err("Discord authorization handshake did not return READY".into());
    }
    ready_user_id(&ready.data)
}

fn authorize_rpc(
    file: &mut Pipe,
    config: &Config,
    deadline: Instant,
) -> Result<(String, String), String> {
    let ready_id = authorization_handshake(file, config, deadline)?;
    let args = authorize_args(&config.discord_client_id);
    let response = pipe_command(file, "AUTHORIZE", "", Some(&args), &ready_id, deadline)?;
    let code = authorization_code(&response, &response.nonce)?;
    Ok((ready_id, code))
}

fn authorize_args(client_id: &str) -> String {
    format!(
        "{{\"client_id\":{},\"scopes\":[\"identify\",\"rpc\",\"rpc.voice.read\"]}}",
        json_string(client_id)
    )
}

fn authorization_code_body(client_id: &str, code: &str, client_secret: &str) -> String {
    form_encode(&[
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_secret", client_secret),
    ])
}

fn authorization_code(payload: &Payload, nonce: &str) -> Result<String, String> {
    if payload.nonce != nonce {
        return Err("Discord authorization response nonce did not match".into());
    }
    payload_error(payload)?;
    let code = field(&payload.data, "code");
    if code.is_empty() {
        Err("Discord authorization response omitted code".into())
    } else {
        Ok(code)
    }
}

pub fn authorize(config: &Config, client_secret: &str) -> Result<(), String> {
    if config.discord_client_id.trim().is_empty() {
        return Err("discord_client_id is not configured".into());
    }
    let client_secret = normalized_client_secret(client_secret).ok_or(
        "LCDSIRPLUS_DISCORD_CLIENT_SECRET must be exposed temporarily only for Discord authorization and cleared immediately afterward",
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut file, _) = open_pipe()?;
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

    let (mut file, _) = open_pipe()?;
    let fresh_ready_id = authorization_handshake(&mut file, config, deadline)?;
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
    validate_authentication(
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
    save_token(&credential_path(&ready_id)?, &record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

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

    fn token_record(user_id: &str, client_id: &str, access_token: &str) -> TokenRecord {
        TokenRecord {
            version: 2,
            user_id: user_id.into(),
            client_id: client_id.into(),
            access_token: access_token.into(),
            refresh_token: format!("refresh-{access_token}"),
            scope: "identify rpc rpc.voice.read".into(),
            ..Default::default()
        }
    }

    struct Partial {
        bytes: Cursor<Vec<u8>>,
        chunk: usize,
        written: Vec<u8>,
    }

    impl Read for Partial {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let size = out.len().min(self.chunk);
            self.bytes.read(&mut out[..size])
        }
    }
    impl Write for Partial {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            let size = input.len().min(self.chunk);
            self.written.extend_from_slice(&input[..size]);
            Ok(size)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frames_round_trip_with_partial_io_and_limits() {
        let mut io = Partial {
            bytes: Cursor::new(Vec::new()),
            chunk: 2,
            written: Vec::new(),
        };
        write_packet(&mut io, OP_HANDSHAKE, br#"{"v":1}"#).unwrap();
        io.bytes = Cursor::new(io.written.clone());
        assert_eq!(
            read_packet(&mut io).unwrap(),
            (OP_HANDSHAKE, br#"{"v":1}"#.to_vec())
        );
        assert!(write_packet(&mut Vec::new(), OP_FRAME, &vec![0; MAX_FRAME + 1]).is_err());
        let mut oversized = Vec::new();
        oversized.extend_from_slice(&OP_FRAME.to_le_bytes());
        oversized.extend_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        assert!(read_packet(&mut Cursor::new(oversized)).is_err());

        let ping_body = br#"{"nonce":"ping"}"#;
        let mut pong = Vec::new();
        write_pong(&mut pong, ping_body).unwrap();
        assert_eq!(
            read_packet(&mut Cursor::new(pong)).unwrap(),
            (OP_PONG, ping_body.to_vec())
        );
    }

    #[test]
    fn remote_errors_never_include_remote_text() {
        let private = [
            "FAKE_REMOTE_SECRET",
            "FAKE_REMOTE_TOKEN",
            "FAKE_REMOTE_NONCE",
        ];
        let payload = decode_payload(
            format!(
                r#"{{"evt":"ERROR","data":{{"code":4000,"message":"Bad request {} {} {}"}}}}"#,
                private[0], private[1], private[2]
            )
            .as_bytes(),
        )
        .unwrap();
        let error = payload_error(&payload).unwrap_err();
        assert_eq!(error, "Discord RPC error code 4000");
        let close =
            close_error(format!(r#"{{"code":4014,"message":"{}"}}"#, private.join(" ")).as_bytes());
        assert_eq!(close, "Discord closed RPC code 4014");
        assert!(private
            .iter()
            .all(|value| !error.contains(value) && !close.contains(value)));
    }

    #[test]
    fn rpc_oauth_errors_use_only_fixed_local_classifications() {
        let private = "FAKE_MESSAGE_SECRET FAKE_MESSAGE_TOKEN FAKE_MESSAGE_NONCE";
        for (message, expected) in [
            (
                format!("Redirect URI cannot be used in this RPC flow {private}"),
                "RPC OAuth redirect URI rejected",
            ),
            (
                format!("invalid scope {private}"),
                "RPC OAuth scope rejected",
            ),
            (
                format!("invalid client_secret {private}"),
                "RPC OAuth client credentials rejected",
            ),
            (
                format!("not an application tester {private}"),
                "RPC application tester access rejected",
            ),
            (format!("unknown OAuth problem {private}"), "OAuth2 error"),
        ] {
            let payload = Payload {
                event: "ERROR".into(),
                data: Json::parse(&format!(
                    r#"{{"code":5000,"message":{}}}"#,
                    json_string(&message)
                ))
                .unwrap(),
                ..Default::default()
            };
            let error = payload_error(&payload).unwrap_err();
            assert_eq!(error, format!("Discord RPC error code 5000: {expected}"));
            assert!(private
                .split_whitespace()
                .all(|value| !error.contains(value)));
        }
    }

    #[test]
    fn tracker_covers_channel_names_membership_speaking_and_voice_state() {
        let mut tracker = Tracker {
            self_id: "1".into(),
            ..Default::default()
        };
        let channel = Json::parse(r#"{"id":"c","name":"General","voice_states":[{"voice_state":{"self_mute":true,"self_deaf":false},"user":{"id":"1","username":"self"}},{"voice_state":{},"user":{"id":"23456","username":"user","global_name":"global"},"nick":"nick"}]}"#).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(10);
        tracker.set_channel(&channel, now).unwrap();
        assert_eq!(
            (
                tracker.state.channel_id.as_str(),
                tracker.state.channel_name.as_str()
            ),
            ("c", "General")
        );
        assert!(tracker.state.self_mute && !tracker.state.self_deaf);
        assert_eq!(tracker.state.speakers[1].name, "nick");
        tracker
            .handle(
                "SPEAKING_START",
                &Json::parse(r#"{"user_id":"23456"}"#).unwrap(),
                now,
            )
            .unwrap();
        assert!(tracker.state.speakers[1].speaking);
        tracker
            .handle(
                "SPEAKING_STOP",
                &Json::parse(r#"{"user_id":"23456"}"#).unwrap(),
                now + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(
            tracker.state.speakers[1].stopped_at,
            Some(now + Duration::from_secs(1))
        );
        tracker
            .handle(
                "VOICE_SETTINGS_UPDATE",
                &Json::parse(r#"{"mute":false,"deaf":true}"#).unwrap(),
                now,
            )
            .unwrap();
        tracker
            .handle(
                "VOICE_CONNECTION_STATUS",
                &Json::parse(r#"{"state":"CONNECTED","average_ping":12.5}"#).unwrap(),
                now,
            )
            .unwrap();
        assert!(!tracker.state.self_mute && tracker.state.self_deaf);
        assert_eq!(
            (
                tracker.state.connection_state.as_str(),
                tracker.state.voice_ping_ms
            ),
            ("CONNECTED", 12.5)
        );
        assert!(tracker
            .handle("VOICE_CHANNEL_SELECT", &Json::Null, now)
            .unwrap());
        tracker.set_channel(&Json::Null, now).unwrap();
        assert!(tracker.state.speakers.is_empty() && tracker.state.channel_id.is_empty());
    }

    #[test]
    fn display_name_fallbacks_are_exact() {
        let user =
            Json::parse(r#"{"id":"123456","username":"name","global_name":"global"}"#).unwrap();
        assert_eq!(
            display_name(&Json::parse(r#"{"nick":"nick"}"#).unwrap(), &user, "123456"),
            "nick"
        );
        assert_eq!(display_name(&Json::Null, &user, "123456"), "global");
        assert_eq!(
            display_name(
                &Json::Null,
                &Json::parse(r#"{"id":"123456"}"#).unwrap(),
                "123456"
            ),
            "USER 3456"
        );
    }

    #[test]
    fn token_record_refresh_and_form_encoding() {
        let response = TokenResponse {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            scope: "rpc.voice.read identify rpc".into(),
            expires_in: 7 * 86400,
            ..Default::default()
        };
        let record =
            TokenRecord::from_response("111", "123", Some("secret"), response, 1000).unwrap();
        assert_eq!(TokenRecord::decode(&record.encode()).unwrap(), record);
        assert!(record.validate_for("111", "123").is_ok());
        assert!(record.validate_for("222", "123").is_err());
        assert!(record.validate_for("111", "456").is_err());
        assert!(!record.needs_refresh(1000));
        assert!(record.needs_refresh(record.expires_at - 23 * 3600));
        assert_eq!(form_component_encode(" *~%é"), "+*%7E%25%C3%A9");
        assert!(TokenRecord::decode(br#"{"version":2,"accessToken":"x"}"#).is_err());
        assert!(TokenRecord::decode(br#"{"version":1,"accessToken":"x"}"#).is_err());
        let mut invalid = record.clone();
        invalid.scope = "identify rpc".into();
        assert!(TokenRecord::decode(&invalid.encode()).is_err());
    }

    #[test]
    fn ready_authentication_and_account_updates_are_identity_bound() {
        let ready = Json::parse(r#"{"user":{"id":"111"}}"#).unwrap();
        assert_eq!(ready_user_id(&ready).unwrap(), "111");
        for invalid in [
            r#"{}"#,
            r#"{"user":{"id":"../111"}}"#,
            r#"{"user":{"id":"0111"}}"#,
        ] {
            assert!(ready_user_id(&Json::parse(invalid).unwrap()).is_err());
        }
        let record = token_record("111", "123", "access-a");
        let auth_a =
            Json::parse(r#"{"user":{"id":"111"},"scopes":["rpc","identify","rpc.voice.read"]}"#)
                .unwrap();
        let auth_b =
            Json::parse(r#"{"user":{"id":"222"},"scopes":["rpc","identify","rpc.voice.read"]}"#)
                .unwrap();
        assert!(validate_authentication(&auth_a, "111", &record, "123").is_ok());
        assert!(validate_authentication(&auth_b, "111", &record, "123").is_err());
        assert!(
            validate_authentication(&auth_a, "111", &token_record("222", "123", "b"), "123")
                .is_err()
        );
        assert!(validate_authentication(
            &Json::parse(r#"{"user":{"id":"111"},"scopes":["rpc","identify"]}"#).unwrap(),
            "111",
            &record,
            "123"
        )
        .is_err());
        assert!(validate_authentication(
            &Json::parse(r#"{"user":{"id":"111"},"scopes":["rpc","identify","rpc.voice.read",7]}"#)
                .unwrap(),
            "111",
            &record,
            "123"
        )
        .is_err());
        assert!(!current_user_changed(
            "CURRENT_USER_UPDATE",
            &Json::parse(r#"{"id":"111"}"#).unwrap(),
            "111"
        )
        .unwrap());
        assert!(current_user_changed(
            "CURRENT_USER_UPDATE",
            &Json::parse(r#"{"id":"222"}"#).unwrap(),
            "111"
        )
        .unwrap());
        let state = disconnected_account_state();
        assert!(!state.connected && !state.authenticated);
        assert!(state.channel_id.is_empty() && state.speakers.is_empty());
        assert!(require_current_user_subscription(false).is_err());
        assert!(require_current_user_subscription(true).is_ok());
    }

    #[test]
    fn token_response_errors_are_redacted() {
        let secret = b"PRIVATE_OAUTH_BODY";
        let error = parse_token_response(secret).unwrap_err();
        assert_eq!(error, "Discord token response is malformed");
        assert!(!error.contains(std::str::from_utf8(secret).unwrap()));
    }

    #[test]
    fn token_response_limit_and_refresh_retention_are_exact() {
        let mut body = vec![0; MAX_TOKEN_RESPONSE];
        assert!(append_token_body(&mut body, &[1]).is_err());
        let mut response = TokenResponse::default();
        retain_if_empty(&mut response.refresh_token, "old-refresh");
        assert_eq!(response.refresh_token, "old-refresh");
        response.refresh_token = "new-refresh".into();
        retain_if_empty(&mut response.refresh_token, "old-refresh");
        assert_eq!(response.refresh_token, "new-refresh");
        retain_if_empty(&mut response.scope, "identify rpc rpc.voice.read");
        assert!(scope_string_is_valid(&response.scope));
    }

    #[test]
    fn token_exchange_owner_rejects_overlap_and_releases_after_terminal_return() {
        let owner = TokenExchangeOwner::default();
        owner
            .run(|| {
                assert_eq!(
                    owner.run(|| Ok(())).unwrap_err(),
                    "Discord token exchange is already active"
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(
            owner.run::<()>(|| Err("terminal".into())).unwrap_err(),
            "terminal"
        );
        assert!(owner.run(|| Ok(())).is_ok());
    }

    #[test]
    fn stalled_write_seam_observes_cancellation_before_another_transfer() {
        let canceled = Cell::new(false);
        let calls = Cell::new(0);
        let error = transfer_all(&[1, 2], &|| canceled.get(), |_| {
            calls.set(calls.get() + 1);
            canceled.set(true);
            Ok(1)
        })
        .unwrap_err();
        assert_eq!(error, "Discord session canceled");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn per_account_credentials_are_independent_and_path_bound() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        let path_a = credential_path_at(&root, "111").unwrap();
        let path_b = credential_path_at(&root, "222").unwrap();
        let record_a = token_record("111", "123", "private-a");
        let record_b = token_record("222", "123", "private-b");
        save_token(&path_a, &record_a).unwrap();
        save_token(&path_b, &record_b).unwrap();
        assert_eq!(
            load_token(&path_a, "111", "123").unwrap(),
            Some(record_a.clone())
        );
        assert_eq!(
            load_token(&path_b, "222", "123").unwrap(),
            Some(record_b.clone())
        );
        assert!(save_token(&path_a, &record_b).is_err());
        assert_eq!(load_token(&path_a, "111", "123").unwrap(), Some(record_a));
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
    fn rotated_refresh_persists_for_original_user_before_later_rpc_failure() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        let path = credential_path_at(&root, "111").unwrap();
        let other_path = credential_path_at(&root, "222").unwrap();
        let old = token_record("111", "123", "old-access");
        let other = token_record("222", "123", "other-access");
        save_token(&path, &old).unwrap();
        save_token(&other_path, &other).unwrap();
        let refreshed = token_record("111", "123", "refreshed-access");
        persist_rotated_refresh(&path, &refreshed, true).unwrap();
        let mismatched =
            Json::parse(r#"{"user":{"id":"222"},"scopes":["rpc","identify","rpc.voice.read"]}"#)
                .unwrap();
        assert!(validate_authentication(&mismatched, "111", &refreshed, "123").is_err());
        assert_eq!(
            load_token(&path, "111", "123").unwrap(),
            Some(refreshed.clone())
        );
        assert!(persist_rotated_refresh(&other_path, &refreshed, true).is_err());
        assert_eq!(load_token(&other_path, "222", "123").unwrap(), Some(other));
        assert!(require_same_ready_user("111", "222", &refreshed).is_err());
        assert!(require_same_ready_user("111", "111", &refreshed).is_ok());
    }

    #[test]
    fn clear_removes_only_exact_owned_credentials_and_legacy() {
        let temp = TestDir::new();
        let root = temp.0.join("runtime");
        let owned = credential_path_at(&root, "111").unwrap();
        save_token(&owned, &token_record("111", "123", "private")).unwrap();
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
                let error = load_token(&hard_link, "111", "123").unwrap_err();
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
    fn authorization_nonce_and_code_are_required_without_echo() {
        let payload = Payload {
            nonce: "expected".into(),
            data: Json::parse(r#"{"code":"private-code"}"#).unwrap(),
            ..Default::default()
        };
        assert_eq!(
            authorization_code(&payload, "expected").unwrap(),
            "private-code"
        );
        let error = authorization_code(&payload, "other").unwrap_err();
        assert!(!error.contains("private-code"));
        let missing = Payload {
            nonce: "expected".into(),
            data: Json::Null,
            ..Default::default()
        };
        assert!(authorization_code(&missing, "expected").is_err());
    }

    #[test]
    fn rpc_authorization_payloads_follow_the_redirectless_flow() {
        let args = authorize_args("123456");
        assert_eq!(
            args,
            r#"{"client_id":"123456","scopes":["identify","rpc","rpc.voice.read"]}"#
        );
        assert!(!args.contains("redirect_uri"));

        let secret = normalized_client_secret("  secret+x  ").unwrap();
        let body = authorization_code_body("123456", " *~%é", secret);
        assert_eq!(
            body,
            "client_id=123456&grant_type=authorization_code&code=+*%7E%25%C3%A9&client_secret=secret%2Bx"
        );
        assert!(!body.contains("redirect_uri"));

        let response = TokenResponse {
            access_token: "access".into(),
            scope: "identify rpc rpc.voice.read".into(),
            ..Default::default()
        };
        let record =
            TokenRecord::from_response("111", "123456", Some(secret), response.clone(), 1).unwrap();
        assert_eq!(record.client_secret, "secret+x");
        assert_eq!(TokenRecord::decode(&record.encode()).unwrap(), record);

        let empty_record = TokenRecord::from_response("111", "123456", None, response, 1).unwrap();
        assert!(empty_record.client_secret.is_empty());
        assert!(TokenRecord::decode(&empty_record.encode())
            .unwrap()
            .client_secret
            .is_empty());
    }

    #[test]
    fn authorization_requires_secret_before_discord_io() {
        let config = Config {
            discord_client_id: "123456".into(),
            ..Config::default()
        };
        for secret in ["", " \t\r\n "] {
            assert_eq!(
                authorize(&config, secret).unwrap_err(),
                "LCDSIRPLUS_DISCORD_CLIENT_SECRET must be exposed temporarily only for Discord authorization and cleared immediately afterward"
            );
        }
    }

    #[test]
    fn lifecycle_fingerprint_safe_mode_and_backoff_are_bounded() {
        let base = Config::default();
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
}
