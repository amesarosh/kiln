//! kiln-bluetooth: turn Bluetooth on and off, and pair, connect and forget devices,
//! in the theme's colors and font. Talks to BlueZ on the system bus.

mod app;
mod bluez;

const HELP: &str = "\
kiln-bluetooth — Bluetooth for the kiln compositor

USAGE:
    kiln-bluetooth

Turn Bluetooth on and off, connect your devices, and search for new ones to pair. While
it's open it also answers pairing questions (confirm a code, type a PIN).
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        return print!("{HELP}");
    }
    let viewport = kiln_ui::egui::ViewportBuilder::default()
        .with_inner_size([480.0, 600.0])
        .with_min_inner_size([380.0, 420.0]);
    if let Err(e) = kiln_ui::run_viewport("kiln-bluetooth", "Bluetooth", viewport, app::BluetoothApp::new) {
        eprintln!("kiln-bluetooth: {e}");
        std::process::exit(1);
    }
}
