//! kiln-image-viewer: look at a picture, and the others in its folder, in the theme's
//! colors and font.

mod app;
mod load;

const HELP: &str = "\
kiln-image-viewer — Image viewer for the kiln compositor

USAGE:
    kiln-image-viewer [IMAGE or FOLDER]

Shows the image (or the first one in the folder); the others in its folder are a key away.

KEYS:
    Left / Right, Space      previous / next image (also the mouse's back and forward)
    Home / End               first / last image
    scroll, + / -            zoom in and out (around the pointer)
    0 / 1                    fit to the window / actual size (or double-click)
    R / Shift+R              turn right / left
    F or F11                 full screen (Escape leaves it)
    Delete                   move to the trash (with Undo)

Opens PNG, JPEG, WebP, GIF (animated too), BMP, TIFF, ICO, TGA, QOI, PNM and SVG.
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let arg = std::env::args_os().nth(1);
    if arg.as_ref().is_some_and(|a| a == "-h" || a == "--help") {
        return print!("{HELP}");
    }
    let path = arg.map(|a| {
        let s = a.to_string_lossy();
        match s.strip_prefix("file://") {
            Some(_) => kiln_files::fs::uri_to_path(&s).unwrap_or_else(|| a.into()),
            None => std::path::PathBuf::from(a),
        }
    });
    let path = path.map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    let viewport = kiln_ui::egui::ViewportBuilder::default()
        .with_inner_size([1000.0, 720.0])
        .with_min_inner_size([360.0, 260.0]);
    if let Err(e) = kiln_ui::run_viewport("kiln-image-viewer", "Image Viewer", viewport, |cc| {
        app::Viewer::new(cc, path)
    }) {
        eprintln!("kiln-image-viewer: {e}");
        std::process::exit(1);
    }
}
