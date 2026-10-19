//! Building blocks shared by the settings pages: cards, setting rows, a toggle switch and
//! small editors. Colors come from the theme palette only.

use wt_config::theme::Palette;
use wt_ui::{
    color,
    egui::{self, Align, Layout, Response, RichText, Sense, StrokeKind, Ui, vec2},
};

/// A titled group of settings, drawn as a card in `surface` so controls (in `raised`) stand out.
pub fn card<R>(ui: &mut Ui, p: &Palette, title: &str, note: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(14.0);
    if !title.is_empty() {
        ui.label(RichText::new(title).strong().color(color(p.text)));
    }
    if !note.is_empty() {
        ui.label(RichText::new(note).small().color(color(p.subtext)));
    }
    ui.add_space(4.0);
    egui::Frame::new()
        .fill(color(p.surface))
        .corner_radius(ui.visuals().widgets.noninteractive.corner_radius)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A thin line between rows in a card.
pub fn divider(ui: &mut Ui, p: &Palette) {
    ui.add_space(4.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color(p.separator));
    ui.add_space(4.0);
}

/// What a row should show next to its label.
pub struct RowInfo<'a> {
    pub label: &'a str,
    pub help: &'a str,
    /// Differs from the default: offer a reset.
    pub changed: bool,
    /// Only applies at the next start of the compositor.
    pub restart: bool,
}

/// One setting: label and help on the left, the control on the right. Returns true if the
/// reset button was clicked.
pub fn row(ui: &mut Ui, p: &Palette, info: RowInfo<'_>, control: impl FnOnce(&mut Ui)) -> bool {
    let mut reset = false;
    ui.horizontal(|ui| {
        let total = ui.available_width();
        let right = (total * 0.48).clamp(200.0, 380.0);
        let left = (total - right - 12.0).max(80.0);
        ui.allocate_ui_with_layout(vec2(left, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(left);
            ui.label(RichText::new(info.label).color(color(p.text)));
            if !info.help.is_empty() {
                ui.label(RichText::new(info.help).small().color(color(p.subtext)));
            }
            if info.restart {
                ui.label(
                    RichText::new("Applies the next time you log in")
                        .small()
                        .color(color(p.accent)),
                );
            }
        });
        ui.allocate_ui_with_layout(vec2(right, 0.0), Layout::right_to_left(Align::Center), |ui| {
            let r = ui
                .add_visible(info.changed, reset_button(p))
                .on_hover_text("Back to the default");
            reset = r.clicked();
            control(ui);
        });
    });
    reset
}

fn reset_button(p: &Palette) -> egui::Button<'static> {
    egui::Button::new(RichText::new("Reset").small().color(color(p.muted))).frame(false)
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, p: &Palette, on: &mut bool) -> Response {
    let h = ui.spacing().interact_size.y * 0.8;
    let (rect, mut response) = ui.allocate_exact_size(vec2(h * 1.8, h), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let off_track = if response.hovered() { p.hover } else { p.selected };
        let track = wt_config::theme::mix(off_track, p.accent, t);
        let knob = wt_config::theme::mix(p.muted, p.on_accent, t);
        let radius = rect.height() / 2.0;
        ui.painter()
            .rect(rect, radius, color(track), egui::Stroke::NONE, StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), radius * 0.7, color(knob));
    }
    response
}

/// A drop-down of fixed choices: (value, label). Returns true when the choice changed.
pub fn choice(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    value: &mut String,
    options: &[(&str, &str)],
    width: f32,
) -> bool {
    let current = options
        .iter()
        .find(|(v, _)| *v == value.as_str())
        .map_or(value.clone(), |(_, l)| l.to_string());
    let before = value.clone();
    egui::ComboBox::from_id_salt(id)
        .selected_text(current)
        .width(width)
        .show_ui(ui, |ui| {
            for (v, label) in options {
                ui.selectable_value(value, v.to_string(), *label);
            }
        });
    *value != before
}

/// A single-line text field filling the width it's given.
pub fn text(ui: &mut Ui, value: &mut String, hint: &str) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(ui.available_width()),
    )
}

/// An editable list of strings (commands, app ids...). Returns true when it changed.
pub fn string_list(ui: &mut Ui, p: &Palette, items: &mut Vec<String>, hint: &str) -> bool {
    let mut changed = false;
    let mut remove = None;
    for (i, item) in items.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new("Remove").small()).clicked() {
                    remove = Some(i);
                }
                changed |= text(ui, item, hint).changed();
            });
        });
    }
    if let Some(i) = remove {
        items.remove(i);
        changed = true;
    }
    if ui.button(RichText::new("Add").color(color(p.text))).clicked() {
        items.push(String::new());
        changed = true;
    }
    changed
}

/// An error line under a field.
pub fn error(ui: &mut Ui, p: &Palette, msg: &str) {
    ui.label(RichText::new(msg).small().color(color(p.urgent)));
}

/// Secondary explanatory text.
pub fn note(ui: &mut Ui, p: &Palette, msg: &str) {
    ui.label(RichText::new(msg).small().color(color(p.subtext)));
}
