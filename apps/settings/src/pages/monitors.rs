//! Per-monitor settings (`[[outputs]]`): mode, scale, position, rotation, HDR, bar, clock and
//! layout.

use toml_edit::{Item, Table, Value};
use kiln_config::theme::Palette;
use kiln_ui::{
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

    arrangement(ui, p, doc, &connected);

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
                let hdr = get("hdr").and_then(|i| i.as_bool()).unwrap_or(false);
                widgets::row(
                    ui,
                    p,
                    info("HDR", "Brighter highlights and richer colours, on monitors that support HDR10."),
                    |ui| {
                        let mut on = hdr;
                        if widgets::toggle(ui, p, &mut on).changed() {
                            edits.push(("hdr", on.then(|| true.into())));
                        }
                    },
                );
                if hdr {
                    widgets::divider(ui, p);
                    widgets::row(
                        ui,
                        p,
                        info("SDR brightness", "How bright ordinary windows are in HDR mode."),
                        |ui| {
                            let mut nits = get("sdr_brightness")
                                .and_then(|i| crate::doc::as_number(&i))
                                .unwrap_or(kiln_config::config::DEFAULT_SDR_BRIGHTNESS as f64);
                            if widgets::slider(ui, p, &mut nits, 80.0..=600.0, 0, "nits").changed() {
                                let v = nits.round() as i64;
                                let default = kiln_config::config::DEFAULT_SDR_BRIGHTNESS as i64;
                                edits.push(("sdr_brightness", (v != default).then(|| v.into())));
                            }
                        },
                    );
                }

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
                widgets::row(
                    ui,
                    p,
                    info("Clock", "A 12-hour clock at the bottom of this bar."),
                    |ui| {
                        let mut on = get("clock").and_then(|i| i.as_bool()).unwrap_or(false);
                        if widgets::toggle(ui, p, &mut on).changed() {
                            edits.push(("clock", on.then(|| true.into())));
                        }
                    },
                );

                widgets::divider(ui, p);
                widgets::row(ui, p, info("Layout", "What its workspaces start with."), |ui| {
                    let mut l = get("layout")
                        .and_then(|i| i.as_str().map(String::from))
                        .unwrap_or_default();
                    let opts = [
                        ("", "Same as the Tiling page"),
                        ("tile", "Tile"),
                        ("vertical", "Vertical"),
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

/// A monitor on the map, in logical (scaled) pixels.
#[derive(Debug, Clone, PartialEq)]
struct Screen {
    name: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Screen {
    fn overlaps(&self, o: &Screen) -> bool {
        self.x < o.x + o.w - 0.5 && o.x < self.x + self.w - 0.5 && self.y < o.y + o.h - 0.5 && o.y < self.y + self.h - 0.5
    }
}

/// The connected monitors as they're arranged: their size at their scale and rotation, and
/// their position from the config (or where the compositor put them).
fn screens(doc: &ConfigDoc, connected: &[Monitor]) -> Vec<Screen> {
    let outputs: Vec<Table> = doc
        .item(&["outputs"])
        .and_then(Item::as_array_of_tables)
        .map(|a| a.iter().cloned().collect())
        .unwrap_or_default();
    connected
        .iter()
        .map(|m| {
            let entry = outputs.iter().find(|t| {
                t.get("name")
                    .and_then(Item::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(&m.name))
            });
            let get = |k: &str| entry.and_then(|t| t.get(k));
            let scale = get("scale").and_then(crate::doc::as_number).unwrap_or(1.0).max(0.1) as f32;
            let (mw, mh) = m
                .modes
                .iter()
                .find(|x| x.current)
                .map_or((1920, 1080), |x| (x.width, x.height));
            let rotated = get("transform")
                .and_then(Item::as_str)
                .is_some_and(|t| t.ends_with("90") || t.ends_with("270"));
            let (w, h) = if rotated { (mh, mw) } else { (mw, mh) };
            let pos = get("position").and_then(|i| {
                let a = i.as_array()?;
                Some((a.get(0)?.as_integer()? as f32, a.get(1)?.as_integer()? as f32))
            });
            let (x, y) = pos.unwrap_or((m.position.0 as f32, m.position.1 as f32));
            Screen {
                name: m.name.clone(),
                x,
                y,
                w: (w as f32 / scale).round(),
                h: (h as f32 / scale).round(),
            }
        })
        .collect()
}

/// Where a monitor dropped at (`x`, `y`) lands: touching one of the others along an edge, at
/// the nearest such spot that doesn't overlap any of them. Close edges line up exactly.
fn snap(dropped: &Screen, others: &[Screen]) -> (f32, f32) {
    if others.is_empty() {
        return (0.0, 0.0);
    }
    const ALIGN: f32 = 48.0;
    let d = dropped;
    let mut best: Option<((f32, f32), f32)> = None;
    for o in others {
        // Along a vertical edge (left or right of `o`), keep y but stay touching.
        let y = (d.y).clamp(o.y - d.h + 1.0, o.y + o.h - 1.0);
        let y = [o.y, o.y + o.h - d.h].into_iter().find(|a| (a - y).abs() < ALIGN).unwrap_or(y);
        let x = (d.x).clamp(o.x - d.w + 1.0, o.x + o.w - 1.0);
        let x = [o.x, o.x + o.w - d.w].into_iter().find(|a| (a - x).abs() < ALIGN).unwrap_or(x);
        for (cx, cy) in [(o.x + o.w, y), (o.x - d.w, y), (x, o.y + o.h), (x, o.y - d.h)] {
            let c = Screen {
                x: cx,
                y: cy,
                ..d.clone()
            };
            if others.iter().any(|s| s.overlaps(&c)) {
                continue;
            }
            let dist = (cx - d.x).hypot(cy - d.y);
            if best.is_none_or(|(_, b)| dist < b) {
                best = Some(((cx, cy), dist));
            }
        }
    }
    best.map_or((d.x, d.y), |(p, _)| p)
}

/// A map of the connected monitors. Drag one to move it; it snaps against the others, and
/// every monitor's position is saved.
fn arrangement(ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc, connected: &[Monitor]) {
    if connected.len() < 2 {
        return;
    }
    let screens = screens(doc, connected);
    let min_x = screens.iter().map(|s| s.x).fold(f32::MAX, f32::min);
    let min_y = screens.iter().map(|s| s.y).fold(f32::MAX, f32::min);
    let max_x = screens.iter().map(|s| s.x + s.w).fold(f32::MIN, f32::max);
    let max_y = screens.iter().map(|s| s.y + s.h).fold(f32::MIN, f32::max);
    let (bw, bh) = ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0));

    let mut save: Option<Vec<Screen>> = None;
    widgets::card(ui, p, "Arrangement", "Drag a monitor to move it. It snaps to the edges of the others.", |ui| {
        let width = ui.available_width();
        let height = 220.0f32.min(width * 0.45);
        let (area, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let pad = 16.0;
        // Leave room around the monitors so one can be dragged past the others.
        let k = ((width - 2.0 * pad) / (bw * 1.4)).min((height - 2.0 * pad) / (bh * 1.4));
        let origin = area.center() - egui::vec2(bw * k, bh * k) / 2.0;
        let to_ui = |s: &Screen| {
            egui::Rect::from_min_size(
                origin + egui::vec2((s.x - min_x) * k, (s.y - min_y) * k),
                egui::vec2(s.w * k, s.h * k),
            )
        };
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 6.0, color(p.base));

        let drag_id = ui.id().with("map-drag");
        let mut dragging: Option<(String, egui::Vec2)> = ui.data(|d| d.get_temp(drag_id));
        for s in &screens {
            let rect = to_ui(s);
            let r = ui.interact(rect, drag_id.with(&s.name), egui::Sense::drag());
            if r.drag_started()
                && let Some(pos) = r.interact_pointer_pos()
            {
                dragging = Some((s.name.clone(), pos - rect.min));
            }
            let held = dragging.as_ref().is_some_and(|(n, _)| *n == s.name);
            let shown = match (&dragging, r.interact_pointer_pos()) {
                (Some((n, grab)), Some(pos)) if *n == s.name => egui::Rect::from_min_size(pos - *grab, rect.size()),
                _ => rect,
            };
            if r.drag_stopped()
                && held
                && let Some((_, grab)) = &dragging
                && let Some(pos) = r.interact_pointer_pos().or(ui.input(|i| i.pointer.latest_pos()))
            {
                let corner = pos - *grab - origin;
                let dropped = Screen {
                    x: corner.x / k + min_x,
                    y: corner.y / k + min_y,
                    ..s.clone()
                };
                let others: Vec<Screen> = screens.iter().filter(|o| o.name != s.name).cloned().collect();
                let (x, y) = snap(&dropped, &others);
                let mut all = others;
                all.push(Screen { x, y, ..s.clone() });
                save = Some(all);
                dragging = None;
            }
            let fill = if held { p.selected } else if r.hovered() { p.hover } else { p.raised };
            let stroke = if held { p.accent } else { p.separator };
            painter.rect(shown, 4.0, color(fill), egui::Stroke::new(1.5, color(stroke)), egui::StrokeKind::Inside);
            painter.text(
                shown.center(),
                egui::Align2::CENTER_CENTER,
                &s.name,
                egui::FontId::proportional(13.0),
                color(p.text),
            );
            if r.hovered() || held {
                ui.ctx().set_cursor_icon(if held { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
            }
        }
        ui.data_mut(|d| match &dragging {
            Some(v) => {
                d.insert_temp(drag_id, v.clone());
            }
            None => d.remove::<(String, egui::Vec2)>(drag_id),
        });
    });

    // Save every monitor's position, shifted so the top-left one is at 0, 0.
    if let Some(all) = save {
        let min_x = all.iter().map(|s| s.x).fold(f32::MAX, f32::min);
        let min_y = all.iter().map(|s| s.y).fold(f32::MAX, f32::min);
        for s in &all {
            let t = entry_mut(doc, &s.name);
            let pos = [(s.x - min_x).round() as i64, (s.y - min_y).round() as i64];
            crate::doc::set_in(t, "position", Value::Array(toml_edit::Array::from_iter(pos)));
        }
        doc.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, x: f32, y: f32, w: f32, h: f32) -> Screen {
        Screen {
            name: name.into(),
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn snaps_to_the_nearest_free_edge() {
        let a = s("A", 0.0, 0.0, 1920.0, 1080.0);
        // Dropped overlapping A's right half: goes to A's right edge, tops lined up.
        assert_eq!(snap(&s("B", 1500.0, 30.0, 1280.0, 1024.0), std::slice::from_ref(&a)), (1920.0, 0.0));
        // Dropped below A, a little left: goes under it, left edges lined up.
        assert_eq!(snap(&s("B", -20.0, 1200.0, 1280.0, 1024.0), std::slice::from_ref(&a)), (0.0, 1080.0));
        // Never on top of a third monitor.
        let c = s("C", 1920.0, 0.0, 1920.0, 1080.0);
        let (x, y) = snap(&s("B", 1800.0, 10.0, 1280.0, 1024.0), &[a, c.clone()]);
        assert!(!s("B", x, y, 1280.0, 1024.0).overlaps(&c), "{x},{y}");
    }
}
