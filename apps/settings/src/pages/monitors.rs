//! Per-monitor settings (`[[outputs]]`): mode, scale, position, rotation, bar, clock and layout.

use toml_edit::{Item, Table, Value};
use wt_config::theme::Palette;
use wt_ui::{
    color,
    egui::{self, Align, Layout, RichText, Ui},
};

use super::Cache;
use crate::{
    doc::ConfigDoc,
    monitors::Monitor,
    widgets::{self, RowInfo},
};

const TRANSFORMS: &[(&str, &str)] = &[
    ("normal", "None"),
    ("90", "90°"),
    ("180", "180°"),
    ("270", "270°"),
    ("flipped", "Mirrored"),
    ("flipped-90", "Mirrored, 90°"),
    ("flipped-180", "Mirrored, 180°"),
    ("flipped-270", "Mirrored, 270°"),
];

pub fn show(ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc, cache: &mut Cache) {
    let configured: Vec<Table> = doc
        .user_item(&["outputs"])
        .and_then(Item::as_array_of_tables)
        .map(|a| a.iter().cloned().collect())
        .unwrap_or_default();
    let connected = cache.monitors().to_vec();

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        widgets::note(
            ui,
            p,
            "Changes apply as you make them. Positions are in scaled pixels, so a 2560 pixel wide \
             monitor at scale 1.5 is 1707 wide.",
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .button("Look again")
                .on_hover_text("Find monitors plugged in since")
                .clicked()
            {
                cache.refresh_monitors();
            }
        });
    });

    let name_of = |t: &Table| t.get("name").and_then(Item::as_str).unwrap_or_default().to_string();
    let mut names: Vec<(String, Option<Monitor>)> =
        connected.iter().map(|m| (m.name.clone(), Some(m.clone()))).collect();
    for t in &configured {
        let n = name_of(t);
        if !names.iter().any(|(m, _)| m.eq_ignore_ascii_case(&n)) {
            names.push((n, None));
        }
    }
    if names.is_empty() {
        widgets::note(ui, p, "No monitors found. Is the compositor running?");
    }

    for (name, monitor) in names {
        let entry = configured
            .iter()
            .find(|t| name_of(t).eq_ignore_ascii_case(&name))
            .cloned();
        let get = |k: &str| entry.as_ref().and_then(|t| t.get(k)).cloned();
        let mut edits: Vec<(&str, Option<Value>)> = Vec::new();
        let mut forget = false;

        ui.push_id(&name, |ui| {
            let title = match &monitor {
                Some(m) if !m.description.is_empty() => format!("{name}  ·  {}", m.description),
                Some(_) => name.clone(),
                None => format!("{name}  ·  not connected"),
            };
            widgets::card(ui, p, "", "", |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).strong().color(color(p.text)));
                    if entry.is_some() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            forget = ui
                                .add(egui::Button::new("Forget").small())
                                .on_hover_text("Remove this monitor's settings")
                                .clicked();
                        });
                    }
                });

                let mut enabled = get("enabled").and_then(|i| i.as_bool()).unwrap_or(true);
                let info = |label, help| RowInfo {
                    label,
                    help,
                    changed: false,
                    restart: false,
                };
                widgets::divider(ui, p);
                widgets::row(ui, p, info("On", "At least one monitor always stays on."), |ui| {
                    if widgets::toggle(ui, p, &mut enabled).changed() {
                        edits.push(("enabled", (!enabled).then(|| false.into())));
                    }
                });

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Resolution", ""), |ui| {
                    let mut mode = get("mode")
                        .and_then(|i| i.as_str().map(String::from))
                        .unwrap_or_default();
                    if mode == "preferred" {
                        mode.clear();
                    }
                    let specs: Vec<(String, String)> = monitor
                        .iter()
                        .flat_map(|m| &m.modes)
                        .map(|m| {
                            let label = format!(
                                "{} × {}  @ {:.2} Hz{}",
                                m.width,
                                m.height,
                                m.refresh as f64 / 1000.0,
                                if m.current { "  (now)" } else { "" }
                            );
                            (m.spec(), label)
                        })
                        .collect();
                    let mut opts: Vec<(&str, &str)> = vec![("", "The monitor's preferred one")];
                    opts.extend(specs.iter().map(|(s, l)| (s.as_str(), l.as_str())));
                    if !mode.is_empty() && !specs.iter().any(|(s, _)| *s == mode) {
                        opts.push((mode.as_str(), mode.as_str()));
                    }
                    let mut m = mode.clone();
                    if widgets::choice(ui, "mode", &mut m, &opts) {
                        edits.push(("mode", (!m.is_empty()).then(|| m.into())));
                    }
                });

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Scale", "Whole numbers look sharpest."), |ui| {
                    let mut scale = get("scale").and_then(|i| crate::doc::as_number(&i)).unwrap_or(1.0);
                    let r = ui.add(
                        egui::DragValue::new(&mut scale)
                            .range(0.5..=4.0)
                            .speed(0.01)
                            .fixed_decimals(2)
                            .suffix("×"),
                    );
                    if r.changed() {
                        let s = (scale * 100.0).round() / 100.0;
                        edits.push(("scale", (s != 1.0).then(|| s.into())));
                    }
                });

                widgets::divider(ui, p);
                widgets::row(
                    ui,
                    p,
                    info("Position", "Automatic places it right of the others."),
                    |ui| {
                        let pos = get("position").and_then(|i| {
                            let a = i.as_array()?;
                            Some([a.get(0)?.as_integer()?, a.get(1)?.as_integer()?])
                        });
                        let mut auto = pos.is_none();
                        let [mut x, mut y] = pos.unwrap_or([0, 0]);
                        let mut changed = false;
                        if !auto {
                            changed |= ui.add(egui::DragValue::new(&mut y).prefix("y ").speed(4)).changed();
                            changed |= ui.add(egui::DragValue::new(&mut x).prefix("x ").speed(4)).changed();
                        }
                        if ui.checkbox(&mut auto, "Automatic").changed() {
                            changed = true;
                        }
                        if changed {
                            let v = (!auto).then(|| Value::Array(toml_edit::Array::from_iter([x, y])));
                            edits.push(("position", v));
                        }
                    },
                );

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Rotation", "Counter-clockwise."), |ui| {
                    let mut t = get("transform")
                        .and_then(|i| i.as_str().map(String::from))
                        .unwrap_or_else(|| "normal".into());
                    if widgets::choice(ui, "transform", &mut t, TRANSFORMS) {
                        edits.push(("transform", (t != "normal").then(|| t.into())));
                    }
                });

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Bar", ""), |ui| {
                    let mut b = get("bar")
                        .and_then(|i| i.as_str().map(String::from))
                        .unwrap_or_default();
                    let opts = [
                        ("", "Same as the Bar page"),
                        ("top", "Top"),
                        ("bottom", "Bottom"),
                        ("left", "Left"),
                        ("right", "Right"),
                        ("none", "Hidden"),
                    ];
                    if widgets::choice(ui, "bar", &mut b, &opts) {
                        edits.push(("bar", (!b.is_empty()).then(|| b.into())));
                    }
                });

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Clock", "A 12-hour clock at the bottom of this bar."), |ui| {
                    let mut on = get("clock").and_then(|i| i.as_bool()).unwrap_or(false);
                    if widgets::toggle(ui, p, &mut on).changed() {
                        edits.push(("clock", on.then(|| true.into())));
                    }
                });

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Layout", "What its workspaces start with."), |ui| {
                    let mut l = get("layout")
                        .and_then(|i| i.as_str().map(String::from))
                        .unwrap_or_default();
                    let opts = [
                        ("", "Same as the Tiling page"),
                        ("tile", "Tile"),
                        ("monocle", "Monocle"),
                    ];
                    if widgets::choice(ui, "layout", &mut l, &opts) {
                        edits.push(("layout", (!l.is_empty()).then(|| l.into())));
                    }
                });
            });
        });

        if forget {
            remove_entry(doc, &name);
        } else if !edits.is_empty() {
            let t = entry_mut(doc, &name);
            for (key, value) in edits {
                match value {
                    Some(v) => crate::doc::set_in(t, key, v),
                    None => {
                        t.remove(key);
                    }
                }
            }
            doc.touch();
        }
    }
}

/// The `[[outputs]]` entry for a monitor, added if there isn't one.
fn entry_mut<'a>(doc: &'a mut ConfigDoc, name: &str) -> &'a mut Table {
    let list = doc.array_of_tables_mut("outputs").expect("outputs is a list of tables");
    let found = list.iter().position(|t| {
        t.get("name")
            .and_then(Item::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    });
    let i = found.unwrap_or_else(|| {
        let mut t = Table::new();
        t.insert("name", toml_edit::value(name));
        list.push(t);
        list.len() - 1
    });
    list.get_mut(i).unwrap()
}

fn remove_entry(doc: &mut ConfigDoc, name: &str) {
    let Some(list) = doc.array_of_tables_mut("outputs") else {
        return;
    };
    list.retain(|t| {
        !t.get("name")
            .and_then(Item::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    });
    if list.is_empty() {
        doc.remove(&["outputs"]);
    }
    doc.touch();
}
