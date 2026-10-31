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
    /// An image file, picked from the pictures found or typed as a path.
    Wallpaper,
    /// Some of a fixed set of values, kept in this order: (value, label).
    Flags(&'static [(&'static str, &'static str)]),
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
            f(
                &["general", "allow_shortcuts_inhibit"],
                "Apps can take every key",
                "Virtual machines, remote desktops and games can ask for all keys, binds included. A bind to toggle-shortcuts-inhibit takes them back.",
                Kind::Toggle,
            ),
            f(
                &["general", "allow_tearing"],
                "Games can skip vsync",
                "Fullscreen games that ask can show each frame as soon as it's drawn: less input lag, but the picture can tear.",
                Kind::Toggle,
            ),
        ],
    },
    Section {
        title: "Power",
        note: "",
        fields: &[
            f(
                &["general", "monitors_off_after"],
                "Turn monitors off after",
                "Time without input before monitors turn off. Any key, click or pointer movement turns them back on.",
                Kind::Seconds,
            ),
            f(
                &["general", "lock_when_monitors_off"],
                "Lock when monitors turn off",
                "Lock the screen when monitors turn off on their own or from the power-off-monitors bind.",
                Kind::Toggle,
            ),
            f(
                &["general", "suspend_after"],
                "Suspend after",
                "Time without input before the computer suspends. The screen locks first.",
                Kind::Seconds,
            ),
            f(
                &["general", "locker"],
                "Screen locker",
                "Used by Lock and Suspend in the power menu. Empty uses the first installed of wayland-time-lock, hyprlock, swaylock, gtklock and waylock.",
                Kind::Text("automatic"),
            ),
        ],
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
            at_start(f(
                &["general", "polkit_agent"],
                "Password prompt for admin rights",
                "The polkit agent that asks for your password when an app needs admin rights. Empty uses wayland-time-polkit, or another one installed; \"none\" starts none.",
                Kind::Text("automatic"),
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
        title: "Wallpaper",
        note: "A monitor can have its own: wallpaper under [[outputs]] in the config file.",
        fields: &[
            f(&["theme", "wallpaper"], "Wallpaper", "PNG, JPEG or WebP.", Kind::Wallpaper),
            f(
                &["theme", "wallpaper_fit"],
                "Fit",
                "How the picture is fitted to each monitor. Uncovered parts show the base color.",
                Kind::Choice(&[
                    ("fill", "Fill the screen (crop the edges)"),
                    ("fit", "Show all of it"),
                    ("stretch", "Stretch"),
                    ("center", "Center at its own size"),
                    ("tile", "Tile"),
                ]),
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
    Section {
        title: "Animations",
        note: "",
        fields: &[
            f(
                &["animations", "enabled"],
                "Animations",
                "Workspace switches, tiles sliding into place, and the overview opening.",
                Kind::Toggle,
            ),
            f(
                &["animations", "workspace"],
                "Switching workspace",
                "",
                Kind::Choice(&[
                    ("slide", "Slide"),
                    ("fade", "Fade"),
                    ("none", "No animation"),
                ]),
            ),
            f(
                &["animations", "direction"],
                "Slide direction",
                "A higher-numbered workspace comes in from below, or from the right.",
                Kind::Choice(&[("vertical", "Vertical"), ("horizontal", "Horizontal")]),
            ),
            f(&["animations", "duration"], "Speed", "How long an animation takes.", Kind::Int(50, 1000, "ms")),
        ],
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
                    ("vertical", "Vertical: master on top, stack below"),
                    ("monocle", "Monocle: one window at a time"),
                ]),
            ),
            f(
                &["layout", "master_ratio"],
                "Master width",
                "Share of the screen given to the master area (its width, or its height in the vertical layout).",
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
        f(
            &["bar", "widgets"],
            "Status widgets",
            "Shown next to the clock (on every bar when no monitor has the clock). Battery and brightness only show on machines that have them.",
            Kind::Flags(&[
                ("tray", "Tray icons from apps"),
                ("media", "What's playing"),
                ("network", "Network"),
                ("battery", "Battery"),
                ("brightness", "Screen brightness"),
                ("notifications", "Notifications bell"),
            ]),
        ),
    ],
}];

pub const NOTIFICATIONS: &[Section] = &[Section {
    title: "Notifications",
    note: "Pop-ups from apps. Click one to open it, right-click to dismiss it. Missed ones wait behind the bell on the bar.",
    fields: &[
        at_start(f(
            &["notifications", "enabled"],
            "Show notifications",
            "Off leaves them to another program, like mako or dunst.",
            Kind::Toggle,
        )),
        f(
            &["notifications", "position"],
            "Corner",
            "",
            Kind::Choice(&[
                ("top-right", "Top right"),
                ("top-left", "Top left"),
                ("bottom-right", "Bottom right"),
                ("bottom-left", "Bottom left"),
            ]),
        ),
        f(
            &["notifications", "timeout"],
            "Stay for",
            "When the app doesn't say. Urgent ones stay until dismissed.",
            Kind::Int(1, 60, "s"),
        ),
        f(&["notifications", "max_visible"], "Most at once", "", Kind::Int(1, 10, "")),
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
            f(
                &["keyboard", "numlock"],
                "Num Lock on at start",
                "So the number pad types numbers.",
                Kind::Toggle,
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
        f(
            &["input", "gestures"],
            "Swipe gestures",
            "Three or four fingers: left and right change workspace, up opens the overview, down closes it.",
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
    [GENERAL, APPEARANCE, LAYOUT, BAR, NOTIFICATIONS, KEYBOARD, INPUT, SESSION]
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
    // Flags are one toggle each, under the label.
    if let Kind::Flags(options) = field.kind {
        let reset = widgets::row(ui, p, info, |_| {});
        let on = doc.str_list(path);
        let mut changed = None;
        for (value, label) in options {
            let mut v = on.iter().any(|x| x == value);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                if widgets::toggle(ui, p, &mut v).changed() {
                    changed = Some((*value, v));
                }
                ui.label(wt_ui::egui::RichText::new(*label).color(wt_ui::color(p.text)));
            });
        }
        if let Some((value, v)) = changed {
            // Keep them in the given order, whatever order they were switched on in.
            let list: Vec<&str> = options
                .iter()
                .map(|(o, _)| *o)
                .filter(|o| if *o == value { v } else { on.iter().any(|x| x == o) })
                .collect();
            doc.set(path, toml_edit::Array::from_iter(list));
        }
        if reset {
            doc.reset(path);
        }
        return;
    }
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
            let mut v = doc.int(path) as f64;
            if widgets::slider(ui, p, &mut v, *min as f64..=*max as f64, 0, unit).changed() {
                doc.set(path, v.round() as i64);
            }
        }
        Kind::Float(min, max, decimals) => {
            let mut v = doc.float(path);
            if widgets::slider(ui, p, &mut v, *min..=*max, *decimals, "").changed() {
                set_number(doc, path, v, *decimals);
            }
        }
        Kind::Seconds => {
            // Picked from a list: dragging or typing a duration is fiddly for a setting like this.
            let v = doc.int(path);
            let mut times: Vec<i64> = SECONDS_CHOICES.to_vec();
            if !times.contains(&v) {
                times.push(v);
                times.sort_unstable_by_key(|&t| if t == 0 { i64::MAX } else { t });
            }
            let values: Vec<String> = times.iter().map(i64::to_string).collect();
            let labels: Vec<String> = times
                .iter()
                .map(|&t| if t == 0 { "Never".to_string() } else { duration(t) })
                .collect();
            let options: Vec<(&str, &str)> = values
                .iter()
                .map(String::as_str)
                .zip(labels.iter().map(String::as_str))
                .collect();
            let mut picked = v.to_string();
            if widgets::choice(ui, path, &mut picked, &options) {
                doc.set(path, picked.parse::<i64>().unwrap_or(0));
            }
        }
        Kind::Choice(options) => {
            let mut v = doc.str(path);
            if widgets::choice(ui, path, &mut v, options) {
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
            if widgets::choice(ui, path, &mut v, &options) {
                doc.set(path, v);
            }
        }
        Kind::Wallpaper => {
            let mut v = doc.str(path);
            if wallpaper_picker(ui, cache, &mut v) {
                doc.set(path, v);
            }
        }
        Kind::List(_) | Kind::Flags(_) => unreachable!(),
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
        numlock: false,
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
    wt_ui::select(ui, |ui| egui::ComboBox::from_id_salt("font")
        .selected_text(value.as_str())
        .width(ui.available_width())
        .height(320.0)
        .show_ui(ui, |ui| {
            ui.add(wt_ui::field(ui, egui::TextEdit::singleline(&mut filter).hint_text("Search fonts")))
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
        }));
    ui.data_mut(|d| d.insert_temp(id, filter));
    *value != before
}

/// A searchable list of the pictures found, plus "none"; a full path can be typed in the
/// search box too.
fn wallpaper_picker(ui: &mut Ui, cache: &mut Cache, value: &mut String) -> bool {
    let before = value.clone();
    let id = ui.id().with("wallpaper-filter");
    let mut filter: String = ui.data_mut(|d| d.get_temp(id).unwrap_or_default());
    let shown = if value.is_empty() {
        "None".to_string()
    } else {
        std::path::Path::new(value.as_str())
            .file_name()
            .map_or(value.clone(), |n| n.to_string_lossy().into_owned())
    };
    wt_ui::select(ui, |ui| egui::ComboBox::from_id_salt("wallpaper")
        .selected_text(shown)
        .width(ui.available_width())
        .height(360.0)
        .show_ui(ui, |ui| {
            ui.add(wt_ui::field(ui, egui::TextEdit::singleline(&mut filter).hint_text("Search, or type a file's path")))
                .request_focus();
            let typed = filter.trim();
            if (typed.starts_with('/') || typed.starts_with('~'))
                && std::path::Path::new(&wt_config::expand_home(typed)).is_file()
            {
                ui.selectable_value(value, typed.to_string(), format!("Use {typed}"));
            }
            ui.selectable_value(value, String::new(), "None");
            let q = typed.to_lowercase();
            for (path, label) in cache.wallpapers() {
                if q.is_empty() || label.to_lowercase().contains(&q) {
                    ui.selectable_value(value, path.clone(), label.as_str())
                        .on_hover_text(path.as_str());
                }
            }
        }));
    ui.data_mut(|d| d.insert_temp(id, filter));
    *value != before
}

/// The times offered for `Kind::Seconds` settings, 0 being "Never".
const SECONDS_CHOICES: &[i64] = &[60, 120, 180, 300, 600, 900, 1200, 1800, 2700, 3600, 5400, 7200, 10800, 14400, 18000, 21600, 0];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(600), "10 min");
        assert_eq!(duration(5430), "1 h 30 min 30 s");
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
