//! Installed apps, from their .desktop files, and how often each has been launched.

use std::{collections::HashMap, path::PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// The desktop file id, e.g. "org.gnome.Nautilus.desktop".
    pub id: String,
    pub name: String,
    pub generic: String,
    pub comment: String,
    pub keywords: Vec<String>,
    /// The command, with the field codes (%U and friends) taken out.
    pub exec: String,
    pub icon: String,
    pub terminal: bool,
}

/// Every app that should show in a launcher. The user's own files (earlier data dirs) win
/// over system ones with the same id.
pub fn load() -> Vec<App> {
    let mut seen = std::collections::HashSet::new();
    let mut apps = Vec::new();
    for dir in wt_icons::data_dirs() {
        let root = dir.join("applications");
        let mut files = Vec::new();
        wt_icons::collect_desktop_files(&root, 3, &mut files);
        files.sort();
        for file in files {
            let id = file
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().replace('/', "-"))
                .unwrap_or_default();
            // A file seen earlier (even a hidden one) shadows this one.
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            if let Some(app) = parse(&id, &text) {
                apps.push(app);
            }
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn parse(id: &str, text: &str) -> Option<App> {
    let mut in_entry = false;
    let mut f: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some((k, v)) = line.split_once('=') {
            f.entry(k.trim()).or_insert(v.trim());
        }
    }
    let get = |k: &str| f.get(k).copied().unwrap_or_default();
    if get("Type") != "Application" || get("NoDisplay") == "true" || get("Hidden") == "true" {
        return None;
    }
    let desktop = "wayland-time";
    let list = |k: &str| get(k).split(';').filter(|s| !s.is_empty()).map(str::to_string).collect::<Vec<_>>();
    let only = list("OnlyShowIn");
    if !only.is_empty() && !only.iter().any(|d| d.eq_ignore_ascii_case(desktop)) {
        return None;
    }
    if list("NotShowIn").iter().any(|d| d.eq_ignore_ascii_case(desktop)) {
        return None;
    }
    let try_exec = get("TryExec");
    if !try_exec.is_empty() && !on_path(try_exec) {
        return None;
    }
    let exec = strip_field_codes(get("Exec"));
    if exec.is_empty() || get("Name").is_empty() {
        return None;
    }
    Some(App {
        id: id.to_string(),
        name: get("Name").to_string(),
        generic: get("GenericName").to_string(),
        comment: get("Comment").to_string(),
        keywords: list("Keywords"),
        exec,
        icon: get("Icon").to_string(),
        terminal: get("Terminal") == "true",
    })
}

/// "firefox %u" → "firefox"; "%%" stays a single "%".
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            // "%%" is a percent sign; %f %F %u %U %i %c %k and the old ones are dropped.
            if chars.next() == Some('%') {
                out.push('%');
            }
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a program exists (an absolute path, or a name found on $PATH).
pub fn on_path(prog: &str) -> bool {
    if prog.contains('/') {
        return std::path::Path::new(prog).is_file();
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
        .unwrap_or(false)
}

/// The terminal to run terminal apps in: $TERMINAL, else the first one installed.
pub fn terminal() -> Option<String> {
    if let Ok(t) = std::env::var("TERMINAL")
        && !t.is_empty()
    {
        return Some(t);
    }
    ["alacritty", "kitty", "foot", "wezterm", "ghostty", "gnome-terminal", "konsole", "xterm"]
        .into_iter()
        .find(|t| on_path(t))
        .map(String::from)
}

/// How many times each app was launched, kept in the state folder.
#[derive(Default)]
pub struct History {
    path: Option<PathBuf>,
    pub counts: HashMap<String, u32>,
}

impl History {
    pub fn load() -> Self {
        let path = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .map(|d| d.join("wayland-time").join("launcher-history"));
        let counts = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|l| {
                        let (n, id) = l.split_once('\t')?;
                        Some((id.to_string(), n.parse().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { path, counts }
    }

    pub fn bump(&mut self, id: &str) {
        *self.counts.entry(id.to_string()).or_default() += 1;
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut lines: Vec<(&String, &u32)> = self.counts.iter().collect();
        lines.sort_by(|a, b| b.1.cmp(a.1));
        let text: String = lines.iter().take(500).map(|(id, n)| format!("{n}\t{id}\n")).collect();
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_codes() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox");
        assert_eq!(strip_field_codes("code --new-window %F"), "code --new-window");
        assert_eq!(strip_field_codes("printf 100%%"), "printf 100%");
    }

    #[test]
    fn desktop_files() {
        let app = parse(
            "x.desktop",
            "[Desktop Entry]\nType=Application\nName=Files\nExec=nautilus --new-window %U\nIcon=org.gnome.Nautilus\nKeywords=folder;manager;\n[Desktop Action new]\nName=Other\n",
        )
        .unwrap();
        assert_eq!(app.name, "Files");
        assert_eq!(app.exec, "nautilus --new-window");
        assert_eq!(app.keywords, vec!["folder", "manager"]);
        assert!(parse("y.desktop", "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n").is_none());
        assert!(parse("z.desktop", "[Desktop Entry]\nType=Application\nName=X\nExec=x\nOnlyShowIn=GNOME;\n").is_none());
    }
}
