//! Plain settings, described as data: where they live in the file, how they're labelled and
//! which control edits them. Pages list sections of these; the search box looks through them.

use wt_config::theme::Palette;
use wt_ui::egui::{self, Ui};

use super::Cache;
use crate::{
    doc::ConfigDoc,
    widgets::{self, RowInfo},
};

pub enum Kind {
    Toggle,
    /// Whole number in a range, with a unit shown after it.
    Int(i64, i64, &'static str),
    /// Number in a range, rounded to this many decimals.
    Float(f64, f64, usize),
    /// Seconds, 0 meaning never.
    Seconds,
    Choice(&'static [(&'static str, &'static str)]),
    /// Free text, with a hint shown while empty.
    Text(&'static str),
    /// A list of strings, with a hint for new entries.
    List(&'static str),
    Font,
    IconTheme,
}

pub struct Field {
    pub path: &'static [&'static str],
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub restart: bool,
}

pub struct Section {
    pub title: &'static str,
    pub note: &'static str,
    pub fields: &'static [Field],
}

const fn f(path: &'static [&'static str], label: &'static str, help: &'static str, kind: Kind) -> Field {
    Field {
        path,
        label,
        help,
        kind,
        restart: false,
    }
}

const fn at_start(field: Field) -> Field {
    Field { restart: true, ..field }
}

pub const GENERAL: &[Section] = &[
    Section {
        title: "Behavior",
        note: "",
        fields: &[
            f(
                &["general", "mod"],
                "Mod key",
                "The key used as \"Mod\" in keybinds. Automatic uses Alt in a nested window and Super otherwise.",
                Kind::Choice(&[
                    ("auto", "Automatic"),
                    ("super", "Super"),
                    ("alt", "Alt"),
                    ("ctrl", "Ctrl"),
                ]),
            ),
            f(
                &["general", "focus_follows_mouse"],
                "Focus follows the mouse",
                "Focus the window under the pointer as it moves.",
                Kind::Toggle,
            ),
            f(
                &["general", "new_window"],
                "New windows go",
                "Where a new tiled window is placed.",
                Kind::Choice(&[("master", "In the master area"), ("stack", "At the end of the stack")]),
            ),
            f(
                &["general", "keep_clipboard"],
                "Keep the clipboard",
                "Keep what you copied after the app you copied from closes.",
                Kind::Toggle,
            ),
        ],
    },
    Section {
        title: "Power",
        note: "",
        fields: &[f(
            &["general", "monitors_off_after"],
            "Turn monitors off after",
            "Time without input before monitors turn off. Any key, click or pointer movement turns them back on.",
            Kind::Seconds,
        )],
    },
    Section {
        title: "Startup",
        note: "",
        fields: &[
            at_start(f(
                &["general", "autostart"],
                "Run at startup",
                "Commands run once when the session starts, through sh -c.",
                Kind::List("command, e.g. waybar"),
            )),
            at_start(f(
                &["general", "xwayland"],
                "X11 apps",
                "Run X11-only apps (Steam, many games) through Xwayland.",
                Kind::Toggle,
            )),
        ],
    },
];

pub const APPEARANCE: &[Section] = &[
    Section {
        title: "Text and shapes",
        note: "Used by the bar, menus, the window switcher and these settings.",
        fields: &[
            f(&["theme", "font"], "Font", "", Kind::Font),
            f(&["theme", "font_size"], "Font size", "", Kind::Float(6.0, 48.0, 0)),
            f(
                &["theme", "radius"],
                "Corner rounding",
                "Of buttons, tabs, menus and panels. 0 is square.",
                Kind::Float(0.0, 24.0, 0),
            ),
            f(
                &["theme", "icon_theme"],
                "Icon theme",
                "Where app icons come from.",
                Kind::IconTheme,
            ),
        ],
    },
    Section {
        title: "Windows",
        note: "",
        fields: &[f(
            &["decoration", "border_width"],
            "Border width",
            "Colors are the border and border_focused colors below.",
            Kind::Int(0, 20, "px"),
        )],
    },
];

pub const LAYOUT: &[Section] = &[
    Section {
        title: "Tiling",
        note: "Changing these resets the layout of every workspace.",
        fields: &[
            f(
                &["layout", "default"],
                "Layout",
                "How workspaces start. A monitor can have its own on the Monitors page.",
                Kind::Choice(&[
                    ("tile", "Tile: master and stack"),
                    ("monocle", "Monocle: one window at a time"),
                ]),
            ),
            f(
                &["layout", "master_ratio"],
                "Master width",
                "Share of the screen given to the master area.",
                Kind::Float(0.05, 0.95, 2),
            ),
            f(
                &["layout", "master_count"],
                "Windows in the master area",
                "",
                Kind::Int(1, 10, ""),
            ),
            f(
                &["layout", "workspaces"],
                "Workspaces",
                "Per monitor.",
                Kind::Int(1, 32, ""),
            ),
        ],
    },
    Section {
        title: "Gaps",
        note: "",
        fields: &[
            f(&["layout", "gaps_inner"], "Between windows", "", Kind::Int(0, 64, "px")),
            f(&["layout", "gaps_outer"], "Around the edge", "", Kind::Int(0, 64, "px")),
            f(
                &["layout", "smart_gaps"],
                "Smart gaps",
                "No gaps when a workspace has only one window.",
                Kind::Toggle,
            ),
        ],
    },
];

pub const BAR: &[Section] = &[Section {
    title: "Bar",
    note: "Each monitor's bar can go elsewhere, or be hidden, on the Monitors page.",
    fields: &[
        f(&["bar", "enabled"], "Show the bar", "", Kind::Toggle),
        f(
            &["bar", "position"],
            "Position",
            "",
            Kind::Choice(&[
                ("top", "Top"),
                ("bottom", "Bottom"),
                ("left", "Left"),
                ("right", "Right"),
            ]),
        ),
        f(
            &["bar", "height"],
            "Height",
            "Of a top or bottom bar, and the rows of a side bar.",
            Kind::Int(16, 96, "px"),
        ),
        f(&["bar", "width"], "Side bar width", "", Kind::Int(40, 800, "px")),
        f(&["bar", "launcher"], "+ button runs", "", Kind::Text("command")),
    ],
}];

pub const KEYBOARD: &[Section] = &[
    Section {
        title: "Layout",
        note: "xkb names, as in /usr/share/X11/xkb/rules/evdev.lst.",
        fields: &[
            f(
                &["keyboard", "layout"],
                "Layout",
                "Several are separated by commas, e.g. us,de.",
                Kind::Text("us"),
            ),
            f(&["keyboard", "variant"], "Variant", "", Kind::Text("e.g. dvorak")),
            f(
                &["keyboard", "options"],
                "Options",
                "",
                Kind::Text("e.g. caps:escape,compose:ralt"),
            ),
        ],
    },
    Section {
        title: "Key repeat",
        note: "",
        fields: &[
            f(
                &["keyboard", "repeat_delay"],
                "Delay",
                "Before a held key repeats.",
                Kind::Int(100, 1000, "ms"),
            ),
            f(
                &["keyboard", "repeat_rate"],
                "Rate",
                "",
                Kind::Int(1, 100, "per second"),
            ),
        ],
    },
];

pub const INPUT: &[Section] = &[Section {
    title: "Mouse and touchpad",
    note: "These apply when running from a TTY, not in a nested window.",
    fields: &[
        f(
            &["input", "pointer_speed"],
            "Pointer speed",
            "",
            Kind::Float(-1.0, 1.0, 2),
        ),
        f(
            &["input", "natural_scroll"],
            "Natural scrolling",
            "Content follows your fingers.",
            Kind::Toggle,
        ),
        f(
            &["input", "tap_to_click"],
            "Tap to click",
            "On touchpads.",
            Kind::Toggle,
        ),
    ],
}];

pub const SESSION: &[Section] = &[Section {
    title: "Session",
    note: "",
    fields: &[
        at_start(f(
            &["session", "restore"],
            "Bring apps back",
            "Start the apps that were open last time, with their windows where they were.",
            Kind::Toggle,
        )),
        f(
            &["session", "exclude"],
            "Never bring back",
            "App ids. A * at the end matches any ending, e.g. steam_app_*.",
            Kind::List("app id"),
        ),
    ],
}];

/// Every section, for searching.
pub fn all() -> impl Iterator<Item = &'static Section> {
    [GENERAL, APPEARANCE, LAYOUT, BAR, KEYBOARD, INPUT, SESSION]
        .into_iter()
        .flatten()
}

pub fn matches(field: &Field, query: &str) -> bool {
    let q = query.to_lowercase();
    field.label.to_lowercase().contains(&q)
        || field.help.to_lowercase().contains(&q)
        || field.path.join(".").contains(&q)
}

pub fn show_sections(ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc, cache: &mut Cache, sections: &[Section]) {
    for s in sections {
        widgets::card(ui, p, s.title, s.note, |ui| {
            for (i, field) in s.fields.iter().enumerate() {
                if i > 0 {
                    widgets::divider(ui, p);
                }
                show_field(ui, p, doc, cache, field);
            }
        });
    }
}

pub fn show_field(ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc, cache: &mut Cache, field: &Field) {
    let path = field.path;
    let info = RowInfo {
        label: field.label,
        help: field.help,
        changed: doc.changed(path),
        restart: field.restart,
    };
    // Lists are tall, so they go under the label instead of beside it.
    if let Kind::List(hint) = field.kind {
        let reset = widgets::row(ui, p, info, |_| {});
        let mut items = doc.str_list(path);
        if widgets::string_list(ui, p, &mut items, hint) {
            doc.set(path, toml_edit::Array::from_iter(items));
        }
        if reset {
            doc.reset(path);
        }
        return;
    }

    let reset = widgets::row(ui, p, info, |ui| match &field.kind {
        Kind::Toggle => {
            let mut v = doc.bool(path);
            if widgets::toggle(ui, p, &mut v).changed() {
                doc.set(path, v);
            }
        }
        Kind::Int(min, max, unit) => {
            let mut v = doc.int(path);
            let mut slider = egui::Slider::new(&mut v, *min..=*max);
            if !unit.is_empty() {
                slider = slider.suffix(format!(" {unit}"));
            }
            if ui.add(slider).changed() {
                doc.set(path, v);
            }
        }
        Kind::Float(min, max, decimals) => {
            let mut v = doc.float(path);
            let step = 10f64.powi(-(*decimals as i32));
            let r = ui.add(
                egui::Slider::new(&mut v, *min..=*max)
                    .step_by(step)
                    .fixed_decimals(*decimals),
            );
            if r.changed() {
                set_number(doc, path, v, *decimals);
            }
        }
        Kind::Seconds => {
            let mut v = doc.int(path);
            let mut never = v == 0;
            ui.horizontal(|ui| {
                if !never {
                    let r = ui.add(
                        egui::DragValue::new(&mut v)
                            .range(1..=86_400)
                            .speed(5)
                            .custom_formatter(|n, _| duration(n as i64))
                            .custom_parser(parse_duration),
                    );
                    if r.changed() {
                        doc.set(path, v);
                    }
                }
                if ui.checkbox(&mut never, "Never").changed() {
                    doc.set(path, if never { 0 } else { 600 });
                }
            });
        }
        Kind::Choice(options) => {
            let mut v = doc.str(path);
            if widgets::choice(ui, path, &mut v, options, ui.available_width().min(260.0)) {
                doc.set(path, v);
            }
        }
        Kind::Text(hint) => {
            let mut v = doc.str(path);
            if widgets::text(ui, &mut v, hint).changed() {
                doc.set(path, v);
            }
        }
        Kind::Font => {
            let mut v = doc.str(path);
            if font_picker(ui, cache, &mut v) {
                doc.set(path, v);
            }
        }
        Kind::IconTheme => {
            let mut v = doc.str(path);
            let themes = cache.icon_themes();
            let options: Vec<(&str, &str)> = std::iter::once(("auto", "Same as the desktop"))
                .chain(themes.iter().map(|t| (t.as_str(), t.as_str())))
                .collect();
            if widgets::choice(ui, path, &mut v, &options, ui.available_width().min(260.0)) {
                doc.set(path, v);
            }
        }
        Kind::List(_) => unreachable!(),
    });
    if reset {
        doc.reset(path);
    }
    // The three keyboard fields only make sense together, so check them under the last one.
    if path == ["keyboard", "options"] {
        keymap_error(ui, p, doc);
    }
}

/// Whole numbers are written without a decimal point, so the file stays as tidy as the default.
pub fn set_number(doc: &mut ConfigDoc, path: &[&str], v: f64, decimals: usize) {
    let scale = 10f64.powi(decimals as i32);
    let v = (v * scale).round() / scale;
    if v.fract() == 0.0 {
        doc.set(path, v as i64);
    } else {
        doc.set(path, v);
    }
}

fn keymap_error(ui: &mut Ui, p: &Palette, doc: &ConfigDoc) {
    let kb = wt_config::KeyboardConfig {
        layout: doc.str(&["keyboard", "layout"]),
        variant: doc.str(&["keyboard", "variant"]),
        options: doc.str(&["keyboard", "options"]),
        repeat_rate: 0,
        repeat_delay: 0,
    };
    if let Err(e) = wt_config::check_keymap(&kb) {
        widgets::error(ui, p, &e);
    }
}

/// A searchable list of installed font families.
fn font_picker(ui: &mut Ui, cache: &mut Cache, value: &mut String) -> bool {
    let before = value.clone();
    let id = ui.id().with("font-filter");
    let mut filter: String = ui.data_mut(|d| d.get_temp(id).unwrap_or_default());
    egui::ComboBox::from_id_salt("font")
        .selected_text(value.as_str())
        .width(ui.available_width().min(260.0))
        .height(320.0)
        .show_ui(ui, |ui| {
            ui.add(egui::TextEdit::singleline(&mut filter).hint_text("Search fonts"))
                .request_focus();
            let q = filter.to_lowercase();
            for family in ["sans", "serif", "monospace"]
                .iter()
                .map(|s| s.to_string())
                .chain(cache.fonts().iter().cloned())
            {
                if q.is_empty() || family.to_lowercase().contains(&q) {
                    ui.selectable_value(value, family.clone(), family.as_str());
                }
            }
        });
    ui.data_mut(|d| d.insert_temp(id, filter));
    *value != before
}

/// "10 min", "1 h 30 min", "45 s".
fn duration(s: i64) -> String {
    let (h, m, sec) = (s / 3600, s % 3600 / 60, s % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} h"));
    }
    if m > 0 {
        parts.push(format!("{m} min"));
    }
    if sec > 0 || parts.is_empty() {
        parts.push(format!("{sec} s"));
    }
    parts.join(" ")
}

/// Accepts "90", "90 s", "10 min", "1h 30min".
fn parse_duration(text: &str) -> Option<f64> {
    let text = text.trim().to_lowercase();
    if let Ok(n) = text.parse::<f64>() {
        return Some(n);
    }
    let mut total = 0.0;
    let mut num = String::new();
    let mut unit = String::new();
    let mut flush = |num: &mut String, unit: &mut String| -> Option<()> {
        if num.is_empty() {
            return Some(());
        }
        let n: f64 = num.parse().ok()?;
        total += n * match unit.trim() {
            "" | "s" | "sec" => 1.0,
            "m" | "min" => 60.0,
            "h" | "hr" => 3600.0,
            _ => return None,
        };
        num.clear();
        unit.clear();
        Some(())
    };
    for c in text.chars() {
        if c.is_ascii_digit() || c == '.' {
            if !unit.trim().is_empty() {
                flush(&mut num, &mut unit)?;
            }
            num.push(c);
        } else {
            unit.push(c);
        }
    }
    flush(&mut num, &mut unit)?;
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(600), "10 min");
        assert_eq!(duration(5430), "1 h 30 min 30 s");
        assert_eq!(parse_duration("1h 30min"), Some(5400.0));
        assert_eq!(parse_duration("90"), Some(90.0));
        assert_eq!(parse_duration("10 min"), Some(600.0));
        assert_eq!(parse_duration("3 parsecs"), None);
    }

    #[test]
    fn every_field_exists_in_the_defaults() {
        let doc = ConfigDoc::open("/nonexistent/config.toml".into());
        for s in all() {
            for field in s.fields {
                assert!(doc.default_item(field.path).is_some(), "{:?}", field.path);
            }
        }
    }
}
