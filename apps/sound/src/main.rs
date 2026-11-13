//! kiln-sound: choose where sound plays and what records it, set their volumes and
//! each app's, in the theme's colors and font. Talks to PipeWire through its own tools.

mod app;

const HELP: &str = "\
kiln-sound — Sound for the kiln compositor

USAGE:
    kiln-sound

Pick the output (speakers, headphones, HDMI) and input (microphone), set their volumes,
and set or mute each app's volume, or send an app to another output. Needs PipeWire with
WirePlumber (pw-dump, wpctl and pw-metadata).
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
        .with_inner_size([520.0, 640.0])
        .with_min_inner_size([400.0, 420.0]);
    if let Err(e) = kiln_ui::run_viewport("kiln-sound", "Sound", viewport, app::SoundApp::new) {
        eprintln!("kiln-sound: {e}");
        std::process::exit(1);
    }
}
