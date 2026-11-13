//! Building blocks shared by the settings pages: cards, setting rows, a toggle switch and
//! small editors. Colors come from the theme palette only.

use kiln_config::theme::Palette;
use kiln_ui::{
    color,
    egui::{self, Align, Color32, Layout, Rect, Response, RichText, Sense, StrokeKind, TextStyle, Ui, UiBuilder, vec2},
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
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A thin line between rows in a card.
pub fn divider(ui: &mut Ui, p: &Palette) {
    ui.add_space(6.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color(p.separator));
    ui.add_space(6.0);
}

/// Width of the controls on the right of a row, so their left edges line up down the page.
const CONTROL_WIDTH: f32 = 260.0;

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
    let single_line = info.help.is_empty() && !info.restart;
    let left = |ui: &mut Ui| {
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
    };
    row_with(ui, p, Trailing::Reset(info.changed), single_line, left, control)
}

/// The small button at the far right of a row.
pub enum Trailing {
    /// "Reset", shown when the setting differs from the default. Its space is kept either way.
    Reset(bool),
    Remove,
}

/// A row with any content on the left. `single_line` content is centered against the control;
/// taller content has the control centered against it. Returns true if the trailing button
/// was clicked.
pub fn row_with(
    ui: &mut Ui,
    p: &Palette,
    trailing: Trailing,
    single_line: bool,
    left: impl FnOnce(&mut Ui),
    control: impl FnOnce(&mut Ui),
) -> bool {
    let mut clicked = false;
    let control_h = kiln_ui::field_height(ui.style());
    let gap = ui.spacing().item_spacing.x;
    let top_left = ui.cursor().min;
    let total = ui.available_width();
    let right = (total * 0.5).clamp(180.0, 360.0);
    let left_w = (total - right - gap).max(80.0);

    // The left side first, to learn the row's height, then the control centered in that height.
    let pad = if single_line {
        ((control_h - ui.text_style_height(&TextStyle::Body)) / 2.0).max(0.0)
    } else {
        0.0
    };
    let left_rect = Rect::from_min_size(top_left + vec2(0.0, pad), vec2(left_w, f32::INFINITY));
    let left_h = ui
        .scope_builder(
            UiBuilder::new()
                .max_rect(left_rect)
                .layout(Layout::top_down(Align::Min)),
            left,
        )
        .response
        .rect
        .height();
    let height = (pad + left_h).max(control_h);

    let right_rect = Rect::from_min_size(top_left + vec2(total - right, 0.0), vec2(right, height));
    let right_layout = Layout::right_to_left(Align::Center);
    let right_used = ui
        .scope_builder(UiBuilder::new().max_rect(right_rect).layout(right_layout), |ui| {
            // A fixed slot, so controls line up whether or not their button shows.
            let slot = vec2(ui.text_style_height(&TextStyle::Small) * 4.0, height);
            ui.allocate_ui_with_layout(slot, right_layout, |ui| {
                ui.set_width(slot.x);
                let (label, visible, hint) = match trailing {
                    Trailing::Reset(changed) => ("Reset", changed, "Back to the default"),
                    Trailing::Remove => ("Remove", true, "Remove"),
                };
                let button = egui::Button::new(RichText::new(label).small().color(color(p.muted))).frame(false);
                clicked = ui.add_visible(visible, button).on_hover_text(hint).clicked();
            });
            let w = ui.available_width().min(CONTROL_WIDTH);
            ui.allocate_ui_with_layout(vec2(w, height), right_layout, |ui| {
                ui.set_width(w);
                control(ui);
            });
        })
        .response
        .rect;
    let bottom = (top_left.y + height).max(right_used.bottom());
    ui.advance_cursor_after_rect(Rect::from_min_max(top_left, egui::pos2(top_left.x + total, bottom)));
    clicked
}

/// A slider with its value in an editable box beside it, filling the width it's given.
/// Returns a response that is `changed()` when the value changed.
pub fn slider(
    ui: &mut Ui,
    p: &Palette,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    decimals: usize,
    unit: &str,
) -> Response {
    let (min, max) = (*range.start(), *range.end());
    let step = 10f64.powi(-(decimals as i32));
    let snap = |v: f64| ((v / step).round() * step).clamp(min, max);
    let h = ui.spacing().interact_size.y;
    let width = ui.available_width();
    let box_w = (ui.text_style_height(&TextStyle::Body) * 4.0).max(56.0);
    ui.allocate_ui_with_layout(vec2(width, h), Layout::left_to_right(Align::Center), |ui| {
        let track_w = (width - box_w - ui.spacing().item_spacing.x).max(40.0);
        let (rect, mut response) = ui.allocate_exact_size(vec2(track_w, h), Sense::click_and_drag());
        let knob_r = (h * 0.3).round();
        let x_range = (rect.left() + knob_r)..=(rect.right() - knob_r);

        let mut v = *value;
        if let Some(pos) = response.interact_pointer_pos() {
            let t = egui::remap_clamp(pos.x, x_range.clone(), 0.0..=1.0) as f64;
            v = snap(min + t * (max - min));
        }
        if response.has_focus() {
            let (down, up) = ui.input(|i| {
                let down = i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown);
                let up = i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp);
                (down, up)
            });
            v = snap(v + step * (up as i32 - down as i32) as f64);
        }
        if v != *value {
            *value = v;
            response.mark_changed();
        }

        if ui.is_rect_visible(rect) {
            let t = if max > min {
                ((*value - min) / (max - min)).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            let x = egui::lerp(x_range, t);
            let rail_h = (h * 0.22).round().max(4.0);
            let rail = egui::Rect::from_center_size(rect.center(), vec2(rect.width(), rail_h));
            let active = response.hovered() || response.dragged() || response.has_focus();
            let painter = ui.painter();
            painter.rect_filled(rail, rail_h / 2.0, color(if active { p.hover } else { p.raised }));
            let fill = egui::Rect::from_min_max(rail.min, egui::pos2(x, rail.max.y));
            painter.rect_filled(fill, rail_h / 2.0, color(p.accent));
            let r = if active { knob_r + 1.0 } else { knob_r };
            painter.circle_filled(egui::pos2(x, rect.center().y), r, color(p.text));
        }

        let suffix = if unit.is_empty() {
            String::new()
        } else {
            format!(" {unit}")
        };
        let speed = ((max - min) / 300.0).max(step);
        let typed = ui.add_sized(
            vec2(box_w, h),
            egui::DragValue::new(value)
                .range(min..=max)
                .speed(speed)
                .fixed_decimals(decimals)
                .suffix(suffix),
        );
        response | typed
    })
    .inner
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
        let track = kiln_config::theme::mix(off_track, p.accent, t);
        let knob = kiln_config::theme::mix(p.muted, p.on_accent, t);
        let radius = rect.height() / 2.0;
        ui.painter()
            .rect(rect, radius, color(track), egui::Stroke::NONE, StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), radius * 0.7, color(knob));
    }
    response
}

/// A drop-down of fixed choices: (value, label), filling the width it's given. Returns true
/// when the choice changed.
pub fn choice(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    value: &mut String,
    options: &[(&str, &str)],
) -> bool {
    let current = options
        .iter()
        .find(|(v, _)| *v == value.as_str())
        .map_or(value.clone(), |(_, l)| l.to_string());
    let before = value.clone();
    kiln_ui::select(ui, |ui| egui::ComboBox::from_id_salt(id)
        .selected_text(current)
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for (v, label) in options {
                ui.selectable_value(value, v.to_string(), *label);
            }
        }));
    *value != before
}

/// A single-line text field filling the width it's given.
pub fn text(ui: &mut Ui, value: &mut String, hint: &str) -> Response {
    let edit = egui::TextEdit::singleline(value)
        .hint_text(hint)
        .desired_width(f32::INFINITY);
    ui.add(padded(ui, edit))
}

/// A text field for values that only make sense once fully typed (like "800x600"). It keeps
/// its own text while focused, so half-typed input isn't replaced by the saved value each
/// frame. Returns true when the text changed; `value` then holds it.
pub fn draft_text(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut String, hint: &str) -> bool {
    let id = ui.id().with(id);
    let mut draft = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| value.clone());
    let r = text(ui, &mut draft, hint);
    if r.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, draft.clone()));
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
    }
    if r.changed() {
        *value = draft;
    }
    r.changed()
}

/// A text field as tall as the other controls, with its text clear of the rounded ends.
pub fn padded<'t>(ui: &Ui, edit: egui::TextEdit<'t>) -> egui::TextEdit<'t> {
    kiln_ui::field(ui, edit)
}

/// A color chip that opens a color picker when clicked. `changed()` when a color was picked.
pub fn swatch(ui: &mut Ui, p: &Palette, c: &mut Color32) -> Response {
    let h = ui.spacing().interact_size.y;
    let (rect, mut response) = ui.allocate_exact_size(vec2(h * 1.6, h), Sense::click());
    let radius = ui.visuals().widgets.inactive.corner_radius;
    if ui.is_rect_visible(rect) {
        // Outlined, so colors close to the card's still show.
        let outline = if response.hovered() { p.muted } else { p.separator };
        ui.painter().rect(
            rect,
            radius,
            *c,
            egui::Stroke::new(1.0, color(outline)),
            StrokeKind::Inside,
        );
    }
    let mut picked = false;
    egui::Popup::menu(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.spacing_mut().slider_width = 275.0;
            picked = egui::color_picker::color_picker_color32(ui, c, egui::color_picker::Alpha::OnlyBlend);
        });
    if picked {
        response.mark_changed();
    }
    response
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
