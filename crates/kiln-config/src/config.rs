//! TOML config: loading, validation, and turning keybind strings into matchable values.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use xkbcommon::xkb::{self, Keysym};

use crate::theme::{self, ColorDefs, Palette, Theme};

/// The fully commented default config, written for users on first start.
pub const DEFAULT_CONFIG: &str = include_str!("../../../resources/default-config.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LayoutKind {
    /// Master area on the left, remaining windows stacked on the right.
    #[default]
    Tile,
    /// Master area on top, remaining windows side by side underneath.
    Vertical,
    /// Every window takes the full area; only the focused one is visible.
    Monocle,
}

impl LayoutKind {
    pub fn next(self) -> Self {
        match self {
            Self::Tile => Self::Vertical,
            Self::Vertical => Self::Monocle,
            Self::Monocle => Self::Tile,
        }
    }
}

/// Monitor rotation, counter-clockwise, optionally mirrored first. Same names as smithay's.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transform {
    #[default]
    Normal,
    _90,
    _180,
    _270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

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
    session: SessionConfig,
    animations: AnimationConfig,
    notifications: NotificationConfig,
    binds: BTreeMap<String, String>,
    /// Names and icons for workspaces, keyed by number ("1", "2", ...).
    #[serde(default)]
    workspaces: BTreeMap<String, WorkspaceLabel>,
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
    session: Option<toml::Table>,
    animations: Option<toml::Table>,
    notifications: Option<toml::Table>,
    binds: Option<BTreeMap<String, String>>,
    workspaces: Option<toml::Table>,
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
    /// Seconds without input before the computer suspends (locking first); 0 never.
    pub suspend_after: u64,
    /// Lock the screen when monitors turn off from `monitors_off_after` or the
    /// `power-off-monitors` action.
    pub lock_when_monitors_off: bool,
    /// Run X11 apps through Xwayland. Read at startup only.
    pub xwayland: bool,
    /// Keep the clipboard after the app that copied closes.
    pub keep_clipboard: bool,
    /// Let apps (virtual machines, remote desktops, games) take every key, binds included.
    pub allow_shortcuts_inhibit: bool,
    /// Let fullscreen games that ask for it show frames right away, tearing, instead of at the
    /// next refresh. Lower input lag.
    pub allow_tearing: bool,
    /// Command the power menu locks the screen with; empty picks an installed locker.
    pub locker: String,
    /// Program that asks for passwords when apps need admin rights (a polkit agent), started
    /// at login; empty picks an installed one, "none" starts none. Read at startup only.
    pub polkit_agent: String,
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
    /// A saved theme from the themes folder; its values sit between the defaults and this table.
    #[serde(default)]
    name: Option<String>,
    font: String,
    font_size: f32,
    radius: f32,
    icon_theme: String,
    /// Picture behind the windows; empty shows the `base` color.
    #[serde(default)]
    wallpaper: String,
    #[serde(default)]
    wallpaper_fit: WallpaperFit,
    colors: ColorDefs,
}

/// How a wallpaper is fitted to a monitor.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WallpaperFit {
    /// Cover the monitor, cropping what doesn't fit.
    #[default]
    Fill,
    /// Show all of it, with `base` around it.
    Fit,
    /// Cover the monitor, squashed or stretched to its shape.
    Stretch,
    /// At its own size in the middle, with `base` around it.
    Center,
    /// Repeated at its own size from the top-left corner.
    Tile,
}

/// The wallpaper for each monitor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WallpaperConfig {
    /// Image file for monitors without their own, `~` expanded; empty for none.
    pub path: String,
    pub fit: WallpaperFit,
    /// `[[outputs]] wallpaper`, by monitor name; an empty path means none on that monitor.
    pub per_output: Vec<(String, String)>,
}

impl WallpaperConfig {
    /// The image for the monitor with this name, if it has one.
    pub fn for_output(&self, output: &str) -> Option<&str> {
        let path = self
            .per_output
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(output))
            .map_or(self.path.as_str(), |(_, p)| p.as_str());
        (!path.is_empty()).then_some(path)
    }
}

/// `~/x` and `~` as paths in the home folder; anything else unchanged.
pub fn expand_home(path: &str) -> String {
    let path = path.trim();
    let home = || std::env::var("HOME").ok().filter(|h| !h.is_empty());
    match path.strip_prefix('~') {
        Some("") => home().unwrap_or_else(|| path.to_string()),
        Some(rest) if rest.starts_with('/') => home().map_or_else(|| path.to_string(), |h| format!("{h}{rest}")),
        _ => path.to_string(),
    }
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
    /// Status widgets next to the clock, in order.
    widgets: Vec<Widget>,
    /// What the old + button ran. It's the power button now; still accepted so older
    /// config files load.
    #[serde(default)]
    #[allow(dead_code)]
    launcher: Option<String>,
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
    /// Num Lock is on when the compositor starts.
    pub numlock: bool,
}

/// Saving open windows on quit and bringing them back at the next start.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Read at startup only.
    pub restore: bool,
    /// App ids never saved or relaunched; a trailing `*` matches any ending.
    pub exclude: Vec<String>,
}

/// How a workspace switch looks.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceAnimation {
    /// The old workspace slides out as the new one slides in.
    Slide,
    /// The old workspace fades into the new one.
    Fade,
    None,
}

/// Which way a sliding workspace switch moves.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlideDirection {
    /// A higher workspace comes in from the right.
    Horizontal,
    /// A higher workspace comes in from below.
    Vertical,
}

/// Motion: workspace switches, tiles sliding into place, the overview opening.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnimationConfig {
    /// Off turns every animation off.
    pub enabled: bool,
    pub workspace: WorkspaceAnimation,
    /// Which way "slide" moves.
    pub direction: SlideDirection,
    /// How long animations take, in milliseconds.
    pub duration: u64,
}

impl AnimationConfig {
    /// How long an animation takes, or zero when animations are off.
    pub fn duration(&self) -> std::time::Duration {
        if self.enabled {
            std::time::Duration::from_millis(self.duration)
        } else {
            std::time::Duration::ZERO
        }
    }
}

/// Corner of the screen notifications pop up in.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

/// The built-in notification daemon.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NotificationConfig {
    /// Show desktop notifications. Read at startup; off leaves them to another daemon (mako).
    pub enabled: bool,
    pub position: Corner,
    /// Seconds a notification stays up when the app doesn't say; urgent ones stay until
    /// dismissed.
    pub timeout: u64,
    /// Most pop-ups shown at once; the rest wait in the bell's list.
    pub max_visible: usize,
}

/// Pointer and touchpad settings (hardware backend only).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    pub tap_to_click: bool,
    pub natural_scroll: bool,
    pub pointer_speed: f64,
    /// Three- and four-finger swipes: sideways changes workspace, up and down open and close
    /// the overview.
    pub gestures: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    /// Exact app id; a trailing `*` matches any ending.
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub floating: Option<bool>,
    pub workspace: Option<usize>,
    /// Monitor to open on, by connector name (e.g. "DP-5"). Ignored if it isn't connected.
    pub output: Option<String>,
    /// Size of a floating window, `[width, height]` in logical pixels.
    pub size: Option<[i32; 2]>,
    /// Where a floating window opens: "center", or `[x, y]` from the top-left of the
    /// monitor's free area.
    pub position: Option<RulePosition>,
    /// Open fullscreen.
    pub fullscreen: Option<bool>,
    /// `false`: open without taking focus from the window you're using.
    pub focus: Option<bool>,
    /// 0.1 (nearly see-through) to 1.0 (solid).
    pub opacity: Option<f32>,
}

/// Where a rule places a floating window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum RulePosition {
    At([i32; 2]),
    Named(NamedPosition),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamedPosition {
    Center,
}

/// What a workspace's button shows instead of its number.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLabel {
    /// Shown instead of the number.
    #[serde(default)]
    pub name: String,
    /// An icon name from the icon theme (an app id works too), shown before the name.
    #[serde(default)]
    pub icon: String,
}

impl WorkspaceLabel {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.icon.is_empty()
    }
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
    /// Show a clock at the end of this monitor's bar.
    #[serde(default)]
    clock: bool,
    /// This monitor's default layout, overriding `[layout] default`.
    layout: Option<LayoutKind>,
    /// Drive the monitor in HDR (BT.2100 PQ) mode, if it supports it.
    #[serde(default)]
    hdr: bool,
    /// How bright ordinary (SDR) content is in HDR mode, in nits.
    sdr_brightness: Option<u32>,
    /// This monitor's wallpaper, overriding `[theme] wallpaper`; "" for none.
    wallpaper: Option<String>,
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
    /// HDR (BT.2100 PQ) mode, when the monitor supports it.
    pub hdr: bool,
    /// In HDR mode, the brightness of ordinary (SDR) white, in nits.
    pub sdr_brightness: u32,
}

/// SDR white in HDR mode when the config doesn't say: the BT.2408 reference white.
pub const DEFAULT_SDR_BRIGHTNESS: u32 = 203;

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            mode: None,
            scale: 1.0,
            position: None,
            transform: Transform::Normal,
            hdr: false,
            sdr_brightness: DEFAULT_SDR_BRIGHTNESS,
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

    /// Whether the bar on the monitor with this connector name shows the clock.
    pub fn clock_on(&self, output: &str) -> bool {
        self.clock_outputs.iter().any(|n| n.eq_ignore_ascii_case(output))
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
    /// Monitors whose bar shows the clock, from `clock = true` in `[[outputs]]`.
    pub clock_outputs: Vec<String>,
    pub font: String,
    pub font_size: f32,
    pub palette: Palette,
    /// From `[theme]`, so the bar repaints when they change.
    pub radius: f32,
    pub icon_theme: String,
    /// Status widgets, in order.
    pub widgets: Vec<Widget>,
}

/// A status widget on the bar.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Widget {
    /// Tray icons from apps (StatusNotifierItem).
    Tray,
    /// What's playing, with play/pause.
    Media,
    /// The volume (click opens the Sound app, right-click mutes, scroll changes it).
    Sound,
    /// Wired or wireless connection.
    Network,
    /// Bluetooth on, off or connected (click opens the Bluetooth app, right-click turns it on
    /// or off), if there's an adapter.
    Bluetooth,
    /// Laptop battery, if there is one.
    Battery,
    /// Screen brightness, if it can be changed.
    Brightness,
    /// A bell: unread notifications and do not disturb.
    Notifications,
}

impl BarConfig {
    /// Whether the bars show this widget.
    pub fn has(&self, w: Widget) -> bool {
        self.widgets.contains(&w)
    }
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
    pub session: SessionConfig,
    pub animations: AnimationConfig,
    pub notifications: NotificationConfig,
    pub binds: Vec<Bind>,
    /// One per workspace, in order; empty labels show the number.
    pub workspace_labels: Vec<WorkspaceLabel>,
    pub rules: Vec<WindowRule>,
    pub outputs: Vec<OutputConfig>,
    /// Kept apart from `outputs`, so changing a wallpaper doesn't reapply monitor modes.
    pub wallpaper: WallpaperConfig,
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
    /// Hide the focused window until it's focused again.
    Minimize,
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
    /// Alt+Tab: open the window switcher, or move through it while open.
    WindowSwitcher(Dir),
    /// Change the default output's volume, or mute it.
    Volume(VolumeChange),
    /// Show every workspace at once, or go back.
    Overview,
    /// Open the app launcher, or close it.
    Launcher,
    /// Lock the screen with the configured locker.
    Lock,
    /// Play/pause or skip in the media player the bar shows (Spotify, browsers, mpv...).
    Media(MediaChange),
    /// Take keys back from an app that holds all of them (or hand them back). Works even while
    /// the app has them.
    ToggleShortcutsInhibit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaChange {
    PlayPause,
    Next,
    Previous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeChange {
    /// Up or down by this many percent.
    Step(i32),
    ToggleMute,
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
    /// `~/.config/kiln/config.toml` (respecting `$XDG_CONFIG_HOME`).
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("kiln").join("config.toml"))
    }

    /// Load from disk. A missing file is created with the defaults so users have something to edit.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse_user_at(&text, path),
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

    /// Read a config without creating it; a missing file means the defaults.
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse_user_at(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(err(format!("reading {}: {e}", path.display()))),
        }
    }

    /// Parse the text of the config file at `path`, reading its theme from the themes folder
    /// next to it.
    pub fn parse_user_at(text: &str, path: &Path) -> Result<Self, ConfigError> {
        Self::parse_user_with(text, |name| {
            let file = theme_path(path, name);
            std::fs::read_to_string(&file).map_err(|e| format!("can't read {}: {e}", file.display()))
        })
    }

    /// Parse a user file, filling any missing sections or keys from the built-in defaults.
    /// A `[theme] name` is an error here, since there's no themes folder to read it from.
    pub fn parse_user(text: &str) -> Result<Self, ConfigError> {
        Self::parse_user_with(text, |_| Err("no themes folder to read it from".into()))
    }

    /// Parse a user file. `theme_text` gives the contents of a saved theme by name.
    pub fn parse_user_with(
        text: &str,
        theme_text: impl FnOnce(&str) -> Result<String, String>,
    ) -> Result<Self, ConfigError> {
        let user: PartialConfig = toml::from_str(text).map_err(|e| err(e.to_string()))?;
        let mut base: toml::Table = toml::from_str(DEFAULT_CONFIG).expect("default config parses");

        // A saved theme goes on top of the default theme, and the file's own [theme] on top of it.
        let name = user.theme.as_ref().and_then(|t| t.get("name"));
        if let Some(name) = name {
            let name = name.as_str().ok_or_else(|| err("theme.name must be text"))?;
            if !valid_theme_name(name) {
                return Err(err(format!("theme.name \"{name}\" can't be used as a file name")));
            }
            let text = theme_text(name).map_err(|e| err(format!("theme \"{name}\": {e}")))?;
            let table: toml::Table = toml::from_str(&text).map_err(|e| err(format!("theme \"{name}\": {e}")))?;
            if table.contains_key("name") {
                return Err(err(format!("theme \"{name}\": a theme file can't pick another theme")));
            }
            if let Some(toml::Value::Table(dst)) = base.get_mut("theme") {
                merge_tables(dst, table);
            }
        }

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
        merge("session", user.session);
        merge("animations", user.animations);
        merge("notifications", user.notifications);
        if let Some(binds) = user.binds {
            let table = binds.into_iter().map(|(k, v)| (k, toml::Value::String(v))).collect();
            base.insert("binds".into(), toml::Value::Table(table));
        }
        if let Some(ws) = user.workspaces {
            base.insert("workspaces".into(), toml::Value::Table(ws));
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
                // Skip just this bind, so one typo (or a bind from a newer version) doesn't
                // throw away the whole config.
                (Err(e), _) | (_, Err(e)) => tracing::warn!("skipping bind \"{combo}\": {e}"),
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
            name: rt.name.clone(),
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
            clock_outputs: raw.outputs.iter().filter(|o| o.clock).map(|o| o.name.clone()).collect(),
            font: rb.font.clone().unwrap_or_else(|| theme.font.clone()),
            font_size: rb.font_size.unwrap_or(theme.font_size),
            radius: theme.radius,
            icon_theme: theme.icon_theme.clone(),
            widgets: rb.widgets.clone(),
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
        if !(1..=10).contains(&raw.notifications.max_visible) {
            problems.push("notifications.max_visible must be between 1 and 10".into());
        }
        if raw.animations.duration > 2000 {
            problems.push("animations.duration must be at most 2000 (milliseconds)".into());
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
            if let Some([w, h]) = rule.size
                && (w < 1 || h < 1)
            {
                problems.push(format!("rules[{i}]: size must be at least [1, 1]"));
            }
            if rule.opacity.is_some_and(|o| !(0.1..=1.0).contains(&o)) {
                problems.push(format!("rules[{i}]: opacity must be between 0.1 and 1.0"));
            }
        }

        let mut workspace_labels = vec![WorkspaceLabel::default(); layout.workspaces];
        for (key, label) in &raw.workspaces {
            match key.parse::<usize>() {
                Ok(n) if (1..=layout.workspaces).contains(&n) => workspace_labels[n - 1] = label.clone(),
                Ok(n) => problems.push(format!(
                    "workspaces.{n}: there are only {} workspaces",
                    layout.workspaces
                )),
                Err(_) => problems.push(format!("workspaces.{key}: expected a workspace number")),
            }
        }

        let wallpaper = WallpaperConfig {
            path: expand_home(&raw.theme.wallpaper),
            fit: raw.theme.wallpaper_fit,
            per_output: raw
                .outputs
                .iter()
                .filter_map(|o| Some((o.name.clone(), expand_home(o.wallpaper.as_deref()?))))
                .collect(),
        };

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
            let sdr_brightness = o.sdr_brightness.unwrap_or(DEFAULT_SDR_BRIGHTNESS);
            if !(80..=1000).contains(&sdr_brightness) {
                problems.push(format!("{at}: sdr_brightness must be between 80 and 1000 (nits)"));
            }
            outputs.push(OutputConfig {
                name: o.name,
                enabled: o.enabled,
                mode,
                scale,
                position: o.position.map(|[x, y]| (x, y)),
                transform,
                hdr: o.hdr,
                sdr_brightness,
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
            session: raw.session,
            animations: raw.animations,
            notifications: raw.notifications,
            binds,
            workspace_labels,
            rules: raw.rules,
            outputs,
            wallpaper,
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
}

impl WindowRule {
    pub fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        let app_ok = match (&self.app_id, app_id) {
            (Some(want), Some(id)) => match want.strip_suffix('*') {
                Some(prefix) => id.starts_with(prefix),
                None => id == want,
            },
            (Some(_), None) => false,
            (None, _) => true,
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
pub fn parse_combo(s: &str) -> Result<(bool, Mods, Option<Keysym>), String> {
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
        // Shift+Tab, as some layouts report it, so "Alt+Shift+Tab" matches.
        0xfe20 => Keysym::new(0xff09),
        _ => k,
    }
}

pub fn parse_action(s: &str) -> Result<Action, String> {
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
            "vertical" => LayoutChoice::Set(LayoutKind::Vertical),
            "monocle" => LayoutChoice::Set(LayoutKind::Monocle),
            "next" => LayoutChoice::Next,
            o => return Err(format!("unknown layout \"{o}\"")),
        }),
        "toggle-floating" => Action::ToggleFloating,
        "toggle-fullscreen" => Action::ToggleFullscreen,
        "minimize" => Action::Minimize,
        "workspace" => Action::Workspace(workspace()?),
        "move-to-workspace" => Action::MoveToWorkspace(workspace()?),
        "focus-monitor" => Action::FocusMonitor(dir()?),
        "move-to-monitor" => Action::MoveToMonitor(dir()?),
        "power-off-monitors" => Action::PowerOffMonitors,
        "window-switcher" => Action::WindowSwitcher(dir()?),
        "overview" => Action::Overview,
        "launcher" => Action::Launcher,
        "lock" => Action::Lock,
        "toggle-shortcuts-inhibit" => Action::ToggleShortcutsInhibit,
        "volume" => Action::Volume(match need_arg()? {
            "mute" => VolumeChange::ToggleMute,
            a => VolumeChange::Step(
                a.trim_end_matches('%')
                    .parse()
                    .ok()
                    .filter(|n: &i32| *n != 0 && a.starts_with(['+', '-']))
                    .ok_or_else(|| format!("expected +5, -5 or mute, got \"{a}\""))?,
            ),
        }),
        "media" => Action::Media(match need_arg()? {
            "play-pause" => MediaChange::PlayPause,
            "next" => MediaChange::Next,
            "prev" => MediaChange::Previous,
            a => return Err(format!("expected play-pause, next or prev, got \"{a}\"")),
        }),
        other => return Err(format!("unknown action \"{other}\"")),
    })
}

/// Every bindable action: (name, what its argument looks like or "" for none, what it does).
/// Kept next to `parse_action` so the two stay in step; a test checks they agree.
pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("spawn", "command", "Run a command through sh -c"),
    ("close", "", "Ask the focused window to close"),
    ("focus", "next|prev", "Move focus through the window list"),
    ("swap", "next|prev", "Move the focused window through the list"),
    ("zoom", "", "Swap the focused window with the master"),
    ("master-ratio", "+0.05|-0.05", "Grow or shrink the master column"),
    ("master-count", "+1|-1", "Add or remove a master window"),
    ("layout", "tile|monocle|next", "Change the workspace layout"),
    ("toggle-floating", "", "Float or re-tile the focused window"),
    ("toggle-fullscreen", "", "Fullscreen the focused window, or stop"),
    (
        "minimize",
        "",
        "Hide the focused window; its bar tab or Alt+Tab brings it back",
    ),
    ("workspace", "number", "Switch to a workspace"),
    ("move-to-workspace", "number", "Send the focused window to a workspace"),
    (
        "focus-monitor",
        "next|prev",
        "Move focus and the pointer to another monitor",
    ),
    (
        "move-to-monitor",
        "next|prev",
        "Send the focused window to another monitor",
    ),
    ("power-off-monitors", "", "Turn monitors off until the next input"),
    (
        "window-switcher",
        "next|prev",
        "Pick a window from a list, most recent first",
    ),
    ("overview", "", "Show every workspace at once, or go back"),
    ("launcher", "", "Find and start an app"),
    ("lock", "", "Lock the screen"),
    (
        "toggle-shortcuts-inhibit",
        "",
        "Take keys back from an app holding all of them, or hand them back",
    ),
    ("volume", "+5|-5|mute", "Change the volume by a percentage, or mute"),
    (
        "media",
        "play-pause|next|prev",
        "Play/pause or skip in the media player",
    ),
    ("reload", "", "Reload the config file"),
    ("quit", "", "Exit the compositor"),
];

/// Check one keybind the way loading the config does; bad binds are skipped there, not fatal.
pub fn check_bind(combo: &str, action: &str) -> Result<(), String> {
    parse_combo(combo)?;
    parse_action(action)?;
    Ok(())
}

/// Try to build the keyboard layout, so a typo can be caught before it's saved. The compositor
/// keeps the old layout when this would fail, which is easy to miss.
pub fn check_keymap(kb: &KeyboardConfig) -> Result<(), String> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        &kb.layout,
        &kb.variant,
        (!kb.options.is_empty()).then(|| kb.options.clone()),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .map(|_| ())
    .ok_or_else(|| {
        format!(
            "keyboard layout \"{}\"{} doesn't exist or has a bad option",
            kb.layout,
            if kb.variant.is_empty() {
                String::new()
            } else {
                format!(" ({})", kb.variant)
            }
        )
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
/// The folder saved themes live in: `themes/` next to the config file.
pub fn themes_dir(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or(Path::new(".")).join("themes")
}

/// Where the theme called `name` is saved.
pub fn theme_path(config_path: &Path, name: &str) -> PathBuf {
    themes_dir(config_path).join(format!("{name}.toml"))
}

/// Saved theme names, sorted.
pub fn theme_names(config_path: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(themes_dir(config_path)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".toml")?;
            valid_theme_name(stem).then(|| stem.to_string())
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// A theme name has to work as a file name: no slashes, not hidden, not too long.
pub fn valid_theme_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.chars().any(|c| c == '/' || c == '\\' || c.is_control())
}

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
        let e = Config::parse_user("[decoration]\nbackground = \"red\"\n[layout]\nmaster_ratio = 2.0\n")
            .unwrap_err()
            .0;
        assert!(e.contains("decoration.background = \"red\""), "{e}");
        assert!(e.contains("master_ratio"), "{e}");
    }

    #[test]
    fn bad_binds_are_skipped() {
        let c = Config::parse_user(
            "[binds]\n\"Mod+nope\" = \"close\"\n\"Mod+a\" = \"fly\"\n\"Mod+x\" = \"close\"\n[layout]\ngaps_inner = 20\n",
        )
        .unwrap();
        assert_eq!(c.binds.len(), 1);
        assert_eq!(c.layout.gaps_inner, 20);
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
        assert_eq!(
            parse_action("volume +5").unwrap(),
            Action::Volume(VolumeChange::Step(5))
        );
        assert_eq!(
            parse_action("volume -2%").unwrap(),
            Action::Volume(VolumeChange::Step(-2))
        );
        assert_eq!(
            parse_action("volume mute").unwrap(),
            Action::Volume(VolumeChange::ToggleMute)
        );
        assert!(parse_action("volume 5").is_err());
        assert!(parse_action("volume +0").is_err());
        assert_eq!(
            parse_action("window-switcher prev").unwrap(),
            Action::WindowSwitcher(Dir::Prev)
        );
        assert!(parse_action("workspace 0").is_err());
        assert!(parse_action("focus up").is_err());
        assert!(parse_action("spawn").is_err());
    }

    #[test]
    fn action_list_matches_parser() {
        for (name, arg, _) in ACTIONS {
            let arg = match *arg {
                "" => "",
                "command" => "true",
                "number" => "1",
                a => a.split('|').next().unwrap(),
            };
            parse_action(&format!("{name} {arg}")).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn keymaps() {
        let kb = Config::default().keyboard;
        assert!(check_keymap(&kb).is_ok());
        let bad = KeyboardConfig {
            layout: "no-such-layout".into(),
            ..kb
        };
        assert!(check_keymap(&bad).is_err());
    }

    #[test]
    fn wallpapers() {
        let c = Config::parse_user("").unwrap();
        assert_eq!(c.wallpaper.for_output("DP-1"), None);
        let c = Config::parse_user(
            "[theme]\nwallpaper = \"/w/a.jpg\"\nwallpaper_fit = \"tile\"\n\
             [[outputs]]\nname = \"DP-2\"\nwallpaper = \"/w/b.png\"\n\
             [[outputs]]\nname = \"DP-3\"\nwallpaper = \"\"\n",
        )
        .unwrap();
        assert_eq!(c.wallpaper.fit, WallpaperFit::Tile);
        assert_eq!(c.wallpaper.for_output("DP-1"), Some("/w/a.jpg"));
        assert_eq!(c.wallpaper.for_output("dp-2"), Some("/w/b.png"));
        assert_eq!(c.wallpaper.for_output("DP-3"), None, "an empty one turns it off");
        assert!(Config::parse_user("[theme]\nwallpaper_fit = \"zoom\"\n").is_err());
    }

    #[test]
    fn home_expansion() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/p.png"), format!("{home}/p.png"));
        assert_eq!(expand_home("~bob/p.png"), "~bob/p.png");
        assert_eq!(expand_home("/a/b"), "/a/b");
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
        assert!(!c.bar.clock_on("DP-4"));
        let c = Config::parse_user("[[outputs]]\nname = \"DP-4\"\nclock = true\n").unwrap();
        assert!(c.bar.clock_on("dp-4"));
        assert!(!c.bar.clock_on("DP-5"));
        assert!(Config::parse_user("[[outputs]]\nname = \"DP-4\"\nbar = \"middle\"\n").is_err());
    }

    #[test]
    fn layout_per_output() {
        let c = Config::parse_user("[[outputs]]\nname = \"DP-4\"\nlayout = \"monocle\"\n").unwrap();
        assert_eq!(c.layout.default_for("dp-4"), LayoutKind::Monocle);
        assert_eq!(c.layout.default_for("DP-5"), LayoutKind::Tile);
    }

    #[test]
    fn saved_themes_layer_between_defaults_and_config() {
        let theme = "font_size = 20\nradius = 2\n[colors]\naccent = \"#ff0000\"\n";
        let user = "[theme]\nname = \"Red\"\nradius = 9\n";
        let c = Config::parse_user_with(user, |n| {
            assert_eq!(n, "Red");
            Ok(theme.into())
        })
        .unwrap();
        assert_eq!(c.theme.name.as_deref(), Some("Red"));
        assert_eq!(c.theme.font_size, 20.0);
        assert_eq!(c.theme.radius, 9.0);
        assert_eq!(c.theme.palette.accent, [1.0, 0.0, 0.0, 1.0]);
        // Unset values still come from the defaults.
        assert_eq!(c.theme.palette.text, Config::default().theme.palette.text);
    }

    #[test]
    fn bad_saved_themes_are_errors() {
        let user = "[theme]\nname = \"X\"\n";
        assert!(Config::parse_user(user).is_err());
        assert!(Config::parse_user_with(user, |_| Ok("bogus = 1\n".into())).is_err());
        assert!(Config::parse_user_with(user, |_| Ok("name = \"Y\"\n".into())).is_err());
        assert!(Config::parse_user_with("[theme]\nname = \"../x\"\n", |_| Ok(String::new())).is_err());
        assert!(valid_theme_name("Catppuccin Latte"));
        assert!(!valid_theme_name(".hidden"));
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
            floating: Some(true),
            ..Default::default()
        };
        assert!(r.matches(Some("mpv"), Some("video.mkv")));
        assert!(!r.matches(Some("mpv2"), None));
        let r = WindowRule {
            title: Some("Picture".into()),
            ..Default::default()
        };
        assert!(r.matches(None, Some("Picture-in-Picture")));
        let r = WindowRule {
            app_id: Some("steam_app_*".into()),
            ..Default::default()
        };
        assert!(r.matches(Some("steam_app_570"), None));
        assert!(!r.matches(Some("steam"), None));
        assert!(!r.matches(None, None));
    }

    #[test]
    fn workspace_labels() {
        let c =
            Config::parse_user("[workspaces]\n1 = { name = \"web\", icon = \"firefox\" }\n3 = { name = \"chat\" }\n")
                .unwrap();
        assert_eq!(c.workspace_labels.len(), 9);
        assert_eq!(c.workspace_labels[0].name, "web");
        assert_eq!(c.workspace_labels[0].icon, "firefox");
        assert!(c.workspace_labels[1].is_empty());
        assert_eq!(c.workspace_labels[2].name, "chat");
        assert!(Config::parse_user("[workspaces]\n10 = { name = \"x\" }\n").is_err());
        assert!(Config::parse_user("[workspaces]\nweb = { name = \"x\" }\n").is_err());
    }

    #[test]
    fn rule_placement() {
        let c = Config::parse_user(
            "[[rules]]\napp_id = \"a\"\nsize = [600, 400]\nposition = \"center\"\nopacity = 0.9\n\
             [[rules]]\napp_id = \"b\"\nposition = [10, 20]\nfocus = false\nfullscreen = true\n",
        )
        .unwrap();
        assert_eq!(c.rules[0].size, Some([600, 400]));
        assert_eq!(c.rules[0].position, Some(RulePosition::Named(NamedPosition::Center)));
        assert_eq!(c.rules[1].position, Some(RulePosition::At([10, 20])));
        assert_eq!(c.rules[1].focus, Some(false));
        assert!(Config::parse_user("[[rules]]\napp_id = \"a\"\nposition = \"left\"\n").is_err());
        assert!(Config::parse_user("[[rules]]\napp_id = \"a\"\nopacity = 0\n").is_err());
        assert!(Config::parse_user("[[rules]]\napp_id = \"a\"\nsize = [0, 5]\n").is_err());
    }
}
