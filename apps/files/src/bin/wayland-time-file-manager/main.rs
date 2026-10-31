//! wayland-time-file-manager: browse, open, copy, move, rename and trash files, in the theme's
//! colors and font. Built on the same listing, places and icons as the file picker.

mod app;
mod drives;
mod open;
mod ops;
mod trash;

use std::path::PathBuf;

use app::Loc;
use wayland_time_files::fs;

const HELP: &str = "\
wayland-time-file-manager — file manager for the wayland-time compositor

USAGE:
    wayland-time-file-manager [FOLDER | FILE]

Opens FOLDER (a path or a file:// URI), or the folder holding FILE with it selected, or
your home folder.
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let arg = std::env::args().nth(1);
    if matches!(arg.as_deref(), Some("-h" | "--help")) {
        return print!("{HELP}");
    }
    let (start, select) = match arg.map(|a| {
        if a.starts_with("file://") {
            fs::uri_to_path(&a).unwrap_or_default()
        } else {
            std::path::absolute(fs::expand(&a)).unwrap_or_default()
        }
    }) {
        Some(p) if p.is_dir() => (Loc::Dir(p), None),
        Some(p) if p.exists() => match p.parent() {
            Some(parent) => (Loc::Dir(parent.to_path_buf()), Some(p)),
            None => (Loc::Dir(fs::home()), None),
        },
        Some(p) => {
            eprintln!("wayland-time-file-manager: {} doesn't exist", p.display());
            (Loc::Dir(fs::home()), None::<PathBuf>)
        }
        None => (Loc::Dir(fs::home()), None),
    };
    let viewport = wt_ui::egui::ViewportBuilder::default()
        .with_inner_size([1000.0, 640.0])
        .with_min_inner_size([560.0, 380.0]);
    let result = wt_ui::run_viewport("wayland-time-file-manager", "Files", viewport, move |cc| {
        app::Manager::new(cc, start, select)
    });
    if let Err(e) = result {
        eprintln!("wayland-time-file-manager: {e}");
        std::process::exit(1);
    }
}
