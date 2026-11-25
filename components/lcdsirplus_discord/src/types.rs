//! Component-side mirrors of the host's plain data types (WO-TEL-D3-C S2/S5).
//!
//! The moved regions of `src/providers/discord.rs` read exactly these field names, so they
//! compile unchanged: `Config` here is the closed seven-field settings mirror the v1
//! `LcsdSettings` carries, and `DiscordState`/`Speaker` are the state mirrors the host projects
//! into `src/model.rs` through the size-checked `LcsdState` POD. No app type crosses the boundary.

use std::time::Duration;
use std::time::SystemTime;

use crate::abi::{
    LcsdSettings, LcsdSpeaker, LcsdState, LCSD_CLIENT_ID_CAPACITY, LCSD_MAX_SPEAKERS,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub discord_enabled: bool,
    pub safe_mode: bool,
    pub discord_client_id: String,
    pub discord_linger: Duration,
    pub discord_max_speakers: i32,
    pub discord_show_self: bool,
    pub discord_show_channel: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Speaker {
    pub user_id: String,
    pub name: String,
    pub speaking: bool,
    pub is_self: bool,
    pub started_at: Option<SystemTime>,
    pub stopped_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscordState {
    pub connected: bool,
    pub authenticated: bool,
    pub channel_id: String,
    pub channel_name: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub voice_ping_ms: f64,
    pub connection_state: String,
    pub speakers: Vec<Speaker>,
    pub error: String,
    pub updated: Option<SystemTime>,
}

pub(crate) fn unix_ms(time: Option<SystemTime>) -> u64 {
    time.map(|value| {
        value
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|age| age.as_millis() as u64)
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

fn write_text<const CAP: usize>(target: &mut [u8; CAP], value: &str) -> u32 {
    let bytes = value.as_bytes();
    let len = bytes.len().min(CAP);
    target[..len].copy_from_slice(&bytes[..len]);
    if len < CAP {
        target[len] = 0;
    }
    len as u32
}

pub(crate) fn read_text<const CAP: usize>(len: u32, bytes: &[u8; CAP]) -> String {
    let len = (len as usize).min(CAP);
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// `LcsdSettings` -> the component's settings mirror. Returns `None` on an out-of-range length,
/// non-UTF-8 client id, or an over-capacity speaker count: the caller answers with a status code.
pub(crate) fn settings_to_config(settings: &LcsdSettings) -> Option<Config> {
    if settings.client_id_len as usize > LCSD_CLIENT_ID_CAPACITY {
        return None;
    }
    let client_id =
        std::str::from_utf8(&settings.client_id[..settings.client_id_len as usize]).ok()?;
    if settings.max_speakers < 1 || settings.max_speakers > LCSD_MAX_SPEAKERS as i32 {
        return None;
    }
    Some(Config {
        discord_enabled: settings.enabled != 0,
        safe_mode: settings.safe_mode != 0,
        discord_client_id: client_id.to_string(),
        discord_linger: Duration::from_millis(settings.linger_ms),
        discord_max_speakers: settings.max_speakers,
        discord_show_self: settings.show_self != 0,
        discord_show_channel: settings.show_channel != 0,
    })
}

#[cfg(test)]
pub(crate) fn config_to_settings(config: &Config) -> LcsdSettings {
    let mut settings = LcsdSettings::default();
    settings.client_id_len = write_text(&mut settings.client_id, &config.discord_client_id);
    settings.enabled = config.discord_enabled as u8;
    settings.safe_mode = config.safe_mode as u8;
    settings.show_self = config.discord_show_self as u8;
    settings.show_channel = config.discord_show_channel as u8;
    settings.linger_ms = config.discord_linger.as_millis() as u64;
    settings.max_speakers = config.discord_max_speakers;
    settings.ipc_probe_count = 10;
    settings
}

/// The state projection into the host-owned POD (bounded, fixed layout, truncating at the
/// documented capacities; user ids are <= 20 digits by `validate_user_id`).
pub(crate) fn project_state(state: &DiscordState, revision: u64, out: &mut LcsdState) {
    out.size = std::mem::size_of::<LcsdState>() as u32;
    out.revision = revision;
    out.connected = state.connected as u8;
    out.authenticated = state.authenticated as u8;
    out.self_mute = state.self_mute as u8;
    out.self_deaf = state.self_deaf as u8;
    out.voice_ping_ms = state.voice_ping_ms;
    out.channel_id_len = write_text(&mut out.channel_id, &state.channel_id);
    out.channel_name_len = write_text(&mut out.channel_name, &state.channel_name);
    out.connection_state_len = write_text(&mut out.connection_state, &state.connection_state);
    out.error_len = write_text(&mut out.error, &state.error);
    out.updated_unix_ms = unix_ms(state.updated);
    let count = state.speakers.len().min(LCSD_MAX_SPEAKERS);
    for (index, speaker) in state.speakers.iter().take(count).enumerate() {
        let mut entry = LcsdSpeaker::default();
        entry.user_id_len = write_text(&mut entry.user_id, &speaker.user_id);
        entry.name_len = write_text(&mut entry.name, &speaker.name);
        entry.speaking = speaker.speaking as u8;
        entry.is_self = speaker.is_self as u8;
        entry.started_unix_ms = unix_ms(speaker.started_at);
        entry.stopped_unix_ms = unix_ms(speaker.stopped_at);
        out.speakers[index] = entry;
    }
    out.speaker_count = count as u32;
}

impl LcsdState {
    /// Inverse projection, used by the host and by the component's own tests to read a POD back.
    pub fn to_state(&self) -> DiscordState {
        DiscordState {
            connected: self.connected != 0,
            authenticated: self.authenticated != 0,
            channel_id: read_text(self.channel_id_len, &self.channel_id),
            channel_name: read_text(self.channel_name_len, &self.channel_name),
            self_mute: self.self_mute != 0,
            self_deaf: self.self_deaf != 0,
            voice_ping_ms: self.voice_ping_ms,
            connection_state: read_text(self.connection_state_len, &self.connection_state),
            speakers: self
                .speakers
                .iter()
                .take((self.speaker_count as usize).min(LCSD_MAX_SPEAKERS))
                .map(|speaker| Speaker {
                    user_id: read_text(speaker.user_id_len, &speaker.user_id),
                    name: read_text(speaker.name_len, &speaker.name),
                    speaking: speaker.speaking != 0,
                    is_self: speaker.is_self != 0,
                    started_at: unix_time(speaker.started_unix_ms),
                    stopped_at: unix_time(speaker.stopped_unix_ms),
                })
                .collect(),
            error: read_text(self.error_len, &self.error),
            updated: unix_time(self.updated_unix_ms),
        }
    }
}

fn unix_time(ms: u64) -> Option<SystemTime> {
    (ms != 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}
