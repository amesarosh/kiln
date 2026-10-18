//! TOML config: loading, validation, and turning keybind strings into matchable values.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use smithay::{
    input::keyboard::{
        ModifiersState,
        xkb::{self, Keysym},
    },
    utils::Transform,
};

use crate::{
    layout::{LayoutKind, Params},
    theme::{self, ColorDefs, Palette, Theme},
};

pub const DEFAULT_CONFIG: &str = include_str!("../resources/default-config.toml");

// ---------- raw file format ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    general: General,
    theme: RawTheme,
    layout: LayoutConfig,
    decoration: RawDecoration,
    bar: RawBar,
    keyboard: KeyboardConfig,
    input: InputConfig,
    binds: BTreeMap<String, String>,
    #[serde(default)]
    rules: Vec<WindowRule>,
    #[serde(default)]
    outputs: Vec<RawOutput>,
}

/// User files may omit any section; missing pieces come from the default config.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PartialConfig {
    general: Option<toml::Table>,
    theme: Option<toml::Table>,
    layout: Option<toml::Table>,
    decoration: Option<toml::Table>,
    bar: Option<toml::Table>,
    keyboard: Option<toml::Table>,
    input: Option<toml::Table>,
    binds: Option<BTreeMap<String, String>>,
    rules: Option<Vec<toml::Table>>,
    outputs: Option<Vec<toml::Table>>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModKey {
    Auto,
    Super,
    Alt,
    Ctrl,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NewWindow {
    Master,
    Stack,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(rename = "mod")]
    pub mod_key: ModKey,
    pub focus_follows_mouse: bool,
    pub new_window: NewWindow,
    pub autostart: Vec<String>,
    /// Seconds without input before monitors turn off; 0 never.
    pub monitors_off_after: u64,
    /// Run X11 apps through Xwayland. Read at startup only.
    pub xwayland: bool,
    /// Keep the clipboard after the app that copied closes.
    pub keep_clipboard: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutConfig {
    pub default: LayoutKind,
    pub master_ratio: f32,
    pub master_count: usize,
    pub gaps_inner: i32,
    pub gaps_outer: i32,
    pub smart_gaps: bool,
    pub workspaces: usize,
    /// Per-monitor default layouts from `[[outputs]]`, by name.
    #[serde(skip)]
    pub per_output: Vec<(String, LayoutKind)>,
}

impl LayoutConfig {
    /// The layout new and reset workspaces start with on the monitor with this name.
    pub fn default_for(&self, output: &str) -> LayoutKind {
        self.per_output
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(output))
            .map_or(self.default, |(_, l)| *l)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    font: String,
    font_size: f32,
    radius: f32,
    icon_theme: String,
    colors: ColorDefs,
}

/// Every component section can carry a `colors` table that overrides theme colors for it alone.
/// The older single-color keys still work and are treated as such overrides.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDecoration {
    border_width: i32,
    border_active: Option<String>,
    border_inactive: Option<String>,
    background: Option<String>,
    #[serde(default)]
    colors: ColorDefs,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BarPosition {
    Top,
    Bottom,
    Left,
    Right,
}

impl BarPosition {
    /// Left and right bars run down the side of the monitor.
    pub fn vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBar {
    enabled: bool,
    position: BarPosition,
    height: i32,
    width: i32,
    font: Option<String>,
    font_size: Option<f32>,
    background: Option<String>,
    foreground: Option<String>,
    accent: Option<String>,
    dim: Option<String>,
    launcher: String,
    #[serde(default)]
    colors: ColorDefs,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyboardConfig {
    pub layout: String,
    pub variant: String,
    pub options: String,
    pub repeat_rate: i32,
    pub repeat_delay: i32,
}

/// Pointer and touchpad settings (hardware backend only).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    pub tap_to_click: bool,
    pub natural_scroll: bool,
    pub pointer_speed: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub floating: Option<bool>,
    pub workspace: Option<usize>,
    /// Monitor to open on, by connector name (e.g. "DP-5"). Ignored if it isn't connected.
    pub output: Option<String>,
}

/// One `[[outputs]]` entry: settings for a monitor, matched by connector name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    name: String,
    #[serde(default = "yes")]
    enabled: bool,
    mode: Option<String>,
    scale: Option<f64>,
    position: Option<[i32; 2]>,
    transform: Option<String>,
    /// Where this monitor's bar goes, overriding `[bar] position`; "none" hides it.
    bar: Option<String>,
    /// This monitor's default layout, overriding `[layout] default`.
    layout: Option<LayoutKind>,
}

fn yes() -> bool {
    true
}

// ---------- validated config ----------

/// A requested resolution, with an optional refresh rate in Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModeSpec {
    pub width: i32,
    pub height: i32,
    pub refresh: Option<f64>,
}

/// Settings for one monitor. Anything left out keeps the monitor's own default.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    pub name: String,
    pub enabled: bool,
    /// None: the monitor's preferred mode.
    pub mode: Option<ModeSpec>,
    pub scale: f64,
    /// None: placed to the right of the other monitors.
    pub position: Option<(i32, i32)>,
    pub transform: Transform,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            mode: None,
            scale: 1.0,
            position: None,
            transform: Transform::Normal,
        }
    }
}

pub type Color = [f32; 4];

#[derive(Debug, Clone)]
pub struct Decoration {
    pub border_width: i32,
    pub border_active: Color,
    pub border_inactive: Color,
    pub background: Color,
}

impl BarConfig {
    /// Where the bar goes on the monitor with this connector name, or `None` for no bar.
    pub fn position_for(&self, output: &str) -> Option<BarPosition> {
        if !self.enabled {
            return None;
        }
        match self.per_output.iter().find(|(n, _)| n.eq_ignore_ascii_case(output)) {
            Some((_, p)) => *p,
            None => Some(self.position),
        }
    }
}

/// The built-in clickable bar.
#[derive(Debug, Clone, PartialEq)]
pub struct BarConfig {
    pub enabled: bool,
    pub position: BarPosition,
    /// Thickness of a top or bottom bar, and the row height of a side bar.
    pub height: i32,
    /// Thickness of a left or right bar.
    pub width: i32,
    /// Per-monitor positions from `[[outputs]]`, by name; `None` means no bar there.
    pub per_output: Vec<(String, Option<BarPosition>)>,
    pub font: String,
    pub font_size: f32,
    pub palette: Palette,
    /// From `[theme]`, so the bar repaints when they change.
    pub radius: f32,
    pub icon_theme: String,
    /// Command run by the bar's + button.
    pub launcher: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub general: General,
    pub theme: Theme,
    pub layout: LayoutConfig,
    pub decoration: Decoration,
    pub bar: BarConfig,
    pub keyboard: KeyboardConfig,
    pub input: InputConfig,
    pub binds: Vec<Bind>,
    pub rules: Vec<WindowRule>,
    pub outputs: Vec<OutputConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub logo: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
}

impl Mods {
    /// The state with only `mod_key` held.
    pub fn only_mod(mod_key: ModKey) -> Self {
        let mut m = Self::default();
        match mod_key {
            ModKey::Super | ModKey::Auto => m.logo = true,
            ModKey::Alt => m.alt = true,
            ModKey::Ctrl => m.ctrl = true,
        }
        m
    }

    pub fn from_state(m: &ModifiersState) -> Self {
        Self {
            logo: m.logo,
            alt: m.alt,
            ctrl: m.ctrl,
            shift: m.shift,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bind {
    /// `Mod` is kept symbolic so the same config works nested and on hardware.
    pub uses_mod: bool,
    pub mods: Mods,
    /// `None` is a bare `"Mod"` bind: it fires when Mod is tapped on its own.
    pub key: Option<Keysym>,
    pub action: Action,
}

impl Bind {
    pub fn matches(&self, pressed: Mods, key: Keysym, mod_key: ModKey) -> bool {
        let mut want = self.mods;
        if self.uses_mod {
            match mod_key {
                ModKey::Super | ModKey::Auto => want.logo = true,
                ModKey::Alt => want.alt = true,
                ModKey::Ctrl => want.ctrl = true,
            }
        }
        want == pressed && self.key == Some(key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutChoice {
    Set(LayoutKind),
    Next,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Spawn(String),
    Close,
    Quit,
    Reload,
    Focus(Dir),
    Swap(Dir),
    Zoom,
    MasterRatio(f32),
    MasterCount(i32),
    Layout(LayoutChoice),
    ToggleFloating,
    ToggleFullscreen,
    Workspace(usize),
    MoveToWorkspace(usize),
    /// Move keyboard focus (and the pointer) to the next or previous monitor.
    FocusMonitor(Dir),
    /// Send the focused window to the next or previous monitor.
    MoveToMonitor(Dir),
    /// Ctrl+Alt+F1..F12 on the hardware backend; not configurable.
    SwitchVt(i32),
    /// Turn every monitor off until the next key press, click or pointer movement.
    PowerOffMonitors,
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError(msg.into())
}

impl Default for Config {
    fn default() -> Self {
        Self::parse(DEFAULT_CONFIG).expect("built-in default config must be valid")
    }
}

impl Config {
    /// `~/.config/wayland-time/config.toml` (respecting `$XDG_CONFIG_HOME`).
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("wayland-time").join("config.toml"))
    }

    /// Load from disk. A missing file is created with the defaults so users have something to edit.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse_user(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::write(path, DEFAULT_CONFIG) {
                    Ok(()) => tracing::info!("wrote default config to {}", path.display()),
                    Err(e) => tracing::warn!("could not write default config: {e}"),
                }
                Ok(Self::default())
            }
            Err(e) => Err(err(format!("reading {}: {e}", path.display()))),
        }
    }

    /// Parse a user file, filling any missing sections or keys from the built-in defaults.
    pub fn parse_user(text: &str) -> Result<Self, ConfigError> {
        let user: PartialConfig = toml::from_str(text).map_err(|e| err(e.to_string()))?;
        let mut base: toml::Table = toml::from_str(DEFAULT_CONFIG).expect("default config parses");

        let mut merge = |name: &str, over: Option<toml::Table>| {
            if let (Some(over), Some(toml::Value::Table(dst))) = (over, base.get_mut(name)) {
                merge_tables(dst, over);
            }
        };
        merge("general", user.general);
        merge("theme", user.theme);
        merge("layout", user.layout);
        merge("decoration", user.decoration);
        merge("bar", user.bar);
        merge("keyboard", user.keyboard);
        merge("input", user.input);
        if let Some(binds) = user.binds {
            let table = binds.into_iter().map(|(k, v)| (k, toml::Value::String(v))).collect();
            base.insert("binds".into(), toml::Value::Table(table));
        }
        if let Some(rules) = user.rules {
            base.insert(
                "rules".into(),
                toml::Value::Array(rules.into_iter().map(toml::Value::Table).collect()),
            );
        }
        if let Some(outputs) = user.outputs {
            base.insert(
                "outputs".into(),
                toml::Value::Array(outputs.into_iter().map(toml::Value::Table).collect()),
            );
        }

        let raw: RawConfig = base.try_into().map_err(|e: toml::de::Error| err(e.to_string()))?;
        Self::validate(raw)
    }

    /// Parse a complete config (every section present).
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| err(e.to_string()))?;
        Self::validate(raw)
    }

    fn validate(raw: RawConfig) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();

        let mut binds = Vec::with_capacity(raw.binds.len());
        for (combo, action) in &raw.binds {
            match (parse_combo(combo), parse_action(action)) {
                (Ok((uses_mod, mods, key)), Ok(action)) => binds.push(Bind {
                    uses_mod,
                    mods,
                    key,
                    action,
                }),
                (Err(e), _) | (_, Err(e)) => problems.push(format!("bind \"{combo}\": {e}")),
            }
        }

        let rt = &raw.theme;
        if !(6.0..=48.0).contains(&rt.font_size) {
            problems.push("theme.font_size must be between 6 and 48".into());
        }
        // Resolve the theme first. If it's broken, report that once and build the components on
        // the default theme, so their own errors still show up but the theme's don't repeat.
        let theme_palette = theme::resolve(&rt.colors, &ColorDefs::new(), "theme.colors");
        let base_defs = match &theme_palette {
            Ok(_) => rt.colors.clone(),
            Err(e) => {
                problems.extend(e.iter().cloned());
                let defaults: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
                defaults.theme.colors
            }
        };
        let fallback = || theme::resolve(&base_defs, &ColorDefs::new(), "").expect("default theme resolves");
        // `legacy` maps older keys to the theme color they now override: (key, color, value).
        let palette =
            |legacy: &[(&str, &str, &Option<String>)], colors: &ColorDefs, scope: &str, problems: &mut Vec<String>| {
                let mut overrides: ColorDefs = legacy
                    .iter()
                    .filter_map(|(_, role, v)| v.as_ref().map(|v| (role.to_string(), v.clone())))
                    .collect();
                overrides.extend(colors.clone());
                theme::resolve(&base_defs, &overrides, scope).unwrap_or_else(|e| {
                    // Name the key the user actually wrote.
                    problems.extend(e.into_iter().map(|mut msg| {
                        for (key, role, v) in legacy {
                            if v.is_some() && !colors.contains_key(*role) {
                                msg = msg.replace(&format!("{scope}: {role} = "), &format!("{scope}.{key} = "));
                            }
                        }
                        msg
                    }));
                    fallback()
                })
            };

        if !(0.0..=24.0).contains(&rt.radius) {
            problems.push("theme.radius must be between 0 and 24".into());
        }
        let theme = Theme {
            font: rt.font.clone(),
            font_size: rt.font_size,
            radius: rt.radius,
            icon_theme: rt.icon_theme.clone(),
            palette: theme_palette.unwrap_or_else(|_| fallback()),
        };

        let rd = &raw.decoration;
        let deco = palette(
            &[
                ("border_active", "border_focused", &rd.border_active),
                ("border_inactive", "border", &rd.border_inactive),
                ("background", "base", &rd.background),
            ],
            &rd.colors,
            "decoration",
            &mut problems,
        );
        let decoration = Decoration {
            border_width: rd.border_width.max(0),
            border_active: deco.border_focused,
            border_inactive: deco.border,
            background: deco.base,
        };

        let rb = &raw.bar;
        if !(16..=96).contains(&rb.height) {
            problems.push("bar.height must be between 16 and 96".into());
        }
        if rb.font_size.is_some_and(|f| !(6.0..=48.0).contains(&f)) {
            problems.push("bar.font_size must be between 6 and 48".into());
        }
        if !(40..=800).contains(&rb.width) {
            problems.push("bar.width must be between 40 and 800".into());
        }
        let per_output = raw
            .outputs
            .iter()
            .filter_map(|o| {
                let pos = o.bar.as_deref()?;
                let parsed = match pos {
                    "none" => Ok(None),
                    "top" => Ok(Some(BarPosition::Top)),
                    "bottom" => Ok(Some(BarPosition::Bottom)),
                    "left" => Ok(Some(BarPosition::Left)),
                    "right" => Ok(Some(BarPosition::Right)),
                    other => Err(format!(
                        "outputs \"{}\": bar: \"{other}\" is not top, bottom, left, right or none",
                        o.name
                    )),
                };
                parsed.map_err(|e| problems.push(e)).ok().map(|p| (o.name.clone(), p))
            })
            .collect();
        let bar = BarConfig {
            enabled: rb.enabled,
            position: rb.position,
            height: rb.height,
            width: rb.width,
            per_output,
            font: rb.font.clone().unwrap_or_else(|| theme.font.clone()),
            font_size: rb.font_size.unwrap_or(theme.font_size),
            radius: theme.radius,
            icon_theme: theme.icon_theme.clone(),
            palette: palette(
                &[
                    ("background", "surface", &rb.background),
                    ("foreground", "text", &rb.foreground),
                    ("accent", "accent", &rb.accent),
                    ("dim", "subtext", &rb.dim),
                ],
                &rb.colors,
                "bar",
                &mut problems,
            ),
            launcher: rb.launcher.clone(),
        };

        let mut layout = raw.layout;
        layout.per_output = raw
            .outputs
            .iter()
            .filter_map(|o| Some((o.name.clone(), o.layout?)))
            .collect();
        if !(0.05..=0.95).contains(&layout.master_ratio) {
            problems.push("layout.master_ratio must be between 0.05 and 0.95".into());
        }
        if !(-1.0..=1.0).contains(&raw.input.pointer_speed) {
            problems.push("input.pointer_speed must be between -1.0 and 1.0".into());
        }
        if !(1..=32).contains(&layout.workspaces) {
            problems.push("layout.workspaces must be between 1 and 32".into());
        }
        for (i, rule) in raw.rules.iter().enumerate() {
            if rule.app_id.is_none() && rule.title.is_none() {
                problems.push(format!("rules[{i}]: needs app_id or title"));
            }
            if let Some(ws) = rule.workspace
                && (ws == 0 || ws > layout.workspaces)
            {
                problems.push(format!("rules[{i}]: workspace {ws} does not exist"));
            }
        }

        let mut outputs: Vec<OutputConfig> = Vec::with_capacity(raw.outputs.len());
        for o in raw.outputs {
            let at = format!("outputs \"{}\"", o.name);
            if outputs.iter().any(|x| x.name.eq_ignore_ascii_case(&o.name)) {
                problems.push(format!("{at}: listed twice"));
            }
            let mode = o.mode.as_deref().filter(|m| *m != "preferred").and_then(|m| {
                parse_mode(m)
                    .map_err(|e| problems.push(format!("{at}: mode: {e}")))
                    .ok()
            });
            let scale = o.scale.unwrap_or(1.0);
            if !(0.5..=4.0).contains(&scale) {
                problems.push(format!("{at}: scale must be between 0.5 and 4"));
            }
            let transform = o.transform.as_deref().map_or(Ok(Transform::Normal), parse_transform);
            let transform = transform.unwrap_or_else(|e| {
                problems.push(format!("{at}: transform: {e}"));
                Transform::Normal
            });
            outputs.push(OutputConfig {
                name: o.name,
                enabled: o.enabled,
                mode,
                scale,
                position: o.position.map(|[x, y]| (x, y)),
                transform,
            });
        }

        if !problems.is_empty() {
            return Err(err(problems.join("\n")));
        }

        Ok(Self {
            general: raw.general,
            theme,
            layout,
            decoration,
            bar,
            keyboard: raw.keyboard,
            input: raw.input,
            binds,
            rules: raw.rules,
            outputs,
        })
    }

    /// Settings for a monitor by connector name; defaults if it isn't listed.
    pub fn output(&self, name: &str) -> OutputConfig {
        self.outputs
            .iter()
            .find(|o| o.name.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| OutputConfig {
                name: name.to_string(),
                ..Default::default()
            })
    }

    pub fn layout_params(&self) -> Params {
        Params {
            master_ratio: self.layout.master_ratio,
            master_count: self.layout.master_count,
            gaps_inner: self.layout.gaps_inner.max(0),
            gaps_outer: self.layout.gaps_outer.max(0),
            smart_gaps: self.layout.smart_gaps,
        }
    }
}

impl WindowRule {
    pub fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        let app_ok = match &self.app_id {
            Some(want) => app_id == Some(want.as_str()),
            None => true,
        };
        let title_ok = match &self.title {
            Some(want) => title.is_some_and(|t| t.contains(want.as_str())),
            None => true,
        };
        app_ok && title_ok
    }
}

/// "Mod+Shift+Return" -> (uses Mod, explicit modifiers, keysym).
/// A bare "Mod" has no key: it means "tap Mod by itself".
fn parse_combo(s: &str) -> Result<(bool, Mods, Option<Keysym>), String> {
    if s.trim().eq_ignore_ascii_case("mod") {
        return Ok((true, Mods::default(), None));
    }
    let mut parts: Vec<&str> = s.split('+').map(str::trim).collect();
    // Allow binding the plus key itself: "Mod+plus" is preferred, but "Mod++" also works.
    if s.ends_with("++") {
        parts.retain(|p| !p.is_empty());
        parts.push("plus");
    }
    let key_name = parts.pop().filter(|k| !k.is_empty()).ok_or("missing key")?;

    let mut uses_mod = false;
    let mut mods = Mods::default();
    for m in parts {
        match m.to_ascii_lowercase().as_str() {
            "mod" => uses_mod = true,
            "super" | "logo" | "win" | "meta" => mods.logo = true,
            "alt" => mods.alt = true,
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            other => return Err(format!("unknown modifier \"{other}\"")),
        }
    }

    let key = xkb::keysym_from_name(key_name, xkb::KEYSYM_NO_FLAGS);
    let key = if key.raw() == 0 {
        xkb::keysym_from_name(key_name, xkb::KEYSYM_CASE_INSENSITIVE)
    } else {
        key
    };
    if key.raw() == 0 {
        return Err(format!("unknown key \"{key_name}\""));
    }
    Ok((uses_mod, mods, Some(normalize_keysym(key))))
}

/// Compare letters case-insensitively so "Mod+Shift+q" works whether or not Shift changes the sym.
pub fn normalize_keysym(k: Keysym) -> Keysym {
    // Latin capitals A-Z share keysym values with ASCII.
    match k.raw() {
        r @ 0x41..=0x5a => Keysym::new(r + 0x20),
        _ => k,
    }
}

fn parse_action(s: &str) -> Result<Action, String> {
    let s = s.trim();
    let (name, arg) = match s.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (s, ""),
    };
    let need_arg = || {
        if arg.is_empty() {
            Err(format!("\"{name}\" needs an argument"))
        } else {
            Ok(arg)
        }
    };
    let dir = || match need_arg()? {
        "next" => Ok(Dir::Next),
        "prev" => Ok(Dir::Prev),
        o => Err(format!("expected next or prev, got \"{o}\"")),
    };
    let workspace = || {
        need_arg()?
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("\"{arg}\" is not a workspace number"))
    };

    Ok(match name {
        "spawn" => Action::Spawn(need_arg()?.to_string()),
        "close" => Action::Close,
        "quit" | "exit" => Action::Quit,
        "reload" => Action::Reload,
        "focus" => Action::Focus(dir()?),
        "swap" => Action::Swap(dir()?),
        "zoom" => Action::Zoom,
        "master-ratio" => Action::MasterRatio(need_arg()?.parse().map_err(|_| format!("\"{arg}\" is not a number"))?),
        "master-count" => Action::MasterCount(
            need_arg()?
                .parse()
                .map_err(|_| format!("\"{arg}\" is not a whole number"))?,
        ),
        "layout" => Action::Layout(match need_arg()? {
            "tile" => LayoutChoice::Set(LayoutKind::Tile),
            "monocle" => LayoutChoice::Set(LayoutKind::Monocle),
            "next" => LayoutChoice::Next,
            o => return Err(format!("unknown layout \"{o}\"")),
        }),
        "toggle-floating" => Action::ToggleFloating,
        "toggle-fullscreen" => Action::ToggleFullscreen,
        "workspace" => Action::Workspace(workspace()?),
        "move-to-workspace" => Action::MoveToWorkspace(workspace()?),
        "focus-monitor" => Action::FocusMonitor(dir()?),
        "move-to-monitor" => Action::MoveToMonitor(dir()?),
        "power-off-monitors" => Action::PowerOffMonitors,
        other => return Err(format!("unknown action \"{other}\"")),
    })
}

/// "2560x1440" or "2560x1440@144" (refresh in Hz, decimals allowed).
fn parse_mode(s: &str) -> Result<ModeSpec, String> {
    let bad = || format!("\"{s}\" is not WIDTHxHEIGHT or WIDTHxHEIGHT@HZ");
    let (size, refresh) = match s.split_once('@') {
        Some((size, hz)) => {
            let hz: f64 = hz.trim().trim_end_matches("Hz").parse().map_err(|_| bad())?;
            if hz <= 0.0 {
                return Err(bad());
            }
            (size, Some(hz))
        }
        None => (s, None),
    };
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    let width: i32 = w.trim().parse().map_err(|_| bad())?;
    let height: i32 = h.trim().parse().map_err(|_| bad())?;
    if width <= 0 || height <= 0 {
        return Err(bad());
    }
    Ok(ModeSpec { width, height, refresh })
}

/// Rotation in degrees counter-clockwise, optionally mirrored first.
fn parse_transform(s: &str) -> Result<Transform, String> {
    Ok(match s {
        "normal" | "0" => Transform::Normal,
        "90" => Transform::_90,
        "180" => Transform::_180,
        "270" => Transform::_270,
        "flipped" => Transform::Flipped,
        "flipped-90" => Transform::Flipped90,
        "flipped-180" => Transform::Flipped180,
        "flipped-270" => Transform::Flipped270,
        _ => {
            return Err(format!(
                "\"{s}\" is not normal, 90, 180, 270, flipped, flipped-90, flipped-180 or flipped-270"
            ));
        }
    })
}

/// Copy `over` into `dst`, merging nested tables key by key instead of replacing them.
fn merge_tables(dst: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (dst.get_mut(&k), v) {
            (Some(toml::Value::Table(d)), toml::Value::Table(o)) => merge_tables(d, o),
            (_, v) => {
                dst.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let c = Config::default();
        assert!(c.binds.len() > 20);
        assert_eq!(c.layout.workspaces, 9);
    }

    #[test]
    fn partial_user_config_inherits_defaults() {
        let c = Config::parse_user("[layout]\ngaps_inner = 20\n").unwrap();
        assert_eq!(c.layout.gaps_inner, 20);
        assert_eq!(c.layout.master_count, 1);
        assert!(!c.binds.is_empty());
    }

    #[test]
    fn user_binds_replace_defaults() {
        let c = Config::parse_user("[binds]\n\"Mod+x\" = \"close\"\n").unwrap();
        assert_eq!(c.binds.len(), 1);
    }

    #[test]
    fn errors_are_collected() {
        let e = Config::parse_user(
            "[binds]\n\"Mod+nope\" = \"close\"\n\"Mod+a\" = \"fly\"\n[decoration]\nbackground = \"red\"\n",
        )
        .unwrap_err()
        .0;
        assert!(e.contains("unknown key"), "{e}");
        assert!(e.contains("unknown action"), "{e}");
        assert!(e.contains("decoration.background = \"red\""), "{e}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(Config::parse_user("[layout]\ngapz = 3\n").is_err());
    }

    #[test]
    fn combos() {
        let (m, mods, key) = parse_combo("Mod+Shift+Return").unwrap();
        assert!(m && mods.shift && !mods.alt);
        assert_eq!(key, Some(Keysym::Return));
        let (_, _, key) = parse_combo("Mod+Q").unwrap();
        assert_eq!(key, Some(Keysym::q));
        assert!(parse_combo("Mod+Hyper+a").is_err());
        assert_eq!(parse_combo("Mod").unwrap(), (true, Mods::default(), None));
        assert!(parse_combo("Super").is_err());
    }

    #[test]
    fn bind_matching_resolves_mod() {
        let (uses_mod, mods, key) = parse_combo("Mod+j").unwrap();
        let b = Bind {
            uses_mod,
            mods,
            key,
            action: Action::Zoom,
        };
        let alt = Mods {
            alt: true,
            ..Default::default()
        };
        let logo = Mods {
            logo: true,
            ..Default::default()
        };
        assert!(b.matches(alt, Keysym::j, ModKey::Alt));
        assert!(!b.matches(alt, Keysym::j, ModKey::Super));
        assert!(b.matches(logo, Keysym::j, ModKey::Super));
    }

    #[test]
    fn actions() {
        assert_eq!(
            parse_action("spawn foot -e htop").unwrap(),
            Action::Spawn("foot -e htop".into())
        );
        assert_eq!(parse_action("master-ratio -0.05").unwrap(), Action::MasterRatio(-0.05));
        assert_eq!(parse_action("workspace 3").unwrap(), Action::Workspace(3));
        assert_eq!(parse_action("power-off-monitors").unwrap(), Action::PowerOffMonitors);
        assert!(parse_action("workspace 0").is_err());
        assert!(parse_action("focus up").is_err());
        assert!(parse_action("spawn").is_err());
    }

    #[test]
    fn outputs() {
        let c = Config::parse_user(
            "[[outputs]]\nname = \"DP-1\"\nmode = \"2560x1440@143.9\"\nscale = 1.5\nposition = [0, 0]\ntransform = \"90\"\n\
             [[outputs]]\nname = \"HDMI-A-1\"\nenabled = false\n",
        )
        .unwrap();
        let dp = c.output("dp-1");
        assert_eq!(
            dp.mode,
            Some(ModeSpec {
                width: 2560,
                height: 1440,
                refresh: Some(143.9)
            })
        );
        assert_eq!(
            (dp.scale, dp.position, dp.transform),
            (1.5, Some((0, 0)), Transform::_90)
        );
        assert!(!c.output("HDMI-A-1").enabled);
        assert_eq!(
            c.output("eDP-1"),
            OutputConfig {
                name: "eDP-1".into(),
                ..Default::default()
            }
        );
        assert_eq!(parse_mode("1920x1080").unwrap().refresh, None);
        assert!(parse_mode("1920").is_err());
        assert!(parse_mode("1920x1080@0").is_err());

        let e = Config::parse_user(
            "[[outputs]]\nname = \"DP-1\"\nscale = 9\ntransform = \"sideways\"\n[[outputs]]\nname = \"dp-1\"\n",
        )
        .unwrap_err()
        .0;
        assert!(
            e.contains("scale") && e.contains("transform") && e.contains("twice"),
            "{e}"
        );
    }

    #[test]
    fn theme_flows_into_components() {
        let c = Config::parse_user("[theme.colors]\naccent = \"#ff0000\"\n").unwrap();
        assert_eq!(c.theme.palette.accent, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.bar.palette.accent, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.decoration.border_active, [1.0, 0.0, 0.0, 1.0]);
        // Only accent changed; the rest of the default palette is kept.
        assert_eq!(c.theme.palette.text, Config::default().theme.palette.text);
    }

    #[test]
    fn component_overrides() {
        let c = Config::parse_user("[bar.colors]\nsurface = \"#ffffff\"\n[bar]\nfont_size = 20\n").unwrap();
        assert_eq!(c.bar.palette.surface, [1.0; 4]);
        assert_ne!(c.theme.palette.surface, [1.0; 4]);
        assert_eq!(c.bar.font_size, 20.0);
        assert_eq!(c.bar.font, c.theme.font);
        // Older per-section keys still work.
        let c = Config::parse_user("[decoration]\nborder_active = \"#00ff00\"\n").unwrap();
        assert_eq!(c.decoration.border_active, [0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn bar_position_per_output() {
        let c = Config::parse_user(
            "[bar]\nposition = \"top\"\n[[outputs]]\nname = \"DP-4\"\nbar = \"left\"\n[[outputs]]\nname = \"DP-5\"\nbar = \"none\"\n",
        )
        .unwrap();
        assert_eq!(c.bar.position_for("dp-4"), Some(BarPosition::Left));
        assert_eq!(c.bar.position_for("DP-5"), None);
        assert_eq!(c.bar.position_for("HDMI-A-5"), Some(BarPosition::Top));
        assert!(Config::parse_user("[[outputs]]\nname = \"DP-4\"\nbar = \"middle\"\n").is_err());
    }

    #[test]
    fn layout_per_output() {
        let c = Config::parse_user("[[outputs]]\nname = \"DP-4\"\nlayout = \"monocle\"\n").unwrap();
        assert_eq!(c.layout.default_for("dp-4"), LayoutKind::Monocle);
        assert_eq!(c.layout.default_for("DP-5"), LayoutKind::Tile);
    }

    #[test]
    fn theme_errors_reported_once() {
        let e = Config::parse_user("[theme.colors]\naccent = \"nope\"\n").unwrap_err().0;
        assert_eq!(e.lines().count(), 1, "{e}");
    }

    #[test]
    fn rules() {
        let r = WindowRule {
            app_id: Some("mpv".into()),
            title: None,
            floating: Some(true),
            workspace: None,
            output: None,
        };
        assert!(r.matches(Some("mpv"), Some("video.mkv")));
        assert!(!r.matches(Some("mpv2"), None));
        let r = WindowRule {
            app_id: None,
            title: Some("Picture".into()),
            floating: None,
            workspace: None,
            output: None,
        };
        assert!(r.matches(None, Some("Picture-in-Picture")));
    }
}
