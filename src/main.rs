// LCDSirPlus: LCDSirReal-style dashboard for the Logitech G13 160x43 LCD.
// Rust port of the original Go implementation — native-first telemetry,
// direct-HID G13 backend, no Logitech runtime dependency.

#![windows_subsystem = "windows"]

mod alerts;
mod app;
mod backends;
mod config;
mod diagnostics;
mod hardware_test;
mod history;
mod http;
mod input;
mod json;
mod logging;
mod model;
mod parser;
mod providers;
mod render;
mod runtime;
mod sha256;
mod slots;
mod telemetry;
mod ui;

use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() -> String {
    format!(
        "LCDSirPlus {VERSION} — LCDSirReal-style dashboard for the Logitech G13 160x43 LCD\n\
         \n\
         USAGE:\n\
         \x20 LCDSirPlus.exe [--config PATH] [COMMAND]\n\
         \n\
         COMMANDS:\n\
         \x20 (none)             Run the dashboard application\n\
         \x20 --validate-config  Validate the configuration and exit\n\
         \x20 --list-hwinfo-sensors  List usable HWiNFO sensor/reading labels and exit\n\
         \x20 --preview          Run with the virtual preview forced on\n\
         \x20 --hardware-test    Run the deterministic 10-step G13 test sequence\n\
         \x20 --hardware-discover  List compatible G13 HID discovery results\n\
         \x20 --diagnostics      Write a bounded offline diagnostics ZIP and exit\n\
         \x20 --discord-authorize  Authorize local RPC (no browser or redirect listener)\n\
         \x20 --discord-clear-token  Remove all LCDSirPlus Discord credentials\n\
         \x20 --backend auto|sdk|hid|virtual  Backend for --hardware-test (default hid)\n\
         \x20 --duration-secs N  Visible-sequence duration for --hardware-test\n\
         \x20 --safe-mode        Run with providers/destructive actions disabled\n\
         \x20 --diagnostic-dir PATH  Log/diagnostic output directory\n\
         \x20 --help, -h         Print command help\n\
         \x20 --version, -v      Print version"
    )
}

fn print_usage() {
    println!("{}", usage());
}

struct Cli {
    config: Option<std::path::PathBuf>,
    validate: bool,
    list_hwinfo_sensors: bool,
    preview: bool,
    hardware_test: bool,
    hardware_discover: bool,
    diagnostics: bool,
    discord_authorize: bool,
    discord_clear_token: bool,
    hang_test_harness: bool,
    hang_detector_smoke: bool,
    hang_action_smoke: bool,
    hang_action_negative_smoke: bool,
    instance_smoke: bool,
    instance_smoke_child: bool,
    backend: backends::BackendKind,
    backend_supplied: bool,
    duration: Duration,
    duration_supplied: bool,
    safe_mode: bool,
    diagnostic_dir: Option<std::path::PathBuf>,
    version: bool,
    help: bool,
}

fn parse_args_from(args: impl IntoIterator<Item = impl Into<String>>) -> Result<Cli, String> {
    let mut cli = Cli {
        config: None,
        validate: false,
        list_hwinfo_sensors: false,
        preview: false,
        hardware_test: false,
        hardware_discover: false,
        diagnostics: false,
        discord_authorize: false,
        discord_clear_token: false,
        hang_test_harness: false,
        hang_detector_smoke: false,
        hang_action_smoke: false,
        hang_action_negative_smoke: false,
        instance_smoke: false,
        instance_smoke_child: false,
        backend: backends::BackendKind::Hid,
        backend_supplied: false,
        duration: Duration::from_secs(30),
        duration_supplied: false,
        safe_mode: false,
        diagnostic_dir: None,
        version: false,
        help: false,
    };
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--config" => {
                i += 1;
                let value = args.get(i).ok_or("--config requires a path")?;
                cli.config = Some(std::path::PathBuf::from(value));
            }
            "--validate-config" => cli.validate = true,
            "--list-hwinfo-sensors" => cli.list_hwinfo_sensors = true,
            "--preview" => cli.preview = true,
            "--hardware-test" => cli.hardware_test = true,
            "--hardware-discover" => cli.hardware_discover = true,
            "--diagnostics" => cli.diagnostics = true,
            "--discord-authorize" => cli.discord_authorize = true,
            "--discord-clear-token" => cli.discord_clear_token = true,
            "--hang-test-harness" => cli.hang_test_harness = true,
            "--hang-detector-smoke" => cli.hang_detector_smoke = true,
            "--hang-action-smoke" => cli.hang_action_smoke = true,
            "--hang-action-negative-smoke" => cli.hang_action_negative_smoke = true,
            "--instance-smoke" => cli.instance_smoke = true,
            "--instance-smoke-child" => cli.instance_smoke_child = true,
            "--backend" => {
                cli.backend_supplied = true;
                i += 1;
                match args.get(i).map(|s| s.as_str()) {
                    Some("auto") => cli.backend = backends::BackendKind::Auto,
                    Some("sdk") => cli.backend = backends::BackendKind::Sdk,
                    Some("hid") => cli.backend = backends::BackendKind::Hid,
                    Some("virtual") => cli.backend = backends::BackendKind::Virtual,
                    other => {
                        return Err(format!(
                            "--backend must be auto, sdk, hid, or virtual, got {:?}",
                            other
                        ))
                    }
                }
            }
            "--duration-secs" => {
                cli.duration_supplied = true;
                i += 1;
                let value: u64 = args
                    .get(i)
                    .ok_or("--duration-secs requires a number")?
                    .parse()
                    .map_err(|_| "--duration-secs must be a number")?;
                if value == 0 || value > 3600 {
                    return Err("--duration-secs must be 1..3600".into());
                }
                cli.duration = Duration::from_secs(value);
            }
            "--safe-mode" => cli.safe_mode = true,
            "--diagnostic-dir" => {
                i += 1;
                let value = args.get(i).ok_or("--diagnostic-dir requires a path")?;
                cli.diagnostic_dir = Some(std::path::PathBuf::from(value));
            }
            "--version" | "-v" => cli.version = true,
            "--help" | "-h" => cli.help = true,
            other => return Err(format!("unknown argument {:?}", other)),
        }
        i += 1;
    }
    if !cli.hardware_test {
        if cli.backend_supplied {
            return Err("--backend requires --hardware-test".into());
        }
        if cli.duration_supplied && !cli.hang_test_harness {
            return Err("--duration-secs requires --hardware-test or --hang-test-harness".into());
        }
    }
    Ok(cli)
}

fn parse_args() -> Result<Cli, String> {
    parse_args_from(std::env::args().skip(1))
}

fn command_count(cli: &Cli) -> usize {
    [
        cli.validate,
        cli.list_hwinfo_sensors,
        cli.hardware_test,
        cli.hardware_discover,
        cli.diagnostics,
        cli.discord_authorize,
        cli.discord_clear_token,
        cli.hang_test_harness,
        cli.hang_detector_smoke,
        cli.hang_action_smoke,
        cli.hang_action_negative_smoke,
        cli.instance_smoke,
        cli.instance_smoke_child,
    ]
    .into_iter()
    .filter(|selected| *selected)
    .count()
}

fn is_cli_invocation(cli: &Cli) -> bool {
    cli.help || cli.version || command_count(cli) != 0
}

fn attach_parent_console() {
    unsafe {
        // Redirected handles supplied by automation remain intact; an interactive
        // console parent supplies handles for direct CLI diagnostics.
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn report_startup_error(message: &str, gui_launch: bool) {
    eprintln!("{message}");
    if !gui_launch {
        return;
    }
    let message: Vec<u16> = message
        .encode_utf16()
        .map(|unit| if unit == 0 { b'?' as u16 } else { unit })
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = "LCDSirPlus startup error\0".encode_utf16().collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

fn run_diagnostics(cli: &Cli) -> i32 {
    let directory = cli.diagnostic_dir.clone().unwrap_or_else(app::log_dir);
    match diagnostics::write_bundle(&directory, cli.safe_mode) {
        Ok(path) => {
            println!("Diagnostics written to {}", path.display());
            0
        }
        Err(error) => {
            eprintln!("diagnostics failed: {error}");
            1
        }
    }
}

fn run_hwinfo_inventory(result: Result<Vec<String>, String>) -> i32 {
    match result {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(error) => {
            eprintln!(
                "HWiNFO sensor listing failed: {error}\nHWiNFO Shared Memory Support must be enabled."
            );
            1
        }
    }
}

fn main() {
    let cli = match parse_args() {
        Ok(cli) => cli,
        Err(e) => {
            attach_parent_console();
            eprintln!("error: {}\n", e);
            print_usage();
            std::process::exit(2);
        }
    };
    let gui_launch = !is_cli_invocation(&cli);
    if !gui_launch {
        attach_parent_console();
    }

    if cli.version {
        println!("LCDSirPlus {}", VERSION);
        return;
    }
    if cli.help {
        print_usage();
        return;
    }

    if command_count(&cli) > 1 {
        eprintln!("error: select only one command");
        std::process::exit(2);
    }

    if cli.diagnostics {
        std::process::exit(run_diagnostics(&cli));
    }

    if cli.list_hwinfo_sensors {
        std::process::exit(run_hwinfo_inventory(providers::hwinfo::inventory()));
    }

    if cli.hang_test_harness {
        std::process::exit(providers::hang::run_harness(cli.duration));
    }
    if cli.hang_detector_smoke {
        std::process::exit(providers::hang::run_smoke());
    }
    if cli.hang_action_smoke {
        std::process::exit(providers::hang::run_action_smoke(false));
    }
    if cli.hang_action_negative_smoke {
        std::process::exit(providers::hang::run_action_smoke(true));
    }
    if cli.instance_smoke_child {
        let blocked = runtime::InstanceGuard::acquire().is_ok_and(|guard| guard.is_none());
        std::process::exit(if blocked { 0 } else { 1 });
    }
    if cli.instance_smoke {
        let first = runtime::InstanceGuard::acquire().ok().flatten();
        let blocked = first.is_some()
            && std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    std::process::Command::new(exe)
                        .arg("--instance-smoke-child")
                        .status()
                        .ok()
                })
                .is_some_and(|status| status.success());
        drop(first);
        let released = runtime::InstanceGuard::acquire().is_ok_and(|guard| guard.is_some());
        let passed = blocked && released;
        println!("INSTANCE SMOKE {}", if passed { "OK" } else { "FAILED" });
        std::process::exit(if passed { 0 } else { 1 });
    }

    if cli.validate {
        let path = match runtime::resolve_config(cli.config.clone()) {
            Ok(resolved) => resolved.path,
            Err(error) => {
                eprintln!("INVALID: configuration resolution failed: {error}");
                std::process::exit(2);
            }
        };
        match app::validate_config(&path) {
            outcome if outcome.ok => {
                println!("OK: {}", outcome.message);
                if let Some(loaded) = outcome.loaded {
                    for file in loaded.files {
                        println!("  file: {}", file.display());
                    }
                }
                std::process::exit(0);
            }
            outcome => {
                eprintln!("INVALID: {}", outcome.message);
                std::process::exit(2);
            }
        }
    }

    if cli.discord_clear_token {
        let path = match runtime::resolve_config(cli.config.clone()) {
            Ok(resolved) => resolved.path,
            Err(error) => {
                eprintln!("configuration resolution failed: {error}");
                std::process::exit(2);
            }
        };
        if let Err(error) = parser::load(&path) {
            eprintln!("configuration error: {}", error);
            std::process::exit(2);
        }
        match providers::discord::clear_token() {
            Ok(()) => println!("All LCDSirPlus Discord credentials removed."),
            Err(error) => {
                eprintln!("error: {}", error);
                std::process::exit(1);
            }
        }
        return;
    }

    if cli.discord_authorize {
        let path = match runtime::resolve_config(cli.config.clone()) {
            Ok(resolved) => resolved.path,
            Err(error) => {
                eprintln!("configuration resolution failed: {error}");
                std::process::exit(2);
            }
        };
        let cfg = match parser::load(&path) {
            Ok(loaded) => loaded.config,
            Err(error) => {
                eprintln!("configuration error: {}", error);
                std::process::exit(2);
            }
        };
        let secret = std::env::var("LCDSIRPLUS_DISCORD_CLIENT_SECRET").unwrap_or_default();
        match providers::discord::authorize(&cfg, &secret) {
            Ok(()) => println!("Discord authorization stored for the current Discord account."),
            Err(error) => {
                eprintln!("Discord authorization failed: {}", error);
                std::process::exit(1);
            }
        }
        return;
    }

    if cli.hardware_discover {
        std::process::exit(app::run_hardware_discover());
    }

    if cli.hardware_test {
        let instance = if cli.backend != backends::BackendKind::Virtual {
            match runtime::InstanceGuard::acquire() {
                Ok(Some(guard)) => Some(guard),
                Ok(None) => {
                    eprintln!("LCDSirPlus is already running; direct-HID test refused");
                    std::process::exit(5);
                }
                Err(error) => {
                    eprintln!("single-instance ownership failed: {error}");
                    std::process::exit(5);
                }
            }
        } else {
            None
        };
        let code = app::run_hardware_test(
            cli.config.clone(),
            cli.backend,
            cli.duration,
            cli.diagnostic_dir.clone(),
        );
        drop(instance);
        std::process::exit(code);
    }

    let instance = match runtime::InstanceGuard::acquire() {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            report_startup_error("LCDSirPlus is already running", gui_launch);
            std::process::exit(5);
        }
        Err(error) => {
            report_startup_error(
                &format!("single-instance ownership failed: {error}"),
                gui_launch,
            );
            std::process::exit(5);
        }
    };
    let result = app::run(app::RunOptions {
        config_path: cli.config,
        preview_always: cli.preview,
        safe_mode: cli.safe_mode,
        diagnostic_dir: cli.diagnostic_dir,
    });
    drop(instance);
    let code = match result {
        Ok(code) => code,
        Err((code, message)) => {
            report_startup_error(&message, gui_launch);
            code
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_ignore_unreachable_unc_config_without_calling_parser() {
        let directory = std::env::temp_dir().join(format!(
            "lcdsirplus-cli-offline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cli = Cli {
            config: Some(r"\\unreachable.invalid\share\lcdsirplus.txt".into()),
            validate: false,
            list_hwinfo_sensors: false,
            preview: false,
            hardware_test: false,
            hardware_discover: false,
            diagnostics: true,
            discord_authorize: false,
            discord_clear_token: false,
            hang_test_harness: false,
            hang_detector_smoke: false,
            hang_action_smoke: false,
            hang_action_negative_smoke: false,
            instance_smoke: false,
            instance_smoke_child: false,
            backend: backends::BackendKind::Hid,
            backend_supplied: false,
            duration: Duration::from_secs(30),
            duration_supplied: false,
            safe_mode: true,
            diagnostic_dir: Some(directory.clone()),
            version: false,
            help: false,
        };
        let before = parser::test_load_calls();
        assert_eq!(run_diagnostics(&cli), 0);
        assert_eq!(parser::test_load_calls(), before);
        let bundles: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(bundles.len(), 1);
        assert!(
            !String::from_utf8_lossy(&std::fs::read(&bundles[0]).unwrap())
                .contains("unreachable.invalid")
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn parses_and_advertises_hwinfo_inventory_command() {
        let cli = parse_args_from(["--list-hwinfo-sensors"]).unwrap();
        assert!(cli.list_hwinfo_sensors);
        assert!(!cli.validate);

        assert!(parse_args_from(["--list-hwinfo-sensor"]).is_err());
        assert!(usage().contains("--list-hwinfo-sensors"));
        assert_eq!(run_hwinfo_inventory(Ok(Vec::new())), 0);
        assert_eq!(run_hwinfo_inventory(Err("inactive".into())), 1);
    }

    #[test]
    fn runtime_options_remain_gui_while_commands_use_cli_io() {
        assert!(!is_cli_invocation(
            &parse_args_from(["--preview", "--safe-mode"]).unwrap()
        ));
        assert!(!is_cli_invocation(
            &parse_args_from(["--config", "other.txt"]).unwrap()
        ));
        assert!(is_cli_invocation(
            &parse_args_from(["--validate-config"]).unwrap()
        ));
        assert!(is_cli_invocation(&parse_args_from(["--help"]).unwrap()));
    }

    #[test]
    fn hardware_options_require_hardware_test() {
        assert_eq!(
            parse_args_from(["--backend", "virtual"]).err().unwrap(),
            "--backend requires --hardware-test"
        );
        assert_eq!(
            parse_args_from(["--duration-secs", "1"]).err().unwrap(),
            "--duration-secs requires --hardware-test or --hang-test-harness"
        );
        let harness = parse_args_from(["--hang-test-harness", "--duration-secs", "120"]).unwrap();
        assert!(harness.hang_test_harness && harness.duration_supplied);
        assert_eq!(harness.duration, Duration::from_secs(120));
        let cli = parse_args_from([
            "--hardware-test",
            "--backend",
            "virtual",
            "--duration-secs",
            "1",
        ])
        .unwrap();
        assert!(cli.hardware_test && cli.backend_supplied && cli.duration_supplied);
        assert_eq!(cli.backend, backends::BackendKind::Virtual);
        assert_eq!(cli.duration, Duration::from_secs(1));
    }
}
