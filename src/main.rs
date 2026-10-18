mod backend {
    pub mod winit;
}
mod config;
mod grabs;
mod handlers;
mod input;
mod layout;
mod render;
mod state;
mod workspace;

use std::path::PathBuf;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use crate::{config::Config, state::State};

const HELP: &str = "\
wayland-time — a lightweight tiling Wayland compositor

USAGE:
    wayland-time [OPTIONS]

OPTIONS:
    -c, --config <PATH>    Use this config file instead of ~/.config/wayland-time/config.toml
    -e, --exec <CMD>       Run a command once the compositor is ready (repeatable)
        --check-config     Validate the config file and exit
    -h, --help             Show this help

Logging is controlled with RUST_LOG, e.g. RUST_LOG=debug.
";

struct Args {
    config: Option<PathBuf>,
    exec: Vec<String>,
    check_config: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config: None,
        exec: Vec::new(),
        check_config: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" | "--config" => args.config = Some(it.next().ok_or("--config needs a path")?.into()),
            "-e" | "--exec" => args.exec.push(it.next().ok_or("--exec needs a command")?),
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = parse_args().map_err(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    })?;

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

    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    if !nested {
        return Err(
            "running from a TTY is not supported yet; start wayland-time inside an X11 or Wayland session".into(),
        );
    }

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

    backend::winit::init(&mut state, &handle)?;
    tracing::info!("listening on WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());

    state.watch_config();
    state.run_autostart();
    for cmd in &args.exec {
        state.spawn(cmd);
    }

    event_loop.run(None, &mut state, |state| {
        // Redraw only when something changed; an idle desktop costs nothing.
        if state.needs_redraw {
            state.needs_redraw = false;
            if let Some(w) = &state.winit {
                w.request_redraw();
            }
        }
        let _ = state.display_handle.flush_clients();
    })?;

    Ok(())
}
