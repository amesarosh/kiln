//! The settings pages. Plain settings are data in `fields`; the rest have their own editors.

pub mod binds;
pub mod colors;
pub mod fields;
pub mod monitors;
pub mod rules;
pub mod themes;

use crate::monitors::Monitor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    General,
    Appearance,
    Layout,
    Bar,
    Monitors,
    Keyboard,
    Input,
    Keybinds,
    Rules,
    Session,
}

impl Page {
    pub const ALL: &[Page] = &[
        Page::General,
        Page::Appearance,
        Page::Layout,
        Page::Bar,
        Page::Monitors,
        Page::Keyboard,
        Page::Input,
        Page::Keybinds,
        Page::Rules,
        Page::Session,
    ];

    /// For `--page`: the title's first word, lowercased.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.to_lowercase();
        Self::ALL.iter().copied().find(|p| {
            let first = p.title().split_whitespace().next().unwrap_or_default().to_lowercase();
            first == name || format!("{p:?}").to_lowercase() == name
        })
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Appearance => "Appearance",
            Page::Layout => "Tiling",
            Page::Bar => "Bar",
            Page::Monitors => "Monitors",
            Page::Keyboard => "Keyboard",
            Page::Input => "Mouse and touchpad",
            Page::Keybinds => "Keybinds",
            Page::Rules => "Window rules",
            Page::Session => "Session",
        }
    }
}

/// Things that are slow to look up, found once when first needed.
#[derive(Default)]
pub struct Cache {
    fonts: Option<Vec<String>>,
    icon_themes: Option<Vec<String>>,
    monitors: Option<Vec<Monitor>>,
}

impl Cache {
    pub fn fonts(&mut self) -> &[String] {
        self.fonts.get_or_insert_with(wt_config::fonts::families)
    }

    pub fn icon_themes(&mut self) -> &[String] {
        self.icon_themes.get_or_insert_with(find_icon_themes)
    }

    pub fn monitors(&mut self) -> &[Monitor] {
        self.monitors.get_or_insert_with(crate::monitors::detect)
    }

    pub fn refresh_monitors(&mut self) {
        self.monitors = None;
    }
}

/// Installed icon themes (not cursor themes), by directory name.
fn find_icon_themes() -> Vec<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let mut dirs: Vec<std::path::PathBuf> = data_home.into_iter().map(|d| d.join("icons")).collect();
    dirs.extend(home.map(|h| h.join(".icons")));
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(data_dirs.split(':').map(|d| std::path::Path::new(d).join("icons")));

    let mut themes = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let index = std::fs::read_to_string(e.path().join("index.theme")).unwrap_or_default();
            // Cursor-only themes have an index.theme too, but no icon directories.
            if index.contains("Directories=")
                && let Some(name) = e.file_name().to_str()
                && name != "default"
            {
                themes.push(name.to_string());
            }
        }
    }
    themes.sort_by_key(|t| t.to_lowercase());
    themes.dedup();
    themes
}
