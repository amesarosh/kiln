//! The shared look and window setup for wayland-time's apps (settings, and more to come).
//!
//! Apps take their font, corner rounding and colors from the compositor's `[theme]`, through the
//! same palette roles the bar and menus use, and follow theme changes as the config is saved:
//!
//! ```ignore
//! wt_ui::run("wayland-time-settings", "Settings", [900.0, 640.0], |cc| MyApp::new(cc))
//! // and in the app's `logic`:
//! self.look.poll(ctx);
//! ```

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

pub use eframe::{self, egui};
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};
use wt_config::{Config, theme::Theme};

/// A config color as egui's color type.
pub fn color(c: wt_config::Color) -> Color32 {
    let [r, g, b, a] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Open the app's window. `app_id` is what the compositor sees (for window rules and icons).
pub fn run<A: eframe::App + 'static>(
    app_id: &str,
    title: &str,
    size: [f32; 2],
    make: impl FnOnce(&eframe::CreationContext<'_>) -> A + 'static,
) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(app_id)
            .with_title(title)
            .with_inner_size(size)
            .with_min_inner_size([480.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(app_id, options, Box::new(|cc| Ok(Box::new(make(cc)))))
}

/// Keeps an app's look in step with the config file's `[theme]`.
pub struct Look {
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
    last_check: Instant,
    /// The theme currently applied.
    pub theme: Theme,
    /// Font name whose file is loaded, so fonts are only reloaded when the name changes.
    font_loaded: Option<(String, f32)>,
}

impl Look {
    /// Read the theme from the config at `path` (the default config when `None`) and apply it.
    pub fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        let mut look = Self {
            path,
            modified: None,
            last_check: Instant::now(),
            theme: Config::default().theme,
            font_loaded: None,
        };
        look.reload(ctx);
        look
    }

    /// Check the config file now and then and re-apply the theme if it changed.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if self.last_check.elapsed() >= Duration::from_millis(500) {
            self.last_check = Instant::now();
            if modified(self.path.as_deref()) != self.modified {
                self.reload(ctx);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// Re-read the config file and apply its theme. A broken config keeps the current look.
    pub fn reload(&mut self, ctx: &egui::Context) {
        self.modified = modified(self.path.as_deref());
        let Some(path) = &self.path else {
            return self.set(ctx, Config::default().theme);
        };
        let config = match std::fs::read_to_string(path) {
            Ok(text) => Config::parse_user(&text),
            Err(_) => Ok(Config::default()),
        };
        if let Ok(config) = config {
            self.set(ctx, config.theme);
        }
    }

    /// Apply `theme` now.
    pub fn set(&mut self, ctx: &egui::Context, theme: Theme) {
        let font_key = (theme.font.clone(), theme.font_size);
        if self.font_loaded.as_ref() != Some(&font_key) {
            ctx.set_fonts(fonts(&theme.font));
            self.font_loaded = Some(font_key);
        }
        apply_style(ctx, &theme);
        self.theme = theme;
    }
}

fn modified(path: Option<&Path>) -> Option<SystemTime> {
    std::fs::metadata(path?).ok()?.modified().ok()
}

/// egui's fonts with the theme font (and the system monospace font) put first.
fn fonts(name: &str) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    for (family, font, key) in [
        (FontFamily::Proportional, name, "theme"),
        (FontFamily::Monospace, "monospace", "theme-mono"),
    ] {
        let Some(data) = wt_config::fonts::candidates(font).find_map(|p| std::fs::read(p).ok()) else {
            tracing::warn!("no font file found for \"{font}\"; using the built-in one");
            continue;
        };
        defs.font_data.insert(key.into(), Arc::new(FontData::from_owned(data)));
        defs.families.entry(family).or_default().insert(0, key.into());
    }
    defs
}

/// Map the palette roles onto egui's style, for both its light and dark modes.
fn apply_style(ctx: &egui::Context, theme: &Theme) {
    let p = &theme.palette;
    let c = |v| color(v);
    let size = theme.font_size;
    let radius = CornerRadius::same(theme.radius.round().clamp(0.0, 255.0) as u8);
    let [r, g, b, _] = p.surface;
    let dark = 0.2126 * r + 0.7152 * g + 0.0722 * b < 0.5;

    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(size * 0.82)),
            (TextStyle::Body, FontId::proportional(size)),
            (TextStyle::Button, FontId::proportional(size)),
            (TextStyle::Heading, FontId::proportional(size * 1.5)),
            (TextStyle::Monospace, FontId::monospace(size * 0.95)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = size + 10.0;

        let v = &mut style.visuals;
        v.dark_mode = dark;
        v.override_text_color = None;
        v.weak_text_color = Some(c(p.subtext));
        v.panel_fill = c(p.surface);
        v.window_fill = c(p.surface);
        v.window_stroke = Stroke::new(1.0, c(p.separator));
        v.window_corner_radius = radius;
        v.menu_corner_radius = radius;
        v.faint_bg_color = c(p.raised);
        v.extreme_bg_color = c(p.base);
        v.text_edit_bg_color = Some(c(p.base));
        v.code_bg_color = c(p.raised);
        v.hyperlink_color = c(p.accent);
        v.warn_fg_color = c(p.urgent);
        v.error_fg_color = c(p.urgent);
        v.selection.bg_fill = c(p.accent);
        v.selection.stroke = Stroke::new(1.0, c(p.on_accent));
        v.text_cursor.stroke = Stroke::new(2.0, c(p.accent));
        v.slider_trailing_fill = true;
        v.striped = false;

        let w = &mut v.widgets;
        for (state, bg, line) in [
            (&mut w.noninteractive, p.surface, p.separator),
            (&mut w.inactive, p.raised, p.raised),
            (&mut w.hovered, p.hover, p.separator),
            (&mut w.active, p.selected, p.accent),
            (&mut w.open, p.selected, p.separator),
        ] {
            state.bg_fill = c(bg);
            state.weak_bg_fill = c(bg);
            state.bg_stroke = Stroke::new(1.0, c(line));
            state.fg_stroke = Stroke::new(1.0, c(p.text));
            state.corner_radius = radius;
            state.expansion = 0.0;
        }
        w.noninteractive.fg_stroke = Stroke::new(1.0, c(p.text));
    });
}
