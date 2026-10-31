//! The wallpaper from `[theme] wallpaper` (or a monitor's own), drawn under everything else.
//!
//! Decoding and scaling a photo takes a moment, so it happens on a worker thread; until the
//! picture is ready the monitor shows the `base` color, or the picture it had before. Each
//! monitor gets a picture made at its exact pixel size, so drawing it is a plain copy, and it
//! is only remade when the image, the fit, the monitor's size or the `base` color changes.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

use image::{RgbaImage, imageops};
use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    output::Output,
    reexports::calloop::{LoopHandle, channel},
    utils::{Rectangle, Transform},
};

use crate::{
    config::{Color, WallpaperFit},
    state::State,
};

/// What one monitor's picture is made from.
#[derive(Debug, Clone, PartialEq)]
pub struct Want {
    pub path: PathBuf,
    pub fit: WallpaperFit,
    /// Pixels, the way up the monitor is used (a rotated monitor's sides swapped).
    pub size: (i32, i32),
    /// Shown around a picture that doesn't cover the monitor.
    pub base: Color,
}

impl Want {
    /// The picture `output` should have now, if any.
    pub fn for_output(state: &State, output: &Output) -> Option<Self> {
        let path = state.config.wallpaper.for_output(&output.name())?;
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);
        let path = Path::new(path);
        // A relative path is taken from the config file's folder.
        let path = match state.config_path.as_deref().and_then(Path::parent) {
            Some(dir) if path.is_relative() => dir.join(path),
            _ => path.to_path_buf(),
        };
        Some(Self {
            path,
            fit: state.config.wallpaper.fit,
            size: (size.w, size.h),
            base: state.config.theme.palette.base,
        })
    }
}

struct Job {
    output: String,
    want: Want,
}

struct Done {
    output: String,
    want: Want,
    /// Premultiplied RGBA, and whether every pixel is opaque.
    picture: Result<(Vec<u8>, bool), String>,
}

struct Shown {
    output: String,
    /// The picture last asked for.
    asked: Want,
    /// The picture on screen, and what it was made from.
    buffer: Option<(Want, MemoryRenderBuffer)>,
}

#[derive(Default)]
pub struct Wallpapers {
    inner: RefCell<Inner>,
}

#[derive(Default)]
struct Inner {
    shown: Vec<Shown>,
    worker: Option<mpsc::Sender<Job>>,
}

impl Wallpapers {
    /// The picture to draw on `output` this frame, asking for a new one if `want` changed.
    /// The one shown may be an older picture while the new one is being made.
    pub fn buffer(
        &self,
        handle: &LoopHandle<'static, State>,
        output: &str,
        want: Option<Want>,
    ) -> Option<MemoryRenderBuffer> {
        let mut inner = self.inner.borrow_mut();
        let Some(want) = want else {
            inner.shown.retain(|s| s.output != output);
            return None;
        };
        let at = match inner.shown.iter().position(|s| s.output == output) {
            Some(i) => i,
            None => {
                inner.shown.push(Shown {
                    output: output.to_string(),
                    asked: want.clone(),
                    buffer: None,
                });
                inner.send(handle, output, &want);
                inner.shown.len() - 1
            }
        };
        if inner.shown[at].asked != want {
            inner.shown[at].asked = want.clone();
            inner.send(handle, output, &want);
        }
        // An old picture of another size is stretched to fit until the new one is ready.
        inner.shown[at].buffer.as_ref().map(|(_, b)| b.clone())
    }

    /// Forget monitors that are gone, freeing their pictures.
    pub fn retain_outputs(&self, names: &[String]) {
        let mut inner = self.inner.borrow_mut();
        if inner.shown.iter().any(|s| !names.contains(&s.output)) {
            inner.shown.retain(|s| names.contains(&s.output));
        }
    }

    /// A picture is ready. False if nobody wants it any more.
    fn finished(&self, done: Done) -> bool {
        let mut inner = self.inner.borrow_mut();
        let Some(shown) = inner
            .shown
            .iter_mut()
            .find(|s| s.output == done.output && s.asked == done.want)
        else {
            return false;
        };
        match done.picture {
            Ok((pixels, opaque)) => {
                let (w, h) = done.want.size;
                let opaque = opaque.then(|| vec![Rectangle::from_size((w, h).into())]);
                let buffer =
                    MemoryRenderBuffer::from_slice(&pixels, Fourcc::Abgr8888, (w, h), 1, Transform::Normal, opaque);
                shown.buffer = Some((done.want, buffer));
            }
            Err(e) => {
                tracing::warn!("wallpaper {}: {e}", done.want.path.display());
                shown.buffer = None;
            }
        }
        true
    }
}

impl Inner {
    fn send(&mut self, handle: &LoopHandle<'static, State>, output: &str, want: &Want) {
        let job = || Job {
            output: output.to_string(),
            want: want.clone(),
        };
        if let Some(w) = &self.worker
            && w.send(job()).is_ok()
        {
            return;
        }
        self.worker = start_worker(handle);
        if let Some(w) = &self.worker {
            let _ = w.send(job());
        }
    }
}

fn start_worker(handle: &LoopHandle<'static, State>) -> Option<mpsc::Sender<Job>> {
    let (jobs, rx) = mpsc::channel::<Job>();
    let (done_tx, done_rx) = channel::channel::<Done>();
    let token = handle
        .insert_source(done_rx, |event, _, state| {
            if let channel::Event::Msg(done) = event
                && state.wallpapers.finished(done)
            {
                state.needs_redraw = true;
            }
        })
        .map_err(|e| tracing::warn!("wallpaper channel: {e}"))
        .ok()?;
    let spawned = std::thread::Builder::new()
        .name("wallpaper".into())
        .spawn(move || worker(rx, done_tx));
    if let Err(e) = spawned {
        tracing::warn!("wallpaper thread: {e}");
        handle.remove(token);
        return None;
    }
    Some(jobs)
}

fn worker(jobs: mpsc::Receiver<Job>, done: channel::Sender<Done>) {
    // The last image decoded, so several monitors with the same wallpaper decode it once.
    let mut cache: Option<(PathBuf, Option<std::time::SystemTime>, Arc<RgbaImage>)> = None;
    while let Ok(first) = jobs.recv() {
        // Only the newest job per monitor matters; older ones were replaced while waiting.
        let mut batch = vec![first];
        while let Ok(job) = jobs.try_recv() {
            batch.retain(|j| j.output != job.output);
            batch.push(job);
        }
        for job in batch {
            let modified = std::fs::metadata(&job.want.path).and_then(|m| m.modified()).ok();
            let image = match &cache {
                Some((p, m, img)) if *p == job.want.path && *m == modified => Ok(img.clone()),
                _ => decode(&job.want.path).map(Arc::new),
            };
            let picture = image.map(|img| {
                cache = Some((job.want.path.clone(), modified, img.clone()));
                compose(&img, job.want.fit, job.want.size, job.want.base)
            });
            let sent = done.send(Done {
                output: job.output,
                want: job.want,
                picture,
            });
            if sent.is_err() {
                return;
            }
        }
        // A photo takes tens of megabytes; don't hold on to it between changes.
        cache = None;
    }
}

fn decode(path: &Path) -> Result<RgbaImage, String> {
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    // Big enough for any real photo, small enough that a broken file can't take all memory.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    Ok(img.into_rgba8())
}

const CLEAR: image::Rgba<u8> = image::Rgba([0, 0, 0, 0]);

/// The picture for a `size` monitor: `img` fitted onto a `base`-colored background.
/// Premultiplied RGBA, and whether it's fully opaque.
fn compose(img: &RgbaImage, fit: WallpaperFit, size: (i32, i32), base: Color) -> (Vec<u8>, bool) {
    let (w, h) = (size.0.max(1) as u32, size.1.max(1) as u32);
    let (iw, ih) = img.dimensions();
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let base = image::Rgba([to_u8(base[0]), to_u8(base[1]), to_u8(base[2]), to_u8(base[3])]);
    let resize = |img: &RgbaImage, w: u32, h: u32| {
        if img.dimensions() == (w, h) {
            img.clone()
        } else {
            imageops::resize(img, w.max(1), h.max(1), imageops::FilterType::CatmullRom)
        }
    };

    let out = match fit {
        WallpaperFit::Stretch => resize(img, w, h),
        WallpaperFit::Fill => {
            // Crop the source to the monitor's shape, then scale: no pixels wasted on scaling
            // parts that are cut off.
            let scale = (w as f64 / iw as f64).max(h as f64 / ih as f64);
            let cw = ((w as f64 / scale).round() as u32).clamp(1, iw);
            let ch = ((h as f64 / scale).round() as u32).clamp(1, ih);
            let cropped = imageops::crop_imm(img, (iw - cw) / 2, (ih - ch) / 2, cw, ch).to_image();
            resize(&cropped, w, h)
        }
        WallpaperFit::Fit => {
            let scale = (w as f64 / iw as f64).min(h as f64 / ih as f64);
            let sw = ((iw as f64 * scale).round() as u32).clamp(1, w);
            let sh = ((ih as f64 * scale).round() as u32).clamp(1, h);
            let mut out = RgbaImage::from_pixel(w, h, CLEAR);
            imageops::overlay(
                &mut out,
                &resize(img, sw, sh),
                ((w - sw) / 2).into(),
                ((h - sh) / 2).into(),
            );
            out
        }
        WallpaperFit::Center => {
            let mut out = RgbaImage::from_pixel(w, h, CLEAR);
            let x = (w as i64 - iw as i64) / 2;
            let y = (h as i64 - ih as i64) / 2;
            imageops::overlay(&mut out, img, x, y);
            out
        }
        WallpaperFit::Tile => {
            let mut out = RgbaImage::from_pixel(w, h, CLEAR);
            for y in (0..h).step_by(ih.max(1) as usize) {
                for x in (0..w).step_by(iw.max(1) as usize) {
                    imageops::overlay(&mut out, img, x.into(), y.into());
                }
            }
            out
        }
    };

    // Laid over `base`: what the picture doesn't cover, and see-through parts of it.
    let mut pixels = out.into_raw();
    let mut opaque = true;
    for px in pixels.chunks_exact_mut(4) {
        if px[3] < 255 {
            let a = px[3] as u32;
            let under = base.0[3] as u32 * (255 - a) / 255;
            let out_a = a + under;
            // The picture's color over `base`'s, both made premultiplied.
            for (c, b) in px[..3].iter_mut().zip(base.0) {
                *c = ((*c as u32 * a + b as u32 * under) / 255) as u8;
            }
            px[3] = out_a.min(255) as u8;
            opaque &= px[3] == 255;
        }
    }
    (pixels, opaque)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Color = [0.0, 0.0, 1.0, 1.0];

    fn red(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]))
    }

    fn px(pixels: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
        let at = ((y * w + x) * 4) as usize;
        pixels[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn fill_covers_and_is_opaque() {
        let (p, opaque) = compose(&red(40, 10), WallpaperFit::Fill, (20, 20), BASE);
        assert!(opaque);
        assert_eq!(p.len(), 20 * 20 * 4);
        assert_eq!(px(&p, 20, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&p, 20, 19, 19), [255, 0, 0, 255]);
    }

    #[test]
    fn fit_letterboxes_with_base() {
        let (p, opaque) = compose(&red(40, 10), WallpaperFit::Fit, (20, 20), BASE);
        assert!(opaque);
        assert_eq!(px(&p, 20, 10, 0), [0, 0, 255, 255], "above the picture");
        assert_eq!(px(&p, 20, 10, 10), [255, 0, 0, 255], "the picture");
    }

    #[test]
    fn center_and_tile_keep_size() {
        let (p, _) = compose(&red(4, 4), WallpaperFit::Center, (10, 10), BASE);
        assert_eq!(px(&p, 10, 0, 0), [0, 0, 255, 255]);
        assert_eq!(px(&p, 10, 5, 5), [255, 0, 0, 255]);
        let mut img = red(4, 4);
        img.put_pixel(0, 0, image::Rgba([0, 255, 0, 255]));
        let (p, _) = compose(&img, WallpaperFit::Tile, (10, 10), BASE);
        assert_eq!(px(&p, 10, 4, 8), [0, 255, 0, 255], "the corner repeats every 4 pixels");
    }

    #[test]
    fn transparent_pixels_show_base() {
        let img = RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
        let (p, opaque) = compose(&img, WallpaperFit::Stretch, (2, 2), BASE);
        assert!(opaque);
        assert_eq!(px(&p, 2, 1, 1), [0, 0, 255, 255]);
        let see_through = [0.0, 0.0, 1.0, 0.5];
        let (_, opaque) = compose(&img, WallpaperFit::Stretch, (2, 2), see_through);
        assert!(!opaque);
    }
}
