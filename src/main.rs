// LCDForge: LCDSirReal-style dashboard for the Logitech G13 160x43 LCD.
// Rust port of the LCDForge Go application (0.2.0) — native-first telemetry,
// direct-HID G13 backend, no Logitech runtime dependency.

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
mod png;
mod providers;
mod render;
mod runtime;
mod sha256;
mod slots;
mod telemetry;
mod ui;

use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    println!(
        "LCDForge {VERSION} — LCDSirReal-style dashboard for the Logitech G13 160x43 LCD\n\
         \n\
         USAGE:\n\
         \x20 lcdforge.exe [--config PATH] [COMMAND]\n\
         \n\
         COMMANDS:\n\
         \x20 (none)             Run the dashboard application\n\
         \x20 --validate-config  Validate the configuration and exit\n\
         \x20 --preview          Run with the virtual preview forced on\n\
         \x20 --hardware-test    Run the deterministic 10-step G13 test sequence\n\
         \x20 --diagnostics      Write a bounded offline diagnostics ZIP and exit\n\
         \x20 --discord-authorize  Authorize Discord RPC for the current user\n\
         \x20 --discord-clear-token  Remove the current-user Discord credential\n\
         \x20 --backend hid|virtual  Backend for --hardware-test (default hid)\n\
         \x20 --duration-secs N  Visible-sequence duration for --hardware-test\n\
         \x20 --safe-mode        Run with providers/destructive actions disabled\n\
         \x20 --diagnostic-dir PATH  Log/diagnostic output directory\n\
         \x20 --version          Print version"
    );
}

struct Cli {
    config: Option<std::path::PathBuf>,
    validate: bool,
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
    duration: Duration,
    safe_mode: bool,
    diagnostic_dir: Option<std::path::PathBuf>,
    version: bool,
    help: bool,
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        config: None,
        validate: false,
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
        duration: Duration::from_secs(30),
        safe_mode: false,
        diagnostic_dir: None,
        version: false,
        help: false,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
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
                i += 1;
                match args.get(i).map(|s| s.as_str()) {
                    Some("hid") => cli.backend = backends::BackendKind::Hid,
                    Some("virtual") => cli.backend = backends::BackendKind::Virtual,
                    other => {
                        return Err(format!("--backend must be hid or virtual, got {:?}", other))
                    }
                }
            }
            "--duration-secs" => {
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
    Ok(cli)
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

fn main() {
    let cli = match parse_args() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("error: {}\n", e);
            print_usage();
            std::process::exit(2);
        }
    };

    if cli.version {
        println!("LCDForge {}", VERSION);
        return;
    }
    if cli.help {
        print_usage();
        return;
    }

    let command_count = [
        cli.validate,
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
    .count();
    if command_count > 1 {
        eprintln!("error: select only one command");
        std::process::exit(2);
    }

    if cli.diagnostics {
        std::process::exit(run_diagnostics(&cli));
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
        let path = cli
            .config
            .clone()
            .unwrap_or_else(app::default_config_path_pub);
        match app::validate_config(&path) {
            outcome if outcome.ok => {
                println!("OK: {}", outcome.message);
                for f in &outcome.files {
                    println!("  file: {}", f.display());
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
        let path = cli
            .config
            .clone()
            .unwrap_or_else(app::default_config_path_pub);
        if let Err(error) = parser::load(&path) {
            eprintln!("configuration error: {}", error);
            std::process::exit(2);
        }
        match providers::discord::clear_token() {
            Ok(()) => println!("Discord token removed."),
            Err(error) => {
                eprintln!("error: {}", error);
                std::process::exit(1);
            }
        }
        return;
    }

    if cli.discord_authorize {
        let path = cli
            .config
            .clone()
            .unwrap_or_else(app::default_config_path_pub);
        let cfg = match parser::load(&path) {
            Ok(loaded) => loaded.config,
            Err(error) => {
                eprintln!("configuration error: {}", error);
                std::process::exit(2);
            }
        };
        let secret = std::env::var("LCDFORGE_DISCORD_CLIENT_SECRET").unwrap_or_default();
        match providers::discord::authorize(&cfg, &secret) {
            Ok(()) => println!(
                "Discord authorization complete. The token is protected with Windows DPAPI."
            ),
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
        let instance = if cli.backend == backends::BackendKind::Hid {
            match runtime::InstanceGuard::acquire() {
                Ok(Some(guard)) => Some(guard),
                Ok(None) => {
                    eprintln!("LCDForge is already running; direct-HID test refused");
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
            eprintln!("LCDForge is already running");
            std::process::exit(5);
        }
        Err(error) => {
            eprintln!("single-instance ownership failed: {error}");
            std::process::exit(5);
        }
    };
    let code = app::run(app::RunOptions {
        config_path: cli.config,
        preview_always: cli.preview,
        safe_mode: cli.safe_mode,
        diagnostic_dir: cli.diagnostic_dir,
    });
    drop(instance);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_ignore_unreachable_unc_config_without_calling_parser() {
        let directory = std::env::temp_dir().join(format!(
            "lcdforge-cli-offline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cli = Cli {
            config: Some(r"\\unreachable.invalid\share\lcdforge.txt".into()),
            validate: false,
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
            duration: Duration::from_secs(30),
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
}
