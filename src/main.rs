// LCDForge: LCDSirReal-style dashboard for the Logitech G13 160x43 LCD.
// Rust port of the LCDForge Go application (0.2.0) — native-first telemetry,
// direct-HID G13 backend, no Logitech runtime dependency.

mod app;
mod backends;
mod config;
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

    if cli.hardware_discover {
        std::process::exit(app::run_hardware_discover());
    }

    if cli.hardware_test {
        std::process::exit(app::run_hardware_test(
            cli.config.clone(),
            cli.backend,
            cli.duration,
        ));
    }

    std::process::exit(app::run(app::RunOptions {
        config_path: cli.config,
        preview_always: cli.preview,
        safe_mode: cli.safe_mode,
    }));
}
