//! Names and icons for workspaces, shown on the bar instead of their numbers.

use kiln_config::theme::Palette;
use kiln_ui::egui::{self, Ui};
use toml_edit::{Item, Value};

use crate::{doc::ConfigDoc, widgets};

pub fn show(ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc) {
    let count = doc.int(&["layout", "workspaces"]).clamp(1, 32);
    widgets::card(
        ui,
        p,
        "Workspace names",
        "Shown on the bar instead of the number. The icon is an icon name from your icon theme, \
         or an app id like firefox. Keybinds still use the numbers.",
        |ui| {
            for n in 1..=count {
                let key = n.to_string();
                let get = |field: &str| doc.str(&["workspaces", &key, field]);
                let (mut name, mut icon) = (get("name"), get("icon"));
                if n > 1 {
                    widgets::divider(ui, p);
                }
                let mut edit = None;
                widgets::row(
                    ui,
                    p,
                    widgets::RowInfo {
                        label: &format!("Workspace {n}"),
                        help: "",
                        changed: false,
                        restart: false,
                    },
                    |ui| {
                        let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                        ui.horizontal(|ui| {
                            ui.allocate_ui(egui::vec2(w, ui.spacing().interact_size.y), |ui| {
                                if widgets::draft_text(ui, ("ws-name", n), &mut name, "name") {
                                    edit = Some(("name", name.clone()));
                                }
                            });
                            ui.allocate_ui(egui::vec2(w, ui.spacing().interact_size.y), |ui| {
                                if widgets::draft_text(ui, ("ws-icon", n), &mut icon, "icon") {
                                    edit = Some(("icon", icon.clone()));
                                }
                            });
                        });
                    },
                );
                if let Some((field, value)) = edit {
                    set_label(doc, &key, field, value.trim());
                }
            }
        },
    );
}

/// Set or clear one field of a workspace's label, dropping tables that end up empty.
fn set_label(doc: &mut ConfigDoc, key: &str, field: &str, value: &str) {
    let Some(table) = doc.table_mut(&["workspaces"]) else {
        return;
    };
    let entry = table
        .entry(key)
        .or_insert_with(|| Item::Value(Value::InlineTable(Default::default())));
    if let Some(t) = entry.as_inline_table_mut() {
        if value.is_empty() {
            t.remove(field);
        } else {
            t.insert(field, value.into());
        }
        if t.is_empty() {
            table.remove(key);
        }
    } else if let Some(t) = entry.as_table_mut() {
        if value.is_empty() {
            t.remove(field);
        } else {
            t.insert(field, toml_edit::value(value));
        }
        if t.is_empty() {
            table.remove(key);
        }
    }
    if table.is_empty() {
        doc.remove(&["workspaces"]);
    }
    doc.touch();
}
