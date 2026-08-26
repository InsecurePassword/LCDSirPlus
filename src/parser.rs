//! LCDSirReal-style configuration parser.
//!
//! Same limits, duplicate-key rules, quoting/escaping, include semantics,
//! and file/line diagnostics as the LCDForge Go implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{
    parse_processor_list, validate, validate_config_string, Config, MAX_CONFIG_AGGREGATE_BYTES,
    MAX_CONFIG_FILES, MAX_CONFIG_INCLUDE_DIRECTIVES, MAX_CONFIG_LINES, MAX_CONFIG_LIST_ITEMS,
    MAX_CONFIG_TOKEN_BYTES, MAX_INCLUDES,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    /// Canonical paths of every file in the include graph, primary first.
    pub files: Vec<PathBuf>,
    /// SHA-256 of each file in `files`, same order (Phase 4 evidence).
    #[allow(dead_code)]
    pub digests: Vec<String>,
}

struct ParseContext {
    cfg: Config,
    stack: Vec<String>,
    files: Vec<PathBuf>,
    completed: Vec<String>,
    seen_in_file: HashMap<String, usize>,
    total_bytes: usize,
    total_lines: usize,
    include_directives: usize,
    digests: HashMap<String, String>,
}

/// Load and parse a configuration file from disk (includes allowed).
pub fn load(path: &Path) -> Result<LoadedConfig, ParseError> {
    let canonical = std::fs::canonicalize(path).map_err(|e| ParseError {
        file: path.display().to_string(),
        line: 0,
        message: format!("cannot resolve configuration path: {}", e),
    })?;
    let mut ctx = ParseContext {
        cfg: Config::default(),
        stack: Vec::new(),
        files: Vec::new(),
        completed: Vec::new(),
        seen_in_file: HashMap::new(),
        total_bytes: 0,
        total_lines: 0,
        include_directives: 0,
        digests: HashMap::new(),
    };
    ctx.parse_file(&canonical, 0)?;
    ctx.cfg.path = ctx.files[0].display().to_string();
    validate(&ctx.cfg).map_err(|message| ParseError {
        file: canonical.display().to_string(),
        line: 0,
        message,
    })?;
    let files = ctx.files.clone();
    let digests = files
        .iter()
        .map(|f| {
            ctx.digests
                .get(&f.display().to_string())
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    Ok(LoadedConfig {
        config: ctx.cfg,
        files,
        digests,
    })
}

/// Parse one immutable byte slice with identical semantics; includes are
/// rejected so the effective configuration binds to exactly these bytes.
/// (Isolated-mode surface: tests now, hardware-test evidence in Phase 4.)
#[allow(dead_code)]
pub fn parse_standalone(contents: &[u8]) -> Result<Config, ParseError> {
    const SOURCE: &str = "standalone-configuration";
    let mut ctx = ParseContext {
        cfg: Config::default(),
        stack: Vec::new(),
        files: vec![PathBuf::from(SOURCE)],
        completed: Vec::new(),
        seen_in_file: HashMap::new(),
        total_bytes: 0,
        total_lines: 0,
        include_directives: 0,
        digests: HashMap::new(),
    };
    let text = std::str::from_utf8(contents).map_err(|_| ParseError {
        file: SOURCE.into(),
        line: 0,
        message: "configuration must be valid UTF-8".into(),
    })?;
    ctx.parse_lines(SOURCE, text, 0, true)?;
    validate(&ctx.cfg).map_err(|message| ParseError {
        file: SOURCE.into(),
        line: 0,
        message,
    })?;
    Ok(ctx.cfg)
}

impl ParseContext {
    fn parse_file(&mut self, path: &Path, depth: usize) -> Result<(), ParseError> {
        if depth > MAX_INCLUDES {
            return Err(ParseError {
                file: path.display().to_string(),
                line: 0,
                message: format!("include depth exceeds {}", MAX_INCLUDES),
            });
        }
        let key = path.display().to_string().to_lowercase();
        if self.stack.contains(&key) {
            return Err(ParseError {
                file: path.display().to_string(),
                line: 0,
                message: format!("include cycle involving {}", path.display()),
            });
        }
        if self.completed.contains(&key) {
            return Ok(());
        }
        if self.files.len() >= MAX_CONFIG_FILES {
            return Err(ParseError {
                file: path.display().to_string(),
                line: 0,
                message: format!("config file count exceeds limit of {}", MAX_CONFIG_FILES),
            });
        }
        let raw = std::fs::read(path).map_err(|e| ParseError {
            file: path.display().to_string(),
            line: 0,
            message: format!("cannot read configuration: {}", e),
        })?;
        if self.total_bytes + raw.len() > MAX_CONFIG_AGGREGATE_BYTES {
            return Err(ParseError {
                file: path.display().to_string(),
                line: 0,
                message: format!(
                    "config input exceeds aggregate limit of {} bytes",
                    MAX_CONFIG_AGGREGATE_BYTES
                ),
            });
        }
        self.total_bytes += raw.len();
        let text = std::str::from_utf8(&raw).map_err(|_| ParseError {
            file: path.display().to_string(),
            line: 0,
            message: "configuration must be valid UTF-8".into(),
        })?;
        self.digests
            .insert(key.clone(), crate::sha256::sha256_hex(&raw));
        self.files.push(path.to_path_buf());
        self.stack.push(key.clone());
        let source = path.display().to_string();
        self.parse_lines(&source, text, depth, false)?;
        self.stack.pop();
        self.completed.push(key);
        Ok(())
    }

    fn parse_lines(
        &mut self,
        source: &str,
        text: &str,
        depth: usize,
        standalone: bool,
    ) -> Result<(), ParseError> {
        // Duplicate-key tracking is per file: fresh map here, parent's map
        // restored on return (mirrors the Go parser's defer semantics).
        let parent_seen = std::mem::take(&mut self.seen_in_file);
        let mut line_no = 0usize;
        for line in text.lines() {
            line_no += 1;
            self.total_lines += 1;
            if self.total_lines > MAX_CONFIG_LINES {
                return Err(ParseError {
                    file: source.into(),
                    line: line_no,
                    message: format!("config line count exceeds limit of {}", MAX_CONFIG_LINES),
                });
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields = match tokenize(line) {
                Ok(fields) => fields,
                Err(message) => {
                    return Err(ParseError {
                        file: source.into(),
                        line: line_no,
                        message,
                    })
                }
            };
            if fields.is_empty() {
                continue;
            }
            let key = fields[0].to_lowercase();
            let values = &fields[1..];
            if standalone && key == "include" {
                return Err(ParseError {
                    file: source.into(),
                    line: line_no,
                    message: "standalone configuration does not permit include directives".into(),
                });
            }
            if key != "include" {
                if let Some(prev) = self.seen_in_file.get(&key) {
                    return Err(ParseError {
                        file: source.into(),
                        line: line_no,
                        message: format!("duplicate key {:?}; first defined at line {}", key, prev),
                    });
                }
                self.seen_in_file.insert(key.clone(), line_no);
            }
            if key == "include" {
                self.include_directives += 1;
                if self.include_directives > MAX_CONFIG_INCLUDE_DIRECTIVES {
                    return Err(ParseError {
                        file: source.into(),
                        line: line_no,
                        message: format!(
                            "config include directive count exceeds limit of {}",
                            MAX_CONFIG_INCLUDE_DIRECTIVES
                        ),
                    });
                }
                if values.len() != 1 {
                    return Err(ParseError {
                        file: source.into(),
                        line: line_no,
                        message: "include requires exactly one relative path".into(),
                    });
                }
                let resolved = match resolve_include_path(Path::new(source), &values[0]) {
                    Ok(path) => path,
                    Err(message) => {
                        return Err(ParseError {
                            file: source.into(),
                            line: line_no,
                            message,
                        })
                    }
                };
                self.parse_file(&resolved, depth + 1)?;
                continue;
            }
            if let Err(message) = self.apply(&key, values) {
                return Err(ParseError {
                    file: source.into(),
                    line: line_no,
                    message,
                });
            }
        }
        self.seen_in_file = parent_seen;
        Ok(())
    }

    fn apply(&mut self, key: &str, v: &[String]) -> Result<(), String> {
        if v.len() > MAX_CONFIG_LIST_ITEMS {
            return Err(format!(
                "{} exceeds limit of {} values",
                key, MAX_CONFIG_LIST_ITEMS
            ));
        }
        for value in v {
            validate_config_string(key, value)?;
        }
        let one = |v: &[String]| -> Result<String, String> {
            if v.len() != 1 {
                Err(format!("{} requires one value", key))
            } else {
                Ok(v[0].clone())
            }
        };
        let boolv = |v: &[String]| -> Result<bool, String> {
            match one(v)?.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok(true),
                "0" | "false" | "no" | "off" => Ok(false),
                _ => Err(format!("{} must be 0/1 or true/false", key)),
            }
        };
        let intv = |v: &[String]| -> Result<i64, String> {
            one(v)?
                .parse()
                .map_err(|_| format!("{} must be an integer", key))
        };
        let floatv = |v: &[String]| -> Result<f64, String> {
            let n: f64 = one(v)?
                .parse()
                .map_err(|_| format!("{} must be a finite number", key))?;
            if !n.is_finite() {
                Err(format!("{} must be a finite number", key))
            } else {
                Ok(n)
            }
        };
        let msv = |v: &[String]| -> Result<Duration, String> {
            let n: i64 = one(v)?
                .parse()
                .map_err(|_| format!("{} must be an integer", key))?;
            if n < 0 {
                Err(format!("{} must be a non-negative integer", key))
            } else {
                Ok(Duration::from_millis(n as u64))
            }
        };
        // "auto" accepted for auto-capable interval selectors: maps to default.
        let ms_auto = |v: &[String], default: Duration| -> Result<Duration, String> {
            if v.len() == 1 && v[0].eq_ignore_ascii_case("auto") {
                return Ok(default);
            }
            msv(v)
        };
        let lower_one =
            |v: &[String]| -> Result<String, String> { one(v).map(|s| s.to_lowercase()) };
        let joined = |v: &[String]| -> Result<String, String> {
            if v.is_empty() {
                return Err(format!("{} requires a value", key));
            }
            Ok(v.join(" "))
        };
        let cfg = &mut self.cfg;
        match key {
            "config_refresh_ms" => cfg.config_refresh = ms_auto(v, Duration::from_secs(1))?,
            "telemetry_interval_ms" => {
                cfg.telemetry_interval = ms_auto(v, Duration::from_millis(300))?
            }
            "render_interval_ms" => cfg.render_interval = ms_auto(v, Duration::from_millis(100))?,
            "preview_mode" => cfg.preview_mode = lower_one(v)?,
            "preview_scale" => cfg.preview_scale = intv(v)? as i32,
            "start_minimized" => cfg.start_minimized = boolv(v)?,
            "start_at_login" => cfg.start_at_login = boolv(v)?,
            "safe_mode" => cfg.safe_mode = boolv(v)?,
            "date_format" => cfg.date_format = joined(v)?,
            "time_format" => cfg.time_format = joined(v)?,
            "slot_0" | "slot_1" | "slot_2" | "slot_3" => {
                let i = key.as_bytes()[5] - b'0';
                if v.is_empty() {
                    return Err(format!("{} requires at least one module", key));
                }
                cfg.slots[i as usize] = v.iter().map(|s| s.to_uppercase()).collect();
            }
            "ccd_source" => cfg.ccd_source = lower_one(v)?,
            "ccd_cache_processors" => {
                cfg.ccd_cache_processors = parse_processor_list(&joined(v)?)?;
            }
            "ccd_frequency_processors" => {
                cfg.ccd_frequency_processors = parse_processor_list(&joined(v)?)?;
            }
            "logitech_backend" => cfg.logitech_backend = lower_one(v)?,
            "logitech_reconnect_ms" => {
                cfg.logitech_reconnect = ms_auto(v, Duration::from_secs(5))?;
            }
            "logitech_reconnect_max_ms" => {
                cfg.logitech_reconnect_max = ms_auto(v, Duration::from_secs(60))?;
            }
            "logitech_button_poll_ms" => {
                cfg.logitech_button_poll = ms_auto(v, Duration::from_millis(50))?;
            }
            "logitech_button_debounce_ms" => {
                cfg.logitech_button_debounce = ms_auto(v, Duration::from_millis(40))?;
            }
            "logitech_friendly_name" => cfg.logitech_friendly_name = joined(v)?,
            "logitech_orientation" => cfg.logitech_orientation = lower_one(v)?,
            "logitech_invert" => cfg.logitech_invert = boolv(v)?,
            "lhm_mode" => cfg.lhm_mode = lower_one(v)?,
            "lhm_url" => cfg.lhm_url = one(v)?,
            "lhm_interval_ms" => cfg.lhm_interval = ms_auto(v, Duration::from_millis(300))?,
            "lhm_stale_ms" => cfg.lhm_stale_after = ms_auto(v, Duration::from_secs(3))?,
            "lhm_cpu_temp_sensor"
            | "lhm_gpu_temp_sensor"
            | "lhm_gpu_load_sensor"
            | "lhm_vram_load_sensor"
            | "lhm_memory_sensor"
            | "lhm_network_in_sensor"
            | "lhm_network_out_sensor" => {
                cfg.lhm_sensors
                    .insert(key.trim_start_matches("lhm_").to_string(), one(v)?);
            }
            "gpu_provider" => cfg.gpu_provider = lower_one(v)?,
            "presentmon_enabled" => cfg.presentmon_enabled = boolv(v)?,
            "presentmon_path" => cfg.presentmon_path = joined(v)?,
            "presentmon_interval_ms" => {
                cfg.presentmon_interval = ms_auto(v, Duration::from_secs(1))?;
            }
            "presentmon_window_ms" => {
                cfg.presentmon_window = ms_auto(v, Duration::from_secs(60))?;
            }
            "presentmon_target_mode" => cfg.presentmon_target_mode = lower_one(v)?,
            "presentmon_process_name" => cfg.presentmon_process_name = one(v)?,
            "presentmon_exclude" => cfg.presentmon_exclude = v.to_vec(),
            "stutter_threshold_ms" => cfg.stutter_threshold_ms = floatv(v)?,
            "headset_enabled" => cfg.headset_enabled = boolv(v)?,
            "headset_poll_ms" => cfg.headset_poll = ms_auto(v, Duration::from_secs(15))?,
            "headset_query_timeout_ms" => {
                cfg.headset_query_timeout = ms_auto(v, Duration::from_millis(1200))?;
            }
            "headset_stale_ms" => cfg.headset_stale_after = ms_auto(v, Duration::from_secs(45))?,
            "headset_warn_percent" => cfg.headset_warn_percent = intv(v)? as i32,
            "headset_critical_percent" => cfg.headset_critical_percent = intv(v)? as i32,
            "headset_estimate_hours" => cfg.headset_estimate_hours = boolv(v)?,
            "controller_enabled" => cfg.controller_enabled = boolv(v)?,
            "controller_index" => cfg.controller_index = intv(v)? as i32,
            "controller_poll_ms" => cfg.controller_poll = ms_auto(v, Duration::from_secs(10))?,
            "network_probe_enabled" => cfg.network_probe_enabled = boolv(v)?,
            "network_probe_method" => cfg.network_probe_method = lower_one(v)?,
            "network_probe_target" => cfg.network_probe_target = one(v)?,
            "network_probe_interval_ms" => {
                cfg.network_probe_interval = ms_auto(v, Duration::from_secs(1))?;
            }
            "network_probe_timeout_ms" => {
                cfg.network_probe_timeout = ms_auto(v, Duration::from_millis(1500))?;
            }
            "network_probe_window" => cfg.network_probe_window = intv(v)? as i32,
            "audio_enabled" => cfg.audio_enabled = boolv(v)?,
            "audio_poll_ms" => cfg.audio_poll = ms_auto(v, Duration::from_secs(1))?,
            "discord_enabled" => cfg.discord_enabled = boolv(v)?,
            "discord_client_id" => cfg.discord_client_id = one(v)?,
            "discord_redirect_uri" => cfg.discord_redirect_uri = one(v)?,
            "discord_linger_ms" => cfg.discord_linger = msv(v)?,
            "discord_max_speakers" => cfg.discord_max_speakers = intv(v)? as i32,
            "discord_show_self" => cfg.discord_show_self = boolv(v)?,
            "discord_show_channel" => cfg.discord_show_channel = boolv(v)?,
            "hang_enabled" => cfg.hang_enabled = boolv(v)?,
            "hang_button" => cfg.hang_button = intv(v)? as i32,
            "hang_hold_ms" => cfg.hang_hold = msv(v)?,
            "hang_probe_interval_ms" => cfg.hang_probe_interval = msv(v)?,
            "hang_probe_timeout_ms" => cfg.hang_probe_timeout = msv(v)?,
            "hang_failures_required" => cfg.hang_failures = intv(v)? as i32,
            "hang_minimum_ms" => cfg.hang_minimum = msv(v)?,
            "hang_ignore" => cfg.hang_ignore = v.to_vec(),
            "cpu_temp_warning" => cfg.cpu_temp_warning = floatv(v)?,
            "cpu_temp_critical" => cfg.cpu_temp_critical = floatv(v)?,
            "gpu_temp_warning" => cfg.gpu_temp_warning = floatv(v)?,
            "gpu_temp_critical" => cfg.gpu_temp_critical = floatv(v)?,
            "memory_warning" => cfg.memory_warning = floatv(v)?,
            "vmem_warning" => cfg.vmem_warning = floatv(v)?,
            "critical_alert_linger_ms" => cfg.critical_alert_linger = msv(v)?,
            "log_level" => cfg.log_level = lower_one(v)?,
            "log_max_bytes" => {
                let n = intv(v)?;
                if n < 0 {
                    return Err(format!("{} must be a non-negative integer", key));
                }
                cfg.log_max_bytes = n as u64;
            }
            "log_backups" => cfg.log_backups = intv(v)? as i32,
            other => return Err(format!("unknown key {:?}", other)),
        }
        Ok(())
    }
}

fn resolve_include_path(source: &Path, include: &str) -> Result<PathBuf, String> {
    validate_config_string("include path", include)?;
    if include.contains('$') || include.contains('%') {
        return Err("include path must not contain environment-variable syntax".into());
    }
    let normalized = include.replace('\\', "/");
    for component in normalized.split('/') {
        if component == ".." {
            return Err("include path must not contain parent traversal".into());
        }
    }
    let candidate = Path::new(include);
    if candidate.is_absolute() || has_drive_prefix(include) {
        return Err("include path must be relative".into());
    }
    let dir = source.parent().unwrap_or(Path::new("."));
    Ok(dir.join(include))
}

fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return true;
    }
    path.starts_with("\\\\")
}

/// Whitespace-separated tokenizer with `"..."`/`'...'` quoting. Only the
/// active quote delimiter and backslash are escaped inside quotes; ordinary
/// Windows path separators are preserved verbatim.
#[allow(unused_assignments)]
pub fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut b = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut token_started = false;
    let mut raw_token_bytes = 0usize;

    macro_rules! flush {
        () => {
            if token_started {
                out.push(std::mem::take(&mut b));
                if out.len() > MAX_CONFIG_LIST_ITEMS + 1 {
                    return Err(format!(
                        "config directive exceeds limit of {} values",
                        MAX_CONFIG_LIST_ITEMS
                    ));
                }
                token_started = false;
                raw_token_bytes = 0;
            }
        };
    }
    macro_rules! write_rune {
        ($r:expr) => {
            b.push($r);
            if b.len() > MAX_CONFIG_TOKEN_BYTES {
                return Err(format!(
                    "config token exceeds limit of {} bytes",
                    MAX_CONFIG_TOKEN_BYTES
                ));
            }
        };
    }

    for r in line.chars() {
        let raw_len = r.len_utf8();
        if !escaped && quote.is_none() && (r == ' ' || r == '\t') {
            flush!();
            continue;
        }
        if !escaped && quote.is_none() && r == '#' && !token_started {
            break;
        }
        raw_token_bytes += raw_len;
        if raw_token_bytes > MAX_CONFIG_TOKEN_BYTES {
            return Err(format!(
                "config token exceeds limit of {} bytes",
                MAX_CONFIG_TOKEN_BYTES
            ));
        }
        if escaped {
            if r != quote.unwrap() && r != '\\' {
                write_rune!('\\');
            }
            write_rune!(r);
            token_started = true;
            escaped = false;
            continue;
        }
        if r == '\\' && quote.is_some() {
            escaped = true;
            token_started = true;
            continue;
        }
        if let Some(q) = quote {
            if r == q {
                quote = None;
            } else {
                write_rune!(r);
            }
            token_started = true;
            continue;
        }
        if r == '"' || r == '\'' {
            quote = Some(r);
            token_started = true;
            continue;
        }
        write_rune!(r);
        token_started = true;
    }
    if quote.is_some() {
        return Err("unterminated quote".into());
    }
    if escaped {
        write_rune!('\\');
    }
    flush!();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_handles_quotes_escapes_and_comments() {
        assert_eq!(tokenize("key one two").unwrap(), vec!["key", "one", "two"]);
        assert_eq!(
            tokenize("name \"John Smith\"").unwrap(),
            vec!["name", "John Smith"]
        );
        assert_eq!(
            tokenize("path C:\\dir\\file.txt").unwrap(),
            vec!["path", "C:\\dir\\file.txt"]
        );
        assert_eq!(
            tokenize("quoted \"back\\\\slash\"").unwrap(),
            vec!["quoted", "back\\slash"]
        );
        assert_eq!(tokenize("a b # trailing").unwrap(), vec!["a", "b"]);
        assert_eq!(tokenize("a#notcomment").unwrap(), vec!["a#notcomment"]);
        assert!(tokenize("bad \"unterminated").is_err());
        assert_eq!(tokenize("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn standalone_rejects_includes_and_unknown_keys() {
        let err = parse_standalone(b"include foo.txt\n").unwrap_err();
        assert!(err.message.contains("standalone"));
        let err = parse_standalone(b"bogus_key 1\n").unwrap_err();
        assert!(err.message.contains("unknown key"));
    }

    #[test]
    fn standalone_parses_slots_and_defaults() {
        let cfg = parse_standalone(
            b"slot_0 HEADSET_BATTERY CPU_TEMP\nslot_1 FPS_CURRENT\nslot_2 GPU_TEMP\nslot_3 PACKET_LOSS\n",
        )
        .unwrap();
        assert_eq!(cfg.slots[0], vec!["HEADSET_BATTERY", "CPU_TEMP"]);
        assert_eq!(cfg.slots[1], vec!["FPS_CURRENT"]);
        assert_eq!(cfg.slots[2], vec!["GPU_TEMP"]);
        assert_eq!(cfg.slots[3], vec!["PACKET_LOSS"]);
        // A config touching no slots keeps all defaults.
        let cfg = parse_standalone(b"preview_scale 2\n").unwrap();
        assert_eq!(
            cfg.slots[0],
            vec!["HEADSET_BATTERY", "CPU_TEMP", "CONTROLLER_BATTERY"]
        );
    }

    #[test]
    fn duplicate_keys_rejected_with_line_diagnostics() {
        let err = parse_standalone(b"preview_scale 3\npreview_scale 4\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("duplicate key"));
    }

    #[test]
    fn auto_intervals_map_to_defaults() {
        let cfg =
            parse_standalone(b"telemetry_interval_ms auto\nrender_interval_ms 250\n").unwrap();
        assert_eq!(cfg.telemetry_interval, Duration::from_millis(300));
        assert_eq!(cfg.render_interval, Duration::from_millis(250));
    }

    #[test]
    fn invalid_values_report_file_and_line() {
        // Value-level errors are caught by post-parse validation, which
        // carries no line info (matching the Go implementation's Validate).
        let err = parse_standalone(b"preview_mode bogus\n").unwrap_err();
        assert_eq!(err.file, "standalone-configuration");
        assert!(err.message.contains("preview_mode"));
        // Tokenizer-level errors do carry line diagnostics.
        let err = parse_standalone(b"preview_scale 3\nbad \"quote\n").unwrap_err();
        assert_eq!(err.line, 2);
    }

    #[test]
    fn include_rejects_traversal_and_absolute() {
        let dir = std::env::temp_dir().join(format!("lcdforge-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main.txt");
        std::fs::write(&main, "include ..\\escape.txt\n").unwrap();
        let err = load(&main).unwrap_err();
        assert!(err.message.contains("parent traversal"));
        std::fs::write(&main, "include C:\\Windows\\evil.txt\n").unwrap();
        let err = load(&main).unwrap_err();
        assert!(err.message.contains("relative"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_cycle_detected() {
        let dir = std::env::temp_dir().join(format!("lcdforge-cfg-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main.txt");
        let inc = dir.join("inc.txt");
        std::fs::write(&main, "preview_scale 2\ninclude inc.txt\n").unwrap();
        std::fs::write(&inc, "include main.txt\n").unwrap();
        let err = load(&main).unwrap_err();
        assert!(err.message.contains("cycle"), "got: {}", err.message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_overrides_primary_values() {
        let dir = std::env::temp_dir().join(format!("lcdforge-cfg-ovr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main.txt");
        std::fs::write(&main, "preview_scale 2\ninclude local.txt\n").unwrap();
        std::fs::write(dir.join("local.txt"), "preview_scale 5\n").unwrap();
        let loaded = load(&main).unwrap();
        assert_eq!(loaded.config.preview_scale, 5);
        assert_eq!(loaded.files.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
