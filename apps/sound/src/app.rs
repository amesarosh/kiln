//! The Sound window: the output and input with their volumes, each app playing or recording
//! with its own volume (and where it plays), and the sound cards' profiles.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kiln_config::theme::Palette;
use kiln_pipewire::{Command, Control, Device, Kind, Model, Stream};
use kiln_ui::{
    Look, color, eframe,
    egui::{self, Align, Layout, Margin, RichText, Sense, TextureHandle, Ui, vec2},
};

const ICON: f32 = 20.0;
const APP_ICON: f32 = 28.0;
/// How long a volume just set is shown as set, before what PipeWire reports takes over again
/// (so the slider doesn't jump back while the change is on its way).
const HOLD: Duration = Duration::from_millis(800);

#[derive(Default)]
struct Shared {
    /// pw-dump has answered (or failed) at least once.
    loaded: bool,
    model: Option<Model>,
    error: Option<String>,
}

pub struct SoundApp {
    look: Look,
    icons: Icons,
    shared: Arc<Mutex<Shared>>,
    control: Control,
    /// Volumes just set, by node id, with when.
    pending: HashMap<u32, (f32, Instant)>,
}

impl SoundApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let look = Look::new(&cc.egui_ctx, kiln_config::Config::default_path());
        let icons = Icons::new(&look.theme.icon_theme);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (s, ctx) = (shared.clone(), cc.egui_ctx.clone());
        kiln_pipewire::watch(move |model| {
            let mut s = s.lock().unwrap();
            s.loaded = true;
            s.model = model;
            ctx.request_repaint();
            true
        });
        let (s, ctx) = (shared.clone(), cc.egui_ctx.clone());
        let control = Control::new(move |e| {
            s.lock().unwrap().error = Some(e);
            ctx.request_repaint();
        });
        Self {
            look,
            icons,
            shared,
            control,
            pending: HashMap::new(),
        }
    }

    /// The volume to show for a node: the one just set, for a moment, else PipeWire's.
    fn shown_volume(&self, id: u32, reported: f32) -> f32 {
        match self.pending.get(&id) {
            Some((v, at)) if at.elapsed() < HOLD => *v,
            _ => reported,
        }
    }
}

impl eframe::App for SoundApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        if self.icons.theme != self.look.theme.icon_theme {
            self.icons = Icons::new(&self.look.theme.icon_theme);
        }
        self.pending.retain(|_, (_, at)| at.elapsed() < HOLD);
        if !self.pending.is_empty() {
            ctx.request_repaint_after(HOLD / 4);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(color(p.base))
                    .inner_margin(Margin::symmetric(18, 14)),
            )
            .show(ui, |ui| self.page(ui, &p));
    }
}

impl SoundApp {
    fn page(&mut self, ui: &mut Ui, p: &Palette) {
        let (loaded, model, error) = {
            let s = self.shared.lock().unwrap();
            (s.loaded, s.model.clone(), s.error.clone())
        };
        ui.label(RichText::new("Sound").heading().color(color(p.text)));
        ui.add_space(12.0);

        if let Some(e) = &error {
            egui::Panel::bottom("error").frame(egui::Frame::new()).show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                        self.shared.lock().unwrap().error = None;
                    }
                    ui.add(egui::Label::new(RichText::new(e).small().color(color(p.urgent))).wrap());
                });
            });
        }
        if !loaded {
            return;
        }
        let Some(m) = model else {
            return empty(
                ui,
                p,
                &mut self.icons,
                "audio-volume-muted-symbolic",
                "Sound isn't available",
                "PipeWire isn't running, or its tools (pw-dump, wpctl) aren't installed.",
            );
        };

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            self.device_section(ui, p, &m, true);
            ui.add_space(18.0);
            self.device_section(ui, p, &m, false);
            ui.add_space(18.0);

            section_title(ui, p, "Apps");
            if m.playing.is_empty() {
                ui.label(RichText::new("No apps are playing sound.").color(color(p.subtext)));
            } else {
                card(ui, p, |ui| {
                    for (i, s) in m.playing.iter().enumerate() {
                        if i > 0 {
                            separator(ui, p);
                        }
                        self.stream_row(ui, p, s, &m.outputs, m.default_output(), true);
                    }
                });
            }
            if !m.recording.is_empty() {
                ui.add_space(18.0);
                section_title(ui, p, "Recording");
                card(ui, p, |ui| {
                    for (i, s) in m.recording.iter().enumerate() {
                        if i > 0 {
                            separator(ui, p);
                        }
                        self.stream_row(ui, p, s, &m.inputs, m.default_input(), false);
                    }
                });
            }

            if !m.cards.is_empty() {
                ui.add_space(18.0);
                section_title(ui, p, "Sound Cards");
                card(ui, p, |ui| {
                    for (i, c) in m.cards.iter().enumerate() {
                        if i > 0 {
                            separator(ui, p);
                        }
                        self.card_row(ui, p, c);
                    }
                });
            }
            ui.add_space(8.0);
        });
    }

    /// "Output" or "Input": which device, and its volume.
    fn device_section(&mut self, ui: &mut Ui, p: &Palette, m: &Model, output: bool) {
        let (title, devices, current) = if output {
            ("Output", &m.outputs, m.default_output())
        } else {
            ("Input", &m.inputs, m.default_input())
        };
        section_title(ui, p, title);
        if devices.is_empty() {
            let text = if output {
                "No speakers or headphones found."
            } else {
                "No microphones found."
            };
            ui.label(RichText::new(text).color(color(p.subtext)));
            return;
        }
        card(ui, p, |ui| {
            inset(ui, |ui| {
                ui.horizontal(|ui| {
                    let icon = current.map_or(if output { Kind::Speakers } else { Kind::Microphone }, |d| d.kind);
                    self.icon(ui, kind_icon(icon), ICON, p.text, "audio-speakers-symbolic");
                    ui.add_space(4.0);
                    let selected = current.map_or("Choose a device", |d| d.label.as_str());
                    kiln_ui::select(ui, |ui| {
                        egui::ComboBox::from_id_salt(title)
                            .selected_text(selected)
                            .width(ui.available_width())
                            .truncate()
                            .show_ui(ui, |ui| {
                                kiln_ui::roomy_menu(ui);
                                for d in devices {
                                    let on = current.is_some_and(|c| c.id == d.id);
                                    if ui.selectable_label(on, &d.label).clicked() && !on {
                                        self.control.send(Command::Default(d.id));
                                    }
                                }
                            });
                    });
                });
                if let Some(d) = current {
                    ui.add_space(4.0);
                    self.volume_row(ui, p, d.id, d.volume, d.muted, output);
                }
            });
        });
    }

    /// An app: its icon and name, where it plays, and its volume.
    fn stream_row(
        &mut self,
        ui: &mut Ui,
        p: &Palette,
        s: &Stream,
        devices: &[Device],
        default: Option<&Device>,
        output: bool,
    ) {
        inset(ui, |ui| {
            ui.horizontal(|ui| {
                self.app_icon(ui, p, s);
                ui.add_space(4.0);
                let text_w = (ui.available_width() - 40.0).max(60.0);
                ui.allocate_ui_with_layout(vec2(text_w, APP_ICON + 6.0), Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let name = RichText::new(&s.app).color(color(if s.active { p.text } else { p.subtext }));
                    ui.add(egui::Label::new(name).truncate());
                    // Where it's connected now, else where it will go.
                    let place = devices
                        .iter()
                        .find(|d| Some(d.id) == s.device)
                        .or_else(|| devices.iter().find(|d| s.targets(d)))
                        .or(default)
                        .map_or(String::new(), |d| d.label.clone());
                    let detail = match (s.title.is_empty(), place.is_empty()) {
                        (false, false) => format!("{} · {place}", s.title),
                        (false, true) => s.title.clone(),
                        (true, _) => place,
                    };
                    if !detail.is_empty() {
                        ui.add(egui::Label::new(RichText::new(detail).small().color(color(p.muted))).truncate());
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let more = self.icons.symbolic(ui.ctx(), "view-more-symbolic", 16.0, p.muted);
                    let button = match &more {
                        Some(t) => egui::Button::image(egui::Image::new(t).fit_to_exact_size(vec2(16.0, 16.0))),
                        None => egui::Button::new("…"),
                    };
                    let hint = if output { "Play on…" } else { "Record from…" };
                    let menu = ui.add(button.frame(false)).on_hover_text(hint);
                    egui::Popup::menu(&menu).show(|ui| {
                        kiln_ui::roomy_menu(ui);
                        ui.label(
                            RichText::new(if output { "Play on" } else { "Record from" })
                                .small()
                                .color(color(p.muted)),
                        );
                        let chosen = devices.iter().find(|d| s.targets(d)).map(|d| d.id);
                        let default_label = match default {
                            Some(d) => format!("Default ({})", d.label),
                            None => "Default".to_string(),
                        };
                        if ui.selectable_label(chosen.is_none(), default_label).clicked() && chosen.is_some() {
                            self.control.send(Command::Target(s.id, None));
                        }
                        for d in devices {
                            if ui.selectable_label(chosen == Some(d.id), &d.label).clicked() && chosen != Some(d.id) {
                                self.control.send(Command::Target(s.id, Some(d.name.clone())));
                            }
                        }
                    });
                });
            });
            ui.add_space(2.0);
            self.volume_row(ui, p, s.id, s.volume, s.muted, output);
        });
    }

    /// A sound card and its profile.
    fn card_row(&mut self, ui: &mut Ui, p: &Palette, c: &kiln_pipewire::Card) {
        inset(ui, |ui| {
            ui.horizontal(|ui| {
                self.icon(ui, "audio-card-symbolic", ICON, p.text, "audio-speakers-symbolic");
                ui.add_space(4.0);
                let combo_w = (ui.available_width() * 0.55).clamp(140.0, 260.0);
                let text_w = (ui.available_width() - combo_w - 8.0).max(40.0);
                ui.allocate_ui_with_layout(
                    vec2(text_w, kiln_ui::field_height(ui.style())),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(RichText::new(&c.label).color(color(p.text))).truncate());
                    },
                );
                let current = c.profiles.iter().find(|pr| Some(pr.index) == c.current);
                let selected = current.map_or("—", |pr| pr.label.as_str());
                kiln_ui::select(ui, |ui| {
                    egui::ComboBox::from_id_salt(("card", c.id))
                        .selected_text(selected)
                        .width(combo_w)
                        .truncate()
                        .show_ui(ui, |ui| {
                            kiln_ui::roomy_menu(ui);
                            ui.set_min_width(280.0);
                            for pr in c
                                .profiles
                                .iter()
                                .filter(|pr| pr.available || Some(pr.index) == c.current)
                            {
                                let on = Some(pr.index) == c.current;
                                if ui.selectable_label(on, &pr.label).clicked() && !on {
                                    self.control.send(Command::Profile(c.id, pr.index));
                                }
                            }
                        });
                });
            });
        });
    }

    /// A mute button, a slider, and the percentage.
    fn volume_row(&mut self, ui: &mut Ui, p: &Palette, id: u32, reported: f32, muted: bool, output: bool) {
        let mut v = self.shown_volume(id, reported);
        ui.horizontal(|ui| {
            let icon = match (output, muted || v <= 0.001) {
                (true, true) => "audio-volume-muted-symbolic",
                (true, false) if v < 0.34 => "audio-volume-low-symbolic",
                (true, false) if v < 0.67 => "audio-volume-medium-symbolic",
                (true, false) => "audio-volume-high-symbolic",
                (false, true) => "microphone-sensitivity-muted-symbolic",
                (false, false) => "audio-input-microphone-symbolic",
            };
            let tint = if muted { p.urgent } else { p.muted };
            let image = self.icons.symbolic(ui.ctx(), icon, 18.0, tint);
            let button = match &image {
                Some(t) => egui::Button::image(egui::Image::new(t).fit_to_exact_size(vec2(18.0, 18.0))),
                None => egui::Button::new(if muted { "🔇" } else { "🔊" }),
            };
            let hint = if muted { "Unmute" } else { "Mute" };
            if ui.add(button.frame(false)).on_hover_text(hint).clicked() {
                self.control.send(Command::Mute(id, !muted));
            }
            let label_w = 44.0;
            ui.spacing_mut().slider_width = (ui.available_width() - label_w - 8.0).max(60.0);
            let r = ui.add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false));
            if r.changed() {
                self.pending.insert(id, (v, Instant::now()));
                self.control.send(Command::Volume(id, v));
                if muted && v > 0.0 {
                    self.control.send(Command::Mute(id, false));
                }
            } else if r.dragged()
                && let Some((_, at)) = self.pending.get_mut(&id)
            {
                // Held still mid-drag: keep showing where the knob is.
                *at = Instant::now();
            }
            let pct = RichText::new(format!("{}%", (v * 100.0).round() as i32)).color(color(if muted {
                p.muted
            } else {
                p.subtext
            }));
            ui.allocate_ui_with_layout(
                vec2(label_w, ui.spacing().interact_size.y),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.label(pct);
                },
            );
        });
    }

    fn icon(&mut self, ui: &mut Ui, name: &str, size: f32, tint: kiln_config::Color, fallback: &str) {
        let t = self
            .icons
            .symbolic(ui.ctx(), name, size, tint)
            .or_else(|| self.icons.symbolic(ui.ctx(), fallback, size, tint));
        match t {
            Some(t) => ui.add(egui::Image::new(&t).fit_to_exact_size(vec2(size, size))),
            None => ui.allocate_exact_size(vec2(size, size), Sense::hover()).1,
        };
    }

    fn app_icon(&mut self, ui: &mut Ui, p: &Palette, s: &Stream) {
        match self.icons.app(ui.ctx(), &s.icons, APP_ICON) {
            Some(t) => {
                ui.add(egui::Image::new(&t).fit_to_exact_size(vec2(APP_ICON, APP_ICON)));
            }
            None => {
                let pad = (APP_ICON - ICON) / 2.0;
                ui.add_space(pad);
                self.icon(ui, "audio-x-generic-symbolic", ICON, p.muted, "audio-speakers-symbolic");
                ui.add_space(pad);
            }
        }
    }
}

fn kind_icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Speakers => "audio-speakers-symbolic",
        Kind::Headphones => "audio-headphones-symbolic",
        Kind::Headset => "audio-headset-symbolic",
        Kind::Display => "video-display-symbolic",
        Kind::Microphone => "audio-input-microphone-symbolic",
    }
}

fn section_title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).strong().color(color(p.subtext)));
    ui.add_space(4.0);
}

fn card(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(color(p.surface))
        .corner_radius(ui.visuals().widgets.noninteractive.corner_radius)
        .inner_margin(Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// Room around a row's contents inside a card.
fn inset(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn separator(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range().shrink(10.0),
        rect.center().y,
        egui::Stroke::new(1.0, color(p.separator)),
    );
}

/// A big icon and a message in the middle, for when there's nothing to show.
fn empty(ui: &mut Ui, p: &Palette, icons: &mut Icons, icon: &str, title: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 90.0).max(20.0));
        if let Some(t) = icons.symbolic(ui.ctx(), icon, 64.0, p.muted) {
            ui.add(egui::Image::new(&t).fit_to_exact_size(vec2(64.0, 64.0)));
        }
        ui.add_space(12.0);
        ui.label(RichText::new(title).heading().color(color(p.text)));
        ui.add_space(4.0);
        ui.label(RichText::new(body).color(color(p.subtext)));
    });
}

/// Icons from the theme, as textures: symbolic ones in the theme's colors, and app icons.
struct Icons {
    icons: kiln_icons::Icons,
    theme: String,
    cache: HashMap<(String, [u8; 4], u32), Option<TextureHandle>>,
}

impl Icons {
    fn new(theme: &str) -> Self {
        Self {
            icons: kiln_icons::Icons::new(theme),
            theme: theme.to_string(),
            cache: HashMap::new(),
        }
    }

    fn symbolic(
        &mut self,
        ctx: &egui::Context,
        name: &str,
        size: f32,
        tint: kiln_config::Color,
    ) -> Option<TextureHandle> {
        let c = color(tint).to_srgba_unmultiplied();
        let px = (size * ctx.pixels_per_point()).round() as u32;
        let key = (name.to_string(), c, px);
        if let Some(t) = self.cache.get(&key) {
            return t.clone();
        }
        let tex = self.icons.symbolic(name, px, c).map(|icon| texture(ctx, name, &icon));
        self.cache.insert(key, tex.clone());
        tex
    }

    /// An app's icon, by the first of `names` that has one.
    fn app(&mut self, ctx: &egui::Context, names: &[String], size: f32) -> Option<TextureHandle> {
        let px = (size * ctx.pixels_per_point()).round() as u32;
        let key = (format!("app:{}", names.join("|")), [0; 4], px);
        if let Some(t) = self.cache.get(&key) {
            return t.clone();
        }
        let tex = names
            .iter()
            .find_map(|n| self.icons.get(n, px))
            .map(|icon| texture(ctx, &key.0, &icon));
        self.cache.insert(key, tex.clone());
        tex
    }
}

fn texture(ctx: &egui::Context, name: &str, icon: &kiln_icons::Icon) -> TextureHandle {
    let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
    ctx.load_texture(format!("icon:{name}"), image, egui::TextureOptions::LINEAR)
}
