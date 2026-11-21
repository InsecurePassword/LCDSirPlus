//! Discord RPC component core (WO-TEL-D3-C S5): the RPC framing, payload codec, OAuth
//! classification, speaker tracker, token-record codec, WinHTTP token transport, IPC pipe
//! transport and the session command driver. This region MOVED (not duplicated) from
//! `src/providers/discord.rs` at `42ad35f`; its text is preserved function-for-function, with
//! only the boundary seams changed: the host owns credentials (DPAPI + `CredentialBoundary`) and
//! the server-identity/Authenticode verifier, reached through the v1 `LcsdHostApi`, and no log,
//! thread, file, `Config` or `model` type of the app crosses into here.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::abi::{LcsdHostApi, LCSD_E_LEGACY, LCSD_E_NOT_FOUND, LCSD_OK};
use crate::json::Json;
use crate::types::{Config, DiscordState, Speaker};
#[cfg(test)]
use std::io::{Read, Write};

include!("shared.rs");

pub(crate) const OP_HANDSHAKE: u32 = 0;
pub(crate) const OP_FRAME: u32 = 1;
pub(crate) const OP_CLOSE: u32 = 2;
pub(crate) const OP_PING: u32 = 3;
pub(crate) const OP_PONG: u32 = 4;
pub(crate) const MAX_FRAME: usize = 4 * 1024 * 1024;
pub(crate) const MAX_TOKEN_RESPONSE: usize = 1024 * 1024;

pub(crate) const PIPE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const REQUIRED_SCOPES: [&str; 3] = ["rpc", "identify", "rpc.voice.read"];

#[derive(Clone, Debug)]
pub(crate) struct Payload {
    pub(crate) event: String,
    pub(crate) nonce: String,
    pub(crate) data: Json,
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

pub(crate) fn json_string(value: &str) -> String {
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
pub(crate) fn write_packet<W: Write>(
    writer: &mut W,
    opcode: u32,
    body: &[u8],
) -> Result<(), String> {
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
pub(crate) fn write_pong<W: Write>(writer: &mut W, body: &[u8]) -> Result<(), String> {
    write_packet(writer, OP_PONG, body)
}

#[cfg(test)]
pub(crate) fn read_packet<R: Read>(reader: &mut R) -> Result<(u32, Vec<u8>), String> {
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

pub(crate) fn decode_payload(body: &[u8]) -> Result<Payload, String> {
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

pub(crate) fn payload_error(payload: &Payload) -> Result<(), String> {
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

pub(crate) fn classify_oauth_error(message: &str) -> &'static str {
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

pub(crate) fn close_error(body: &[u8]) -> String {
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
pub(crate) struct Tracker {
    pub(crate) state: DiscordState,
    pub(crate) self_id: String,
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

pub(crate) fn field(value: &Json, key: &str) -> String {
    value
        .get(key)
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string()
}

pub(crate) fn bool_field(value: &Json, key: &str) -> bool {
    value.get(key).and_then(Json::as_bool).unwrap_or(false)
}

pub(crate) fn user_id(value: &Json) -> String {
    value
        .get("user")
        .map(|user| field(user, "id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| field(value, "user_id"))
}

pub(crate) fn ready_user_id(data: &Json) -> Result<String, String> {
    let id = data
        .get("user")
        .map(|user| field(user, "id"))
        .unwrap_or_default();
    validate_user_id(&id)?;
    Ok(id)
}

pub(crate) fn has_required_scopes<'a>(scopes: impl Iterator<Item = &'a str> + Clone) -> bool {
    REQUIRED_SCOPES
        .iter()
        .all(|required| scopes.clone().any(|scope| scope == *required))
}

pub(crate) fn scope_string_is_valid(scope: &str) -> bool {
    has_required_scopes(scope.split_ascii_whitespace())
}

pub(crate) fn authenticated_user_id(data: &Json) -> Result<String, String> {
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

pub(crate) fn validate_authentication(
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

pub(crate) fn current_user_changed(
    event: &str,
    data: &Json,
    ready_id: &str,
) -> Result<bool, String> {
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

pub(crate) fn disconnected_account_state() -> DiscordState {
    DiscordState {
        error: "Discord current account changed; reconnecting".into(),
        updated: Some(SystemTime::now()),
        ..Default::default()
    }
}

pub(crate) fn display_name(voice: &Json, user: &Json, id: &str) -> String {
    [
        field(voice, "nick"),
        field(user, "global_name"),
        field(user, "username"),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or_else(|| format!("USER {}", tail(id, 4)))
}

pub(crate) fn tail(value: &str, count: usize) -> &str {
    let start = value
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map(|(index, _)| index)
        .unwrap_or(0);
    &value[start..]
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TokenRecord {
    pub(crate) version: u32,
    pub(crate) user_id: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) token_type: String,
    pub(crate) scope: String,
    pub(crate) expires_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) token_type: String,
    pub(crate) scope: String,
    pub(crate) expires_in: u64,
}

impl TokenRecord {
    pub(crate) fn from_response(
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

    pub(crate) fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at != 0 && now.saturating_add(24 * 60 * 60) >= self.expires_at
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
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

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
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

    pub(crate) fn validate_for(&self, user_id: &str, client_id: &str) -> Result<(), String> {
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

pub(crate) fn parse_token_response(body: &[u8]) -> Result<TokenResponse, String> {
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

pub(crate) fn retain_if_empty(value: &mut String, current: &str) {
    if value.is_empty() {
        *value = current.into();
    }
}

pub(crate) fn append_token_body(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if body.len() + chunk.len() > MAX_TOKEN_RESPONSE {
        return Err("Discord token response exceeded 1 MiB".into());
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub(crate) fn form_encode(fields: &[(&str, &str)]) -> String {
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

pub(crate) fn form_component_encode(value: &str) -> String {
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

pub(crate) fn normalized_client_secret(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) struct WinHttpHandle(*mut std::ffi::c_void);
impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
            }
        }
    }
}

pub(crate) fn remaining_timeout_ms(deadline: Instant) -> Result<i32, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Discord token exchange timed out".into());
    }
    Ok(remaining.as_millis().clamp(1, i32::MAX as u128) as i32)
}

pub(crate) fn exchange_token_native(
    form: &str,
    deadline: Instant,
) -> Result<TokenResponse, String> {
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
pub(crate) struct TokenExchangeOwner {
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

    /// WO-TEL-D3-C: the quiescence the ABI `join`/`release` entries assert — an exchange in flight
    /// means the host must REFUSE the unload and retain the module (audit condition iv).
    pub(crate) fn active(&self) -> bool {
        self.active.get()
    }

    pub(crate) fn reset(&self) {
        self.active.set(false);
    }
}

pub(crate) fn exchange_token(
    owner: &TokenExchangeOwner,
    form: &str,
    deadline: Instant,
) -> Result<TokenResponse, String> {
    owner.run(|| exchange_token_native(form, deadline))
}

pub(crate) fn open_pipe(api: &LcsdHostApi) -> Result<(Pipe, String), String> {
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
            // WO-TEL-D3-C (audit condition iii): the Discord IPC server identity check, including
            // the Authenticode verifier that detaches on timeout, is HOST code. The component hands
            // over the connected pipe handle and takes a status code back, so no component code
            // ever runs in a thread the host cannot join.
            Ok(file) => {
                let code = (api.verify_server)(api.context, file.as_raw_handle());
                if code == LCSD_OK {
                    return Ok((Pipe::new(file)?, path));
                }
                rejected = true;
                drop(file);
            }
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

pub(crate) fn transfer_all<F, C>(data: &[u8], canceled: &C, mut transfer: F) -> Result<(), String>
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

pub(crate) struct Pipe {
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

    /// WO-TEL-D3-C: the bounded step primitive. A `poll_io` step may not block on an idle pipe, so
    /// the component asks the OS how many bytes are queued and waits in the shipped 20 ms quanta
    /// until the step deadline; the frame read that follows is then already deliverable, so no
    /// overlapped read is cancelled mid-frame (no partial-frame data loss).
    pub(crate) fn data_available(
        &self,
        canceled: &dyn Fn() -> bool,
        deadline: Instant,
    ) -> Result<bool, String> {
        use windows::Win32::System::Pipes::PeekNamedPipe;
        loop {
            if canceled() {
                return Err("Discord session canceled".into());
            }
            let mut available = 0u32;
            match unsafe { PeekNamedPipe(self.handle(), None, 0, None, Some(&mut available), None) }
            {
                Ok(()) if available > 0 => return Ok(true),
                Ok(()) => {}
                Err(_) => return Err("Discord IPC peek failed".into()),
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
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

pub(crate) fn refresh_token_if_needed(
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

/// WO-TEL-D3-C: the rotated record leaves through the HOST `store_credential` callback (the host
/// DPAPI-wraps and writes it inside its pinned boundary); the component never sees a path and
/// never touches the credential file. The shipped rule "rotation persists for the ORIGINAL user
/// before any later RPC failure" is unchanged; the host answers with a status code.
pub(crate) fn persist_rotated_refresh(
    api: &LcsdHostApi,
    credential: &TokenRecord,
    refreshed: bool,
) -> Result<(), String> {
    if !refreshed {
        return Ok(());
    }
    // OAuth refresh-token rotation may invalidate the old record before IPC validation.
    let blob = credential.encode();
    let user = std::ffi::CString::new(credential.user_id.clone())
        .map_err(|_| "Discord credential user id is not representable".to_string())?;
    let code = (api.store_credential)(
        api.context,
        user.as_ptr().cast(),
        blob.as_ptr(),
        blob.len() as u32,
    );
    if code == LCSD_OK {
        Ok(())
    } else {
        Err("Discord rotated credential could not be stored".into())
    }
}

/// The host's stored record for a READY user, read through `provide_token` as an opaque bounded
/// blob and decoded here. `Ok(None)` carries the shipped distinction between "no credential" and
/// "legacy single-account credential" as a status code; the host never hands the component a path.
pub(crate) fn provide_record(
    api: &LcsdHostApi,
    user_id: &str,
    client_id: &str,
) -> Result<Option<TokenRecord>, String> {
    let user = std::ffi::CString::new(user_id.to_string())
        .map_err(|_| "Discord user id is not representable".to_string())?;
    let client = std::ffi::CString::new(client_id.to_string())
        .map_err(|_| "Discord client id is not representable".to_string())?;
    const CAP: u32 = 64 * 1024;
    let mut blob = vec![0u8; CAP as usize];
    let code = (api.provide_token)(
        api.context,
        user.as_ptr().cast(),
        client.as_ptr().cast(),
        blob.as_mut_ptr(),
        CAP,
    );
    if code == LCSD_E_NOT_FOUND {
        return Ok(None);
    }
    if code == LCSD_E_LEGACY {
        return Err("legacy Discord credential is unsupported; run --discord-clear-token, then authorize the current account".into());
    }
    if code < 0 || code as u32 > CAP {
        return Err("Discord credential is not available".into());
    }
    let record = TokenRecord::decode(&blob[..code as usize])?;
    record.validate_for(user_id, client_id)?;
    Ok(Some(record))
}

pub(crate) fn require_same_ready_user(
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

pub(crate) fn require_current_user_subscription(subscribed: bool) -> Result<(), String> {
    if subscribed {
        Ok(())
    } else {
        Err("Discord current-account monitor is not established".into())
    }
}

pub(crate) struct Rpc {
    pub(crate) file: Pipe,
    pub(crate) tracker: Tracker,
    pub(crate) api: LcsdHostApi,
    pub(crate) ready_user_id: String,
    nonce: u64,
    pub(crate) account_changed: bool,
    pub(crate) current_user_subscribed: bool,
    pending_refresh: bool,
    global_subscribed: bool,
    channel_subscribed: String,
    pub(crate) state: DiscordState,
    pub(crate) revision: u64,
}

impl Rpc {
    fn clear_session(&mut self) {
        self.account_changed = true;
        self.state = disconnected_account_state();
        self.revision += 1;
    }

    pub(crate) fn handle_event(&mut self, event: &str, data: &Json) -> Result<bool, String> {
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

    pub(crate) fn read(&mut self) -> Result<(u32, Vec<u8>), String> {
        let api = self.api;
        self.file.read_packet(&|| (api.canceled)(api.context) != 0)
    }

    pub(crate) fn write(&mut self, opcode: u32, body: &[u8]) -> Result<(), String> {
        let api = self.api;
        self.file
            .write_packet(opcode, body, &|| (api.canceled)(api.context) != 0)
    }

    pub(crate) fn command(
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

    pub(crate) fn refresh_channel(&mut self) -> Result<(), String> {
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

    pub(crate) fn publish(&mut self) {
        let mut state = self.tracker.state.clone();
        state.connected = true;
        state.authenticated = true;
        state.error.clear();
        state.updated = Some(SystemTime::now());
        self.state = state;
        self.revision += 1;
    }
}

pub(crate) fn new_rpc(file: Pipe, api: LcsdHostApi) -> Rpc {
    Rpc {
        file,
        tracker: Tracker::default(),
        api,
        ready_user_id: String::new(),
        nonce: 0,
        account_changed: false,
        current_user_subscribed: false,
        pending_refresh: false,
        global_subscribed: false,
        channel_subscribed: String::new(),
        state: DiscordState::default(),
        revision: 0,
    }
}

pub(crate) fn runtime_handshake(rpc: &mut Rpc, client_id: &str) -> Result<String, String> {
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

pub(crate) fn pipe_command(
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

pub(crate) fn authorization_handshake(
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

pub(crate) fn authorize_rpc(
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

pub(crate) fn authorize_args(client_id: &str) -> String {
    format!(
        "{{\"client_id\":{},\"scopes\":[\"identify\",\"rpc\",\"rpc.voice.read\"]}}",
        json_string(client_id)
    )
}

pub(crate) fn authorization_code_body(client_id: &str, code: &str, client_secret: &str) -> String {
    form_encode(&[
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_secret", client_secret),
    ])
}

pub(crate) fn authorization_code(payload: &Payload, nonce: &str) -> Result<String, String> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{self, Cursor};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // WO-TEL-D3-C: the v1 HOST table stub the component's tests drive. The component is the callee
    // here, so these counters prove exactly which host callbacks it invokes (and that it invokes
    // none before its own input validation). Production uses the executable's real table. The
    // counter lives in the `context` pointer, so each test owns its own: parallel tests in this
    // binary cannot cross each other's call counts.
    pub(crate) static STUB_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counted(context: *mut std::ffi::c_void) {
        let slot = if context.is_null() {
            &STUB_CALLS
        } else {
            unsafe { &*(context as *const AtomicUsize) }
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    extern "system" fn stub_canceled(_context: *mut std::ffi::c_void) -> i32 {
        0
    }
    extern "system" fn stub_verify_server(
        context: *mut std::ffi::c_void,
        _pipe_handle: *mut std::ffi::c_void,
    ) -> i32 {
        counted(context);
        crate::abi::LCSD_E_IDENTITY
    }
    extern "system" fn stub_verify_path(
        context: *mut std::ffi::c_void,
        _exe_path: *const u16,
        _out_signer: *mut u8,
        _out_cap: u32,
    ) -> i32 {
        counted(context);
        crate::abi::LCSD_E_IDENTITY
    }
    extern "system" fn stub_provide_token(
        context: *mut std::ffi::c_void,
        _user_id: *const u8,
        _client_id: *const u8,
        _out_blob: *mut u8,
        _out_cap: u32,
    ) -> i32 {
        counted(context);
        crate::abi::LCSD_E_NOT_FOUND
    }
    extern "system" fn stub_store_credential(
        context: *mut std::ffi::c_void,
        _user_id: *const u8,
        _blob: *const u8,
        _blob_len: u32,
    ) -> i32 {
        counted(context);
        crate::abi::LCSD_OK
    }
    pub(crate) fn stub_api(counter: &'static AtomicUsize) -> LcsdHostApi {
        LcsdHostApi {
            size: std::mem::size_of::<LcsdHostApi>() as u32,
            context: counter as *const AtomicUsize as *mut std::ffi::c_void,
            canceled: stub_canceled,
            verify_server: stub_verify_server,
            verify_path: stub_verify_path,
            provide_token: stub_provide_token,
            store_credential: stub_store_credential,
        }
    }

    // WO-TEL-D3-C: the app's `TestDir` temp-dir fixture (and its sandbox-TMP note) is host-side
    // credential-file support; the moved RPC/OAuth tests need no temp directory, so it is not
    // duplicated in the component.

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
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let config = Config {
            discord_client_id: "123456".into(),
            ..Config::default()
        };
        let api = stub_api(&CALLS);
        for secret in ["", " \t\r\n "] {
            assert_eq!(
                crate::engine::authorize_flow(&api, &config, secret).unwrap_err(),
                "LCDSIRPLUS_DISCORD_CLIENT_SECRET must be exposed temporarily only for Discord authorization and cleared immediately afterward"
            );
        }
        assert_eq!(
            CALLS.load(Ordering::Relaxed),
            0,
            "the shipped order is kept: no Discord I/O is attempted before the secret check"
        );
        let unconfigured = Config::default();
        assert_eq!(
            crate::engine::authorize_flow(&api, &unconfigured, "secret").unwrap_err(),
            "discord_client_id is not configured"
        );
        assert_eq!(
            CALLS.load(Ordering::Relaxed),
            0,
            "an unconfigured client id reaches no Discord I/O (the D2 gate's rule, kept)"
        );
    }
}
