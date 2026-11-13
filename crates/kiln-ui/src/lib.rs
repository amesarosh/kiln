//! The shared look and window setup for kiln's apps (settings, and more to come).
//!
//! Apps take their font, corner rounding and colors from the compositor's `[theme]`, through the
//! same palette roles the bar and menus use, and follow theme changes as the config is saved:
//!
//! ```ignore
//! kiln_ui::run("kiln-settings", "Settings", [900.0, 640.0], |cc| MyApp::new(cc))
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
use kiln_config::{Config, theme::Theme};

/// A config color as egui's color type.
pub fn color(c: kiln_config::Color) -> Color32 {
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

/// Open a window set up by the caller (size, decorations...), with `app_id` and `title`.
pub fn run_viewport<A: eframe::App + 'static>(
    app_id: &str,
    title: &str,
    viewport: egui::ViewportBuilder,
    make: impl FnOnce(&eframe::CreationContext<'_>) -> A + 'static,
) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: viewport.with_app_id(app_id).with_title(title),
        ..Default::default()
    };
    eframe::run_native(app_id, options, Box::new(|cc| Ok(Box::new(make(cc)))))
}

/// Keeps an app's look in step with the config file's `[theme]`.
pub struct Look {
    path: Option<PathBuf>,
    /// Modification times of the config and of the saved theme it uses.
    modified: [Option<SystemTime>; 2],
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
            modified: [None; 2],
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
            if self.stamps() != self.modified {
                self.reload(ctx);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// Re-read the config file and apply its theme. A broken config keeps the current look.
    pub fn reload(&mut self, ctx: &egui::Context) {
        let Some(path) = &self.path else {
            return self.set(ctx, Config::default().theme);
        };
        if let Ok(config) = Config::read(path) {
            self.set(ctx, config.theme);
        }
        self.modified = self.stamps();
    }

    fn stamps(&self) -> [Option<SystemTime>; 2] {
        let theme = self.theme.name.as_ref().zip(self.path.as_ref());
        [
            modified(self.path.as_deref()),
            modified(theme.map(|(name, path)| kiln_config::theme_path(path, name)).as_deref()),
        ]
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

/// Room between a control's ends and its text: more the rounder its corners, so text in a
/// pill-shaped field or drop-down doesn't crowd the curve.
pub fn side_padding(style: &egui::Style) -> f32 {
    let h = style.spacing.interact_size.y;
    let radius = f32::from(style.visuals.widgets.inactive.corner_radius.nw).min(h / 2.0);
    (10.0 + radius * 0.4).round()
}

/// Height of text fields and drop-downs: a little taller than buttons, for room above and
/// below the text.
pub fn field_height(style: &egui::Style) -> f32 {
    style.spacing.interact_size.y + 8.0
}

/// A one-line text field [`field_height`] tall, its text clear of the rounded ends.
pub fn field<'t>(ui: &egui::Ui, edit: egui::TextEdit<'t>) -> egui::TextEdit<'t> {
    let pad = side_padding(ui.style()) as i8;
    edit.margin(egui::Margin::symmetric(pad, 4))
        .min_size(egui::vec2(0.0, field_height(ui.style())))
        .vertical_align(egui::Align::Center)
}

/// Runs `add` (which shows an `egui::ComboBox`) so the drop-down's button is
/// [`field_height`] tall, matching the text fields. Its menu keeps the normal sizes.
pub fn select<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope(|ui| {
        let h = field_height(ui.style());
        let s = ui.spacing_mut();
        s.interact_size.y = h;
        s.button_padding.y = s.button_padding.y.max(4.0);
        add(ui)
    })
    .inner
}

/// Roomier items for a menu: call first thing in the menu's contents. egui's menus squeeze
/// their buttons to almost no padding, which feels cramped for a list of actions.
pub fn roomy_menu(ui: &mut egui::Ui) {
    let s = ui.spacing_mut();
    s.button_padding = egui::vec2(12.0, 6.0);
    s.item_spacing.y = 2.0;
    ui.set_min_width(200.0);
}

/// The semi-bold face of the theme font, for text that should stand out (like folder names).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

const BOLD: &str = "semibold";

/// egui's fonts with the theme font (and the system monospace font) put first, plus a
/// [`semibold`] family.
fn fonts(name: &str) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    for (family, font, key) in [
        (FontFamily::Proportional, name, "theme"),
        (FontFamily::Monospace, "monospace", "theme-mono"),
    ] {
        let Some(data) = kiln_config::fonts::candidates(font).find_map(|p| std::fs::read(p).ok()) else {
            tracing::warn!("no font file found for \"{font}\"; using the built-in one");
            continue;
        };
        defs.font_data.insert(key.into(), Arc::new(FontData::from_owned(data)));
        defs.families.entry(family).or_default().insert(0, key.into());
    }
    // A font given as a file has no bold face to ask fontconfig for; it's used as it is.
    let bold_file = (!name.contains('/'))
        .then(|| kiln_config::fonts::resolve(&format!("{name}:semibold")))
        .flatten()
        .and_then(|p| std::fs::read(p).ok());
    let mut bold_family = defs.families[&FontFamily::Proportional].clone();
    if let Some(data) = bold_file {
        let mut font = FontData::from_owned(data);
        // A variable font is the same file for every weight; pick semi-bold on its weight axis.
        if let Some(axis) = font
            .variation_axes()
            .into_iter()
            .find(|a| a.tag.to_be_bytes() == *b"wght")
        {
            font.tweak.coords = egui::epaint::text::VariationCoords::new([(*b"wght", axis.range.clamp(600.0))]);
        }
        defs.font_data.insert("theme-bold".into(), Arc::new(font));
        bold_family.insert(0, "theme-bold".into());
    }
    defs.families.insert(FontFamily::Name(BOLD.into()), bold_family);
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
        // Fields look like the other controls (drop-downs, buttons) rather than holes in the card.
        v.text_edit_bg_color = Some(c(p.raised));
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
        // Buttons and drop-downs keep their text clear of rounded ends (needs the radius above).
        style.spacing.button_padding.x = side_padding(style);
    });
}
