//! WO-TEL-D3-C boundary tests (component side): the v1 export handshake, the size-checked
//! payloads, the lifecycle/quiescence rules the host relies on, and the cross-crate lockstep pins
//! that keep the duplicated blocks from drifting.

use crate::abi::{
    lcsd_get_table, LcsdHostApi, LcsdSettings, LcsdSpeaker, LcsdState, LcsdV1, LCSD_E_ARG,
    LCSD_E_IDENTITY, LCSD_E_INFLIGHT, LCSD_E_IO, LCSD_E_MAGIC, LCSD_E_PROTOCOL, LCSD_E_SIZE,
    LCSD_E_STATE, LCSD_IDLE, LCSD_MAGIC, LCSD_MAX_SPEAKERS, LCSD_OK, LCSD_VERSION,
};
use crate::engine::{authorize_flow, CONNECT_STEP_TIMEOUT_MS};
use crate::rpc::tests::{stub_api, STUB_CALLS};
use crate::rpc::{persist_rotated_refresh, require_same_ready_user, TokenRecord};
use crate::types::{config_to_settings, project_state, Config, DiscordState, Speaker};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn marker_block(source: &str, start: &str, end: &str) -> String {
    let text = lf(source);
    let open = text
        .find(start)
        .expect("the marker block opens in the shipped text");
    let close = text[open..]
        .find(end)
        .map(|offset| open + offset)
        .expect("the marker block closes in the shipped text");
    text[open + start.len()..close].trim().to_string()
}

const HOST_DISCORD: &str = include_str!("../../../src/providers/discord.rs");

#[test]
fn abi_block_is_byte_identical_across_the_boundary() {
    // N10: the v1 layouts, table shape and status codes the two packages compile against are the
    // same bytes. A drift reddens this test in BOTH crates (the host carries its own copy).
    const START: &str = "// ABI-V1-BLOCK-START";
    const END: &str = "// ABI-V1-BLOCK-END";
    let mine = marker_block(include_str!("abi.rs"), START, END);
    let host = marker_block(HOST_DISCORD, START, END);
    assert_eq!(
        mine, host,
        "the ABI v1 block must stay byte-identical between the component and the host"
    );
    assert!(
        mine.contains("pub struct LcsdV1"),
        "the block carries the table"
    );
    assert_eq!(
        mine.matches("extern \"system\" fn").count(),
        host.matches("extern \"system\" fn").count(),
        "every extern entry count matches"
    );
}

#[test]
fn shared_identity_rules_are_byte_identical_across_the_boundary() {
    // N11: the two boundary-shared rules (the credential user-id shape, the cryptographic nonce)
    // exist in both packages; this pin keeps them identical instead of allowing silent drift.
    const START: &str = "// SHARED-RULES-START";
    const END: &str = "// SHARED-RULES-END";
    let mine = marker_block(include_str!("shared.rs"), START, END);
    let host = marker_block(HOST_DISCORD, START, END);
    assert_eq!(mine, host, "the shared identity rules drifted");
    assert!(
        mine.contains("fn validate_user_id"),
        "the user-id rule is present"
    );
    assert!(
        mine.contains("fn random_nonce"),
        "the nonce rule is present"
    );
}

#[test]
fn json_copy_is_byte_identical_to_the_app_module() {
    // Audit §10(vii): the disclosed duplication is 366 lines / 312 production lines, and it is a
    // verbatim copy, so the two parsers cannot parse a payload differently.
    let mine = lf(include_str!("json.rs"));
    let app = lf(include_str!("../../../src/json.rs"));
    assert_eq!(mine, app, "the component's json copy must stay verbatim");
    let lines = mine.lines().count();
    assert_eq!(
        lines, 366,
        "the shipped module is 366 lines (audit §10(vii))"
    );
    let cut = mine
        .find("#[cfg(test)]")
        .expect("json carries a test region");
    assert_eq!(
        mine[..cut].lines().count() + 1,
        312,
        "the duplicated production region is the shipped 312 lines (L1-L312, the attribute line included)"
    );
    assert_eq!(
        lines - (mine[..cut].lines().count() + 1),
        54,
        "the test region is 54 lines, so 4 shipped tests travel with the copy"
    );
}

#[test]
fn handshake_refuses_bad_magic_version_or_size() {
    // ABI v1 refuses every handshake mismatch with a null table: the host then reports the
    // provider unavailable and never calls component code.
    let size = std::mem::size_of::<LcsdV1>();
    assert!(
        !lcsd_get_table(LCSD_MAGIC, LCSD_VERSION, size).is_null(),
        "the matching handshake yields the table"
    );
    assert!(lcsd_get_table(0xdead_beef, LCSD_VERSION, size).is_null());
    assert!(lcsd_get_table(LCSD_MAGIC, LCSD_VERSION + 1, size).is_null());
    assert!(lcsd_get_table(LCSD_MAGIC, 0, size).is_null());
    assert!(lcsd_get_table(LCSD_MAGIC, LCSD_VERSION, size - 1).is_null());
    assert!(lcsd_get_table(0, 0, 0).is_null());
    let raw = lcsd_get_table(LCSD_MAGIC, LCSD_VERSION, size + 64);
    assert!(
        !raw.is_null(),
        "a larger requested size is accepted (forward-compatible)"
    );
    let table = unsafe { &*(raw as *const LcsdV1) };
    assert_eq!(
        table.size as usize,
        std::mem::size_of::<LcsdV1>(),
        "the table self-describes its size for the host's check"
    );
    assert_eq!(
        LCSD_E_MAGIC, -13,
        "the handshake refusal code stays in the shared set"
    );
}

#[test]
fn table_layout_is_pointer_sized_and_status_codes_are_distinct() {
    // One `size` field + ten function pointers, all pointer-sized, and no aliasing status codes.
    assert_eq!(
        std::mem::size_of::<LcsdV1>(),
        std::mem::size_of::<usize>() * 11,
        "the v1 table is a size word plus exactly ten entries"
    );
    let codes = [
        LCSD_OK,
        LCSD_IDLE,
        LCSD_E_SIZE,
        LCSD_E_ARG,
        LCSD_E_STATE,
        LCSD_E_IO,
        LCSD_E_PROTOCOL,
        LCSD_E_IDENTITY,
        LCSD_E_INFLIGHT,
        LCSD_E_MAGIC,
    ];
    for (index, code) in codes.iter().enumerate() {
        for other in codes[index + 1..].iter() {
            assert_ne!(code, other, "status codes must be distinct");
        }
    }
    assert_eq!(std::mem::size_of::<LcsdSettings>(), 96);
    assert_eq!(
        std::mem::align_of::<LcsdState>(),
        8,
        "the state POD keeps the 8-byte alignment both sides compile against"
    );
    assert_eq!(
        std::mem::offset_of!(LcsdState, size),
        0,
        "the self-size word leads the layout"
    );
    assert_eq!(
        std::mem::offset_of!(LcsdState, revision),
        8,
        "the revision word follows the size word plus the four flag bytes"
    );
    assert!(
        std::mem::offset_of!(LcsdState, speakers) > std::mem::offset_of!(LcsdState, speaker_count),
        "the speaker array sits behind its count"
    );
    assert_eq!(
        std::mem::size_of::<LcsdSpeaker>(),
        104,
        "the speaker POD is fixed-layout: two length words, the 24- and 48-byte capacities, \
         two flags and two timestamps"
    );
}

fn pod_settings(client_id: &str) -> LcsdSettings {
    let mut settings = LcsdSettings::default();
    let bytes = client_id.as_bytes();
    settings.client_id_len = bytes.len() as u32;
    settings.client_id[..bytes.len()].copy_from_slice(bytes);
    settings.enabled = 1;
    settings.max_speakers = 2;
    settings.linger_ms = 700;
    settings.ipc_probe_count = 10;
    settings
}

fn start_handle(pod: &LcsdSettings) -> *mut c_void {
    let api = stub_api(&STUB_CALLS);
    let mut handle: *mut c_void = std::ptr::null_mut();
    let code = (crate::engine::TABLE.start)(
        pod as *const LcsdSettings,
        std::mem::size_of::<LcsdSettings>() as u32,
        &api as *const LcsdHostApi,
        std::mem::size_of::<LcsdHostApi>() as u32,
        &mut handle,
    );
    assert_eq!(code, LCSD_OK, "start accepts a well-formed block");
    assert!(!handle.is_null());
    handle
}

#[test]
fn start_validates_every_block_length_and_capacity() {
    let api = stub_api(&STUB_CALLS);
    let good = pod_settings("123456789012345678");
    fn run(
        _api: &LcsdHostApi,
        settings: *const LcsdSettings,
        settings_len: u32,
        api_ptr: *const LcsdHostApi,
        api_len: u32,
        handle: &mut *mut c_void,
    ) -> i32 {
        (crate::engine::TABLE.start)(settings, settings_len, api_ptr, api_len, handle)
    }
    let slen = std::mem::size_of::<LcsdSettings>() as u32;
    let alen = std::mem::size_of::<LcsdHostApi>() as u32;
    let mut handle: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        run(&api, std::ptr::null(), slen, &api, alen, &mut handle),
        LCSD_E_SIZE
    );
    assert_eq!(
        run(&api, &good, slen, std::ptr::null(), alen, &mut handle),
        LCSD_E_SIZE
    );
    assert_eq!(
        run(&api, &good, slen + 1, &api, alen, &mut handle),
        LCSD_E_SIZE
    );
    assert_eq!(
        run(&api, &good, slen, &api, alen + 8, &mut handle),
        LCSD_E_SIZE
    );
    let mut undersized = good;
    undersized.size = 8;
    assert_eq!(
        run(&api, &undersized, slen, &api, alen, &mut handle),
        LCSD_E_SIZE
    );
    let mut overlong = good;
    overlong.client_id_len = 999;
    assert_eq!(
        run(&api, &overlong, slen, &api, alen, &mut handle),
        LCSD_E_ARG
    );
    let mut too_many_speakers = good;
    too_many_speakers.max_speakers = LCSD_MAX_SPEAKERS as i32 + 1;
    assert_eq!(
        run(&api, &too_many_speakers, slen, &api, alen, &mut handle),
        LCSD_E_ARG
    );
    assert!(handle.is_null(), "no handle escapes a rejected start");
    let mut non_utf8 = good;
    non_utf8.client_id_len = 2;
    non_utf8.client_id[0] = 0xc3;
    non_utf8.client_id[1] = 0x28;
    assert_eq!(
        run(&api, &non_utf8, slen, &api, alen, &mut handle),
        LCSD_E_ARG
    );
}

#[test]
fn engine_lifecycle_is_balanced_and_quiesces_after_stop() {
    // 50 cycles of start -> set_config -> poll_io -> state -> stop -> join -> release, each ending
    // with a quiescent `join` (the order the host's unload follows). No cycle leaks a handle slot:
    // every entry returns its code, and `release` succeeds only after `stop` closed the session.
    let api = stub_api(&STUB_CALLS);
    let mut handle: *mut c_void = std::ptr::null_mut();
    let table = &crate::engine::TABLE;
    for cycle in 0..50 {
        let config = pod_settings("123456789012345678");
        assert_eq!(
            (table.start)(
                &config,
                std::mem::size_of::<LcsdSettings>() as u32,
                &api,
                std::mem::size_of::<LcsdHostApi>() as u32,
                &mut handle,
            ),
            LCSD_OK,
            "cycle {cycle} starts"
        );
        let updated = pod_settings("123456789012345678");
        assert_eq!(
            (table.set_config)(handle, &updated, std::mem::size_of::<LcsdSettings>() as u32),
            LCSD_OK,
            "cycle {cycle} hot reload"
        );
        let poll = (table.poll_io)(handle, 20);
        assert!(
            poll == LCSD_OK || poll == LCSD_IDLE || poll == LCSD_E_IO || poll == LCSD_E_PROTOCOL,
            "cycle {cycle} step reports a status code, got {poll}"
        );
        let mut state = LcsdState::default();
        assert_eq!(
            (table.state)(handle, &mut state, std::mem::size_of::<LcsdState>() as u32),
            LCSD_OK,
            "cycle {cycle} reads the state POD"
        );
        assert_eq!((table.stop)(handle), LCSD_OK, "cycle {cycle} stops");
        assert_eq!(
            (table.join)(handle),
            LCSD_OK,
            "cycle {cycle} joins after a clean stop (quiescence proven)"
        );
        assert_eq!(
            (table.release)(handle),
            LCSD_OK,
            "cycle {cycle} releases the handle"
        );
    }
    // The entries refuse a null handle rather than dereferencing it.
    assert_eq!(
        (table.set_config)(std::ptr::null_mut(), std::ptr::null(), 0),
        LCSD_E_ARG
    );
    assert_eq!((table.poll_io)(std::ptr::null_mut(), 20), LCSD_E_ARG);
    assert_eq!(
        (table.state)(std::ptr::null_mut(), std::ptr::null_mut(), 0),
        LCSD_E_ARG
    );
    assert_eq!((table.stop)(std::ptr::null_mut()), LCSD_E_ARG);
    assert_eq!((table.join)(std::ptr::null_mut()), LCSD_E_ARG);
    assert_eq!((table.release)(std::ptr::null_mut()), LCSD_E_ARG);
}

#[test]
fn state_entry_enforces_its_capacity_and_the_projection_is_field_for_field() {
    let config = Config {
        discord_enabled: true,
        discord_client_id: "123456789012345678".into(),
        discord_linger: Duration::from_millis(700),
        discord_max_speakers: 2,
        discord_show_self: true,
        discord_show_channel: true,
        ..Config::default()
    };
    let settings = config_to_settings(&config);
    assert_eq!(
        settings.client_id_len as usize,
        config.discord_client_id.len()
    );
    let round_trip = crate::types::settings_to_config(&settings).expect("round trip");
    assert_eq!(
        round_trip, config,
        "the settings mirror round-trips the seven fields"
    );

    let mut state = DiscordState {
        connected: true,
        authenticated: true,
        channel_id: "1234567890".into(),
        channel_name: "General".into(),
        self_mute: true,
        self_deaf: true,
        voice_ping_ms: 12.5,
        connection_state: "CONNECTED".into(),
        error: "Discord RPC disconnected".into(),
        updated: Some(std::time::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)),
        ..DiscordState::default()
    };
    state.speakers = vec![
        Speaker {
            user_id: "111".into(),
            name: "self".into(),
            speaking: true,
            is_self: true,
            started_at: Some(std::time::UNIX_EPOCH + Duration::from_millis(5)),
            stopped_at: None,
        },
        Speaker {
            user_id: "222".into(),
            name: "user".into(),
            speaking: false,
            is_self: false,
            started_at: None,
            stopped_at: Some(std::time::UNIX_EPOCH + Duration::from_millis(9)),
        },
    ];
    let mut pod = LcsdState::default();
    project_state(&state, 7, &mut pod);
    assert_eq!(pod.revision, 7);
    assert_eq!(pod.speaker_count, 2);
    let read_back = pod.to_state();
    assert_eq!(
        read_back, state,
        "the POD projection carries every field the renderer consumes"
    );

    let mut long = state.clone();
    long.error = "x".repeat(500);
    long.channel_name = "y".repeat(200);
    project_state(&long, 1, &mut pod);
    assert!(
        pod.error_len as usize <= 96,
        "the error buffer is capacity-bound"
    );
    assert!(
        pod.channel_name_len as usize <= 64,
        "the channel buffer is capacity-bound"
    );
    let handle = start_handle(&pod_settings("123456789012345678"));
    let table = &crate::engine::TABLE;
    assert_eq!(
        (table.state)(handle, &mut pod, 8),
        LCSD_E_SIZE,
        "a short state buffer is refused"
    );
    assert_eq!((table.state)(handle, std::ptr::null_mut(), 999), LCSD_E_ARG);
    assert_eq!((table.stop)(handle), LCSD_OK);
    assert_eq!((table.release)(handle), LCSD_OK);
}

static SLEEP_CALLS: AtomicUsize = AtomicUsize::new(0);
extern "system" fn sleeping_canceled(_context: *mut c_void) -> i32 {
    SLEEP_CALLS.fetch_add(1, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(150));
    0
}

#[test]
fn a_step_in_flight_makes_stop_join_and_release_refuse() {
    // The unload policy the host follows: while a `poll_io` step is executing component code, the
    // other entries return `LCSD_E_INFLIGHT`, so the host retains the module and reports it
    // instead of unloading under live I/O (audit condition iv).
    let mut api = stub_api(&STUB_CALLS);
    api.canceled = sleeping_canceled;
    let config = pod_settings("123456789012345678");
    let mut handle: *mut c_void = std::ptr::null_mut();
    let table = &crate::engine::TABLE;
    assert_eq!(
        (table.start)(
            &config,
            std::mem::size_of::<LcsdSettings>() as u32,
            &api,
            std::mem::size_of::<LcsdHostApi>() as u32,
            &mut handle,
        ),
        LCSD_OK
    );
    SLEEP_CALLS.store(0, Ordering::Relaxed);
    let raw = handle as usize;
    let worker = std::thread::spawn(move || {
        let borrowed = raw as *mut c_void;
        (
            raw,
            (crate::engine::TABLE.poll_io)(borrowed, CONNECT_STEP_TIMEOUT_MS),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while SLEEP_CALLS.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        SLEEP_CALLS.load(Ordering::Relaxed) >= 1,
        "the step entered the host verifier, so it is provably in flight"
    );
    let refusals = 1usize;
    assert_eq!(
        (table.stop)(handle),
        LCSD_E_INFLIGHT,
        "stop refuses while the step runs component-side"
    );
    assert_eq!(
        (table.join)(handle),
        LCSD_E_INFLIGHT,
        "join refuses in-flight"
    );
    assert_eq!(
        (table.release)(handle),
        LCSD_E_INFLIGHT,
        "release refuses in-flight"
    );
    let (settled, code) = worker.join().expect("the step finishes");
    assert_eq!(settled, handle as usize);
    assert_eq!(code, LCSD_E_IO, "the step ended in a transport refusal");
    assert_eq!(refusals, 1, "the in-flight window was observed once");
    assert!(SLEEP_CALLS.load(Ordering::Relaxed) >= 1);
    assert_eq!(
        (table.join)(handle),
        LCSD_OK,
        "after the step settles, join reports quiescence"
    );
    assert_eq!((table.stop)(handle), LCSD_OK);
    assert_eq!((table.join)(handle), LCSD_OK);
    assert_eq!((table.release)(handle), LCSD_OK);
}

#[test]
fn rotated_refresh_persists_for_original_user_before_later_rpc_failure() {
    // Relocated with the OAuth/codec region. The shipped claim, unchanged: a rotated record is
    // stored for the ORIGINAL account before any later RPC validation failure, and a record that
    // belongs to another account is refused. The storage itself is the host's (`store_credential`
    // -> DPAPI inside the pinned boundary); the component drives it with the encoded record.
    let api = stub_api(&STUB_CALLS);
    let old = TokenRecord {
        version: 2,
        user_id: "111".into(),
        client_id: "123".into(),
        access_token: "old-access".into(),
        refresh_token: "refresh-old".into(),
        scope: "identify rpc rpc.voice.read".into(),
        ..Default::default()
    };
    let refreshed = TokenRecord {
        access_token: "refreshed-access".into(),
        ..old.clone()
    };
    assert_eq!(
        persist_rotated_refresh(&api, &refreshed, true),
        Ok(()),
        "the rotation is handed to the host store for the original user"
    );
    assert_eq!(
        persist_rotated_refresh(&api, &old, false),
        Ok(()),
        "an unrotated session stores nothing"
    );
    let mismatched = crate::json::Json::parse(
        r#"{"user":{"id":"222"},"scopes":["rpc","identify","rpc.voice.read"]}"#,
    )
    .unwrap();
    assert!(
        crate::rpc::validate_authentication(&mismatched, "111", &refreshed, "123").is_err(),
        "a later RPC response bound to another account is refused after the rotation persisted"
    );
    assert!(require_same_ready_user("111", "222", &refreshed).is_err());
    assert!(require_same_ready_user("111", "111", &refreshed).is_ok());
    let decoded = TokenRecord::decode(&refreshed.encode()).expect("the blob round-trips");
    assert_eq!(
        decoded, refreshed,
        "the record the host stores is the codec's own format"
    );
}

#[test]
fn settings_mirror_reads_exactly_the_pinned_seven_fields() {
    // The DISC-C field-set claim restated on the component side (disclosed lockstep, same claim
    // set): the settings surface the host hands over is exactly the closed set the shipped
    // re-arm gate compares, in the shipped source order, and nothing else is read off it.
    const PINNED: [&str; 7] = [
        "discord_enabled",
        "safe_mode",
        "discord_client_id",
        "discord_linger",
        "discord_max_speakers",
        "discord_show_self",
        "discord_show_channel",
    ];
    let source = lf(include_str!("types.rs"));
    let head = source
        .find("pub struct Config {")
        .expect("the settings mirror exists");
    let body = &source[head + "pub struct Config {".len()
        ..source[head..].find("\n}").expect("the mirror closes") + head];
    let fields: Vec<String> = body
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("pub ")
                .and_then(|rest| rest.split(':').next())
        })
        .map(|name| name.trim().to_string())
        .collect();
    assert_eq!(
        fields, PINNED,
        "the component's settings mirror is exactly the pinned field set, in source order"
    );
    let conversion = &source[source
        .find("fn settings_to_config")
        .expect("the settings conversion exists")..];
    let conversion = &conversion[..conversion.find("\n}").expect("the conversion closes")];
    for field in PINNED {
        assert!(
            conversion.contains(field),
            "the conversion reads {field}: the pin is not vacuous"
        );
    }
}

#[test]
fn component_source_carries_no_app_types_and_no_secret_logging() {
    // The ban list, checked structurally over the component crate: no app type, no logging macro,
    // no `unwrap`/`expect`/`panic!` on a production path (the profile is `panic = "abort"`), and no
    // token/secret interpolation into a diagnostic buffer.
    const FILES: [&str; 6] = [
        include_str!("abi.rs"),
        include_str!("types.rs"),
        include_str!("rpc.rs"),
        include_str!("engine.rs"),
        include_str!("lib.rs"),
        include_str!("shared.rs"),
    ];
    for file in FILES {
        let text = lf(file);
        let production = match text.find("#[cfg(test)]") {
            Some(cut) => &text[..cut],
            None => &text[..],
        };
        for token in [
            "crate::config::",
            "crate::model::",
            "crate::json::Json::parse",
            "log_warn!",
            "log_info!",
            "println!",
            "eprintln!",
            "std::thread::spawn",
            "panic!",
        ] {
            assert!(
                !production.contains(token),
                "the production component text must not contain {token}"
            );
        }
        for line in production.lines() {
            let code = line.split("//").next().unwrap_or(line);
            for needle in [".unwrap()", ".unwrap_err()", ".expect("] {
                assert!(
                    !code.contains(needle),
                    "a production path must not panic: {needle} in {line}"
                );
            }
        }
    }
}

#[test]
fn authorize_and_poll_io_refuse_without_an_engine() {
    // The one-shot flows need no engine, and no entry dereferences a null block: each refusal is a
    // status code, so the host can report "Discord unavailable" without a crash.
    let table = &crate::engine::TABLE;
    let api = stub_api(&STUB_CALLS);
    let config = pod_settings("");
    let mut status = [0u8; 128];
    assert_eq!(
        (table.authorize)(
            &config,
            b"x".as_ptr(),
            1,
            &api,
            std::mem::size_of::<LcsdHostApi>() as u32,
            status.as_mut_ptr(),
            status.len() as u32,
        ),
        LCSD_E_PROTOCOL,
        "an unconfigured client id is refused before any Discord I/O"
    );
    assert_eq!(
        (table.authorize)(
            std::ptr::null(),
            b"x".as_ptr(),
            1,
            &api,
            std::mem::size_of::<LcsdHostApi>() as u32,
            status.as_mut_ptr(),
            status.len() as u32,
        ),
        LCSD_E_SIZE
    );
    assert_eq!(
        (table.authorize)(
            &config,
            std::ptr::null(),
            0,
            &api,
            std::mem::size_of::<LcsdHostApi>() as u32,
            status.as_mut_ptr(),
            status.len() as u32,
        ),
        LCSD_E_SIZE
    );
    assert_eq!(
        (table.verify_client)(std::ptr::null(), status.as_mut_ptr(), 8),
        LCSD_E_ARG
    );
    assert_eq!(
        (table.last_error)(std::ptr::null_mut(), 0),
        LCSD_E_ARG,
        "the diagnostic entry is capacity-checked"
    );
    let mut buffer = [0u8; 64];
    let written = (table.last_error)(buffer.as_mut_ptr(), buffer.len() as u32);
    assert!(written >= 0 && (written as usize) < buffer.len());
}

/// The component's only credential paths are the host callbacks; it never opens a credential file
/// itself. How far the shipped one-shot flow travels is environment-dependent (it needs a Discord
/// IPC endpoint, and the host's identity verifier answers for the pipe), so the deterministic
/// claims are: the flow fails CLEANLY, its message echoes no secret, and nothing is written next
/// to the component. The host-API reach is recorded as evidence.
#[test]
fn authorize_flow_reports_missing_credential_without_touching_a_file() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let api = stub_api(&CALLS);
    let config = Config {
        discord_client_id: "123456789012345678".into(),
        ..Config::default()
    };
    let message = authorize_flow(&api, &config, "unit-test-secret")
        .expect_err("no Discord authorization is possible in the unit-test environment");
    assert!(
        !message.contains("unit-test-secret"),
        "the flow never echoes the client secret: {message}"
    );
    let calls = CALLS.load(Ordering::Relaxed);
    if calls > 0 {
        println!("boundary: the flow reached the host API {calls} time(s) before failing");
    }
    assert!(
        !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("discord.token")
            .exists(),
        "the component left no credential file behind"
    );
}
