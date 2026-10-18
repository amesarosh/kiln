use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};

use smithay::{
    desktop::{LayerSurface, PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output},
    input::{
        Seat, SeatState,
        keyboard::{KeyboardHandle, Keycode, XkbConfig},
    },
    output::Output,
    reexports::{
        calloop::{Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            wlr_layer::{KeyboardInteractivity, Layer, WlrLayerShellState},
            xdg::{XdgShellState, XdgToplevelSurfaceData, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

use crate::{
    backend::winit::WinitData,
    config::{Action, Config, LayoutChoice, ModKey, NewWindow},
    layout::{self, Rect},
    workspace::Workspace,
};

pub struct State {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, State>,
    pub loop_signal: LoopSignal,
    pub socket_name: OsString,

    pub space: Space<Window>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    // Owned so their protocol globals stay alive, even though nothing reads them.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub seat: Seat<State>,

    pub config: Config,
    pub config_path: Option<PathBuf>,
    /// Running inside another desktop (winit backend) rather than on real hardware.
    pub nested: bool,

    pub workspaces: Vec<Workspace<Window>>,
    pub active_ws: usize,
    /// Toplevels created but not yet committed; rules are applied on first commit once app_id is known.
    pub pending_windows: Vec<Window>,
    /// Layer surface (launcher, lock screen…) currently holding keyboard focus.
    pub focused_layer: Option<WlSurface>,

    /// Keys whose press triggered a bind; their release is hidden from clients.
    pub suppressed_keys: Vec<Keycode>,
    pub winit: Option<WinitData>,
    pub needs_redraw: bool,
}

impl State {
    pub fn new(
        display: Display<State>,
        loop_handle: LoopHandle<'static, State>,
        loop_signal: LoopSignal,
        config: Config,
        config_path: Option<PathBuf>,
        nested: bool,
    ) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        let kb = &config.keyboard;
        let xkb = XkbConfig {
            layout: &kb.layout,
            variant: &kb.variant,
            options: (!kb.options.is_empty()).then(|| kb.options.clone()),
            ..Default::default()
        };
        if seat.add_keyboard(xkb, kb.repeat_delay, kb.repeat_rate).is_err() {
            tracing::error!("keyboard layout \"{}\" failed to load, falling back to us", kb.layout);
            seat.add_keyboard(XkbConfig::default(), kb.repeat_delay, kb.repeat_rate)
                .expect("default keymap");
        }
        seat.add_pointer();

        let socket_name = init_wayland_listener(display, &loop_handle);

        let workspaces = make_workspaces(&config);

        Self {
            start_time: Instant::now(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            socket_name,
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            layer_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            seat,
            config,
            config_path,
            nested,
            workspaces,
            active_ws: 0,
            pending_windows: Vec::new(),
            focused_layer: None,
            suppressed_keys: Vec::new(),
            winit: None,
            needs_redraw: true,
        }
    }

    pub fn keyboard(&self) -> KeyboardHandle<State> {
        self.seat.get_keyboard().expect("seat has a keyboard")
    }

    pub fn mod_key(&self) -> ModKey {
        match self.config.general.mod_key {
            ModKey::Auto if self.nested => ModKey::Alt,
            ModKey::Auto => ModKey::Super,
            other => other,
        }
    }

    pub fn output(&self) -> Option<Output> {
        self.space.outputs().next().cloned()
    }

    pub fn ws(&self) -> &Workspace<Window> {
        &self.workspaces[self.active_ws]
    }

    pub fn ws_mut(&mut self) -> &mut Workspace<Window> {
        &mut self.workspaces[self.active_ws]
    }

    // ---------- window lookup ----------

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.entries.iter().map(|e| &e.window))
            .chain(self.pending_windows.iter())
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    fn workspace_of(&self, w: &Window) -> Option<usize> {
        self.workspaces.iter().position(|ws| ws.contains(w))
    }

    // ---------- window lifecycle ----------

    /// Called on a toplevel's first commit: apply rules, place it, and send the initial configure.
    pub fn manage_window(&mut self, window: Window) {
        self.pending_windows.retain(|w| w != &window);

        let (app_id, title) = toplevel_ids(&window);
        let mut floating = false;
        let mut target_ws = self.active_ws;
        for rule in &self.config.rules {
            if rule.matches(app_id.as_deref(), title.as_deref()) {
                if let Some(f) = rule.floating {
                    floating = f;
                }
                if let Some(n) = rule.workspace {
                    target_ws = n - 1;
                }
            }
        }
        // Dialogs and fixed-size windows float.
        if !floating && let Some(t) = window.toplevel() {
            floating = t.parent().is_some();
        }

        let at_master = self.config.general.new_window == NewWindow::Master;
        let float_rect = floating.then_some(Rect::new(0, 0, 0, 0));
        self.workspaces[target_ws].add(window.clone(), at_master, float_rect);

        if let Some(t) = window.toplevel() {
            t.with_pending_state(|s| {
                s.decoration_mode = Some(
                    smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ServerSide,
                );
            });
        }

        if target_ws == self.active_ws {
            self.arrange();
        } else if let Some(t) = window.toplevel() {
            // Not visible yet; give it a reasonable size so it can map, arrange() fixes it later.
            t.send_configure();
        }
        self.refocus();
    }

    pub fn unmanage_window(&mut self, window: &Window) {
        self.pending_windows.retain(|w| w != window);
        let mut changed = false;
        for ws in &mut self.workspaces {
            changed |= ws.remove(window);
        }
        self.space.unmap_elem(window);
        if changed {
            self.arrange();
            self.refocus();
        }
    }

    /// The client picked its size; center floating windows that haven't been placed yet.
    pub fn on_window_commit(&mut self, window: &Window) {
        let size = window.geometry().size;
        let Some(area) = self.work_area() else { return };
        let Some(ws_idx) = self.workspace_of(window) else {
            return;
        };
        let Some(entry) = self.workspaces[ws_idx].entry_mut(window) else {
            return;
        };
        if let Some(rect) = &mut entry.floating {
            if rect.w == 0 && size.w > 0 {
                *rect = Rect::new(
                    area.loc.x + (area.size.w - size.w) / 2,
                    area.loc.y + (area.size.h - size.h) / 2,
                    size.w,
                    size.h,
                );
                if ws_idx == self.active_ws {
                    self.arrange();
                }
            } else if size.w > 0 {
                rect.w = size.w;
                rect.h = size.h;
            }
        }
    }

    // ---------- layout ----------

    /// Area not reserved by bars/panels, in global coordinates.
    pub fn work_area(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.output()?;
        let geo = self.space.output_geometry(&output)?;
        let mut zone = layer_map_for_output(&output).non_exclusive_zone();
        zone.loc += geo.loc;
        Some(zone)
    }

    /// Recompute positions for the active workspace, map them into the space, and configure clients.
    pub fn arrange(&mut self) {
        self.needs_redraw = true;
        let Some(output) = self.output() else { return };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let Some(area) = self.work_area() else { return };

        // Only the active workspace lives in the space.
        let active: Vec<Window> = self.ws().entries.iter().map(|e| e.window.clone()).collect();
        let stale: Vec<Window> = self.space.elements().filter(|w| !active.contains(w)).cloned().collect();
        for w in stale {
            self.space.unmap_elem(&w);
        }

        let ws = &self.workspaces[self.active_ws];
        let border = self.config.decoration.border_width;

        if let Some(fs) = ws.fullscreen.clone() {
            for w in active.iter().filter(|w| **w != fs) {
                self.space.unmap_elem(w);
            }
            configure(&fs, output_geo.size, true, false);
            self.space.map_element(fs, output_geo.loc, false);
            return;
        }

        let tiled: Vec<Window> = ws.tiled().cloned().collect();
        let mut params = self.config.layout_params();
        params.master_ratio = ws.master_ratio;
        params.master_count = ws.master_count;
        let rects = layout::arrange(
            ws.layout,
            tiled.len(),
            Rect::new(area.loc.x, area.loc.y, area.size.w, area.size.h),
            &params,
        );
        let monocle = ws.layout == layout::LayoutKind::Monocle;
        let focus = ws.focus.clone();
        let floating: Vec<(Window, Rect)> = ws.floating().map(|(w, r)| (w.clone(), r)).collect();

        for (w, r) in tiled.iter().zip(rects) {
            let inner = r.inset(border);
            configure(w, (inner.w, inner.h).into(), false, true);
            self.space.map_element(w.clone(), (inner.x, inner.y), false);
        }
        if monocle && let Some(f) = focus.as_ref().filter(|f| tiled.contains(f)) {
            self.space.raise_element(f, false);
        }
        for (w, r) in floating {
            if let Some(t) = w.toplevel() {
                t.with_pending_state(|s| {
                    s.states.unset(xdg_toplevel::State::Fullscreen);
                    for st in TILED_STATES {
                        s.states.unset(st);
                    }
                });
                t.send_pending_configure();
            }
            // Unplaced windows stay off-screen until their first sized commit.
            let loc = if r.w == 0 { (-100_000, -100_000) } else { (r.x, r.y) };
            self.space.map_element(w.clone(), loc, false);
            self.space.raise_element(&w, false);
        }
        // Keep the focused floating window on top.
        if let Some(f) = focus.filter(|f| self.ws().is_floating(f)) {
            self.space.raise_element(&f, false);
        }
    }

    // ---------- focus ----------

    /// Push the workspace's idea of focus to the seat, unless a layer surface needs the keyboard.
    pub fn refocus(&mut self) {
        self.needs_redraw = true;
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.keyboard();

        // An exclusive layer (launcher, lock screen) always wins.
        if let Some(surface) = self.exclusive_layer() {
            self.focused_layer = Some(surface.clone());
            keyboard.set_focus(self, Some(surface), serial);
            return;
        }
        if let Some(layer) = self.focused_layer.clone() {
            if layer.is_alive() {
                return;
            }
            self.focused_layer = None;
        }

        let focus = self.ws().focus.clone();
        for w in self.space.elements() {
            w.set_activated(Some(w) == focus.as_ref());
            if let Some(t) = w.toplevel() {
                t.send_pending_configure();
            }
        }
        if let Some(f) = &focus
            && self.ws().is_floating(f)
        {
            self.space.raise_element(f, false);
        }
        let surface = focus.and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));
        keyboard.set_focus(self, surface, serial);
    }

    fn exclusive_layer(&self) -> Option<WlSurface> {
        let output = self.output()?;
        let map = layer_map_for_output(&output);
        map.layers()
            .filter(|l| matches!(l.layer(), Layer::Top | Layer::Overlay))
            .find(|l| l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive)
            .map(|l| l.wl_surface().clone())
    }

    pub fn focus_window(&mut self, window: &Window) {
        if self.ws().focus.as_ref() == Some(window) && self.focused_layer.is_none() {
            return;
        }
        if self.ws().contains(window) {
            self.focused_layer = None;
            self.ws_mut().focus = Some(window.clone());
            self.refocus();
        }
    }

    pub fn focus_layer(&mut self, layer: &LayerSurface) {
        if layer.can_receive_keyboard_focus() {
            let surface = layer.wl_surface().clone();
            self.focused_layer = Some(surface.clone());
            self.keyboard()
                .set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
        }
    }

    // ---------- hit testing ----------

    /// The surface under a global point, checking layers and windows in stacking order.
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.output()?;
        let output_loc = self.space.output_geometry(&output)?.loc;
        let map = layer_map_for_output(&output);
        let fullscreen = self.ws().fullscreen.is_some();

        let layer_hit = |layer: Layer| {
            let rel = pos - output_loc.to_f64();
            map.layer_under(layer, rel).and_then(|l| {
                let loc = map.layer_geometry(l)?.loc + output_loc;
                l.surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + loc).to_f64()))
            })
        };

        layer_hit(Layer::Overlay)
            .or_else(|| if fullscreen { None } else { layer_hit(Layer::Top) })
            .or_else(|| {
                self.space.element_under(pos).and_then(|(w, loc)| {
                    w.surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, (p + loc).to_f64()))
                })
            })
            .or_else(|| layer_hit(Layer::Bottom))
            .or_else(|| layer_hit(Layer::Background))
    }

    pub fn layer_under(&self, pos: Point<f64, Logical>) -> Option<LayerSurface> {
        let output = self.output()?;
        let rel = pos - self.space.output_geometry(&output)?.loc.to_f64();
        let map = layer_map_for_output(&output);
        [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
            .into_iter()
            .find_map(|l| map.layer_under(l, rel).cloned())
    }

    // ---------- actions ----------

    pub fn run_action(&mut self, action: Action) {
        match action {
            Action::Spawn(cmd) => self.spawn(&cmd),
            Action::Close => {
                if let Some(t) = self.ws().focus.as_ref().and_then(|w| w.toplevel().cloned()) {
                    t.send_close();
                }
            }
            Action::Quit => self.loop_signal.stop(),
            Action::Reload => self.reload_config(),
            Action::Focus(dir) => {
                self.ws_mut().focus_dir(dir);
                self.focused_layer = None;
                if self.ws().layout == layout::LayoutKind::Monocle {
                    self.arrange();
                }
                self.refocus();
            }
            Action::Swap(dir) => {
                self.ws_mut().swap_dir(dir);
                self.arrange();
            }
            Action::Zoom => {
                self.ws_mut().zoom();
                self.arrange();
                self.refocus();
            }
            Action::MasterRatio(delta) => {
                let ws = self.ws_mut();
                ws.master_ratio = (ws.master_ratio + delta).clamp(0.05, 0.95);
                self.arrange();
            }
            Action::MasterCount(delta) => {
                let ws = self.ws_mut();
                ws.master_count = (ws.master_count as i32 + delta).max(0) as usize;
                self.arrange();
            }
            Action::Layout(choice) => {
                let ws = self.ws_mut();
                ws.layout = match choice {
                    LayoutChoice::Set(k) => k,
                    LayoutChoice::Next => ws.layout.next(),
                };
                self.arrange();
            }
            Action::ToggleFloating => {
                let Some(w) = self.ws().focus.clone() else { return };
                let current = self.space.element_geometry(&w);
                let Some(entry) = self.ws_mut().entry_mut(&w) else {
                    return;
                };
                entry.floating = match entry.floating {
                    Some(_) => None,
                    None => Some(current.map_or(Rect::new(0, 0, 0, 0), |g| {
                        Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h)
                    })),
                };
                self.arrange();
                self.refocus();
            }
            Action::ToggleFullscreen => {
                let Some(w) = self.ws().focus.clone() else { return };
                let ws = self.ws_mut();
                ws.fullscreen = if ws.fullscreen.as_ref() == Some(&w) {
                    None
                } else {
                    Some(w)
                };
                self.arrange();
            }
            Action::Workspace(n) => self.switch_workspace(n - 1),
            Action::MoveToWorkspace(n) => self.move_to_workspace(n - 1),
        }
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        if idx >= self.workspaces.len() || idx == self.active_ws {
            return;
        }
        self.active_ws = idx;
        self.focused_layer = None;
        self.arrange();
        self.refocus();
    }

    fn move_to_workspace(&mut self, idx: usize) {
        if idx >= self.workspaces.len() || idx == self.active_ws {
            return;
        }
        let Some(w) = self.ws().focus.clone() else { return };
        let floating = self.ws().entry(&w).and_then(|e| e.floating);
        self.ws_mut().remove(&w);
        let at_master = self.config.general.new_window == NewWindow::Master;
        let target = &mut self.workspaces[idx];
        target.add(w.clone(), at_master, floating);
        if target.fullscreen.is_some() {
            target.fullscreen = None;
        }
        w.set_activated(false);
        self.arrange();
        self.refocus();
    }

    pub fn spawn(&self, cmd: &str) {
        tracing::info!("spawn: {cmd}");
        let result = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .spawn();
        match result {
            // Reap the child off-thread so finished programs don't linger as zombies.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => tracing::error!("failed to spawn \"{cmd}\": {e}"),
        }
    }

    // ---------- config ----------

    pub fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        match Config::load(&path) {
            Ok(config) => {
                tracing::info!("config reloaded");
                self.apply_config(config);
            }
            Err(e) => tracing::error!("config error, keeping previous config:\n{e}"),
        }
    }

    fn apply_config(&mut self, config: Config) {
        if config.keyboard != self.config.keyboard {
            let kb = &config.keyboard;
            let keyboard = self.keyboard();
            let xkb = XkbConfig {
                layout: &kb.layout,
                variant: &kb.variant,
                options: (!kb.options.is_empty()).then(|| kb.options.clone()),
                ..Default::default()
            };
            if keyboard.set_xkb_config(self, xkb).is_err() {
                tracing::error!("keyboard layout \"{}\" failed to load", kb.layout);
            }
            keyboard.change_repeat_info(kb.repeat_rate, kb.repeat_delay);
        }

        // Resize the workspace list, moving windows from removed workspaces to the last one.
        let want = config.layout.workspaces;
        while self.workspaces.len() > want {
            let gone = self.workspaces.pop().unwrap();
            let last = self.workspaces.last_mut().unwrap();
            for e in gone.entries {
                last.add(e.window, false, e.floating);
            }
        }
        while self.workspaces.len() < want {
            self.workspaces.push(new_workspace(&config));
        }
        self.active_ws = self.active_ws.min(want - 1);

        for ws in &mut self.workspaces {
            ws.layout = config.layout.default;
            ws.master_ratio = config.layout.master_ratio;
            ws.master_count = config.layout.master_count;
        }

        self.config = config;
        self.arrange();
        self.refocus();
    }

    pub fn run_autostart(&self) {
        for cmd in &self.config.general.autostart {
            self.spawn(cmd);
        }
    }

    /// Watch the config directory and reload on save. Editors often replace the file, so watch the dir.
    pub fn watch_config(&mut self) {
        use inotify::{Inotify, WatchMask};

        let Some(path) = self.config_path.clone() else { return };
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().map(|n| n.to_owned())) else {
            return;
        };
        let inotify = match Inotify::init() {
            Ok(i) => i,
            Err(e) => return tracing::warn!("config hot reload disabled: {e}"),
        };
        if let Err(e) = inotify
            .watches()
            .add(dir, WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE)
        {
            return tracing::warn!("config hot reload disabled: cannot watch {}: {e}", dir.display());
        }

        let source = Generic::new(inotify, Interest::READ, Mode::Level);
        let result = self.loop_handle.insert_source(source, move |_, inotify, state| {
            let mut buf = [0u8; 4096];
            // SAFETY: we only read events; the inotify fd itself is never dropped or replaced.
            let inotify = unsafe { inotify.get_mut() };
            let mut hit = false;
            while let Ok(events) = inotify.read_events(&mut buf) {
                let mut any = false;
                for ev in events {
                    any = true;
                    hit |= ev.name == Some(name.as_os_str());
                }
                if !any {
                    break;
                }
            }
            if hit {
                state.reload_config();
            }
            Ok(PostAction::Continue)
        });
        if let Err(e) = result {
            tracing::warn!("config hot reload disabled: {e}");
        }
    }
}

const TILED_STATES: [xdg_toplevel::State; 4] = [
    xdg_toplevel::State::TiledLeft,
    xdg_toplevel::State::TiledRight,
    xdg_toplevel::State::TiledTop,
    xdg_toplevel::State::TiledBottom,
];

/// Set size and state flags on a toplevel and send a configure if anything changed.
fn configure(w: &Window, size: Size<i32, Logical>, fullscreen: bool, tiled: bool) {
    let Some(t) = w.toplevel() else { return };
    t.with_pending_state(|s| {
        s.size = Some(size);
        if fullscreen {
            s.states.set(xdg_toplevel::State::Fullscreen);
        } else {
            s.states.unset(xdg_toplevel::State::Fullscreen);
        }
        for st in TILED_STATES {
            // Tiled states tell clients like GTK to drop shadows and rounded corners.
            if tiled {
                s.states.set(st)
            } else {
                s.states.unset(st)
            };
        }
    });
    t.send_pending_configure();
}

fn toplevel_ids(w: &Window) -> (Option<String>, Option<String>) {
    let Some(t) = w.toplevel() else { return (None, None) };
    with_states(t.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map(|d| {
                let d = d.lock().unwrap();
                (d.app_id.clone(), d.title.clone())
            })
            .unwrap_or_default()
    })
}

fn new_workspace(config: &Config) -> Workspace<Window> {
    Workspace::new(
        config.layout.default,
        config.layout.master_ratio,
        config.layout.master_count,
    )
}

fn make_workspaces(config: &Config) -> Vec<Workspace<Window>> {
    (0..config.layout.workspaces).map(|_| new_workspace(config)).collect()
}

fn init_wayland_listener(display: Display<State>, handle: &LoopHandle<'static, State>) -> OsString {
    let socket = ListeningSocketSource::new_auto().expect("failed to create wayland socket");
    let name = socket.socket_name().to_os_string();

    handle
        .insert_source(socket, |stream, _, state| {
            if let Err(e) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to add client: {e}");
            }
        })
        .expect("failed to init wayland socket source");

    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: the display is never dropped while the loop runs.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("failed to init wayland display source");

    name
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
