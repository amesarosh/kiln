//! Window rules: which windows float, and which workspace or monitor they open on.

use toml_edit::{Item, Table};
use wt_config::theme::Palette;
use wt_ui::{
    color,
    egui::{self, Align, Layout, RichText, Ui},
};

use super::Cache;
use crate::{doc::ConfigDoc, widgets};

pub fn show(ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc, cache: &mut Cache) {
    let rules: Vec<Table> = doc
        .user_item(&["rules"])
        .and_then(Item::as_array_of_tables)
        .map(|a| a.iter().cloned().collect())
        .unwrap_or_default();
    let workspaces = doc.int(&["layout", "workspaces"]).max(1);
    let monitors: Vec<String> = cache.monitors().iter().map(|m| m.name.clone()).collect();

    ui.add_space(14.0);
    widgets::note(
        ui,
        p,
        "A rule applies to windows whose app id matches exactly (a * at the end matches any ending), \
         and whose title contains the given text. Leave either empty to match any. Rules apply to \
         windows as they open; when several match, later ones win.",
    );

    let mut remove = None;
    for (i, rule) in rules.iter().enumerate() {
        let get = |k: &str| rule.get(k).and_then(Item::as_str).unwrap_or_default().to_string();
        let mut app_id = get("app_id");
        let mut title = get("title");
        let output = get("output");
        let mut floating = match rule.get("floating").and_then(Item::as_bool) {
            None => "",
            Some(true) => "float",
            Some(false) => "tile",
        }
        .to_string();
        let mut workspace = rule
            .get("workspace")
            .and_then(Item::as_integer)
            .map_or(String::new(), |w| w.to_string());

        let heading = match (app_id.is_empty(), title.is_empty()) {
            (false, _) => app_id.clone(),
            (true, false) => format!("Title contains \"{title}\""),
            (true, true) => "New rule".to_string(),
        };
        let mut edits: Vec<(&str, Option<toml_edit::Value>)> = Vec::new();
        ui.push_id(i, |ui| {
            widgets::card(ui, p, "", "", |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&heading).strong().color(color(p.text)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(egui::Button::new("Remove").small()).clicked() {
                            remove = Some(i);
                        }
                    });
                });
                let text_row = |ui: &mut Ui, label: &str, help: &str, v: &mut String, hint: &str| {
                    let mut changed = false;
                    widgets::row(
                        ui,
                        p,
                        widgets::RowInfo {
                            label,
                            help,
                            changed: false,
                            restart: false,
                        },
                        |ui| changed = widgets::draft_text(ui, label, v, hint),
                    );
                    changed
                };
                widgets::divider(ui, p);
                if text_row(ui, "App id", "", &mut app_id, "e.g. pavucontrol") {
                    edits.push(("app_id", (!app_id.is_empty()).then(|| app_id.clone().into())));
                }
                widgets::divider(ui, p);
                if text_row(ui, "Title contains", "", &mut title, "any title") {
                    edits.push(("title", (!title.is_empty()).then(|| title.clone().into())));
                }
                widgets::divider(ui, p);
                widgets::row(
                    ui,
                    p,
                    widgets::RowInfo {
                        label: "Floating",
                        help: "",
                        changed: false,
                        restart: false,
                    },
                    |ui| {
                        let opts = [("", "Don't change"), ("float", "Float"), ("tile", "Tile")];
                        if widgets::choice(ui, "floating", &mut floating, &opts) {
                            edits.push(("floating", (!floating.is_empty()).then(|| (floating == "float").into())));
                        }
                    },
                );
                widgets::divider(ui, p);
                widgets::row(
                    ui,
                    p,
                    widgets::RowInfo {
                        label: "Workspace",
                        help: "",
                        changed: false,
                        restart: false,
                    },
                    |ui| {
                        let numbers: Vec<String> = (1..=workspaces).map(|n| n.to_string()).collect();
                        let opts: Vec<(&str, &str)> = std::iter::once(("", "Current"))
                            .chain(numbers.iter().map(|n| (n.as_str(), n.as_str())))
                            .collect();
                        if widgets::choice(ui, "workspace", &mut workspace, &opts) {
                            edits.push(("workspace", workspace.parse::<i64>().ok().map(Into::into)));
                        }
                    },
                );
                widgets::divider(ui, p);
                widgets::row(
                    ui,
                    p,
                    widgets::RowInfo {
                        label: "Monitor",
                        help: "",
                        changed: false,
                        restart: false,
                    },
                    |ui| {
                        let mut opts: Vec<(&str, &str)> = vec![("", "Focused")];
                        opts.extend(monitors.iter().map(|m| (m.as_str(), m.as_str())));
                        if !output.is_empty() && !monitors.contains(&output) {
                            opts.push((output.as_str(), output.as_str()));
                        }
                        let mut o = output.clone();
                        if widgets::choice(ui, "output", &mut o, &opts) {
                            edits.push(("output", (!o.is_empty()).then(|| o.into())));
                        }
                    },
                );
                widgets::divider(ui, p);
                let pair = |key: &str| -> Option<(i64, i64)> {
                    let a = rule.get(key)?.as_array()?;
                    Some((a.get(0)?.as_integer()?, a.get(1)?.as_integer()?))
                };
                let mut size = pair("size").map_or(String::new(), |(w, h)| format!("{w}x{h}"));
                if text_row(
                    ui,
                    "Size",
                    "Width and height of a floating window",
                    &mut size,
                    "e.g. 800x600",
                ) {
                    match parse_pair(&size, 'x') {
                        Some((w, h)) if w > 0 && h > 0 => edits.push(("size", Some(pair_value(w, h)))),
                        _ if size.trim().is_empty() => edits.push(("size", None)),
                        _ => {}
                    }
                }
                widgets::divider(ui, p);
                let mut position = match (rule.get("position").and_then(Item::as_str), pair("position")) {
                    (Some(_), _) => "center".to_string(),
                    (None, Some((x, y))) => format!("{x}, {y}"),
                    (None, None) => String::new(),
                };
                if text_row(
                    ui,
                    "Position",
                    "Where a floating window opens: center, or x, y from the top-left corner",
                    &mut position,
                    "center",
                ) {
                    let t = position.trim();
                    if t.is_empty() {
                        edits.push(("position", None));
                    } else if t.eq_ignore_ascii_case("center") {
                        edits.push(("position", Some("center".into())));
                    } else if let Some((x, y)) = parse_pair(t, ',') {
                        edits.push(("position", Some(pair_value(x, y))));
                    }
                }
                let tri = |key: &str| match rule.get(key).and_then(Item::as_bool) {
                    None => String::new(),
                    Some(true) => "yes".into(),
                    Some(false) => "no".into(),
                };
                for (key, label, help, yes, no) in [
                    ("fullscreen", "Fullscreen", "", "Open fullscreen", "Open normally"),
                    (
                        "focus",
                        "Take focus",
                        "Whether the window takes the keyboard when it opens",
                        "Yes",
                        "No, open behind",
                    ),
                ] {
                    widgets::divider(ui, p);
                    let mut v = tri(key);
                    widgets::row(
                        ui,
                        p,
                        widgets::RowInfo {
                            label,
                            help,
                            changed: false,
                            restart: false,
                        },
                        |ui| {
                            let opts = [("", "Don't change"), ("yes", yes), ("no", no)];
                            if widgets::choice(ui, key, &mut v, &opts) {
                                edits.push((key, (!v.is_empty()).then(|| (v == "yes").into())));
                            }
                        },
                    );
                }
                widgets::divider(ui, p);
                let mut opacity = rule.get("opacity").and_then(crate::doc::as_number).unwrap_or(1.0);
                widgets::row(
                    ui,
                    p,
                    widgets::RowInfo {
                        label: "Opacity",
                        help: "Below 1, you can see through the window",
                        changed: false,
                        restart: false,
                    },
                    |ui| {
                        if widgets::slider(ui, p, &mut opacity, 0.1..=1.0, 2, "").changed() {
                            let v = (opacity * 100.0).round() / 100.0;
                            edits.push(("opacity", (v < 1.0).then(|| v.into())));
                        }
                    },
                );
                if app_id.is_empty() && title.is_empty() {
                    widgets::error(ui, p, "Needs an app id or a title.");
                }
            });
        });
        if !edits.is_empty()
            && let Some(t) = doc.array_of_tables_mut("rules").and_then(|a| a.get_mut(i))
        {
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

    if let Some(i) = remove
        && let Some(a) = doc.array_of_tables_mut("rules")
    {
        a.remove(i);
        if a.is_empty() {
            doc.remove(&["rules"]);
        }
        doc.touch();
    }

    ui.add_space(14.0);
    if ui.button("Add a rule").clicked()
        && let Some(a) = doc.array_of_tables_mut("rules")
    {
        let mut t = Table::new();
        // A rule with nothing to match is rejected; start it out as an example to edit.
        t.insert("app_id", toml_edit::value("app-id"));
        t.insert("floating", toml_edit::value(true));
        a.push(t);
        doc.touch();
    }
}

/// "800x600" or "10, 20" → the two numbers.
fn parse_pair(s: &str, sep: char) -> Option<(i64, i64)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn pair_value(a: i64, b: i64) -> toml_edit::Value {
    let mut arr = toml_edit::Array::new();
    arr.push(a);
    arr.push(b);
    arr.into()
}
