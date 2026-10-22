//! Volume keys: change the default output's volume through WirePlumber's `wpctl`, and show the
//! new level in a small panel near the bottom of the focused monitor, which fades away.
//!
//! `wpctl` runs on a worker thread so a slow PipeWire never stalls the desktop. Presses that
//! arrive while it's busy are added up and applied together. Holding a volume key repeats it
//! (the compositor handles the key itself, so the usual client-side repeat doesn't apply).

use std::{
    cell::RefCell,
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::keyboard::Keycode,
    output::Output,
    reexports::calloop::{
        RegistrationToken, channel,
        timer::{TimeoutAction, Timer},
    },
    utils::{Logical, Point, Transform},
};

use crate::{
    config::{Color, VolumeChange},
    state::State,
    text::{Canvas, Text},
    theme::Theme,
};

const SINK: &str = "@DEFAULT_AUDIO_SINK@";
/// How long the panel stays after the last change, then how long it takes to fade.
const HOLD: Duration = Duration::from_millis(1400);
const FADE: Duration = Duration::from_millis(220);
const FRAME: Duration = Duration::from_millis(16);
/// Space between the panel and the bottom of the monitor.
const BOTTOM_MARGIN: i32 = 72;
/// Fastest a held volume key repeats, whatever the keyboard repeat rate.
const MIN_REPEAT: Duration = Duration::from_millis(50);

/// What the volume is now. `volume` is 1.0 at 100%.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub volume: f32,
    pub muted: bool,
}

#[derive(Default)]
pub struct Volume {
    worker: Option<mpsc::Sender<VolumeChange>>,
    pub osd: Option<Osd>,
    fade_timer: Option<RegistrationToken>,
    /// The held volume key and its repeat timer.
    repeat: Option<(Keycode, RegistrationToken)>,
}

/// The on-screen panel.
pub struct Osd {
    level: Level,
    output: Output,
    /// Top-left corner, global logical coordinates.
    loc: Point<i32, Logical>,
    size: (i32, i32),
    /// When it starts to fade.
    until: Instant,
    /// Last painted image: (scale, level, image).
    cache: RefCell<Option<(i32, Level, MemoryRenderBuffer)>>,
}

impl Osd {
    pub fn on_output(&self, output: &Output) -> bool {
        &self.output == output
    }

    fn alpha(&self, now: Instant) -> f32 {
        let gone = now.saturating_duration_since(self.until).as_secs_f32() / FADE.as_secs_f32();
        // Ease out: quick at first, gentle at the end.
        (1.0 - gone.clamp(0.0, 1.0)).powi(2)
    }
}

impl State {
    /// The `volume` action.
    pub fn change_volume(&mut self, change: VolumeChange) {
        if self.volume.worker.is_none() {
            self.volume.worker = self.start_volume_worker();
        }
        let sent = self.volume.worker.as_ref().is_some_and(|w| w.send(change).is_ok());
        if !sent {
            self.volume.worker = None;
        }
    }

    fn start_volume_worker(&mut self) -> Option<mpsc::Sender<VolumeChange>> {
        let (tx, rx) = mpsc::channel();
        let (level_tx, level_rx) = channel::channel();
        let token = self
            .loop_handle
            .insert_source(level_rx, |event, _, state| {
                if let channel::Event::Msg(level) = event {
                    state.show_volume(level);
                }
            })
            .map_err(|e| tracing::warn!("volume channel: {e}"))
            .ok()?;
        let spawned = std::thread::Builder::new()
            .name("volume".into())
            .spawn(move || worker(rx, level_tx));
        if let Err(e) = spawned {
            tracing::warn!("volume thread: {e}");
            self.loop_handle.remove(token);
            return None;
        }
        Some(tx)
    }

    /// Show `level` on the focused monitor, or update the panel already showing.
    fn show_volume(&mut self, level: Level) {
        if self.is_locked() {
            return;
        }
        self.load_ui_text();
        let Some(output) = self.output() else { return };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        let (w, h) = panel_size(&self.config.theme);
        let loc = Point::from((
            geo.loc.x + (geo.size.w - w) / 2,
            geo.loc.y + geo.size.h - h - BOTTOM_MARGIN.min(geo.size.h / 8),
        ));
        let cache = match self.volume.osd.take() {
            // Same place: keep the painted image, it's still right if the level is.
            Some(old) if old.output == output && old.loc == loc => old.cache,
            _ => RefCell::new(None),
        };
        self.volume.osd = Some(Osd {
            level,
            output,
            loc,
            size: (w, h),
            until: Instant::now() + HOLD,
            cache,
        });
        self.needs_redraw = true;
        if self.volume.fade_timer.is_none() {
            let timer = Timer::from_duration(HOLD);
            match self
                .loop_handle
                .insert_source(timer, |_, _, state| state.volume_osd_tick())
            {
                Ok(token) => self.volume.fade_timer = Some(token),
                Err(e) => tracing::warn!("volume panel timer: {e}"),
            }
        }
    }

    /// Wait until the panel's time is up, then fade it out a frame at a time.
    fn volume_osd_tick(&mut self) -> TimeoutAction {
        let now = Instant::now();
        let Some(osd) = &self.volume.osd else {
            self.volume.fade_timer = None;
            return TimeoutAction::Drop;
        };
        if now < osd.until {
            return TimeoutAction::ToInstant(osd.until);
        }
        self.needs_redraw = true;
        if now >= osd.until + FADE {
            self.volume.osd = None;
            self.volume.fade_timer = None;
            return TimeoutAction::Drop;
        }
        TimeoutAction::ToDuration(FRAME)
    }

    pub fn hide_volume_osd(&mut self) {
        if self.volume.osd.take().is_some() {
            self.needs_redraw = true;
        }
    }

    /// The panel's image and opacity for `output`, if it's showing there.
    pub fn volume_osd_buffer(
        &self,
        output: &Output,
        scale: i32,
    ) -> Option<(Point<i32, Logical>, MemoryRenderBuffer, f32)> {
        let osd = self.volume.osd.as_ref().filter(|o| o.on_output(output))?;
        let (_, text) = self.ui_text.as_ref()?;
        let scale = scale.max(1);
        let alpha = osd.alpha(Instant::now());
        let mut cache = osd.cache.borrow_mut();
        if let Some((sc, level, buffer)) = &*cache
            && *sc == scale
            && *level == osd.level
        {
            return Some((osd.loc, buffer.clone(), alpha));
        }
        let canvas = paint(osd.level, osd.size, text, &self.config.theme, scale);
        let buffer = MemoryRenderBuffer::from_slice(
            &canvas.to_premultiplied_rgba(),
            Fourcc::Abgr8888,
            (canvas.w, canvas.h),
            scale,
            Transform::Normal,
            None,
        );
        *cache = Some((scale, osd.level, buffer.clone()));
        Some((osd.loc, buffer, alpha))
    }

    // ---------- holding a volume key ----------

    /// A volume key was pressed: after the keyboard's repeat delay, keep changing the volume
    /// until it's let go.
    pub fn start_volume_repeat(&mut self, code: Keycode, change: VolumeChange) {
        self.stop_volume_repeat();
        let kb = &self.config.keyboard;
        let delay = Duration::from_millis(kb.repeat_delay.max(0) as u64);
        let every = Duration::from_secs_f64(1.0 / f64::from(kb.repeat_rate.max(1))).max(MIN_REPEAT);
        let result = self
            .loop_handle
            .insert_source(Timer::from_duration(delay), move |_, _, state| {
                // Also stops if the release never arrived (say, a VT switch while held).
                if state.is_locked() || !state.keyboard().pressed_keys().contains(&code) {
                    state.volume.repeat = None;
                    return TimeoutAction::Drop;
                }
                state.change_volume(change);
                TimeoutAction::ToDuration(every)
            });
        match result {
            Ok(token) => self.volume.repeat = Some((code, token)),
            Err(e) => tracing::warn!("volume repeat timer: {e}"),
        }
    }

    /// Stop repeating: on the held key's release, or `None` for any key.
    pub fn stop_volume_repeat_for(&mut self, released: Option<Keycode>) {
        if let Some((code, _)) = self.volume.repeat
            && released.is_none_or(|r| r == code)
        {
            self.stop_volume_repeat();
        }
    }

    fn stop_volume_repeat(&mut self) {
        if let Some((_, token)) = self.volume.repeat.take() {
            self.loop_handle.remove(token);
        }
    }
}

// ---------- talking to WirePlumber ----------

fn worker(rx: mpsc::Receiver<VolumeChange>, levels: channel::Sender<Level>) {
    let mut warned = false;
    while let Ok(first) = rx.recv() {
        let mut step = 0;
        let mut toggles = 0;
        for change in std::iter::once(first).chain(rx.try_iter()) {
            match change {
                VolumeChange::Step(n) => step += n,
                VolumeChange::ToggleMute => toggles += 1,
            }
        }
        if toggles % 2 == 1 {
            wpctl(&["set-mute", SINK, "toggle"], &mut warned);
        }
        if step != 0 {
            let sign = if step > 0 { '+' } else { '-' };
            let amount = format!("{}%{sign}", step.unsigned_abs());
            // Never past 100%: louder than that distorts.
            wpctl(&["set-volume", "-l", "1.0", SINK, &amount], &mut warned);
            // Turning it up means you want to hear it.
            if step > 0 {
                wpctl(&["set-mute", SINK, "0"], &mut warned);
            }
        }
        let level = wpctl(&["get-volume", SINK], &mut warned).and_then(|out| parse_level(&out));
        if let Some(level) = level
            && levels.send(level).is_err()
        {
            return;
        }
    }
}

/// Run `wpctl` and return what it printed, or None (logged once) if it failed.
fn wpctl(args: &[&str], warned: &mut bool) -> Option<String> {
    let mut warn = |msg: String| {
        if !std::mem::replace(warned, true) {
            tracing::warn!("{msg}");
        }
    };
    match Command::new("wpctl").args(args).output() {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            warn(format!(
                "wpctl {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            None
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            warn("volume keys need wpctl, which comes with WirePlumber".into());
            None
        }
        Err(e) => {
            warn(format!("running wpctl: {e}"));
            None
        }
    }
}

/// Read `wpctl get-volume` output: "Volume: 0.45" or "Volume: 0.45 [MUTED]".
fn parse_level(out: &str) -> Option<Level> {
    let rest = out.trim().strip_prefix("Volume:")?;
    let mut words = rest.split_whitespace();
    let volume = words.next()?.parse().ok()?;
    Some(Level {
        volume,
        muted: words.any(|w| w == "[MUTED]"),
    })
}

// ---------- drawing ----------

/// The panel's size in logical pixels.
fn panel_size(theme: &Theme) -> (i32, i32) {
    let h = (theme.font_size * 3.0).round().max(40.0) as i32;
    let w = (theme.font_size * 19.0).round().clamp(240.0, 360.0) as i32;
    (w, h)
}

fn paint(level: Level, (w, h): (i32, i32), text: &Text, theme: &Theme, sc: i32) -> Canvas {
    let p = &theme.palette;
    let s = sc as f32;
    let (w, h) = (w * sc, h * sc);
    let radius = theme.radius * s;
    let mut canvas = Canvas::new(w, h, [0.0; 4]);
    canvas.fill_rounded_rect(0, 0, w, h, radius + s, p.separator);
    canvas.fill_rounded_rect(sc, sc, w - 2 * sc, h - 2 * sc, radius, p.surface);

    let px = theme.font_size * s;
    let icon_px = (px * 1.75).round() as i32;
    let inset = (h - icon_px) / 2;
    let gap = 12 * sc;
    let (fg, fill) = if level.muted {
        (p.subtext, p.subtext)
    } else {
        (p.text, p.accent)
    };

    // Speaker.
    let waves = if level.muted || level.volume <= 0.0 {
        0
    } else if level.volume < 0.5 {
        1
    } else {
        2
    };
    if let Some(icon) = speaker_icon(waves, level.muted, fg, icon_px as u32) {
        canvas.draw_image(inset, inset, icon_px as u32, &icon);
    }

    // Percentage on the right, in a fixed width so the bar doesn't jump around.
    let label = format!("{}%", (level.volume * 100.0).round() as i32);
    let right = w - inset;
    let label_w = text.width(&label, px).ceil() as i32;
    let slot = text.width("100%", px).ceil() as i32;
    let baseline = text.centered_baseline(0.0, h as f32, px);
    let color = if level.muted { p.subtext } else { p.text };
    text.draw(
        &mut canvas,
        &label,
        px,
        ((right - label_w) as f32, baseline),
        color,
        (right - slot, right),
    );

    // The level bar.
    let x0 = inset + icon_px + gap;
    let x1 = right - slot - gap;
    let track_h = (6 * sc).max(2);
    let ty = (h - track_h) / 2;
    let track_r = radius.min(track_h as f32 / 2.0);
    if x1 > x0 {
        canvas.fill_rounded_rect(x0, ty, x1 - x0, track_h, track_r, p.hover);
        let filled = ((x1 - x0) as f32 * level.volume.clamp(0.0, 1.0)).round() as i32;
        if filled > 0 {
            canvas.fill_rounded_rect(x0, ty, filled.max(track_h), track_h, track_r, fill);
        }
    }
    canvas
}

/// A speaker with 0-2 sound waves, or a cross when muted, as premultiplied RGBA.
fn speaker_icon(waves: u32, muted: bool, color: Color, px: u32) -> Option<Vec<u8>> {
    use resvg::{tiny_skia, usvg};
    let hex = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    let rgb = format!("#{:02x}{:02x}{:02x}", hex(color[0]), hex(color[1]), hex(color[2]));
    let mut marks = String::new();
    if muted {
        marks.push_str("M15.5 9l6 6M21.5 9l-6 6");
    } else {
        if waves >= 1 {
            marks.push_str("M15.5 9.5a3.5 3.5 0 0 1 0 5");
        }
        if waves >= 2 {
            marks.push_str("M18.5 6.5a7.5 7.5 0 0 1 0 11");
        }
    }
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" opacity="{a}">
<path d="M3.5 9.5h3l4.5-4v13l-4.5-4h-3z" fill="{rgb}" stroke="{rgb}" stroke-width="1.5" stroke-linejoin="round"/>
<path d="{marks}" fill="none" stroke="{rgb}" stroke-width="2" stroke-linecap="round"/>
</svg>"#,
        a = color[3],
    );
    let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let scale = px as f32 / 24.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(pixmap.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wpctl_output() {
        assert_eq!(
            parse_level("Volume: 0.45\n"),
            Some(Level {
                volume: 0.45,
                muted: false
            })
        );
        assert_eq!(
            parse_level("Volume: 0.00 [MUTED]\n"),
            Some(Level {
                volume: 0.0,
                muted: true
            })
        );
        assert_eq!(parse_level("Error"), None);
    }

    /// `VOLUME_PREVIEW=/path/out.png cargo test volume_preview` draws the panel at a few levels.
    #[test]
    fn volume_preview() {
        let Some(path) = std::env::var_os("VOLUME_PREVIEW") else {
            return;
        };
        let text = Text::load("sans").expect("font");
        let config = crate::config::Config::default();
        let sc = 2;
        let size = panel_size(&config.theme);
        let levels = [(0.0, false), (0.3, false), (0.75, false), (1.0, false), (0.5, true)];
        let panels: Vec<Canvas> = levels
            .iter()
            .map(|&(volume, muted)| paint(Level { volume, muted }, size, &text, &config.theme, sc))
            .collect();
        let gap = 12 * sc;
        let (w, h) = (panels[0].w + 2 * gap, (panels[0].h + gap) * panels.len() as i32 + gap);
        let mut sheet = Canvas::new(w, h, config.theme.palette.base);
        let mut out = Vec::new();
        for (i, panel) in panels.iter().enumerate() {
            let y = gap + i as i32 * (panel.h + gap);
            // Draw it as a square image would be drawn, a row at a time.
            let rgba = panel.to_premultiplied_rgba();
            for (row, line) in rgba.chunks_exact(panel.w as usize * 4).enumerate() {
                for (col, px) in line.chunks_exact(4).enumerate() {
                    let a = px[3] as f32 / 255.0;
                    if a > 0.0 {
                        let c = [px[0], px[1], px[2]].map(|v| v as f32 / 255.0 / a);
                        sheet.blend(gap + col as i32, y + row as i32, [c[0], c[1], c[2], 1.0], a);
                    }
                }
            }
        }
        out.extend(sheet.to_premultiplied_rgba());
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(file, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&out).unwrap();
    }
}
