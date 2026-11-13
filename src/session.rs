//! Saving and restoring the session: which apps were open, and where their windows were.
//!
//! The state is written to `~/.local/state/kiln/session.toml` a few seconds after
//! anything changes, and right away on quit. Waiting a moment matters at shutdown: the
//! system stops apps and the compositor at the same time, and apps closing then must not be
//! saved as "closed by you". So a change is only written after it has lasted [`SAVE_DELAY`].
//!
//! At the next start, each app is launched again (from its .desktop file, or the command
//! line it was running with), and as its windows appear they are put back on the same
//! monitor and workspace, in the same tiling order, size and floating position.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use smithay::{
    desktop::Window,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::Resource,
    },
    utils::IsAlive,
    wayland::seat::WaylandFocus,
};

use crate::{
    layout::{LayoutKind, Rect},
    state::{State, toplevel_ids},
};

/// How long a change must last before it is written.
const SAVE_DELAY: Duration = Duration::from_secs(3);
/// How long after startup windows from the last session are still expected.
const RESTORE_WINDOW: Duration = Duration::from_secs(120);

/// What is written to disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The monitor that had focus.
    #[serde(default)]
    pub active_output: String,
    #[serde(default, rename = "monitor")]
    pub monitors: Vec<SavedMonitor>,
    #[serde(default, rename = "window")]
    pub windows: Vec<SavedWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedMonitor {
    pub output: String,
    pub active_workspace: usize,
    #[serde(rename = "workspace")]
    pub workspaces: Vec<SavedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub layout: LayoutKind,
    pub master_ratio: f32,
    pub master_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub app_id: String,
    #[serde(default)]
    pub title: String,
    /// Monitor name; empty if no monitor was connected.
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub workspace: usize,
    /// Position in the workspace's window list (tiling order).
    #[serde(default)]
    pub order: usize,
    /// Floating position and size, relative to the monitor's top-left corner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating: Option<[i32; 4]>,
    #[serde(default = "one")]
    pub weight: f32,
    #[serde(default)]
    pub fullscreen: bool,
    /// The workspace's focused window.
    #[serde(default)]
    pub focused: bool,
    /// Command that starts this app again. Set on one window per app process: an app with
    /// several windows in one process (a browser) opens them all itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
    /// The process that owns the window, while running.
    #[serde(skip)]
    pub pid: Option<u32>,
}

fn one() -> f32 {
    1.0
}

/// Where a window from the last session goes.
pub struct Placement {
    pub mon: Option<usize>,
    pub workspace: usize,
    /// Absolute floating rect.
    pub floating: Option<Rect>,
    pub weight: f32,
    pub fullscreen: bool,
    pub focused: bool,
    pub order: usize,
}

pub struct Session {
    path: Option<PathBuf>,
    /// The state as last seen, without launch commands; a difference means something changed.
    seen: Option<Snapshot>,
    save_scheduled: bool,
    /// Set once quitting: nothing is saved after the final write.
    finished: bool,

    // What's left to restore from the last session.
    deadline: Instant,
    pending_windows: Vec<SavedWindow>,
    pending_monitors: Vec<SavedMonitor>,
    pending_active_output: Option<String>,
    launches: Vec<String>,
    /// Restored windows and their saved order, to slot later ones in between.
    ranks: Vec<(Window, usize)>,

    /// Launch commands by process id, and the .desktop file index; both found once.
    commands: HashMap<u32, Option<String>>,
    desktop: Option<HashMap<String, String>>,
}

impl Session {
    /// Load the last session. `nested` runs use their own file, so testing inside another
    /// desktop never overwrites the real session.
    pub fn new(enabled: bool, nested: bool) -> Self {
        let path = enabled.then(|| session_path(nested)).flatten();
        let saved = path.as_deref().and_then(load).unwrap_or_default();
        if !saved.windows.is_empty() {
            tracing::info!("restoring {} windows from the last session", saved.windows.len());
        }
        let launches = saved.windows.iter().filter_map(|w| w.launch.clone()).collect();
        Self {
            path,
            seen: None,
            save_scheduled: false,
            finished: false,
            deadline: Instant::now() + RESTORE_WINDOW,
            pending_active_output: Some(saved.active_output).filter(|o| !o.is_empty()),
            pending_windows: saved.windows,
            pending_monitors: saved.monitors,
            launches,
            ranks: Vec::new(),
            commands: HashMap::new(),
            desktop: None,
        }
    }

    fn restoring(&self) -> bool {
        Instant::now() < self.deadline
    }
}

impl State {
    /// Start the apps from the last session. Ones also in autostart are left to autostart.
    pub fn launch_session_apps(&mut self) {
        let autostart: Vec<String> = self
            .config
            .general
            .autostart
            .iter()
            .filter_map(|c| program_name(c))
            .collect();
        for cmd in std::mem::take(&mut self.session.launches) {
            if program_name(&cmd).is_some_and(|p| autostart.contains(&p)) {
                continue;
            }
            self.spawn_with(&cmd, false);
        }
    }

    /// A monitor was connected: give its workspaces the layouts they had last time.
    pub fn restore_monitor(&mut self, mon: usize) {
        let name = self.monitors[mon].output.name();
        if let Some(i) = self
            .session
            .pending_monitors
            .iter()
            .position(|m| m.output.eq_ignore_ascii_case(&name))
        {
            let saved = self.session.pending_monitors.remove(i);
            let m = &mut self.monitors[mon];
            for (ws, s) in m.workspaces.iter_mut().zip(&saved.workspaces) {
                ws.layout = s.layout;
                ws.master_ratio = s.master_ratio;
                ws.master_count = s.master_count;
            }
            m.active_ws = saved.active_workspace.min(m.workspaces.len() - 1);
        }
        if self
            .session
            .pending_active_output
            .as_ref()
            .is_some_and(|o| o.eq_ignore_ascii_case(&name))
        {
            self.session.pending_active_output = None;
            self.active_mon = mon;
        }
    }

    /// If this window is one from the last session, where it goes.
    pub fn restored_placement(&mut self, window: &Window, app_id: &str, title: &str) -> Option<Placement> {
        if !self.session.restoring() {
            self.session.pending_windows.clear();
            return None;
        }
        let saved = take_match(&mut self.session.pending_windows, app_id, title)?;
        tracing::info!(
            "session: \"{app_id}\" back to {} workspace {}",
            if saved.output.is_empty() { "-" } else { &saved.output },
            saved.workspace + 1
        );
        let mon = self
            .monitors
            .iter()
            .position(|m| m.output.name().eq_ignore_ascii_case(&saved.output))
            .or((!self.monitors.is_empty()).then_some(self.active_mon));
        let origin = mon.map_or((0, 0), |m| {
            let loc = self.monitors[m].output.current_location();
            (loc.x, loc.y)
        });
        let workspaces = self.config.layout.workspaces;
        self.session.ranks.retain(|(w, _)| w.alive());
        self.session.ranks.push((window.clone(), saved.order));
        Some(Placement {
            mon,
            workspace: saved.workspace.min(workspaces - 1),
            floating: saved
                .floating
                .map(|[x, y, w, h]| Rect::new(origin.0 + x, origin.1 + y, w, h)),
            weight: saved.weight,
            fullscreen: saved.fullscreen,
            focused: saved.focused,
            order: saved.order,
        })
    }

    /// Where a restored window with this saved order goes among the windows already there:
    /// before the first restored window that came after it last time.
    pub fn restored_index(&self, entries: &[Window], order: usize) -> usize {
        entries
            .iter()
            .position(|w| self.session.ranks.iter().any(|(r, o)| r == w && *o > order))
            .unwrap_or(entries.len())
    }

    /// Called once per event-loop pass: if the windows changed, save soon.
    pub fn track_session(&mut self) {
        if self.session.path.is_none() || self.session.finished {
            return;
        }
        let now = self.snapshot();
        if self.session.seen.as_ref() == Some(&now) {
            return;
        }
        self.session.seen = Some(now);
        if self.session.save_scheduled {
            return;
        }
        self.session.save_scheduled = true;
        let result = self
            .loop_handle
            .insert_source(Timer::from_duration(SAVE_DELAY), |_, _, state| {
                state.session.save_scheduled = false;
                if !state.session.finished {
                    state.save_session();
                }
                TimeoutAction::Drop
            });
        if let Err(e) = result {
            tracing::warn!("session save timer: {e}");
        }
    }

    /// Write the session now and stop saving: we're quitting.
    pub fn save_session_final(&mut self) {
        if self.session.path.is_some() && !self.session.finished {
            self.save_session();
            self.session.finished = true;
        }
    }

    /// Write the session now, e.g. before restarting the machine. Saving carries on after,
    /// in case the restart doesn't happen.
    pub fn save_session_now(&mut self) {
        if !self.session.finished {
            self.save_session();
        }
    }

    fn save_session(&mut self) {
        let Some(path) = self.session.path.clone() else { return };
        let mut snap = self.snapshot();
        self.fill_launch_commands(&mut snap.windows);
        if let Err(e) = write(&path, &snap) {
            tracing::warn!("could not save session to {}: {e}", path.display());
        } else {
            tracing::debug!("session saved ({} windows)", snap.windows.len());
        }
    }

    /// The current state, plus anything from the last session still on its way back.
    fn snapshot(&self) -> Snapshot {
        let exclude = &self.config.session.exclude;
        let mut snap = Snapshot {
            active_output: self.output().map(|o| o.name()).unwrap_or_default(),
            ..Default::default()
        };
        let mut add = |ws: &crate::workspace::Workspace<Window>, output: String, origin: (i32, i32), idx: usize| {
            for (order, e) in ws.entries.iter().enumerate() {
                let (app_id, title) = toplevel_ids(&e.window);
                let app_id = app_id.unwrap_or_default();
                // Dialogs come back with their app, if at all.
                if app_id.is_empty()
                    || e.window.toplevel().is_some_and(|t| t.parent().is_some())
                    || excluded(exclude, &app_id)
                    || app_id == crate::state::LAUNCHER
                {
                    continue;
                }
                snap.windows.push(SavedWindow {
                    app_id,
                    title: title.unwrap_or_default(),
                    output: output.clone(),
                    workspace: idx,
                    order,
                    floating: e
                        .floating
                        .filter(|r| r.w > 0)
                        .map(|r| [r.x - origin.0, r.y - origin.1, r.w, r.h]),
                    weight: e.weight,
                    fullscreen: ws.fullscreen.as_ref() == Some(&e.window),
                    focused: ws.focus.as_ref() == Some(&e.window),
                    launch: None,
                    pid: self.window_pid(&e.window),
                });
            }
        };
        for m in &self.monitors {
            let loc = m.output.current_location();
            for (i, ws) in m.workspaces.iter().enumerate() {
                add(ws, m.output.name(), (loc.x, loc.y), i);
            }
        }
        for (i, ws) in self.parked.iter().enumerate() {
            add(ws, String::new(), (0, 0), i);
        }
        snap.monitors = self
            .monitors
            .iter()
            .map(|m| SavedMonitor {
                output: m.output.name(),
                active_workspace: m.active_ws,
                workspaces: m
                    .workspaces
                    .iter()
                    .map(|ws| SavedWorkspace {
                        layout: ws.layout,
                        master_ratio: ws.master_ratio,
                        master_count: ws.master_count,
                    })
                    .collect(),
            })
            .collect();
        // Quitting before everything came back must not forget what hasn't yet.
        if self.session.restoring() {
            snap.windows.extend(self.session.pending_windows.iter().cloned());
            snap.monitors.extend(self.session.pending_monitors.iter().cloned());
        }
        snap
    }

    fn window_pid(&self, w: &Window) -> Option<u32> {
        if let Some(x) = w.x11_surface() {
            return x.pid();
        }
        let surface = w.wl_surface()?;
        let client = surface.client()?;
        let pid = client.get_credentials(&self.display_handle).ok()?.pid;
        u32::try_from(pid).ok()
    }

    /// Decide how each app is started again: one command per process, none for windows a
    /// running app opened itself (a game started from Steam, a viewer from a terminal).
    fn fill_launch_commands(&mut self, windows: &mut [SavedWindow]) {
        let pids: Vec<u32> = windows.iter().filter_map(|w| w.pid).collect();
        self.session.commands.retain(|pid, _| pids.contains(pid));
        let own = std::process::id();
        let mut launched: Vec<u32> = Vec::new();
        for w in windows.iter_mut() {
            // Still pending from the last session: keep what it had.
            let Some(pid) = w.pid else { continue };
            if launched.contains(&pid) || ancestors(pid, own).iter().any(|a| pids.contains(a)) {
                continue;
            }
            launched.push(pid);
            if !self.session.commands.contains_key(&pid) {
                let desktop = self.session.desktop.get_or_insert_with(desktop_execs);
                let cmd = desktop_command(desktop, &w.app_id).or_else(|| process_command(pid));
                if cmd.is_none() {
                    tracing::debug!("session: no way to start \"{}\" again", w.app_id);
                }
                self.session.commands.insert(pid, cmd);
            }
            w.launch = self.session.commands[&pid].clone();
        }
    }
}

/// Take the saved window that best matches a new one: same app, same title if possible.
fn take_match(pending: &mut Vec<SavedWindow>, app_id: &str, title: &str) -> Option<SavedWindow> {
    let same_app = |w: &SavedWindow| w.app_id.eq_ignore_ascii_case(app_id);
    let i = pending
        .iter()
        .position(|w| same_app(w) && w.title == title)
        .or_else(|| pending.iter().position(same_app))?;
    Some(pending.remove(i))
}

/// App ids listed in `[session] exclude`; a trailing `*` matches any ending.
pub fn excluded(list: &[String], app_id: &str) -> bool {
    let id = app_id.to_lowercase();
    list.iter().any(|pat| {
        let pat = pat.to_lowercase();
        match pat.strip_suffix('*') {
            Some(prefix) => id.starts_with(prefix),
            None => id == pat,
        }
    })
}

/// The program a shell command runs, e.g. "env FOO=1 /usr/bin/firefox -P x" → "firefox".
fn program_name(cmd: &str) -> Option<String> {
    let word = cmd
        .split_whitespace()
        .find(|w| *w != "env" && !w.contains('=') && !w.starts_with('-'))?;
    let word = word.trim_matches(|c| c == '"' || c == '\'');
    Some(word.rsplit('/').next()?.to_lowercase())
}

/// Parent, grandparent… of a process, stopping at the compositor or init.
fn ancestors(pid: u32, stop: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..32 {
        let Some(parent) = parent_pid(cur) else { break };
        if parent <= 1 || parent == stop {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name in parentheses may hold spaces; fields after it are plain.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// A process's command line, if its program is installed somewhere ordinary (on `$PATH` or
/// under /opt). That rules out games and other things started from inside an app's folder.
fn process_command(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let dir = exe.parent()?;
    let path = std::env::var("PATH").unwrap_or_default();
    let ordinary = dir.starts_with("/opt") || path.split(':').any(|p| !p.is_empty() && Path::new(p) == dir);
    if !ordinary {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| shell_quote(&String::from_utf8_lossy(a)))
        .collect();
    (!args.is_empty()).then(|| args.join(" "))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The .desktop command for an app id, tried as given, then by its last dotted part
/// ("org.gnome.Nautilus" → "nautilus").
fn desktop_command(index: &HashMap<String, String>, app_id: &str) -> Option<String> {
    let lower = app_id.to_lowercase();
    let short = lower.rsplit('.').next().unwrap_or(&lower).to_string();
    index.get(&lower).or_else(|| index.get(&short)).cloned()
}

/// Lowercased app id, desktop file name or WM class → command, from every .desktop file.
fn desktop_execs() -> HashMap<String, String> {
    let mut index = HashMap::new();
    let mut weak = HashMap::new();
    for dir in crate::icons::data_dirs() {
        let mut files = Vec::new();
        crate::icons::collect_desktop_files(&dir.join("applications"), 2, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Some(entry) = parse_desktop(&text) else { continue };
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            // Snap names files "app_app.desktop".
            let snap = stem.split('_').next().unwrap_or(&stem).to_string();
            let short = stem.rsplit('.').next().unwrap_or(&stem).to_string();
            let program = program_name(&entry.exec);
            // Hidden helper entries only count if nothing else matches.
            let target = if entry.hidden { &mut weak } else { &mut index };
            // Earlier directories (the user's own) win.
            for key in [Some(stem.clone()), entry.wm_class, Some(snap), Some(short)]
                .into_iter()
                .flatten()
            {
                target.entry(key).or_insert_with(|| entry.exec.clone());
            }
            if let Some(p) = program {
                weak.entry(p).or_insert_with(|| entry.exec.clone());
            }
        }
    }
    for (k, v) in weak {
        index.entry(k).or_insert(v);
    }
    index
}

struct DesktopEntry {
    exec: String,
    wm_class: Option<String>,
    /// NoDisplay: a helper, not something you'd launch from a menu.
    hidden: bool,
}

/// The parts of a .desktop file that matter here. None for entries that can't be launched
/// directly (terminal apps, removed entries, links).
fn parse_desktop(text: &str) -> Option<DesktopEntry> {
    let (mut exec, mut wm_class, mut hidden, mut in_entry) = (None, None, false, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "Exec" => exec = Some(strip_field_codes(v)),
                "StartupWMClass" => wm_class = Some(v.to_lowercase()),
                "NoDisplay" => hidden = v == "true",
                "Terminal" | "Hidden" if v == "true" => return None,
                "Type" if v != "Application" => return None,
                _ => {}
            }
        }
    }
    let exec = exec.filter(|e| !e.is_empty())?;
    Some(DesktopEntry { exec, wm_class, hidden })
}

/// Drop `%u`, `%F` and friends (and arguments built from them); `%%` is a literal `%`.
fn strip_field_codes(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|w| {
            let w = w.replace("%%", "");
            !w.contains('%')
        })
        .map(|w| w.replace("%%", "%"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn session_path(nested: bool) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let name = if nested { "session-nested.toml" } else { "session.toml" };
    Some(base.join("kiln").join(name))
}

fn load(path: &Path) -> Option<Snapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("ignoring unreadable session file {}: {e}", path.display());
            None
        }
    }
}

/// Write through a temporary file, so a crash mid-write never leaves half a file.
fn write(path: &Path, snap: &Snapshot) -> std::io::Result<()> {
    let text = toml::to_string(snap).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("# Written by kiln; restored at the next start.\n{text}"))?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(app_id: &str, title: &str) -> SavedWindow {
        SavedWindow {
            app_id: app_id.into(),
            title: title.into(),
            output: "DP-1".into(),
            workspace: 0,
            order: 0,
            floating: None,
            weight: 1.0,
            fullscreen: false,
            focused: false,
            launch: None,
            pid: None,
        }
    }

    #[test]
    fn match_prefers_title_then_order() {
        let mut p = vec![win("foot", "a"), win("foot", "b"), win("firefox", "x")];
        assert_eq!(take_match(&mut p, "foot", "b").unwrap().title, "b");
        assert_eq!(take_match(&mut p, "Foot", "zzz").unwrap().title, "a");
        assert!(take_match(&mut p, "foot", "a").is_none());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn exclude_patterns() {
        let list = vec!["steam_app_*".to_string(), "Pavucontrol".to_string()];
        assert!(excluded(&list, "steam_app_570"));
        assert!(excluded(&list, "pavucontrol"));
        assert!(!excluded(&list, "steam"));
    }

    #[test]
    fn program_names() {
        assert_eq!(
            program_name("env A=1 /usr/bin/firefox -P x").as_deref(),
            Some("firefox")
        );
        assert_eq!(program_name("alacritty").as_deref(), Some("alacritty"));
    }

    #[test]
    fn field_codes_are_dropped() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox");
        assert_eq!(strip_field_codes("code --file=%F --new-window"), "code --new-window");
        assert_eq!(strip_field_codes("printf 100%%"), "printf 100%");
    }

    #[test]
    fn desktop_parsing() {
        let e = parse_desktop("[Desktop Entry]\nType=Application\nExec=foot %U\nStartupWMClass=Foot\n").unwrap();
        assert_eq!(e.exec, "foot");
        assert_eq!(e.wm_class.as_deref(), Some("foot"));
        assert!(parse_desktop("[Desktop Entry]\nExec=htop\nTerminal=true\n").is_none());
        // Keys in other groups (actions) don't count.
        let e = parse_desktop("[Desktop Entry]\nExec=a\n[Desktop Action new]\nExec=a --new\n").unwrap();
        assert_eq!(e.exec, "a");
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
    }

    #[test]
    fn snapshot_round_trips() {
        let mut w = win("foot", "t");
        w.floating = Some([1, 2, 3, 4]);
        w.launch = Some("foot".into());
        let snap = Snapshot {
            active_output: "DP-1".into(),
            monitors: vec![SavedMonitor {
                output: "DP-1".into(),
                active_workspace: 2,
                workspaces: vec![SavedWorkspace {
                    layout: LayoutKind::Monocle,
                    master_ratio: 0.6,
                    master_count: 1,
                }],
            }],
            windows: vec![w, win("firefox", "x")],
        };
        let text = toml::to_string(&snap).unwrap();
        let back: Snapshot = toml::from_str(&text).unwrap();
        assert_eq!(back, snap);
    }
}
