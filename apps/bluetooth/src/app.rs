//! The Bluetooth window: a switch to turn it on and off, the devices paired with this
//! computer (connect, disconnect, forget), and a search for new ones to pair.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use kiln_config::theme::Palette;
use kiln_ui::{
    Look, color, eframe,
    egui::{self, Align, Layout, Margin, RichText, Sense, TextureHandle, Ui, vec2},
};

use crate::bluez::{Answer, Bluez, Device, Prompt};

/// A search stops by itself after this: searching slows other Bluetooth traffic (music
/// on headphones can stutter).
const SEARCH_FOR: Duration = Duration::from_secs(120);
const ICON: f32 = 22.0;

pub struct BluetoothApp {
    look: Look,
    icons: Icons,
    bt: Result<Bluez, String>,
    /// When we started a search, to stop it after a while.
    searching_since: Option<Instant>,
    /// Typed into a "type the code" prompt.
    code: String,
}

impl BluetoothApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let look = Look::new(&cc.egui_ctx, kiln_config::Config::default_path());
        let icons = Icons::new(&look.theme.icon_theme);
        Self {
            look,
            icons,
            bt: Bluez::start(&cc.egui_ctx),
            searching_since: None,
            code: String::new(),
        }
    }

    fn search(&mut self, bt: &Bluez, on: bool) {
        self.searching_since = on.then(Instant::now);
        bt.search(on);
    }
}

impl eframe::App for BluetoothApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        if self.icons.theme != self.look.theme.icon_theme {
            self.icons = Icons::new(&self.look.theme.icon_theme);
        }
        if let Ok(bt) = self.bt.clone()
            && let Some(since) = self.searching_since
        {
            let discovering = bt.model.lock().unwrap().adapter.as_ref().is_some_and(|a| a.discovering);
            if since.elapsed() > SEARCH_FOR && discovering {
                self.search(&bt, false);
            } else {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
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
            .show(ui, |ui| match self.bt.clone() {
                Ok(bt) => self.page(ui, &p, &bt),
                Err(e) => empty(
                    ui,
                    &p,
                    &mut self.icons,
                    "bluetooth-disabled-symbolic",
                    "Bluetooth isn't available",
                    &e,
                ),
            });
        if let Ok(bt) = self.bt.clone() {
            self.prompt(ui.ctx(), &p, &bt);
        }
    }
}

impl BluetoothApp {
    fn page(&mut self, ui: &mut Ui, p: &Palette, bt: &Bluez) {
        let (loaded, service, adapter, devices, busy, error) = {
            let m = bt.model.lock().unwrap();
            (
                m.loaded,
                m.service,
                m.adapter.clone(),
                m.devices.clone(),
                m.busy.clone(),
                m.error.clone(),
            )
        };
        let powered = adapter.as_ref().is_some_and(|a| a.powered);

        // The title and the on/off switch.
        ui.horizontal(|ui| {
            ui.label(RichText::new("Bluetooth").heading().color(color(p.text)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = powered;
                if adapter.is_some() && toggle(ui, p, &mut on).changed() {
                    bt.set_powered(on);
                }
            });
        });
        if let Some(a) = adapter.as_ref().filter(|a| a.powered && !a.name.is_empty()) {
            ui.label(
                RichText::new(format!("This computer is “{}”", a.name))
                    .small()
                    .color(color(p.muted)),
            );
        }
        ui.add_space(12.0);

        // The error line sits at the bottom.
        if let Some(e) = &error {
            egui::Panel::bottom("error").frame(egui::Frame::new()).show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                        bt.clear_error();
                    }
                    ui.add(egui::Label::new(RichText::new(e).small().color(color(p.urgent))).wrap());
                });
            });
        }

        if !loaded {
            return;
        }
        if !service {
            return empty(
                ui,
                p,
                &mut self.icons,
                "bluetooth-disabled-symbolic",
                "The Bluetooth service isn't running",
                "Start it with: sudo systemctl enable --now bluetooth",
            );
        }
        let Some(adapter) = adapter else {
            return empty(
                ui,
                p,
                &mut self.icons,
                "bluetooth-disabled-symbolic",
                "No Bluetooth adapter",
                "This computer has no Bluetooth, or it's switched off in the firmware settings.",
            );
        };
        if !adapter.powered {
            self.searching_since = None;
            return empty(
                ui,
                p,
                &mut self.icons,
                "bluetooth-disabled-symbolic",
                "Bluetooth is off",
                "Turn it on with the switch above to use your devices.",
            );
        }

        let mut mine: Vec<&Device> = devices.iter().filter(|d| d.paired).collect();
        mine.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        // New devices: only while searching, and only ones with a name.
        let mut nearby: Vec<&Device> = devices
            .iter()
            .filter(|d| !d.paired && d.named && (adapter.discovering || busy.contains(&d.path)))
            .collect();
        nearby.sort_by_key(|d| std::cmp::Reverse(d.rssi.unwrap_or(i16::MIN)));

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            section_title(ui, p, "My Devices");
            if mine.is_empty() {
                ui.label(RichText::new("No devices paired yet. Search below to add one.").color(color(p.subtext)));
            } else {
                card(ui, p, |ui| {
                    for (i, d) in mine.iter().enumerate() {
                        if i > 0 {
                            separator(ui, p);
                        }
                        self.device_row(ui, p, bt, d, busy.contains(&d.path));
                    }
                });
            }
            ui.add_space(18.0);

            ui.horizontal(|ui| {
                section_title(ui, p, "Other Devices");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if adapter.discovering {
                        if ui.button("Stop").clicked() {
                            self.search(bt, false);
                        }
                        ui.add(egui::Spinner::new().size(14.0).color(color(p.muted)));
                    } else if ui.button("Search").clicked() {
                        self.search(bt, true);
                    }
                });
            });
            if nearby.is_empty() {
                let text = if adapter.discovering {
                    "Looking for devices… Put the device in pairing mode."
                } else {
                    "Search to find devices to pair."
                };
                ui.label(RichText::new(text).color(color(p.subtext)));
            } else {
                card(ui, p, |ui| {
                    for (i, d) in nearby.iter().enumerate() {
                        if i > 0 {
                            separator(ui, p);
                        }
                        self.device_row(ui, p, bt, d, busy.contains(&d.path));
                    }
                });
            }
        });
    }

    /// One device: its icon, name and state, and what can be done.
    fn device_row(&mut self, ui: &mut Ui, p: &Palette, bt: &Bluez, d: &Device, busy: bool) {
        let h = 46.0;
        let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(vec2(10.0, 0.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let icon = if d.icon.is_empty() {
            "bluetooth-symbolic".to_string()
        } else {
            format!("{}-symbolic", d.icon)
        };
        let tint = if d.connected { p.accent } else { p.text };
        if let Some(t) = self
            .icons
            .symbolic(row.ctx(), &icon, ICON, tint)
            .or_else(|| self.icons.symbolic(row.ctx(), "bluetooth-symbolic", ICON, tint))
        {
            row.add(egui::Image::new(&t).fit_to_exact_size(vec2(ICON, ICON)));
        }
        row.add_space(8.0);
        let state = match (busy, d.paired, d.connected) {
            (true, false, _) => "Pairing…".to_string(),
            (true, true, true) => "Disconnecting…".to_string(),
            (true, true, false) => "Connecting…".to_string(),
            (false, _, true) => match d.battery {
                Some(b) => format!("Connected · Battery {b}%"),
                None => "Connected".to_string(),
            },
            (false, true, false) => "Not connected".to_string(),
            (false, false, false) => d.address.clone(),
        };
        // Name and state on two lines, leaving room for the buttons.
        let text_w = (row.available_width() - 150.0).max(60.0);
        row.allocate_ui_with_layout(vec2(text_w, h), Layout::top_down(Align::Min), |ui| {
            ui.add_space(5.0);
            ui.add(egui::Label::new(RichText::new(&d.name).color(color(p.text))).truncate());
            ui.add(egui::Label::new(RichText::new(state).small().color(color(p.muted))).truncate());
        });
        row.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if d.paired {
                let more = self.icons.symbolic(ui.ctx(), "view-more-symbolic", 16.0, p.muted);
                let button = match &more {
                    Some(t) => egui::Button::image(egui::Image::new(t).fit_to_exact_size(vec2(16.0, 16.0))),
                    None => egui::Button::new("…"),
                };
                let menu = ui.add_enabled(!busy, button.frame(false)).on_hover_text("More");
                egui::Popup::menu(&menu).show(|ui| {
                    kiln_ui::roomy_menu(ui);
                    if ui.button("Forget").clicked() {
                        bt.forget(&d.path);
                    }
                });
            }
            if busy {
                ui.add(egui::Spinner::new().size(16.0).color(color(p.muted)));
            } else if !d.paired {
                if accent_button(ui, p, "Pair").clicked() {
                    bt.pair(&d.path);
                }
            } else if d.connected {
                if ui.button("Disconnect").clicked() {
                    bt.disconnect(&d.path);
                }
            } else if accent_button(ui, p, "Connect").clicked() {
                bt.connect(&d.path);
            }
        });
        if d.paired {
            r.context_menu(|ui| {
                kiln_ui::roomy_menu(ui);
                if ui.add_enabled(!busy, egui::Button::new("Forget")).clicked() {
                    bt.forget(&d.path);
                }
            });
        }
    }

    /// The pairing agent's question, over the window.
    fn prompt(&mut self, ctx: &egui::Context, p: &Palette, bt: &Bluez) {
        let Some(prompt) = bt.model.lock().unwrap().prompt.as_ref().map(|(q, _)| q.clone()) else {
            self.code.clear();
            return;
        };
        let modal = egui::Modal::new(egui::Id::new("pairing")).show(ctx, |ui| {
            ui.set_width(340.0);
            let title = |ui: &mut Ui, t: String| {
                ui.label(RichText::new(t).heading().color(color(p.text)));
                ui.add_space(8.0);
            };
            let big = |ui: &mut Ui, code: String| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(code).size(32.0).strong().color(color(p.accent)));
                    ui.add_space(6.0);
                });
            };
            let mut answer = None;
            match &prompt {
                Prompt::Confirm { device, passkey } => {
                    title(ui, format!("Pair with {device}?"));
                    ui.label(RichText::new(format!("Check that {device} shows this code:")).color(color(p.subtext)));
                    big(ui, format!("{passkey:06}"));
                    answer = buttons(ui, p, "Pair");
                }
                Prompt::ShowPin { device, pin } => {
                    title(ui, format!("Pair with {device}"));
                    ui.label(
                        RichText::new(format!("Type this code on {device}, then press Enter on it:"))
                            .color(color(p.subtext)),
                    );
                    big(ui, pin.clone());
                    if ui.button("Close").clicked() {
                        answer = Some(Answer::No);
                    }
                }
                Prompt::ShowPasskey { device, passkey } => {
                    title(ui, format!("Pair with {device}"));
                    ui.label(
                        RichText::new(format!("Type this code on {device}, then press Enter on it:"))
                            .color(color(p.subtext)),
                    );
                    big(ui, format!("{passkey:06}"));
                    if ui.button("Close").clicked() {
                        answer = Some(Answer::No);
                    }
                }
                Prompt::EnterPin { device } | Prompt::EnterPasskey { device } => {
                    title(ui, format!("Pair with {device}"));
                    ui.label(RichText::new(format!("Type the code {device} shows:")).color(color(p.subtext)));
                    ui.add_space(6.0);
                    let field = ui
                        .add(kiln_ui::field(ui, egui::TextEdit::singleline(&mut self.code)).desired_width(f32::INFINITY));
                    field.request_focus();
                    let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(6.0);
                    answer = buttons(ui, p, "Pair");
                    if entered {
                        answer = Some(Answer::Yes);
                    }
                    if matches!(answer, Some(Answer::Yes)) {
                        answer = Some(Answer::Text(std::mem::take(&mut self.code)));
                    }
                }
                Prompt::Authorize { device } => {
                    title(ui, format!("Let {device} connect?"));
                    ui.label(
                        RichText::new(format!("{device} wants to connect to this computer.")).color(color(p.subtext)),
                    );
                    ui.add_space(8.0);
                    answer = buttons(ui, p, "Allow");
                }
            }
            answer
        });
        if let Some(answer) = modal.inner {
            bt.answer(answer);
        } else if modal.should_close() {
            bt.answer(Answer::No);
        }
    }
}

/// Cancel and a highlighted button on the right; Some once one is clicked.
fn buttons(ui: &mut Ui, p: &Palette, yes: &str) -> Option<Answer> {
    let mut out = None;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if accent_button(ui, p, yes).clicked() {
            out = Some(Answer::Yes);
        }
        if ui.button("Cancel").clicked() {
            out = Some(Answer::No);
        }
    });
    out
}

fn accent_button(ui: &mut Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(color(p.on_accent))).fill(color(p.accent)))
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

fn separator(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range().shrink(10.0),
        rect.center().y,
        egui::Stroke::new(1.0, color(p.separator)),
    );
}

/// A big icon and a message in the middle, for when there's nothing to list.
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

/// An on/off switch (like the settings app's).
fn toggle(ui: &mut Ui, p: &Palette, on: &mut bool) -> egui::Response {
    let h = ui.spacing().interact_size.y * 0.9;
    let (rect, mut response) = ui.allocate_exact_size(vec2(h * 1.8, h), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let off_track = if response.hovered() { p.hover } else { p.selected };
        let track = kiln_config::theme::mix(off_track, p.accent, t);
        let knob = kiln_config::theme::mix(p.muted, p.on_accent, t);
        let radius = rect.height() / 2.0;
        ui.painter()
            .rect(rect, radius, color(track), egui::Stroke::NONE, egui::StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), radius * 0.7, color(knob));
    }
    response
}

/// Symbolic icons in the theme's colors, as textures.
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
        let tex = self.icons.symbolic(name, px, c).map(|icon| {
            let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
            ctx.load_texture(format!("icon:{name}"), image, egui::TextureOptions::LINEAR)
        });
        self.cache.insert(key, tex.clone());
        tex
    }
}
