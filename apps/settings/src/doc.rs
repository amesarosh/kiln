//! The user's config file as an editable document.
//!
//! Edits go into a `toml_edit` document, so the user's comments and layout survive. Shortly after
//! the last edit the file is checked with the compositor's own config loader and, only if it
//! loads, written; the compositor reloads it on save, so changes apply right away.

use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use toml_edit::{DocumentMut, Item, Table, Value};
use wt_config::{Config, DEFAULT_CONFIG};

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

pub struct ConfigDoc {
    pub path: PathBuf,
    user: DocumentMut,
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
                if let Ok(c) = Config::parse_user(&text) {
                    self.config = c;
                }
            }
            Err(e) => self.broken = Some(format!("{} has a syntax error:\n{e}", self.path.display())),
        }
        self.disk_text = text;
        self.generation += 1;
    }

    // ---------- reading values ----------

    /// The user's own value at `path`, if the file sets it.
    pub fn user_item(&self, path: &[&str]) -> Option<&Item> {
        lookup(self.user.as_item(), path)
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
        let Some(table) = table_mut(&mut self.user, &path[..path.len() - 1]) else {
            return;
        };
        let key = path[path.len() - 1];
        set_in(table, key, value);
        self.touch();
    }

    /// Put a setting back to its default. The key stays in the file with the default value, so
    /// the comments around it (the whole default config is commented) aren't lost.
    pub fn reset(&mut self, path: &[&str]) {
        match self.default_item(path).and_then(Item::as_value).cloned() {
            Some(v) => self.set(path, v),
            None => self.remove(path),
        }
    }

    pub fn remove(&mut self, path: &[&str]) {
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
            if modified(&self.path) != self.disk_time {
                let before = self.disk_text.clone();
                self.read();
                if self.disk_text != before {
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
        let config = match Config::parse_user(&text) {
            Ok(c) => c,
            Err(e) => {
                self.status = Status::Invalid(e.0);
                return false;
            }
        };
        if let Err(e) = wt_config::check_keymap(&config.keyboard) {
            self.status = Status::Invalid(e);
            return false;
        }
        self.config = config;
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
    let tmp = path.with_extension("toml.saving");
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
        let dir = std::env::temp_dir().join(format!("wt-settings-test-{}", std::process::id()));
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

    #[test]
    fn reset_writes_the_default() {
        let mut d = doc("[layout]\ngaps_inner = 20 # mine\n");
        d.reset(&["layout", "gaps_inner"]);
        assert!(!d.changed(&["layout", "gaps_inner"]));
        assert!(d.user.to_string().contains("# mine"));
    }
}
