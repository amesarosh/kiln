//! Decoding images on a worker thread, newest request first, so flipping through a folder
//! never waits on a big file it has already left.

use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::Duration,
};

use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage,
    codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    imageops::FilterType,
};
use kiln_ui::egui;

/// File name endings we open.
pub const EXTENSIONS: &[&str] = &[
    "png", "apng", "jpg", "jpeg", "jpe", "jfif", "webp", "gif", "bmp", "dib", "tif", "tiff", "ico", "tga", "qoi",
    "pnm", "pbm", "pgm", "ppm", "pam", "svg",
];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Most memory an animation's frames may take; past it, the rest of the frames are dropped.
const MAX_ANIMATION_BYTES: usize = 768 << 20;
/// SVGs are drawn this big (longest side, pixels) at least, so zooming in stays sharp.
const SVG_MIN_SIDE: f32 = 2048.0;

/// A decoded image, ready to become textures.
pub struct Decoded {
    /// One frame for a still image; with how long each shows for an animation.
    pub frames: Vec<(egui::ColorImage, Duration)>,
    /// The image's own size in pixels (the frames may be smaller, if it's huge).
    pub size: [u32; 2],
    /// Some pixel is see-through (so a checkerboard goes behind it).
    pub alpha: bool,
    pub format: &'static str,
    pub bytes: u64,
}

pub type Result = std::result::Result<Decoded, String>;

/// Decodes what it's asked for on its own thread.
pub struct Loader {
    wanted: Arc<(Mutex<Vec<PathBuf>>, Condvar)>,
    pub done: mpsc::Receiver<(PathBuf, Result)>,
}

impl Loader {
    /// `max_side`: the biggest texture the GPU takes; larger images are scaled down to it.
    pub fn new(ctx: egui::Context, max_side: usize) -> Self {
        let wanted: Arc<(Mutex<Vec<PathBuf>>, Condvar)> = Arc::default();
        let (tx, done) = mpsc::channel();
        let queue = wanted.clone();
        let spawned = std::thread::Builder::new().name("decode".into()).spawn(move || {
            loop {
                let path = {
                    let (list, ready) = &*queue;
                    let mut list = list.lock().unwrap();
                    while list.is_empty() {
                        list = ready.wait(list).unwrap();
                    }
                    list.remove(0)
                };
                let result = decode(&path, max_side as u32);
                if tx.send((path, result)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("decode thread: {e}");
        }
        Self { wanted, done }
    }

    /// Decode these, in this order, instead of whatever was asked for before.
    pub fn want(&self, paths: Vec<PathBuf>) {
        let (list, ready) = &*self.wanted;
        *list.lock().unwrap() = paths;
        ready.notify_one();
    }
}

fn decode(path: &Path, max_side: u32) -> Result {
    let bytes = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "svg" {
        return svg(path, max_side, bytes);
    }
    let mut reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    reader.limits(Limits::no_limits());
    let format = reader.format().ok_or("not an image this viewer knows")?;
    let name = format_name(format);
    let open = || File::open(path).map(BufReader::new).map_err(|e| e.to_string());

    // Animations.
    let frames = match format {
        ImageFormat::Gif => Some(GifDecoder::new(open()?).map(|d| d.into_frames())),
        ImageFormat::WebP => match WebPDecoder::new(open()?) {
            Ok(d) if d.has_animation() => Some(Ok(d.into_frames())),
            _ => None,
        },
        ImageFormat::Png => match PngDecoder::new(open()?) {
            Ok(d) if d.is_apng().unwrap_or(false) => Some(d.apng().map(|a| a.into_frames())),
            _ => None,
        },
        _ => None,
    };
    if let Some(frames) = frames {
        let frames = frames.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let (mut size, mut alpha, mut total) = ([0, 0], false, 0usize);
        for frame in frames {
            let frame = frame.map_err(|e| e.to_string())?;
            let (n, d) = frame.delay().numer_denom_ms();
            let ms = if d == 0 { 0 } else { n / d };
            // Like browsers: "as fast as possible" means a tenth of a second.
            let delay = Duration::from_millis(if ms <= 10 { 100 } else { ms as u64 });
            let buffer = frame.into_buffer();
            size = [buffer.width(), buffer.height()];
            alpha |= buffer.pixels().any(|p| p.0[3] < 255);
            let image = fit(buffer, max_side);
            total += image.len();
            out.push((color_image(&image), delay));
            if total > MAX_ANIMATION_BYTES {
                tracing::warn!(
                    "{}: animation too big, showing its first {} frames",
                    path.display(),
                    out.len()
                );
                break;
            }
        }
        if !out.is_empty() {
            return Ok(Decoded {
                frames: out,
                size,
                alpha,
                format: name,
                bytes,
            });
        }
    }

    // A still image, turned the way the camera says.
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(o) = orientation {
        image.apply_orientation(o);
    }
    let alpha = image.color().has_alpha();
    let image = image.into_rgba8();
    let alpha = alpha && image.pixels().any(|p| p.0[3] < 255);
    let size = [image.width(), image.height()];
    let image = fit(image, max_side);
    Ok(Decoded {
        frames: vec![(color_image(&image), Duration::ZERO)],
        size,
        alpha,
        format: name,
        bytes,
    })
}

fn svg(path: &Path, max_side: u32, bytes: u64) -> Result {
    use resvg::{tiny_skia, usvg};
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let (w, h) = (tree.size().width(), tree.size().height());
    let longest = w.max(h).max(1.0);
    let scale = longest.max(SVG_MIN_SIDE).min(max_side as f32) / longest;
    let (pw, ph) = (((w * scale).ceil() as u32).max(1), ((h * scale).ceil() as u32).max(1));
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph).ok_or("SVG too big to draw")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let image = egui::ColorImage::from_rgba_premultiplied([pw as usize, ph as usize], pixmap.data());
    Ok(Decoded {
        frames: vec![(image, Duration::ZERO)],
        size: [w.round().max(1.0) as u32, h.round().max(1.0) as u32],
        alpha: true,
        format: "SVG",
        bytes,
    })
}

/// Scaled down to fit in a `max_side` square, if it doesn't.
fn fit(image: RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = image.dimensions();
    if w <= max_side && h <= max_side {
        return image;
    }
    let s = max_side as f32 / w.max(h) as f32;
    let (nw, nh) = (((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1));
    image::imageops::resize(&image, nw, nh, FilterType::Triangle)
}

fn color_image(image: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw())
}

fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::WebP => "WebP",
        ImageFormat::Gif => "GIF",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Ico => "ICO",
        ImageFormat::Tga => "TGA",
        ImageFormat::Qoi => "QOI",
        ImageFormat::Pnm => "PNM",
        _ => "Image",
    }
}
