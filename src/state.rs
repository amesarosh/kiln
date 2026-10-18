use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};

use smithay::{
    backend::renderer::element::Id,
    desktop::{LayerSurface, PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output},
    input::{
        Seat, SeatState,
        keyboard::{KeyboardHandle, Keycode, XkbConfig},
        pointer::CursorImageStatus,
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
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::FractionalScaleManagerState,
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        output::OutputManagerState,
        seat::WaylandFocus,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        session_lock::{LockSurface, SessionLockManagerState},
        shell::{
            wlr_layer::{KeyboardInteractivity, Layer, WlrLayerShellState},
            xdg::{XdgShellState, XdgToplevelSurfaceData, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Surface, X11Wm},
};

use crate::{
    backend::Backend,
    bar::Bar,
    config::{Action, Config, LayoutChoice, ModKey, NewWindow},
    focus::KeyboardFocusTarget,
    handlers::lock::LockState,
    handlers::xwayland::{x11_float_rect, x11_wants_float},
    layout::{self, Rect},
    workspace::{Workspace, merge_workspaces, step},
};

/// A connected monitor and its own set of workspaces.
pub struct Monitor {
    pub output: Output,
    pub workspaces: Vec<Workspace<Window>>,
    pub active_ws: usize,
}

impl Monitor {
    pub fn ws(&self) -> &Workspace<Window> {
        &self.workspaces[self.active_ws]
    }

    pub fn ws_mut(&mut self) -> &mut Workspace<Window> {
        &mut self.workspaces[self.active_ws]
    }
}

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
    pub session_lock_state: SessionLockManagerState,
    pub idle_notifier: IdleNotifierState<State>,
    #[allow(dead_code)]
    pub idle_inhibit_state: IdleInhibitManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    pub xwayland_shell_state: XWaylandShellState,
    /// The X11 window manager, once Xwayland is running.
    pub xwm: Option<X11Wm>,
    /// Xwayland's display number, set as `DISPLAY` for launched apps.
    pub xdisplay: Option<u32>,
    /// Mapped override-redirect X11 windows (menus, tooltips), bottom to top. Not tiled.
    pub x11_unmanaged: Vec<X11Surface>,
    /// Autostart and `--exec` commands, held until Xwayland is up so they get `DISPLAY`.
    pub startup_commands: Vec<String>,

    /// Screen lock: while locked only lock surfaces are drawn and get input.
    pub lock: LockState,
    /// One lock surface per monitor, from the locker.
    pub lock_surfaces: Vec<(Output, LockSurface)>,
    /// Surfaces asking to keep the screen awake (video players).
    pub idle_inhibitors: Vec<WlSurface>,
    /// Monitors turned off (power saving) until input or a client turns them back on.
    pub powered_off: Vec<Output>,
    /// wlr-output-power-management controls handed to clients.
    pub output_powers: Vec<(
        smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_v1::ZwlrOutputPowerV1,
        Output,
    )>,
    /// When the last input happened, for `monitors_off_after`.
    pub last_activity: Instant,
    pub idle_power_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Damage-tracking ids for the lock screen fill: [normal, locker died].
    pub lock_fill_ids: [Id; 2],

    pub config: Config,
    pub bar: Bar,
    pub config_path: Option<PathBuf>,
    /// Running inside another desktop (winit backend) rather than on real hardware.
    pub nested: bool,

    /// Connected monitors, left to right. Each has its own workspaces.
    pub monitors: Vec<Monitor>,
    /// The monitor with keyboard focus; workspace actions and new windows go here.
    pub active_mon: usize,
    /// Workspaces kept while no monitor is connected. Floating positions are relative to (0, 0).
    /// The next monitor to connect takes them over.
    pub parked: Vec<Workspace<Window>>,
    /// Toplevels created but not yet committed; rules are applied on first commit once app_id is known.
    pub pending_windows: Vec<Window>,
    /// Layer surface (launcher, lock screen…) currently holding keyboard focus.
    pub focused_layer: Option<WlSurface>,

    /// Keys whose press triggered a bind; their release is hidden from clients.
    pub suppressed_keys: Vec<Keycode>,
    /// Mod key pressed on its own; a bare `"Mod"` bind fires if it's released before anything else.
    pub mod_tap: Option<Keycode>,
    pub backend: Option<Backend>,
    pub cursor_status: CursorImageStatus,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
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
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);
        let idle_notifier = IdleNotifierState::new(&dh, loop_handle.clone());
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        Self::create_output_power_global(&dh);

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

        let parked = make_workspaces(&config);
        let bar = Bar::new(&config.bar);

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
            session_lock_state,
            idle_notifier,
            idle_inhibit_state,
            viewporter_state,
            fractional_scale_state,
            xwayland_shell_state,
            xwm: None,
            xdisplay: None,
            x11_unmanaged: Vec::new(),
            startup_commands: Vec::new(),
            lock: LockState::Unlocked,
            lock_surfaces: Vec::new(),
            idle_inhibitors: Vec::new(),
            lock_fill_ids: [Id::new(), Id::new()],
            powered_off: Vec::new(),
            last_activity: Instant::now(),
            idle_power_timer: None,
            output_powers: Vec::new(),
            config,
            bar,
            config_path,
            nested,
            monitors: Vec::new(),
            active_mon: 0,
            parked,
            pending_windows: Vec::new(),
            focused_layer: None,
            suppressed_keys: Vec::new(),
            mod_tap: None,
            backend: None,
            cursor_status: CursorImageStatus::default_named(),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
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

    /// The focused monitor's output.
    pub fn output(&self) -> Option<Output> {
        self.monitors.get(self.active_mon).map(|m| m.output.clone())
    }

    /// The workspace shown on the focused monitor.
    pub fn ws(&self) -> &Workspace<Window> {
        match self.monitors.get(self.active_mon) {
            Some(m) => m.ws(),
            None => &self.parked[0],
        }
    }

    pub fn ws_mut(&mut self) -> &mut Workspace<Window> {
        match self.monitors.get_mut(self.active_mon) {
            Some(m) => m.ws_mut(),
            None => &mut self.parked[0],
        }
    }

    pub fn monitor_index(&self, output: &Output) -> Option<usize> {
        self.monitors.iter().position(|m| &m.output == output)
    }

    /// The monitor under a global point.
    pub fn monitor_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        self.monitors.iter().position(|m| {
            self.space
                .output_geometry(&m.output)
                .is_some_and(|g| g.to_f64().contains(pos))
        })
    }

    /// The monitor whose visible workspace holds this window.
    pub fn monitor_showing(&self, w: &Window) -> Option<usize> {
        self.monitors.iter().position(|m| m.ws().contains(w))
    }

    /// The monitor that owns this window, shown or not.
    pub fn monitor_of(&self, w: &Window) -> Option<usize> {
        self.monitors
            .iter()
            .position(|m| m.workspaces.iter().any(|ws| ws.contains(w)))
    }

    pub fn all_workspaces(&self) -> impl Iterator<Item = &Workspace<Window>> {
        self.monitors
            .iter()
            .flat_map(|m| m.workspaces.iter())
            .chain(self.parked.iter())
    }

    fn all_workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace<Window>> {
        self.monitors
            .iter_mut()
            .flat_map(|m| m.workspaces.iter_mut())
            .chain(self.parked.iter_mut())
    }

    pub fn workspace_of_mut(&mut self, w: &Window) -> Option<&mut Workspace<Window>> {
        self.all_workspaces_mut().find(|ws| ws.contains(w))
    }

    fn output_loc(&self, mon: usize) -> Point<i32, Logical> {
        self.monitors[mon].output.current_location()
    }

    // ---------- monitors ----------

    /// A monitor was connected and mapped into the space.
    pub fn add_monitor(&mut self, output: Output) {
        let workspaces = if self.monitors.is_empty() {
            // The first monitor takes over windows kept while none was connected.
            let mut ws = std::mem::replace(&mut self.parked, make_workspaces(&self.config));
            let loc = output.current_location();
            shift_floating(&mut ws, (loc.x, loc.y));
            ws
        } else {
            make_workspaces(&self.config)
        };
        tracing::info!("monitor {} added", output.name());
        self.monitors.push(Monitor {
            output,
            workspaces,
            active_ws: 0,
        });
        self.sort_monitors();
        self.arrange();
        self.refocus();
    }

    /// A monitor is going away. Its windows move to the focused monitor's workspaces with the
    /// same numbers, or are parked until a monitor comes back.
    pub fn remove_monitor(&mut self, output: &Output) {
        let Some(i) = self.monitor_index(output) else { return };
        let old = self.monitors.remove(i);
        let old_loc = old.output.current_location();
        tracing::info!("monitor {} removed", output.name());
        self.bar.forget_output(&output.name());
        self.forget_monitor_power(output);

        if self.monitors.is_empty() {
            self.active_mon = 0;
            merge_workspaces(&mut self.parked, old.workspaces, (-old_loc.x, -old_loc.y));
        } else {
            if self.active_mon == i {
                self.active_mon = 0;
            } else if self.active_mon > i {
                self.active_mon -= 1;
            }
            let loc = self.output_loc(self.active_mon);
            let target = &mut self.monitors[self.active_mon].workspaces;
            merge_workspaces(target, old.workspaces, (loc.x - old_loc.x, loc.y - old_loc.y));
        }
        self.focused_layer = None;
        self.arrange();
        self.refocus();
        // A lock waiting on this monitor's frame shouldn't wait forever.
        self.confirm_lock_if_ready();
    }

    /// Move a monitor to a new spot in the global space, carrying its floating windows along.
    pub fn move_monitor(&mut self, output: &Output, loc: Point<i32, Logical>) {
        let old = output.current_location();
        output.change_current_state(None, None, None, Some(loc));
        self.space.map_output(output, loc);
        if old != loc
            && let Some(i) = self.monitor_index(output)
        {
            shift_floating(&mut self.monitors[i].workspaces, (loc.x - old.x, loc.y - old.y));
        }
    }

    /// Place every monitor: those with a `position` in the config go there, the rest follow
    /// left to right (in the order they were connected) after the rightmost placed one.
    pub fn layout_monitors(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let size = |o: &Output| self.space.output_geometry(o).map(|g| g.size).unwrap_or_default();
        let mut placed: Vec<(Output, Rectangle<i32, Logical>)> = Vec::new();
        let mut auto = Vec::new();
        for o in outputs {
            match self.config.output(&o.name()).position {
                Some((x, y)) => {
                    let rect = Rectangle::new((x, y).into(), size(&o));
                    if let Some((other, _)) = placed.iter().find(|(_, r)| r.overlaps(rect)) {
                        tracing::warn!("monitors {} and {} overlap", o.name(), other.name());
                    }
                    placed.push((o, rect));
                }
                None => auto.push(o),
            }
        }
        let mut x = placed.iter().map(|(_, r)| r.loc.x + r.size.w).max().unwrap_or(0);
        for o in auto {
            let rect = Rectangle::new((x, 0).into(), size(&o));
            x += rect.size.w;
            placed.push((o, rect));
        }
        for (o, rect) in placed {
            self.move_monitor(&o, rect.loc);
        }
    }

    /// Monitors were added, removed, moved, resized or rescaled: redo everything that depends
    /// on their geometry.
    pub fn monitors_changed(&mut self) {
        for o in self.space.outputs() {
            layer_map_for_output(o).arrange();
        }
        self.sort_monitors();
        // The pointer may now be off every monitor.
        let pos = self.seat.get_pointer().unwrap().current_location();
        if self.monitor_at(pos).is_none()
            && let Some(geo) = self.output().and_then(|o| self.space.output_geometry(&o))
        {
            let center = geo.to_f64().loc + geo.to_f64().size.downscale(2.0).to_point();
            self.move_pointer(center);
        }
        self.configure_lock_surfaces();
        self.arrange();
        self.refocus();
        self.needs_redraw = true;
    }

    /// Put the pointer at `pos` and tell whatever surface is now under it.
    pub fn move_pointer(&mut self, pos: Point<f64, Logical>) {
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        let time = self.start_time.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    /// What's under the pointer changed without it moving (e.g. the screen locked).
    pub fn refresh_pointer_focus(&mut self) {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.move_pointer(pos);
    }

    /// Keep monitors ordered left to right (then top to bottom), so next/prev are spatial.
    fn sort_monitors(&mut self) {
        let active = self.monitors.get(self.active_mon).map(|m| m.output.clone());
        self.monitors.sort_by_key(|m| {
            let loc = m.output.current_location();
            (loc.x, loc.y)
        });
        self.active_mon = active.and_then(|o| self.monitor_index(&o)).unwrap_or(0);
    }

    fn focus_monitor(&mut self, dir: crate::config::Dir) {
        let n = self.monitors.len();
        if n < 2 {
            return;
        }
        self.active_mon = step(self.active_mon, n, dir);
        self.focused_layer = None;
        self.refocus();
        self.warp_pointer_to_focus();
    }

    /// Put the pointer on the focused window, or the middle of the focused monitor, so focus
    /// follows mouse doesn't snap straight back to where the pointer was.
    fn warp_pointer_to_focus(&mut self) {
        let target = self
            .ws()
            .focus
            .as_ref()
            .and_then(|w| self.space.element_geometry(w))
            .or_else(|| self.output().and_then(|o| self.space.output_geometry(&o)));
        let Some(rect) = target else { return };
        let rect = rect.to_f64();
        let center = rect.loc + rect.size.downscale(2.0).to_point();
        self.move_pointer(center);
        self.needs_redraw = true;
    }

    fn move_to_monitor(&mut self, dir: crate::config::Dir) {
        let n = self.monitors.len();
        if n < 2 {
            return;
        }
        let Some(w) = self.ws().focus.clone() else { return };
        let target = step(self.active_mon, n, dir);
        let floating = self.ws().entry(&w).and_then(|e| e.floating);
        self.ws_mut().remove(&w);
        // Keep a floating window at the same spot relative to its monitor.
        let (from, to) = (self.output_loc(self.active_mon), self.output_loc(target));
        let floating = floating.map(|mut r| {
            if r.w > 0 {
                r.x += to.x - from.x;
                r.y += to.y - from.y;
            }
            r
        });
        let at_master = self.config.general.new_window == NewWindow::Master;
        let ws = self.monitors[target].ws_mut();
        ws.add(w.clone(), at_master, floating);
        ws.fullscreen = None;
        w.set_activated(false);
        self.arrange();
        self.refocus();
    }

    /// A dragged window was dropped: it now belongs to the monitor under the pointer.
    pub fn adopt_dropped_window(&mut self, w: &Window, pos: Point<f64, Logical>) {
        let (Some(from), Some(to)) = (self.monitor_showing(w), self.monitor_at(pos)) else {
            return;
        };
        if from == to {
            return;
        }
        let Some(entry) = self.monitors[from].ws().entry(w).cloned() else {
            return;
        };
        self.monitors[from].ws_mut().remove(w);
        let ws = self.monitors[to].ws_mut();
        ws.add(entry.window, false, entry.floating);
        ws.fullscreen = None;
        self.active_mon = to;
        self.arrange();
        self.refocus();
    }

    // ---------- window lookup ----------

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.all_workspaces()
            .flat_map(|ws| ws.entries.iter().map(|e| &e.window))
            .chain(self.pending_windows.iter())
            .find(|w| w.wl_surface().is_some_and(|s| *s == *surface))
            .cloned()
    }

    // ---------- window lifecycle ----------

    /// Called on a toplevel's first commit: apply rules, place it, and send the initial configure.
    pub fn manage_window(&mut self, window: Window) {
        self.pending_windows.retain(|w| w != &window);

        let (app_id, title) = toplevel_ids(&window);
        let mut floating = false;
        let mut target_mon = self.active_mon;
        let mut target_ws = None;
        for rule in &self.config.rules {
            if rule.matches(app_id.as_deref(), title.as_deref()) {
                if let Some(f) = rule.floating {
                    floating = f;
                }
                if let Some(n) = rule.workspace {
                    target_ws = Some(n - 1);
                }
                if let Some(name) = &rule.output
                    && let Some(i) = self
                        .monitors
                        .iter()
                        .position(|m| m.output.name().eq_ignore_ascii_case(name))
                {
                    target_mon = i;
                }
            }
        }
        // Dialogs and fixed-size windows float.
        if !floating && let Some(t) = window.toplevel() {
            floating = t.parent().is_some();
        }
        if !floating && let Some(x) = window.x11_surface() {
            floating = x11_wants_float(x);
        }

        let at_master = self.config.general.new_window == NewWindow::Master;
        // Wayland windows are centered once they pick a size (see on_window_commit). X11 windows
        // already have one.
        let float_rect = floating.then(|| {
            match (
                window.x11_surface(),
                self.monitors.get(target_mon).and(self.work_area(target_mon)),
            ) {
                (Some(x), Some(area)) => x11_float_rect(x, area),
                _ => Rect::new(0, 0, 0, 0),
            }
        });
        let visible = match self.monitors.get_mut(target_mon) {
            Some(m) => {
                let idx = target_ws.unwrap_or(m.active_ws);
                m.workspaces[idx].add(window.clone(), at_master, float_rect);
                idx == m.active_ws
            }
            None => {
                self.parked[target_ws.unwrap_or(0)].add(window.clone(), at_master, float_rect);
                false
            }
        };

        if let Some(t) = window.toplevel() {
            t.with_pending_state(|s| {
                s.decoration_mode = Some(
                    smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ServerSide,
                );
            });
        }

        if visible {
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
        for ws in self.all_workspaces_mut() {
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
        // Parked windows have no monitor to be centered on yet.
        let Some(mon) = self.monitor_of(window) else { return };
        let Some(area) = self.work_area(mon) else { return };
        let shown = self.monitors[mon].ws().contains(window);
        let Some(entry) = self.workspace_of_mut(window).and_then(|ws| ws.entry_mut(window)) else {
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
                if shown {
                    self.arrange();
                }
            } else if size.w > 0 {
                rect.w = size.w;
                rect.h = size.h;
            }
        }
    }

    // ---------- layout ----------

    /// Recompute positions for every monitor's visible workspace, map them into the space, and
    /// configure clients.
    pub fn arrange(&mut self) {
        self.needs_redraw = true;
        // Only the workspaces shown on a monitor live in the space.
        let shown: Vec<Window> = self
            .monitors
            .iter()
            .flat_map(|m| m.ws().entries.iter().map(|e| e.window.clone()))
            .collect();
        let stale: Vec<Window> = self.space.elements().filter(|w| !shown.contains(w)).cloned().collect();
        for w in stale {
            self.space.unmap_elem(&w);
        }
        for mon in 0..self.monitors.len() {
            self.arrange_monitor(mon);
        }
        self.update_idle_inhibit();
    }

    fn arrange_monitor(&mut self, mon: usize) {
        let output = self.monitors[mon].output.clone();
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let Some(area) = self.work_area(mon) else { return };
        let active: Vec<Window> = self.monitors[mon]
            .ws()
            .entries
            .iter()
            .map(|e| e.window.clone())
            .collect();

        let ws = self.monitors[mon].ws();
        let border = self.config.decoration.border_width;

        if let Some(fs) = ws.fullscreen.clone() {
            for w in active.iter().filter(|w| **w != fs) {
                self.space.unmap_elem(w);
            }
            configure(&fs, output_geo, true, false);
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
            configure(
                w,
                Rectangle::new((inner.x, inner.y).into(), (inner.w, inner.h).into()),
                false,
                true,
            );
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
            if r.w > 0 {
                configure_x11(&w, Rectangle::new((r.x, r.y).into(), (r.w, r.h).into()), false);
            }
            // Unplaced windows stay off-screen until their first sized commit.
            let loc = if r.w == 0 { (-100_000, -100_000) } else { (r.x, r.y) };
            self.space.map_element(w.clone(), loc, false);
            self.space.raise_element(&w, false);
        }
        // Keep the focused floating window on top.
        if let Some(f) = focus.filter(|f| self.monitors[mon].ws().is_floating(f)) {
            self.space.raise_element(&f, false);
        }
    }

    // ---------- focus ----------

    /// Push the workspace's idea of focus to the seat, unless a layer surface needs the keyboard.
    pub fn refocus(&mut self) {
        self.needs_redraw = true;
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.keyboard();

        // While locked the keyboard goes to the lock surface on the focused monitor, or any.
        if self.is_locked() {
            let current = keyboard.current_focus();
            let keep = current
                .as_ref()
                .and_then(|f| f.wl_surface().map(|s| s.into_owned()))
                .is_some_and(|f| {
                    self.lock_surfaces
                        .iter()
                        .any(|(_, s)| s.alive() && *s.wl_surface() == f)
                });
            if !keep {
                let surface = self
                    .output()
                    .and_then(|o| self.lock_surface(&o).cloned())
                    .or_else(|| {
                        self.lock_surfaces
                            .iter()
                            .find(|(_, s)| s.alive())
                            .map(|(_, s)| s.clone())
                    })
                    .map(|s| s.wl_surface().clone().into());
                keyboard.set_focus(self, surface, serial);
            }
            return;
        }

        // An exclusive layer (launcher, lock screen) always wins.
        if let Some(surface) = self.exclusive_layer() {
            self.focused_layer = Some(surface.clone());
            keyboard.set_focus(self, Some(surface.into()), serial);
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
        // Keep X11's stacking order in step, so it sends input to the window shown on top.
        if let (Some(x), Some(xwm)) = (focus.as_ref().and_then(|w| w.x11_surface()), self.xwm.as_mut()) {
            let _ = xwm.raise_window(x);
        }
        let target = focus.as_ref().and_then(KeyboardFocusTarget::for_window);
        keyboard.set_focus(self, target, serial);
    }

    fn exclusive_layer(&self) -> Option<WlSurface> {
        self.monitors.iter().find_map(|m| {
            let map = layer_map_for_output(&m.output);
            map.layers()
                .filter(|l| matches!(l.layer(), Layer::Top | Layer::Overlay))
                .find(|l| l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive)
                .map(|l| l.wl_surface().clone())
        })
    }

    /// Focus a visible window, and its monitor.
    pub fn focus_window(&mut self, window: &Window) {
        let Some(mon) = self.monitor_showing(window) else {
            return;
        };
        if mon == self.active_mon && self.ws().focus.as_ref() == Some(window) && self.focused_layer.is_none() {
            return;
        }
        self.active_mon = mon;
        self.focused_layer = None;
        self.ws_mut().focus = Some(window.clone());
        self.refocus();
    }

    /// Give keyboard focus to the monitor under a point, e.g. when the pointer moves onto it.
    pub fn focus_monitor_at(&mut self, pos: Point<f64, Logical>) {
        if let Some(mon) = self.monitor_at(pos)
            && mon != self.active_mon
        {
            self.active_mon = mon;
            self.focused_layer = None;
            self.refocus();
        }
    }

    pub fn focus_layer(&mut self, layer: &LayerSurface) {
        if layer.can_receive_keyboard_focus() && !self.is_locked() {
            let surface = layer.wl_surface().clone();
            self.focused_layer = Some(surface.clone());
            self.keyboard()
                .set_focus(self, Some(surface.into()), SERIAL_COUNTER.next_serial());
        }
    }

    // ---------- hit testing ----------

    /// The surface under a global point, checking layers and windows in stacking order.
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let mon = self.monitor_at(pos)?;
        let output = &self.monitors[mon].output;
        let output_loc = self.space.output_geometry(output)?.loc;
        if self.is_locked() {
            let surface = self.lock_surface(output)?.wl_surface();
            return smithay::desktop::utils::under_from_surface_tree(
                surface,
                pos - output_loc.to_f64(),
                (0, 0),
                WindowSurfaceType::ALL,
            )
            .map(|(s, p)| (s, (p + output_loc).to_f64()));
        }
        // Before taking the layer map lock: over_bar() locks it too, and the lock isn't re-entrant.
        let on_bar = self.over_bar(pos);
        let map = layer_map_for_output(output);
        let fullscreen = self.monitors[mon].ws().fullscreen.is_some();

        let layer_hit = |layer: Layer| {
            let rel = pos - output_loc.to_f64();
            map.layer_under(layer, rel).and_then(|l| {
                let loc = map.layer_geometry(l)?.loc + output_loc;
                l.surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + loc).to_f64()))
            })
        };

        if let Some(hit) = layer_hit(Layer::Overlay) {
            return Some(hit);
        }
        // X11 menus and tooltips sit above everything but overlays.
        for x in self.x11_unmanaged.iter().rev() {
            let geo = x.geometry();
            if let Some(surface) = x.wl_surface()
                && geo.to_f64().contains(pos)
                && let Some((s, p)) = smithay::desktop::utils::under_from_surface_tree(
                    &surface,
                    pos - geo.loc.to_f64(),
                    (0, 0),
                    WindowSurfaceType::ALL,
                )
            {
                return Some((s, (p + geo.loc).to_f64()));
            }
        }
        // The bar covers anything below it; clicks there are handled by the compositor.
        if on_bar {
            return None;
        }
        None.or_else(|| if fullscreen { None } else { layer_hit(Layer::Top) })
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
        let output = &self.monitors[self.monitor_at(pos)?].output;
        let rel = pos - self.space.output_geometry(output)?.loc.to_f64();
        let map = layer_map_for_output(output);
        [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
            .into_iter()
            .find_map(|l| map.layer_under(l, rel).cloned())
    }

    // ---------- actions ----------

    pub fn run_action(&mut self, action: Action) {
        match action {
            Action::Spawn(cmd) => self.spawn(&cmd),
            Action::Close => {
                if let Some(w) = self.ws().focus.clone() {
                    close_window(&w);
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
            Action::FocusMonitor(dir) => self.focus_monitor(dir),
            Action::MoveToMonitor(dir) => self.move_to_monitor(dir),
            Action::SwitchVt(vt) => self.switch_vt(vt),
            Action::PowerOffMonitors => self.set_all_monitors_power(false),
        }
    }

    /// Show workspace `idx` on the focused monitor.
    pub fn switch_workspace(&mut self, idx: usize) {
        let Some(m) = self.monitors.get_mut(self.active_mon) else {
            return;
        };
        if idx >= m.workspaces.len() || idx == m.active_ws {
            return;
        }
        m.active_ws = idx;
        self.focused_layer = None;
        self.arrange();
        self.refocus();
    }

    fn move_to_workspace(&mut self, idx: usize) {
        let Some(m) = self.monitors.get(self.active_mon) else {
            return;
        };
        if idx >= m.workspaces.len() || idx == m.active_ws {
            return;
        }
        let Some(w) = self.ws().focus.clone() else { return };
        let floating = self.ws().entry(&w).and_then(|e| e.floating);
        self.ws_mut().remove(&w);
        let at_master = self.config.general.new_window == NewWindow::Master;
        let target = &mut self.monitors[self.active_mon].workspaces[idx];
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
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "wayland-time")
            .env("XDG_DATA_DIRS", xdg_data_dirs())
            .stdin(Stdio::null());
        // Never hand out the host's DISPLAY: X11 apps would open on another desktop.
        match self.xdisplay {
            Some(n) => command.env("DISPLAY", format!(":{n}")),
            None => command.env_remove("DISPLAY"),
        };
        let result = command.spawn();
        match result {
            // Reap the child off-thread so finished programs don't linger as zombies.
            Ok(mut child) => {
                let cmd = cmd.to_owned();
                std::thread::spawn(move || match child.wait() {
                    // The child's stderr usually lands on a hidden VT, so surface failures here.
                    Ok(status) if status.code() == Some(127) => {
                        tracing::warn!("\"{cmd}\": command not found (exit 127)");
                    }
                    Ok(status) if !status.success() => tracing::warn!("\"{cmd}\" exited with {status}"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("failed to wait for \"{cmd}\": {e}"),
                });
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

        if config.input != self.config.input
            && let Some(Backend::Udev(u)) = &mut self.backend
        {
            u.reapply_input_config(&config.input);
        }

        // Resize each workspace list, moving windows from removed workspaces to the last one.
        let want = config.layout.workspaces;
        resize_workspaces(&mut self.parked, want, &config);
        for m in &mut self.monitors {
            resize_workspaces(&mut m.workspaces, want, &config);
            m.active_ws = m.active_ws.min(want - 1);
        }

        for ws in self.all_workspaces_mut() {
            ws.layout = config.layout.default;
            ws.master_ratio = config.layout.master_ratio;
            ws.master_count = config.layout.master_count;
        }

        if config.bar != self.config.bar {
            self.bar.reconfigure(&config.bar);
        }
        let outputs_changed = config.outputs != self.config.outputs;
        let idle_changed = config.general.monitors_off_after != self.config.general.monitors_off_after;
        self.config = config;
        if idle_changed {
            self.arm_idle_power_off();
        }
        if outputs_changed {
            self.apply_output_config();
        }
        self.arrange();
        self.refocus();
    }

    /// Run autostart and `--exec` commands, once.
    pub fn run_startup_commands(&mut self) {
        for cmd in std::mem::take(&mut self.startup_commands) {
            self.spawn(&cmd);
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

/// Ask a window to close.
pub fn close_window(w: &Window) {
    if let Some(t) = w.toplevel() {
        t.send_close();
    } else if let Some(x) = w.x11_surface() {
        let _ = x.close();
    }
}

const TILED_STATES: [xdg_toplevel::State; 4] = [
    xdg_toplevel::State::TiledLeft,
    xdg_toplevel::State::TiledRight,
    xdg_toplevel::State::TiledTop,
    xdg_toplevel::State::TiledBottom,
];

/// X11 windows are told their position as well as their size: they place their menus from it.
fn configure_x11(w: &Window, rect: Rectangle<i32, Logical>, fullscreen: bool) {
    if let Some(x) = w.x11_surface() {
        let _ = x.set_fullscreen(fullscreen);
        if x.geometry() != rect {
            let _ = x.configure(rect);
        }
    }
}

/// Set size and state flags on a window and send a configure if anything changed.
fn configure(w: &Window, rect: Rectangle<i32, Logical>, fullscreen: bool, tiled: bool) {
    configure_x11(w, rect, fullscreen);
    let size = rect.size;
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

/// (app_id, title). For X11 windows the app_id is the WM_CLASS class, e.g. "steam".
pub fn toplevel_ids(w: &Window) -> (Option<String>, Option<String>) {
    if let Some(x) = w.x11_surface() {
        return (Some(x.class()), Some(x.title()));
    }
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

fn resize_workspaces(list: &mut Vec<Workspace<Window>>, want: usize, config: &Config) {
    while list.len() > want {
        let gone = list.pop().unwrap();
        let last = list.last_mut().unwrap();
        for e in gone.entries {
            last.add(e.window, false, e.floating);
        }
    }
    while list.len() < want {
        list.push(new_workspace(config));
    }
}

/// Move placed floating windows by `by`, e.g. when their workspaces change monitor.
fn shift_floating(list: &mut [Workspace<Window>], by: (i32, i32)) {
    for e in list.iter_mut().flat_map(|ws| ws.entries.iter_mut()) {
        if let Some(r) = &mut e.floating
            && r.w > 0
        {
            r.x += by.0;
            r.y += by.1;
        }
    }
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

/// `$XDG_DATA_DIRS` for spawned clients, so launchers find Flatpak and Snap apps.
/// Starts from the inherited value (or the spec default) and appends any missing dirs.
fn xdg_data_dirs() -> String {
    let inherited = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let mut dirs: Vec<String> = if inherited.is_empty() {
        vec!["/usr/local/share".into(), "/usr/share".into()]
    } else {
        inherited
            .split(':')
            .filter(|d| !d.is_empty())
            .map(String::from)
            .collect()
    };
    let mut extra = vec![
        "/var/lib/flatpak/exports/share".to_string(),
        "/var/lib/snapd/desktop".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        extra.insert(0, format!("{home}/.local/share/flatpak/exports/share"));
    }
    for dir in extra {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}
