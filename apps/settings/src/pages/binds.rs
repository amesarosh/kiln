//! The keybind editor: one row per bind, with the key combination, the action and its argument.

use std::collections::HashMap;

use toml_edit::{Item, Key, Table};
use wt_config::{ACTIONS, check_bind, parse_combo, theme::Palette};
use wt_ui::{
    color,
    egui::{self, Align, Layout, RichText, Ui},
};

use crate::{doc::ConfigDoc, widgets};

#[derive(Default)]
pub struct BindsPage {
    filter: String,
    /// Key combinations being typed, by the combination they started as. They're only written
    /// once they're valid and the field loses focus, since the combination is the bind's name.
    editing: HashMap<String, String>,
    new_combo: String,
    new_action: String,
    generation: u64,
}

impl BindsPage {
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc) {
        if self.generation != doc.generation {
            self.generation = doc.generation;
            self.editing.clear();
        }
        let own = doc.user_item(&["binds"]).is_some();
        let binds: Vec<(String, String)> = doc
            .item(&["binds"])
            .and_then(Item::as_table_like)
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| Some((k.to_string(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        widgets::card(ui, p, "Add a keybind", "", |ui| {
            let combo_err = (!self.new_combo.is_empty())
                .then(|| parse_combo(&self.new_combo).err())
                .flatten()
                .or_else(|| {
                    binds
                        .iter()
                        .any(|(c, _)| c.eq_ignore_ascii_case(&self.new_combo))
                        .then(|| "already bound".to_string())
                });
            if self.new_action.is_empty() {
                self.new_action = "spawn ".into();
            }
            let mut add = false;
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_combo)
                        .hint_text("Mod+Shift+b")
                        .desired_width(170.0),
                );
                action_editor(ui, "new-action", &mut self.new_action);
                let ok =
                    !self.new_combo.is_empty() && combo_err.is_none() && check_bind("Mod", &self.new_action).is_ok();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    add = ui.add_enabled(ok, egui::Button::new("Add")).clicked();
                });
            });
            if let Some(e) = combo_err {
                widgets::error(ui, p, &e);
            }
            widgets::note(
                ui,
                p,
                "Modifiers: Mod, Super, Alt, Ctrl, Shift. Keys use xkb names: Return, space, a, 1, F1, Print, \
                 period, comma, XF86AudioRaiseVolume... A bare \"Mod\" fires when you tap Mod on its own.",
            );
            if add {
                let combo = std::mem::take(&mut self.new_combo);
                let action = std::mem::take(&mut self.new_action);
                if let Some(t) = own_binds(doc) {
                    t.insert(&combo, toml_edit::value(action.trim()));
                    doc.touch();
                }
            }
        });

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Keybinds").strong().color(color(p.text)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("Search")
                        .desired_width(200.0),
                );
            });
        });
        if !own {
            widgets::note(
                ui,
                p,
                "These are the built-in keybinds. Changing any of them copies them all into your config, \
                 where they replace the built-in ones.",
            );
        }
        ui.add_space(4.0);

        let q = self.filter.to_lowercase();
        let mut rename = None;
        let mut set = None;
        let mut remove = None;
        egui::Frame::new()
            .fill(color(p.surface))
            .corner_radius(ui.visuals().widgets.noninteractive.corner_radius)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut first = true;
                for (combo, action) in &binds {
                    if !q.is_empty() && !combo.to_lowercase().contains(&q) && !action.to_lowercase().contains(&q) {
                        continue;
                    }
                    if !first {
                        widgets::divider(ui, p);
                    }
                    first = false;
                    ui.push_id(combo, |ui| {
                        let mut buf = self.editing.get(combo).cloned().unwrap_or_else(|| combo.clone());
                        let mut action_buf = action.clone();
                        let mut combo_err = None;
                        ui.horizontal(|ui| {
                            let r = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(170.0));
                            if r.changed() {
                                self.editing.insert(combo.clone(), buf.clone());
                            }
                            if buf != *combo {
                                combo_err = parse_combo(&buf).err().or_else(|| {
                                    binds
                                        .iter()
                                        .any(|(c, _)| c != combo && c.eq_ignore_ascii_case(&buf))
                                        .then(|| "already bound".to_string())
                                });
                                if r.lost_focus() && combo_err.is_none() {
                                    rename = Some((combo.clone(), buf.clone()));
                                }
                            }
                            if action_editor(ui, combo, &mut action_buf) {
                                set = Some((combo.clone(), action_buf.clone()));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.add(egui::Button::new("Remove").small()).clicked() {
                                    remove = Some(combo.clone());
                                }
                            });
                        });
                        if let Some(e) = combo_err {
                            widgets::error(ui, p, &format!("{e} (press Enter or click away to apply)"));
                        } else if let Err(e) = check_bind(combo, &action_buf) {
                            widgets::error(ui, p, &e);
                        }
                    });
                }
                if first {
                    widgets::note(ui, p, "No keybinds match.");
                }
            });

        if let Some((old, new)) = rename {
            self.editing.remove(&old);
            if let Some(t) = own_binds(doc) {
                rename_key(t, &old, &new);
                doc.touch();
            }
        }
        if let Some((combo, action)) = set
            && let Some(t) = own_binds(doc)
        {
            crate::doc::set_in(t, &combo, action.trim().into());
            doc.touch();
        }
        if let Some(combo) = remove
            && let Some(t) = own_binds(doc)
        {
            t.remove(&combo);
            doc.touch();
        }
    }
}

/// The action picker and its argument. Returns true when the action string changed.
fn action_editor(ui: &mut Ui, id: &str, action: &mut String) -> bool {
    let (name, arg) = match action.trim_start().split_once(char::is_whitespace) {
        Some((n, a)) => (n.to_string(), a.trim_start().to_string()),
        None => (action.trim().to_string(), String::new()),
    };
    let mut new_name = name.clone();
    let mut new_arg = arg.clone();
    let known = ACTIONS.iter().find(|(n, _, _)| *n == name);
    egui::ComboBox::from_id_salt(("action", id))
        .selected_text(if name.is_empty() {
            "Pick an action"
        } else {
            name.as_str()
        })
        .width(170.0)
        .height(400.0)
        .show_ui(ui, |ui| {
            for (n, _, help) in ACTIONS {
                ui.selectable_value(&mut new_name, n.to_string(), *n)
                    .on_hover_text(*help);
            }
        });
    let hint = known.map_or("", |(_, a, _)| *a);
    if !hint.is_empty() || !arg.is_empty() {
        ui.add(
            egui::TextEdit::singleline(&mut new_arg)
                .hint_text(hint)
                .desired_width((ui.available_width() - 80.0).max(80.0)),
        );
    }
    if new_name != name {
        // A different action takes a different argument; start from its first example.
        let example = ACTIONS
            .iter()
            .find(|(n, _, _)| *n == new_name)
            .map_or("", |(_, a, _)| a.split('|').next().unwrap_or(""));
        new_arg = match example {
            "command" | "number" => String::new(),
            e => e.to_string(),
        };
    }
    let rebuilt = if new_arg.is_empty() && ACTIONS.iter().any(|(n, a, _)| *n == new_name && a.is_empty()) {
        new_name
    } else {
        format!("{new_name} {new_arg}")
    };
    if rebuilt != *action {
        *action = rebuilt;
        true
    } else {
        false
    }
}

/// The user's own `[binds]`, first copying the built-in ones in if there isn't one yet, since a
/// `[binds]` table replaces the defaults entirely.
fn own_binds(doc: &mut ConfigDoc) -> Option<&mut Table> {
    if doc.user_item(&["binds"]).is_none() {
        let defaults = doc.default_item(&["binds"]).and_then(Item::as_table).cloned()?;
        let t = doc.table_mut(&[])?;
        t.insert("binds", Item::Table(defaults));
    }
    doc.table_mut(&["binds"])
}

/// Rename a key in place, keeping its position and comments.
fn rename_key(t: &mut Table, old: &str, new: &str) {
    let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
    let entries: Vec<(Key, Item)> = keys.iter().filter_map(|k| t.remove_entry(k)).collect();
    for (key, item) in entries {
        let key = if key.get() == old {
            Key::new(new).with_leaf_decor(key.leaf_decor().clone())
        } else {
            key
        };
        t.insert_formatted(&key, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_keeps_order_and_comments() {
        let mut doc: toml_edit::DocumentMut = "[binds]\n\"a\" = \"x\"\n# about b\n\"b\" = \"y\"\n\"c\" = \"z\"\n"
            .parse()
            .unwrap();
        let t = doc["binds"].as_table_mut().unwrap();
        rename_key(t, "b", "B");
        let text = doc.to_string();
        assert!(text.contains("\"a\" = \"x\"\n# about b\nB = \"y\"\n\"c\""), "{text}");
    }
}
