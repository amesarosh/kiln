//! wayland-time-tasks: see what's running and how busy the machine is, and stop processes.

mod app;
mod graph;
mod sample;

const HELP: &str = "\
wayland-time-tasks — task manager for the wayland-time compositor

USAGE:
    wayland-time-tasks [-p PAGE]

OPTIONS:
    -p, --page <NAME>     Open at a page: processes or performance
    -h, --help            Show this help
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    let mut page = app::Page::Processes;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-p" | "--page" => {
                let name = args.next().ok_or("--page needs a name")?;
                page = app::Page::from_name(&name).ok_or(format!("no page called \"{name}\" (see --help)"))?;
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            other => return Err(format!("unknown argument \"{other}\" (see --help)").into()),
        }
    }

    wt_ui::run("wayland-time-tasks", "Task Manager", [960.0, 680.0], move |cc| {
        app::TasksApp::new(cc, page)
    })?;
    Ok(())
}
