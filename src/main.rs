mod backend;
mod bar;
mod config;
mod cursor;
mod focus;
mod grabs;
mod handlers;
mod input;
mod layout;
mod render;
mod state;
mod text;
mod theme;
mod workspace;

use std::{fs::File, path::PathBuf, sync::Mutex};

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use crate::{config::Config, state::State};

const HELP: &str = "\
wayland-time — a lightweight tiling Wayland compositor

USAGE:
    wayland-time [OPTIONS]

OPTIONS:
    -c, --config <PATH>      Use this config file instead of ~/.config/wayland-time/config.toml
    -e, --exec <CMD>         Run a command once the compositor is ready (repeatable)
    -b, --backend <NAME>     auto (default), winit (nested window) or tty (real hardware)
    -o, --output <NAME>      tty only: light up just this monitor, e.g. DP-4 (repeatable).
                             Monitors not listed are switched off. Names are in the log.
        --check-config       Validate the config file and exit
    -h, --help               Show this help

Logging is controlled with RUST_LOG, e.g. RUST_LOG=debug. When running from a TTY,
logs are also written to ~/.local/state/wayland-time/wayland-time.log.
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    Auto,
    Winit,
    Tty,
}

struct Args {
    config: Option<PathBuf>,
    exec: Vec<String>,
    backend: BackendChoice,
    outputs: Vec<String>,
    check_config: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config: None,
        exec: Vec::new(),
        backend: BackendChoice::Auto,
        outputs: Vec::new(),
        check_config: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" | "--config" => args.config = Some(it.next().ok_or("--config needs a path")?.into()),
            "-e" | "--exec" => args.exec.push(it.next().ok_or("--exec needs a command")?),
            "-b" | "--backend" => {
                args.backend = match it.next().as_deref() {
                    Some("auto") => BackendChoice::Auto,
                    Some("winit") => BackendChoice::Winit,
                    Some("tty") => BackendChoice::Tty,
                    _ => return Err("--backend must be auto, winit or tty".into()),
                }
            }
            "-o" | "--output" => args.outputs.push(it.next().ok_or("--output needs a monitor name")?),
            "--check-config" => args.check_config = true,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument \"{other}\" (see --help)")),
        }
    }
    Ok(args)
}

/// On a TTY the console is hidden behind the compositor, so keep a log file too.
fn init_logging(to_file: bool) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = to_file.then(log_file).flatten();
    let file_layer = file.map(|(f, path)| {
        eprintln!("logging to {}", path.display());
        fmt::layer().with_ansi(false).with_writer(Mutex::new(f))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(file_layer)
        .init();
}

fn log_file() -> Option<(File, PathBuf)> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let dir = base.join("wayland-time");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("wayland-time.log");
    // Keep the previous run's log for debugging crashes.
    let _ = std::fs::rename(&path, dir.join("wayland-time.log.old"));
    File::create(&path).ok().map(|f| (f, path))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });

    let has_display = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let nested = match args.backend {
        BackendChoice::Auto => has_display,
        BackendChoice::Winit => true,
        BackendChoice::Tty => false,
    };
    init_logging(!nested && !args.check_config);

    let config_path = args.config.or_else(Config::default_path);

    if args.check_config {
        let path = config_path.ok_or("no config path")?;
        let text = std::fs::read_to_string(&path)?;
        return match Config::parse_user(&text) {
            Ok(_) => {
                println!("{}: ok", path.display());
                Ok(())
            }
            Err(e) => {
                eprintln!("{}:\n{e}", path.display());
                std::process::exit(1);
            }
        };
    }

    // A broken config must never leave you without a desktop: fall back to defaults.
    let config = match &config_path {
        Some(path) => Config::load(path).unwrap_or_else(|e| {
            tracing::error!("config error, using defaults:\n{e}");
            Config::default()
        }),
        None => Config::default(),
    };

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let handle = event_loop.handle();
    let mut state = State::new(
        display,
        handle.clone(),
        event_loop.get_signal(),
        config,
        config_path,
        nested,
    );

    let result = if nested {
        backend::winit::init(&mut state, &handle)
    } else {
        backend::udev::init(&mut state, &handle, args.outputs)
    };
    if let Err(e) = result {
        tracing::error!("failed to start {} backend: {e}", if nested { "nested" } else { "tty" });
        return Err(e);
    }
    tracing::info!("listening on WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());

    state.watch_config();
    state.arm_idle_power_off();
    state.startup_commands = state.config.general.autostart.clone();
    state.startup_commands.extend(args.exec.iter().cloned());
    if state.config.general.xwayland {
        state.start_xwayland();
    } else {
        state.run_startup_commands();
    }

    event_loop.run(None, &mut state, |state| {
        // Redraw only when something changed; an idle desktop costs nothing.
        if state.needs_redraw {
            state.needs_redraw = false;
            state.schedule_redraw();
        }
        let _ = state.display_handle.flush_clients();
    })?;

    tracing::info!("exiting");
    Ok(())
}
