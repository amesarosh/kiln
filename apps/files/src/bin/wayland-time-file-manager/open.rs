//! Opening files in apps: which apps can open a file type (from their .desktop files), which
//! one is the default (from mimeapps.list), and starting them.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use wayland_time_files::fs::{self, Mime};

/// An app that can open files.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// The desktop file id, e.g. "org.gnome.TextEditor.desktop".
    pub id: String,
    pub name: String,
    pub icon: String,
    /// The Exec line, field codes and all.
    exec: String,
    terminal: bool,
    mime_types: Vec<String>,
    /// Shown in app lists (not NoDisplay).
    listed: bool,
    /// The .desktop file, for %k.
    file: PathBuf,
}

/// Installed apps and the user's choices of which opens what.
#[derive(Default)]
pub struct Apps {
    /// In data folder order, the user's own first.
    apps: Vec<App>,
    /// MIME type → desktop ids, most preferred first.
    defaults: HashMap<String, Vec<String>>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, Vec<String>>,
}

impl Apps {
    pub fn load() -> Self {
        let mut me = Self::default();
        let mut seen = std::collections::HashSet::new();
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
                // The user's own files shadow system ones with the same id.
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(app) = std::fs::read_to_string(&file).ok().and_then(|t| parse(&id, &t, &file)) {
                    me.apps.push(app);
                }
            }
        }
        // Kept in data folder order (the user's own apps first), which decides the default
        // when mimeapps.list doesn't, as xdg-open does.

        // mimeapps.list files, most important first; earlier entries win.
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| fs::home().join(".config"), PathBuf::from);
        let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|d| !d.is_empty())
            .map(str::to_lowercase)
            .collect();
        let mut lists = Vec::new();
        for dir in std::iter::once(config).chain([PathBuf::from("/etc/xdg")]) {
            lists.extend(desktops.iter().map(|d| dir.join(format!("{d}-mimeapps.list"))));
            lists.push(dir.join("mimeapps.list"));
        }
        for dir in wt_icons::data_dirs() {
            let apps = dir.join("applications");
            lists.extend(desktops.iter().map(|d| apps.join(format!("{d}-mimeapps.list"))));
            lists.push(apps.join("mimeapps.list"));
            lists.push(apps.join("defaults.list"));
        }
        for list in lists {
            let Ok(text) = std::fs::read_to_string(&list) else {
                continue;
            };
            let mut section = "";
            for line in text.lines().map(str::trim) {
                if line.starts_with('[') {
                    section = line;
                    continue;
                }
                let Some((mime, ids)) = line.split_once('=') else {
                    continue;
                };
                let target = match section {
                    "[Default Applications]" => &mut me.defaults,
                    "[Added Associations]" => &mut me.added,
                    "[Removed Associations]" => &mut me.removed,
                    _ => continue,
                };
                let entry = target.entry(mime.trim().to_string()).or_default();
                for id in ids.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                    if !entry.iter().any(|e| e == id) {
                        entry.push(id.to_string());
                    }
                }
            }
        }
        me
    }

    fn by_id(&self, id: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.id == id)
    }

    /// Apps that can open `mime`: the default first, then the others by name.
    pub fn for_type(&self, mime: &str) -> Vec<&App> {
        let mut out: Vec<&App> = Vec::new();
        fn push<'a>(a: &'a App, out: &mut Vec<&'a App>) {
            if !out.iter().any(|o| o.id == a.id) {
                out.push(a);
            }
        }
        let removed = self.removed.get(mime).cloned().unwrap_or_default();
        for types in related(mime) {
            for id in self
                .defaults
                .get(&types)
                .into_iter()
                .chain(self.added.get(&types))
                .flatten()
            {
                if let Some(a) = self.by_id(id) {
                    push(a, &mut out);
                }
            }
            for a in self.apps.iter().filter(|a| a.mime_types.contains(&types)) {
                if !removed.contains(&a.id) {
                    push(a, &mut out);
                }
            }
        }
        if out.len() > 2 {
            out[1..].sort_by_key(|a| a.name.to_lowercase());
        }
        out
    }

    /// The app that opens `mime` when nothing else is said.
    pub fn default_for(&self, mime: &str) -> Option<&App> {
        self.for_type(mime).into_iter().next()
    }

    /// Every app people can pick, for "Other app", by name.
    pub fn all(&self) -> Vec<&App> {
        let mut out: Vec<&App> = self.apps.iter().filter(|a| a.listed).collect();
        out.sort_by_key(|a| a.name.to_lowercase());
        out
    }
}

/// `mime` and the more general types that can stand in for it.
fn related(mime: &str) -> Vec<String> {
    let mut out = vec![mime.to_string()];
    if mime.starts_with("text/") && mime != "text/plain" {
        out.push("text/plain".into());
    }
    out
}

fn parse(id: &str, text: &str, file: &Path) -> Option<App> {
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
    if get("Type") != "Application" || get("Hidden") == "true" || get("Exec").is_empty() || get("Name").is_empty() {
        return None;
    }
    let try_exec = get("TryExec");
    if !try_exec.is_empty() && !on_path(try_exec) {
        return None;
    }
    let mime_types: Vec<String> = get("MimeType")
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    // Hidden apps are still offered for the types they open, never in the full list.
    if get("NoDisplay") == "true" && mime_types.is_empty() {
        return None;
    }
    Some(App {
        id: id.to_string(),
        name: get("Name").to_string(),
        icon: get("Icon").to_string(),
        exec: get("Exec").to_string(),
        terminal: get("Terminal") == "true",
        listed: get("NoDisplay") != "true",
        mime_types,
        file: file.to_path_buf(),
    })
}

/// The MIME type of a file: by its name, or for files without a known extension, plain text
/// if it looks like text.
pub fn type_of(mime: &Mime, path: &Path) -> String {
    if path.is_dir() {
        return "inode/directory".into();
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if let Some(t) = mime.type_of(&name) {
        return t.to_string();
    }
    let mut head = [0u8; 1024];
    let n = std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    let head = &head[..n];
    let texty = !head.contains(&0)
        && (std::str::from_utf8(head).is_ok()
            // The cut may land inside a character.
            || std::str::from_utf8(head).is_err_and(|e| e.valid_up_to() + 4 > head.len()));
    if n == 0 || texty {
        "text/plain".into()
    } else {
        "application/octet-stream".into()
    }
}

/// Open `paths` in `app`.
pub fn launch(app: &App, paths: &[PathBuf]) -> Result<(), String> {
    let words = split_exec(&app.exec);
    let takes_many = words.iter().any(|w| w.contains("%F") || w.contains("%U"));
    let takes_one = words.iter().any(|w| w.contains("%f") || w.contains("%u"));
    // An app that takes one file at a time is started once per file.
    let runs: Vec<&[PathBuf]> = if takes_one && !takes_many {
        paths.chunks(1).collect()
    } else {
        vec![paths]
    };
    for files in runs {
        let mut args: Vec<String> = Vec::new();
        for w in &words {
            match w.as_str() {
                "%F" => args.extend(files.iter().map(|p| p.to_string_lossy().into_owned())),
                "%U" => args.extend(files.iter().map(|p| fs::path_to_uri(p))),
                "%i" if !app.icon.is_empty() => args.extend(["--icon".to_string(), app.icon.clone()]),
                _ => {
                    let first = files.first();
                    let mut out = String::new();
                    let mut chars = w.chars();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            out.push(c);
                            continue;
                        }
                        match chars.next() {
                            Some('%') => out.push('%'),
                            Some('f') => {
                                out.push_str(&first.map_or(String::new(), |p| p.to_string_lossy().into_owned()))
                            }
                            Some('u') => out.push_str(&first.map_or(String::new(), |p| fs::path_to_uri(p))),
                            Some('c') => out.push_str(&app.name),
                            Some('k') => out.push_str(&app.file.to_string_lossy()),
                            _ => {}
                        }
                    }
                    if !out.is_empty() || !w.starts_with('%') {
                        args.push(out);
                    }
                }
            }
        }
        // No field code: the files go on the end.
        if !takes_one && !takes_many {
            args.extend(files.iter().map(|p| p.to_string_lossy().into_owned()));
        }
        if app.terminal {
            let term = terminal().ok_or("no terminal found to run it in")?;
            args.splice(0..0, [term, "-e".to_string()]);
        }
        spawn(&args, None)?;
    }
    Ok(())
}

/// Open `path` with whatever the system says, when we know of no app for it.
pub fn open_with_system(path: &Path) -> Result<(), String> {
    spawn(&["xdg-open".to_string(), path.to_string_lossy().into_owned()], None)
}

/// Open a terminal in `dir`.
pub fn terminal_in(dir: &Path) -> Result<(), String> {
    let term = terminal().ok_or("no terminal found ($TERMINAL isn't set and none of the usual ones are installed)")?;
    spawn(&[term], Some(dir))
}

/// Start a program on its own, so it lives on after we close.
fn spawn(args: &[String], dir: Option<&Path>) -> Result<(), String> {
    let (prog, rest) = args.split_first().ok_or("nothing to run")?;
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("XDG_DATA_DIRS", wt_icons::xdg_data_dirs());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe; it detaches the app from our session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("can't start {prog}: {e}"))?;
    // Reap it when it exits, so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Split an Exec line into words, minding double quotes and backslash escapes.
fn split_exec(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn on_path(prog: &str) -> bool {
    if prog.contains('/') {
        return Path::new(prog).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// The terminal to use: $TERMINAL, else the first one installed.
fn terminal() -> Option<String> {
    if let Ok(t) = std::env::var("TERMINAL")
        && !t.is_empty()
    {
        return Some(t);
    }
    [
        "alacritty",
        "kitty",
        "foot",
        "wezterm",
        "ghostty",
        "gnome-terminal",
        "konsole",
        "xterm",
    ]
    .into_iter()
    .find(|t| on_path(t))
    .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_words() {
        assert_eq!(split_exec(r#"code --new-window %F"#), ["code", "--new-window", "%F"]);
        assert_eq!(
            split_exec(r#""/opt/My App/app" "a \"b\"" %u"#),
            ["/opt/My App/app", "a \"b\"", "%u"]
        );
        assert_eq!(split_exec(r#"x """#), ["x", ""]);
    }

    #[test]
    fn desktop_entry() {
        let app = parse(
            "t.desktop",
            "[Desktop Entry]\nType=Application\nName=Text\nExec=text %U\nMimeType=text/plain;text/x-csrc;\nNoDisplay=true\n",
            Path::new("/x/t.desktop"),
        )
        .unwrap();
        assert_eq!(app.mime_types, ["text/plain", "text/x-csrc"]);
        assert!(
            parse(
                "n.desktop",
                "[Desktop Entry]\nType=Application\nName=N\nExec=n\nNoDisplay=true\n",
                Path::new("/n")
            )
            .is_none()
        );
    }
}
