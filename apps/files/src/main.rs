//! wayland-time-files: the file picker apps get when they ask the desktop portal to open or
//! save a file, drawn in the theme's colors and font.
//!
//! `--portal` serves `org.freedesktop.impl.portal.FileChooser` on the session bus (D-Bus starts
//! it on demand). Each request runs this program again with `--pick`, which shows the window,
//! reads the request on stdin and prints the answer on stdout.

mod app;
mod fs;
mod portal;
mod request;

use std::io::Read;

use request::{Mode, Request};

const HELP: &str = "\
wayland-time-files — file picker for the wayland-time compositor

USAGE:
    wayland-time-files [--open | --open-many | --folder | --save NAME]
    wayland-time-files --portal

With no option, or one of the first ones, shows a picker and prints what was picked, one
path per line (exit status 1 if cancelled). Apps get it through the desktop portal: --portal
serves it there, started by D-Bus when xdg-desktop-portal asks for it.
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut req = Request::default();
    let mut plain = true;
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => return print!("{HELP}"),
        Some("--portal") => {
            if let Err(e) = portal::serve() {
                tracing::error!("{e}");
                std::process::exit(1);
            }
            return;
        }
        Some("--pick") => {
            let mut text = String::new();
            let _ = std::io::stdin().read_to_string(&mut text);
            req = Request::decode(&text);
            plain = false;
        }
        None | Some("--open") => {}
        Some("--open-many") => req.multiple = true,
        Some("--folder") => req.directory = true,
        Some("--save") => {
            req.mode = Mode::Save;
            req.name = args.get(1).cloned().unwrap_or_default();
        }
        Some(other) => {
            eprintln!("unknown option \"{other}\"\n\n{HELP}");
            std::process::exit(2);
        }
    }
    let title = if req.title.is_empty() {
        match req.mode {
            Mode::Open if req.directory => "Choose a Folder",
            Mode::Open => "Open",
            _ => "Save",
        }
        .to_string()
    } else {
        req.title.clone()
    };
    let viewport = wt_ui::egui::ViewportBuilder::default()
        .with_inner_size([920.0, 600.0])
        .with_min_inner_size([560.0, 380.0]);
    let result = wt_ui::run_viewport("wayland-time-files", &title, viewport, move |cc| {
        app::Picker::new(cc, req, plain)
    });
    if let Err(e) = result {
        eprintln!("wayland-time-files: {e}");
        std::process::exit(2);
    }
    // The window was closed without picking anything.
    std::process::exit(1);
}
