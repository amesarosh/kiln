//! The palette editor: every named color with a swatch, its formula and what it's used for.

use toml_edit::Item;
use wt_config::theme::{self, ColorDefs, Palette};
use wt_ui::{
    color,
    egui::{self, Align, Color32, Layout, RichText, Ui, color_picker::Alpha, vec2},
};

use crate::{doc::ConfigDoc, widgets};

const PATH: [&str; 2] = ["theme", "colors"];

#[derive(Default)]
pub struct ColorsPage {
    new_name: String,
}

impl ColorsPage {
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc) {
        let defaults = names(doc.default_item(&PATH));
        let user = names(doc.user_item(&PATH));
        // Default order first (palette, then derived shades), then the user's own names.
        let mut order: Vec<String> = defaults.iter().map(|(k, _)| k.clone()).collect();
        order.extend(
            user.iter()
                .map(|(k, _)| k.clone())
                .filter(|k| !defaults.iter().any(|(d, _)| d == k)),
        );
        let mut defs: ColorDefs = defaults.iter().cloned().collect();
        defs.extend(user.iter().cloned());
        let resolved = theme::resolve_each(&defs);

        widgets::card(
            ui,
            p,
            "Colors",
            "Every part of the desktop refers to these names. A value is a #hex color, another name, \
             or mix(a, b, 0.2): 20% of the way from a to b. Shades built with mix follow along when \
             you change the colors they're made from; picking a color for one replaces its formula.",
            |ui| {
                for (i, name) in order.iter().enumerate() {
                    if i > 0 {
                        widgets::divider(ui, p);
                    }
                    let is_default = defaults.iter().any(|(k, _)| k == name);
                    let mut expr = defs[name].clone();
                    let path = [PATH[0], PATH[1], name.as_str()];
                    let changed = doc.changed(&path);
                    let mut action = None;
                    ui.horizontal(|ui| {
                        let current = resolved.get(name).and_then(|r| r.as_ref().ok()).copied();
                        let mut c = current.map_or(Color32::TRANSPARENT, color);
                        let r = egui::color_picker::color_edit_button_srgba(ui, &mut c, Alpha::OnlyBlend);
                        if r.changed() {
                            action = Some(Some(hex(c)));
                        }
                        // Outline it, so colors close to the card's (base, surface) still show.
                        ui.painter().rect_stroke(
                            r.rect,
                            ui.visuals().widgets.inactive.corner_radius,
                            egui::Stroke::new(1.0, color(p.separator)),
                            egui::StrokeKind::Outside,
                        );
                        ui.allocate_ui_with_layout(vec2(170.0, 0.0), Layout::top_down(Align::Min), |ui| {
                            ui.set_width(170.0);
                            ui.label(RichText::new(name).monospace().color(color(p.text)));
                            if let Some((_, doc)) = Palette::ROLE_DOCS.iter().find(|(r, _)| r == name) {
                                ui.label(RichText::new(doc.replace('`', "")).small().color(color(p.subtext)));
                            } else if !is_default {
                                ui.label(RichText::new("Your own color").small().color(color(p.subtext)));
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if is_default {
                                let r = ui
                                    .add_visible(
                                        changed,
                                        egui::Button::new(RichText::new("Reset").small()).frame(false),
                                    )
                                    .on_hover_text("Back to the default");
                                if r.clicked() {
                                    action = Some(None);
                                }
                            } else if ui.add(egui::Button::new("Remove").small()).clicked() {
                                action = Some(None);
                            }
                            if widgets::text(ui, &mut expr, "#rrggbb").changed() {
                                action = Some(Some(expr.clone()));
                            }
                        });
                    });
                    if let Some(Err(e)) = resolved.get(name) {
                        widgets::error(ui, p, e);
                    }
                    match action {
                        Some(Some(v)) => doc.set(&path, v),
                        Some(None) if is_default => doc.reset(&path),
                        Some(None) => doc.remove(&path),
                        None => {}
                    }
                }
            },
        );

        widgets::card(
            ui,
            p,
            "Add a color",
            "Give a color a name to use it in other colors, e.g. pink, then set accent to pink.",
            |ui| {
                ui.horizontal(|ui| {
                    let valid = !self.new_name.is_empty()
                        && self
                            .new_name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                        && !defs.contains_key(&self.new_name);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(valid, egui::Button::new("Add")).clicked() {
                            doc.set(&[PATH[0], PATH[1], &self.new_name], "#888888");
                            self.new_name.clear();
                        }
                        widgets::text(ui, &mut self.new_name, "name, e.g. pink");
                    });
                });
            },
        );
    }
}

/// (name, expression) pairs of a colors table, in file order.
fn names(item: Option<&Item>) -> Vec<(String, String)> {
    let Some(table) = item.and_then(Item::as_table_like) else {
        return Vec::new();
    };
    table
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.as_str()?.to_string())))
        .collect()
}

fn hex(c: Color32) -> String {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}
