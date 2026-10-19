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
        "A rule applies to windows whose app id matches exactly, and whose title contains the given \
         text. Leave either empty to match any. Rules apply to windows as they open.",
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
                        |ui| changed = widgets::text(ui, v, hint).changed(),
                    );
                    changed
                };
                if text_row(ui, "App id", "", &mut app_id, "e.g. pavucontrol") {
                    edits.push(("app_id", (!app_id.is_empty()).then(|| app_id.clone().into())));
                }
                if text_row(ui, "Title contains", "", &mut title, "any title") {
                    edits.push(("title", (!title.is_empty()).then(|| title.clone().into())));
                }
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
                        if widgets::choice(ui, "floating", &mut floating, &opts, 200.0) {
                            edits.push(("floating", (!floating.is_empty()).then(|| (floating == "float").into())));
                        }
                    },
                );
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
                        if widgets::choice(ui, "workspace", &mut workspace, &opts, 200.0) {
                            edits.push(("workspace", workspace.parse::<i64>().ok().map(Into::into)));
                        }
                    },
                );
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
                        if widgets::choice(ui, "output", &mut o, &opts, 200.0) {
                            edits.push(("output", (!o.is_empty()).then(|| o.into())));
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
