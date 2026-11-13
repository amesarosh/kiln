//! The user's config file as an editable document.
//!
//! Edits go into a `toml_edit` document, so the user's comments and layout survive. Shortly after
//! the last edit the file is checked with the compositor's own config loader and, only if it
//! loads, written; the compositor reloads it on save, so changes apply right away.
//!
//! When `[theme] name` picks a saved theme, theme settings are edited in that theme's file
//! instead (`themes/<name>.toml`), unless the config file itself sets the value.

use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use toml_edit::{DocumentMut, Item, Table, Value};
use kiln_config::{Config, DEFAULT_CONFIG};

/// How long after the last edit to save. Short enough to feel live, long enough that dragging a
/// slider doesn't make the compositor reload on every frame.
const SAVE_DELAY: Duration = Duration::from_millis(250);

pub enum Status {
    /// Nothing changed since the file was opened.
    Idle,
    /// Edited; saving shortly.
    Pending,
    Saved(Instant),
    /// The edits don't load (the reason is shown); the file is left as it was.
    Invalid(String),
    WriteFailed(String),
    /// The file was changed by something else and has been read again.
    Reloaded(Instant),
}

/// The saved theme in use: its file, edited alongside the config.
struct ThemeDoc {
    name: String,
    path: PathBuf,
    doc: DocumentMut,
    disk_time: Option<SystemTime>,
    disk_text: String,
}

impl ThemeDoc {
    /// A missing file reads as an empty theme (all defaults); it's written on the next save.
    fn open(config_path: &std::path::Path, name: &str) -> Result<Self, String> {
        let path = kiln_config::theme_path(config_path, name);
        let disk_time = modified(&path);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("Can't read {}: {e}", path.display())),
        };
        let doc = text
            .parse()
            .map_err(|e| format!("{} has a syntax error:\n{e}", path.display()))?;
        Ok(Self {
            name: name.to_string(),
            path,
            doc,
            disk_time,
            disk_text: text,
        })
    }
}

pub struct ConfigDoc {
    pub path: PathBuf,
    user: DocumentMut,
    theme: Option<ThemeDoc>,
    defaults: DocumentMut,
    /// When the last unsaved edit happened.
    pending: Option<Instant>,
    pub status: Status,
    /// Modification time of the file as we last read or wrote it.
    disk_time: Option<SystemTime>,
    disk_text: String,
    last_check: Instant,
    /// The file couldn't be parsed at all, so editing is off until it's fixed by hand.
    pub broken: Option<String>,
    /// Goes up whenever the document is replaced from disk, so pages holding copies resync.
    pub generation: u64,
    /// The config as last saved (or loaded), for things like the live theme.
    pub config: Config,
}

impl ConfigDoc {
    pub fn open(path: PathBuf) -> Self {
        let mut doc = Self {
            path,
            user: DocumentMut::new(),
            theme: None,
            defaults: DEFAULT_CONFIG.parse().expect("default config parses"),
            pending: None,
            status: Status::Idle,
            disk_time: None,
            disk_text: String::new(),
            last_check: Instant::now(),
            broken: None,
            generation: 0,
            config: Config::default(),
        };
        doc.read();
        doc
    }

    /// (Re)read the file. A missing file starts from the commented defaults, which is also what
    /// the compositor writes on first start.
    fn read(&mut self) {
        self.disk_time = modified(&self.path);
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_string(),
            Err(e) => {
                self.broken = Some(format!("Can't read {}: {e}", self.path.display()));
                return;
            }
        };
        match text.parse::<DocumentMut>() {
            Ok(doc) => {
                self.user = doc;
                self.broken = None;
                self.load_theme();
                if let Ok(c) = self.parse(&text) {
                    self.config = c;
                }
            }
            Err(e) => self.broken = Some(format!("{} has a syntax error:\n{e}", self.path.display())),
        }
        self.disk_text = text;
        self.generation += 1;
    }

    /// Open the theme file `[theme] name` points at, if any.
    fn load_theme(&mut self) {
        self.theme = None;
        let Some(name) = self.theme_name() else { return };
        if !kiln_config::valid_theme_name(&name) {
            self.broken = Some(format!("theme.name \"{name}\" can't be used as a file name"));
            return;
        }
        match ThemeDoc::open(&self.path, &name) {
            Ok(t) => self.theme = Some(t),
            Err(e) => self.broken = Some(e),
        }
    }

    /// Parse config text the way the compositor would, with the theme as edited here.
    fn parse(&self, text: &str) -> Result<Config, kiln_config::ConfigError> {
        let theme = self.theme.as_ref().map(|t| t.doc.to_string());
        Config::parse_user_with(text, |_| theme.ok_or_else(|| "not loaded".to_string()))
    }

    // ---------- saved themes ----------

    /// The saved theme in use, or `None` for the built-in one.
    pub fn theme_name(&self) -> Option<String> {
        self.user_raw(&["theme", "name"])
            .and_then(Item::as_str)
            .map(String::from)
    }

    /// Saved theme names.
    pub fn theme_names(&self) -> Vec<String> {
        let mut names = kiln_config::theme_names(&self.path);
        // One just made exists only here until the save lands.
        if let Some(t) = &self.theme
            && !names.contains(&t.name)
        {
            names.push(t.name.clone());
            names.sort_by_key(|n| n.to_lowercase());
        }
        names
    }

    /// Theme settings the config file sets itself, which win over any saved theme, as
    /// dotted names like `font_size` or `colors.accent`.
    pub fn theme_overrides(&self) -> Vec<String> {
        let Some(table) = self.user_raw(&["theme"]).and_then(Item::as_table_like) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, v) in table.iter() {
            match (k, v.as_table_like()) {
                ("name", _) => {}
                (_, Some(sub)) => out.extend(sub.iter().map(|(c, _)| format!("{k}.{c}"))),
                (_, None) => out.push(k.to_string()),
            }
        }
        out
    }

    /// Switch to a saved theme, or the built-in one with `None`. Changes made on top of the
    /// built-in theme are first kept as a new theme, whose name is returned.
    pub fn use_theme(&mut self, name: Option<&str>) -> Option<String> {
        self.flush();
        let mut kept = None;
        if name.is_some() && self.theme.is_none() && self.has_theme_changes() {
            let keep = self.unused_theme_name("Custom");
            self.save_theme_as(&keep);
            self.flush();
            kept = Some(keep);
        }
        match name {
            Some(n) => self.set_raw(&["theme", "name"], n),
            None => self.remove_raw(&["theme", "name"]),
        }
        self.load_theme();
        self.generation += 1;
        self.touch();
        kept
    }

    /// Whether the config file changes any theme setting from its default.
    fn has_theme_changes(&self) -> bool {
        self.theme_overrides().iter().any(|key| {
            let path: Vec<&str> = std::iter::once("theme").chain(key.split('.')).collect();
            self.changed(&path)
        })
    }

    /// Save the look in effect as a new theme (built-in values filled in, so the file stands on
    /// its own) and switch to it. Theme settings in the config file move into it.
    pub fn save_theme_as(&mut self, name: &str) {
        let Some(defaults) = self.default_item(&["theme"]).and_then(Item::as_table).cloned() else {
            return;
        };
        let mut out = DocumentMut::new();
        out.decor_mut()
            .set_prefix(format!("# kiln theme \"{name}\"\n\n"));
        let root = out.as_table_mut();
        for (k, v) in defaults.iter() {
            root.insert(k, v.clone());
        }
        if let Some(Item::Table(colors)) = root.get_mut("colors") {
            // Was [theme.colors]; now just [colors].
            colors.set_position(None);
            colors.decor_mut().clear();
        }
        let layers: Vec<Table> = [
            self.theme.as_ref().map(|t| t.doc.as_table().clone()),
            self.user_raw(&["theme"]).and_then(Item::as_table).cloned(),
        ]
        .into_iter()
        .flatten()
        .collect();
        for layer in &layers {
            for (k, v) in layer.iter() {
                match (k, v) {
                    ("name", _) => {}
                    (_, Item::Table(sub)) => {
                        let dst = root.entry(k).or_insert(Item::Table(Table::new()));
                        if let Some(dst) = dst.as_table_mut() {
                            for (c, cv) in sub.iter() {
                                if let Some(cv) = cv.as_value() {
                                    set_in(dst, c, cv.clone());
                                }
                            }
                        }
                    }
                    (_, v) => {
                        if let Some(v) = v.as_value() {
                            set_in(root, k, v.clone());
                        }
                    }
                }
            }
        }
        // The config keeps only the choice of theme.
        if let Some(t) = self.user.get_mut("theme").and_then(Item::as_table_mut) {
            t.retain(|k, _| k == "name");
        }
        self.set_raw(&["theme", "name"], name);
        self.theme = Some(ThemeDoc {
            name: name.to_string(),
            path: kiln_config::theme_path(&self.path, name),
            doc: out,
            disk_time: None,
            disk_text: String::new(),
        });
        self.generation += 1;
        self.touch();
    }

    /// Delete a saved theme. Deleting the one in use switches to the built-in theme.
    pub fn delete_theme(&mut self, name: &str) -> Result<(), String> {
        if self.theme_name().as_deref() == Some(name) {
            self.use_theme(None);
            self.flush();
        }
        let path = kiln_config::theme_path(&self.path, name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Can't delete {}: {e}", path.display())),
        }
    }

    /// `base`, or `base 2`, `base 3`... whichever isn't taken.
    pub fn unused_theme_name(&self, base: &str) -> String {
        let names = self.theme_names();
        let taken = |n: &str| names.iter().any(|t| t.eq_ignore_ascii_case(n));
        (1..)
            .map(|i| {
                if i == 1 {
                    base.to_string()
                } else {
                    format!("{base} {i}")
                }
            })
            .find(|n| !taken(n))
            .expect("some number is free")
    }

    /// Save now if there are unsaved edits.
    fn flush(&mut self) {
        if self.pending.take().is_some() {
            self.save();
        }
    }

    /// Where a theme setting is edited: the config file if it sets it (or no saved theme is in
    /// use), else the theme file. `None` means `path` isn't a theme setting.
    fn theme_route<'p>(&self, path: &'p [&'p str]) -> Option<(bool, &'p [&'p str])> {
        if self.theme.is_none() || path.len() < 2 || path[0] != "theme" || path[1] == "name" {
            return None;
        }
        let in_config = self.user_raw(path).is_some();
        Some((in_config, &path[1..]))
    }

    fn user_raw(&self, path: &[&str]) -> Option<&Item> {
        lookup(self.user.as_item(), path)
    }

    fn set_raw(&mut self, path: &[&str], value: impl Into<Value>) {
        if let Some(table) = table_mut(&mut self.user, &path[..path.len() - 1]) {
            set_in(table, path[path.len() - 1], value.into());
        }
    }

    fn remove_raw(&mut self, path: &[&str]) {
        if let Some(table) = table_mut(&mut self.user, &path[..path.len() - 1]) {
            table.remove(path[path.len() - 1]);
        }
    }

    /// The user's tables at `path` from every layer that has one: the saved theme first, then
    /// the config file. For tables like `[theme.colors]`, whose entries can come from either.
    pub fn user_layers(&self, path: &[&str]) -> Vec<&Item> {
        let theme = match (self.theme_route(path), &self.theme) {
            (Some((_, rest)), Some(t)) => lookup(t.doc.as_item(), rest),
            _ => None,
        };
        theme.into_iter().chain(self.user_raw(path)).collect()
    }

    // ---------- reading values ----------

    /// The user's own value at `path`, if the file (or the saved theme in use) sets it.
    pub fn user_item(&self, path: &[&str]) -> Option<&Item> {
        match (self.theme_route(path), &self.theme) {
            (Some((false, rest)), Some(t)) => lookup(t.doc.as_item(), rest),
            _ => self.user_raw(path),
        }
    }

    pub fn default_item(&self, path: &[&str]) -> Option<&Item> {
        lookup(self.defaults.as_item(), path)
    }

    /// The value in effect: the user's, or else the default.
    pub fn item(&self, path: &[&str]) -> Option<&Item> {
        self.user_item(path).or_else(|| self.default_item(path))
    }

    pub fn bool(&self, path: &[&str]) -> bool {
        self.item(path).and_then(Item::as_bool).unwrap_or(false)
    }

    pub fn int(&self, path: &[&str]) -> i64 {
        self.item(path).and_then(Item::as_integer).unwrap_or(0)
    }

    /// Integers count as floats too: `font_size = 14` is a valid float setting.
    pub fn float(&self, path: &[&str]) -> f64 {
        self.item(path).and_then(as_number).unwrap_or(0.0)
    }

    pub fn str(&self, path: &[&str]) -> String {
        self.item(path).and_then(Item::as_str).unwrap_or_default().to_string()
    }

    pub fn str_list(&self, path: &[&str]) -> Vec<String> {
        self.item(path)
            .and_then(Item::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    /// Whether the effective value differs from the built-in default.
    pub fn changed(&self, path: &[&str]) -> bool {
        match (self.user_item(path), self.default_item(path)) {
            (Some(u), Some(d)) => !same(u, d),
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    // ---------- editing ----------

    /// Set a value, keeping any comment written next to it.
    pub fn set(&mut self, path: &[&str], value: impl Into<Value>) {
        let value: Value = value.into();
        let route = self.theme_route(path);
        let table = match (route, &mut self.theme) {
            (Some((false, rest)), Some(t)) => table_mut(&mut t.doc, &rest[..rest.len() - 1]),
            _ => table_mut(&mut self.user, &path[..path.len() - 1]),
        };
        let Some(table) = table else {
            return;
        };
        let key = path[path.len() - 1];
        set_in(table, key, value);
        self.touch();
    }

    /// Put a setting back to its default. The key stays in the file with the default value, so
    /// the comments around it (the whole default config is commented) aren't lost.
    pub fn reset(&mut self, path: &[&str]) {
        // With a saved theme, writing the default would pin it; dropping the value lets the
        // theme's own (or the default) show through.
        if self.theme_route(path).is_some() {
            return self.remove(path);
        }
        match self.default_item(path).and_then(Item::as_value).cloned() {
            Some(v) => self.set(path, v),
            None => self.remove(path),
        }
    }

    pub fn remove(&mut self, path: &[&str]) {
        if let (Some((_, rest)), Some(t)) = (self.theme_route(path), &mut self.theme) {
            if let Some(table) = table_mut(&mut t.doc, &rest[..rest.len() - 1]) {
                table.remove(rest[rest.len() - 1]);
            }
            // Fall through: a value in the config file would still win.
        }
        if let Some(table) = table_mut(&mut self.user, &path[..path.len() - 1]) {
            table.remove(path[path.len() - 1]);
            self.touch();
        }
    }

    /// The user's table at `path`, created if needed. Callers must call [`Self::touch`] after
    /// changing it.
    pub fn table_mut(&mut self, path: &[&str]) -> Option<&mut Table> {
        table_mut(&mut self.user, path)
    }

    /// The user's `[[name]]` list, created if needed. Callers must call [`Self::touch`].
    pub fn array_of_tables_mut(&mut self, name: &str) -> Option<&mut toml_edit::ArrayOfTables> {
        let item = self.user.entry(name).or_insert(Item::ArrayOfTables(Default::default()));
        item.as_array_of_tables_mut()
    }

    /// Note an edit; it's saved shortly unless more edits follow.
    pub fn touch(&mut self) {
        self.pending = Some(Instant::now());
        self.status = Status::Pending;
    }

    // ---------- saving and watching ----------

    /// Save when due and pick up outside changes. Returns true when `config` changed.
    pub fn tick(&mut self) -> bool {
        if let Some(t) = self.pending
            && t.elapsed() >= SAVE_DELAY
        {
            self.pending = None;
            return self.save();
        }
        if self.pending.is_none() && self.last_check.elapsed() >= Duration::from_secs(1) {
            self.last_check = Instant::now();
            let theme_moved = self.theme.as_ref().is_some_and(|t| modified(&t.path) != t.disk_time);
            if modified(&self.path) != self.disk_time || theme_moved {
                let before = (self.disk_text.clone(), self.theme.as_ref().map(|t| t.disk_text.clone()));
                self.read();
                if (self.disk_text.clone(), self.theme.as_ref().map(|t| t.disk_text.clone())) != before {
                    self.status = Status::Reloaded(Instant::now());
                    return true;
                }
            }
        }
        false
    }

    /// When the next save is due, so the app can wake up for it.
    pub fn next_due(&self) -> Option<Duration> {
        self.pending.map(|t| SAVE_DELAY.saturating_sub(t.elapsed()))
    }

    fn save(&mut self) -> bool {
        let text = self.user.to_string();
        let config = match self.parse(&text) {
            Ok(c) => c,
            Err(e) => {
                self.status = Status::Invalid(e.0);
                return false;
            }
        };
        if let Err(e) = kiln_config::check_keymap(&config.keyboard) {
            self.status = Status::Invalid(e);
            return false;
        }
        self.config = config;
        // The theme first, so when the compositor sees the config change the theme is there.
        if let Some(t) = &mut self.theme {
            let theme_text = t.doc.to_string();
            if theme_text != t.disk_text || t.disk_time.is_none() {
                if let Err(e) = write(&t.path, &theme_text) {
                    self.status = Status::WriteFailed(format!("Can't save {}: {e}", t.path.display()));
                    return true;
                }
                t.disk_text = theme_text;
                t.disk_time = modified(&t.path);
            }
        }
        if text == self.disk_text {
            self.status = Status::Saved(Instant::now());
            return true;
        }
        match write(&self.path, &text) {
            Ok(()) => {
                self.disk_text = text;
                self.disk_time = modified(&self.path);
                self.status = Status::Saved(Instant::now());
            }
            Err(e) => self.status = Status::WriteFailed(format!("Can't save {}: {e}", self.path.display())),
        }
        true
    }
}

/// Write by replacing the file, so the compositor never reads a half-written one. A symlinked
/// config (dotfile repos) is written through the link instead, so the link stays a link.
fn write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.is_symlink() {
        return std::fs::write(path, text);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".saving");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Set `table[key]`, keeping the comments above the key and beside the old value.
pub fn set_in(table: &mut Table, key: &str, mut value: Value) {
    match table.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn lookup<'a>(mut item: &'a Item, path: &[&str]) -> Option<&'a Item> {
    for key in path {
        item = item.get(key)?;
    }
    Some(item)
}

/// Walk to (creating as needed) the table at `path`. Sub-tables become `[a.b]` sections.
fn table_mut<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Option<&'a mut Table> {
    let mut table = doc.as_table_mut();
    for key in path {
        let item = table.entry(key).or_insert_with(|| Item::Table(Table::new()));
        table = item.as_table_mut()?;
    }
    Some(table)
}

pub fn as_number(item: &Item) -> Option<f64> {
    item.as_float().or_else(|| item.as_integer().map(|i| i as f64))
}

/// Same setting value, ignoring formatting and comments; `14` and `14.0` are the same number.
fn same(a: &Item, b: &Item) -> bool {
    match (a.as_value(), b.as_value()) {
        (Some(a), Some(b)) => same_value(a, b),
        _ => a.to_string().trim() == b.to_string().trim(),
    }
}

fn same_value(a: &Value, b: &Value) -> bool {
    let num = |v: &Value| v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| same_value(a, b))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => {
                let (mut a, mut b) = (a.clone(), b.clone());
                a.decor_mut().clear();
                b.decor_mut().clear();
                a.to_string() == b.to_string()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> ConfigDoc {
        let mut d = ConfigDoc::open(PathBuf::from("/nonexistent/config.toml"));
        d.user = text.parse().unwrap();
        d
    }

    #[test]
    fn effective_values_fall_back_to_defaults() {
        let d = doc("[layout]\ngaps_inner = 20\n");
        assert_eq!(d.int(&["layout", "gaps_inner"]), 20);
        assert_eq!(d.int(&["layout", "gaps_outer"]), 6);
        assert!(d.changed(&["layout", "gaps_inner"]));
        assert!(!d.changed(&["layout", "gaps_outer"]));
        assert_eq!(d.float(&["theme", "font_size"]), 14.0);
    }

    #[test]
    fn set_keeps_comments() {
        let mut d = doc("[layout]\n# above\ngaps_inner = 6 # beside\n");
        d.set(&["layout", "gaps_inner"], 12);
        let text = d.user.to_string();
        assert!(text.contains("# above\ngaps_inner = 12 # beside"), "{text}");
    }

    #[test]
    fn set_creates_sections() {
        let mut d = doc("");
        d.set(&["theme", "colors", "accent"], "#ff0000");
        let text = d.user.to_string();
        assert!(Config::parse_user(&text).is_ok(), "{text}");
        assert!(text.contains("accent = \"#ff0000\""), "{text}");
    }

    #[test]
    fn numbers_compare_by_value() {
        let d = doc("[theme]\nfont_size = 14.0\n");
        assert!(!d.changed(&["theme", "font_size"]));
    }

    #[test]
    fn saves_only_valid_edits() {
        let dir = std::env::temp_dir().join(format!("kiln-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\n[layout]\ngaps_inner = 6\n").unwrap();
        let mut d = ConfigDoc::open(path.clone());

        d.set(&["layout", "gaps_inner"], 15);
        std::thread::sleep(SAVE_DELAY);
        assert!(d.tick());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# mine\n[layout]\ngaps_inner = 15\n");
        assert_eq!(d.config.layout.gaps_inner, 15);

        // Out of range: reported, and the file is left alone.
        d.set(&["layout", "master_ratio"], 2.0);
        std::thread::sleep(SAVE_DELAY);
        assert!(!d.tick());
        assert!(matches!(d.status, Status::Invalid(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // A keyboard layout that doesn't exist is caught too.
        d.set(&["layout", "master_ratio"], 0.5);
        d.set(&["keyboard", "layout"], "no-such-layout");
        std::thread::sleep(SAVE_DELAY);
        assert!(!d.tick());
        assert!(matches!(&d.status, Status::Invalid(e) if e.contains("no-such-layout")));

        // Edits made elsewhere are picked up.
        d.set(&["keyboard", "layout"], "us");
        std::thread::sleep(SAVE_DELAY);
        assert!(d.tick());
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "[layout]\ngaps_inner = 3\n").unwrap();
        d.last_check = Instant::now() - Duration::from_secs(2);
        assert!(d.tick());
        assert_eq!(d.int(&["layout", "gaps_inner"]), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn temp_config(tag: &str, text: &str) -> (PathBuf, ConfigDoc) {
        let dir = std::env::temp_dir().join(format!("kiln-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, ConfigDoc::open(path))
    }

    fn save_now(d: &mut ConfigDoc) {
        d.flush();
        assert!(!matches!(d.status, Status::Invalid(_) | Status::WriteFailed(_)));
    }

    #[test]
    fn themes_save_switch_and_delete() {
        let (dir, mut d) = temp_config(
            "themes",
            "# mine\n[theme]\nradius = 2\n[theme.colors]\naccent = \"#ff0000\"\n",
        );
        let red = [1.0, 0.0, 0.0, 1.0];

        // Saving moves the config's theme settings into the new file.
        d.save_theme_as("Red");
        save_now(&mut d);
        let config_text = std::fs::read_to_string(&d.path).unwrap();
        assert!(config_text.contains("name = \"Red\""), "{config_text}");
        assert!(!config_text.contains("radius"), "{config_text}");
        assert!(config_text.contains("# mine"));
        let theme_text = std::fs::read_to_string(dir.join("themes/Red.toml")).unwrap();
        assert!(theme_text.contains("radius = 2"), "{theme_text}");
        assert!(theme_text.contains("[colors]"), "{theme_text}");
        let c = Config::read(&d.path).unwrap();
        assert_eq!((c.theme.radius, c.theme.palette.accent), (2.0, red));

        // Edits now go to the theme file.
        d.set(&["theme", "radius"], 7);
        save_now(&mut d);
        assert!(
            std::fs::read_to_string(dir.join("themes/Red.toml"))
                .unwrap()
                .contains("radius = 7")
        );
        assert!(!std::fs::read_to_string(&d.path).unwrap().contains("radius"));
        assert!(d.changed(&["theme", "radius"]));

        // Back to built-in, then a change there is kept when switching away.
        assert_eq!(d.use_theme(None), None);
        save_now(&mut d);
        assert_eq!(Config::read(&d.path).unwrap().theme.radius, 6.0);
        d.set(&["theme", "radius"], 11);
        assert_eq!(d.use_theme(Some("Red")).as_deref(), Some("Custom"));
        save_now(&mut d);
        assert_eq!(d.theme_names(), ["Custom", "Red"]);
        let c = Config::read(&d.path).unwrap();
        assert_eq!((c.theme.name.as_deref(), c.theme.radius), (Some("Red"), 7.0));
        d.use_theme(Some("Custom"));
        save_now(&mut d);
        assert_eq!(Config::read(&d.path).unwrap().theme.radius, 11.0);

        // Deleting the theme in use falls back to built-in.
        d.delete_theme("Custom").unwrap();
        assert_eq!(d.theme_name(), None);
        assert_eq!(d.theme_names(), ["Red"]);
        assert_eq!(Config::read(&d.path).unwrap().theme.radius, 6.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_values_win_over_the_theme() {
        let (dir, mut d) = temp_config("override", "[theme]\nname = \"T\"\nfont_size = 20\n");
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(dir.join("themes/T.toml"), "font_size = 10\nradius = 3\n").unwrap();
        d.read();
        assert_eq!(d.float(&["theme", "font_size"]), 20.0);
        assert_eq!(d.float(&["theme", "radius"]), 3.0);
        assert_eq!(d.theme_overrides(), ["font_size"]);
        // Editing it changes the value that's in effect: the config's.
        d.set(&["theme", "font_size"], 18);
        save_now(&mut d);
        assert_eq!(Config::read(&d.path).unwrap().theme.font_size, 18.0);
        // Moving it into the theme leaves the config with just the name.
        d.save_theme_as("T");
        save_now(&mut d);
        assert!(d.theme_overrides().is_empty());
        let c = Config::read(&d.path).unwrap();
        assert_eq!((c.theme.font_size, c.theme.radius), (18.0, 3.0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reset_writes_the_default() {
        let mut d = doc("[layout]\ngaps_inner = 20 # mine\n");
        d.reset(&["layout", "gaps_inner"]);
        assert!(!d.changed(&["layout", "gaps_inner"]));
        assert!(d.user.to_string().contains("# mine"));
    }
}
