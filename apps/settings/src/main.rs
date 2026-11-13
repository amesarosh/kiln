//! kiln-settings: edit the compositor's config with changes applied as you make them.

mod app;
mod doc;
mod monitors;
mod pages;
mod widgets;

use std::path::PathBuf;

const HELP: &str = "\
kiln-settings — settings for the kiln compositor

USAGE:
    kiln-settings [-c PATH]

OPTIONS:
    -c, --config <PATH>   Edit this config file instead of ~/.config/kiln/config.toml
    -p, --page <NAME>     Open at a page: general, appearance, tiling, bar, notifications,
                          monitors, keyboard, mouse, keybinds, rules or session
    -h, --help            Show this help
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    let mut path: Option<PathBuf> = None;
    let mut page = pages::Page::General;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => path = Some(args.next().ok_or("--config needs a path")?.into()),
            "-p" | "--page" => {
                let name = args.next().ok_or("--page needs a name")?;
                page = pages::Page::from_name(&name).ok_or(format!("no page called \"{name}\" (see --help)"))?;
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            other => return Err(format!("unknown argument \"{other}\" (see --help)").into()),
        }
    }
    let path = path
        .or_else(kiln_config::Config::default_path)
        .ok_or("no config path: set HOME or use --config")?;

    kiln_ui::run("kiln-settings", "Settings", [980.0, 720.0], move |cc| {
        app::SettingsApp::new(cc, path, page)
    })?;
    Ok(())
}
