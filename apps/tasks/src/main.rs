//! kiln-tasks: see what's running and how busy the machine is, and stop processes.

mod app;
mod graph;
mod sample;

const HELP: &str = "\
kiln-tasks — task manager for the kiln compositor

USAGE:
    kiln-tasks [-p PAGE]

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

    kiln_ui::run("kiln-tasks", "Task Manager", [960.0, 680.0], move |cc| {
        app::TasksApp::new(cc, page)
    })?;
    Ok(())
}
