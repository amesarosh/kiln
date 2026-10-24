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
    /// Clipboard access for clipboard managers (cliphist, `wl-paste --watch`): wlroots and ext.
    pub wlr_data_control_state: smithay::wayland::selection::wlr_data_control::DataControlState,
    pub ext_data_control_state: smithay::wayland::selection::ext_data_control::DataControlState,
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
    #[allow(dead_code)]
    pub pointer_constraints_state: smithay::wayland::pointer_constraints::PointerConstraintsState,
    #[allow(dead_code)]
    pub relative_pointer_state: smithay::wayland::relative_pointer::RelativePointerManagerState,
    /// Lets apps ask for a named cursor, so every app shows the same theme and size.
    #[allow(dead_code)]
    pub cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    pub activation_state: smithay::wayland::xdg_activation::XdgActivationState,
    pub shortcuts_inhibit_state: smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
    /// wp_color_management_v1: image descriptions handed out, and who watches what.
    pub color: crate::handlers::color_management::ColorManagement,
    /// Tells apps (video players, games) exactly when their frames reached the screen.
    #[allow(dead_code)]
    pub presentation_state: smithay::wayland::presentation::PresentationState,
    /// The clock presentation times are given in.
    pub clock: smithay::utils::Clock<smithay::utils::Monotonic>,
    /// Set when keyboard focus changes; the main loop then re-checks pointer constraints.
    pub keyboard_focus_moved: bool,
    /// The X11 window manager, once Xwayland is running.
    pub xwm: Option<X11Wm>,
    /// Xwayland's display number, set as `DISPLAY` for launched apps.
    pub xdisplay: Option<u32>,
    /// Our own connection to Xwayland, for window properties the window manager doesn't set.
    pub x11_props: Option<crate::handlers::xwayland::X11Props>,
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
    /// The window list shown to taskbars and window switchers.
    pub foreign_toplevels: crate::handlers::foreign_toplevel::ForeignToplevels,
    /// Copies of the clipboard, kept after the app that copied closes.
    pub clipboard: crate::clipboard::Keeper,
    /// Saving the open windows, and bringing back the last session's.
    pub session: crate::session::Session,
    /// Every workspace at once, while open.
    pub overview: Option<crate::overview::Overview>,
    /// A touchpad swipe in progress, and whether it's ours or an app's.
    pub swipe: Option<crate::input::Swipe>,
    /// Lets apps (browsers, image viewers) get pinch and swipe gestures.
    #[allow(dead_code)]
    pub pointer_gestures_state: smithay::wayland::pointer_gestures::PointerGesturesState,
    /// The session bus: tray icons, media players, and the notification daemon.
    pub bus: crate::dbus::Bus,
    /// Notifications on screen and in the bell's list.
    pub notifications: crate::notifications::Notifications,
    /// Battery, network and brightness for the bar.
    pub status: crate::status::Status,
    /// A tray menu asked for, and where to show it once it arrives: (item, pointer, monitor).
    pub tray_menu_request: Option<(String, Point<f64, Logical>, usize)>,
    /// The Alt+Tab switcher, while open.
    pub switcher: Option<crate::switcher::Switcher>,
    /// Volume keys and the panel that shows the level.
    pub volume: crate::volume::Volume,
    /// Windows by when they last had focus, most recent first.
    pub focus_history: Vec<Window>,
    /// The open pop-up menu, if any.
    pub menu: Option<crate::menu::Menu>,
    /// Suspend picked from the power menu: suspend as soon as the locker has locked.
    pub suspend_when_locked: bool,
    /// When the locker was last started, so key presses on a crashed lock screen don't start
    /// a pile of them before the first one connects.
    pub locker_started: Option<std::time::Instant>,
    /// The theme font, loaded for menus; reloaded when `[theme] font` changes.
    pub ui_text: Option<(String, crate::text::Text)>,
    /// Screencopy frames waiting for the screen to change.
    pub screencopy_pending: Vec<crate::handlers::screencopy::PendingCopy>,
    /// Which screencopy clients have seen each output's latest contents.
    pub screencopy_watch: Vec<crate::handlers::screencopy::DamageWatch>,
    /// When the last input happened, for `monitors_off_after`.
    pub last_activity: Instant,
    pub idle_power_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// The `suspend_after` countdown; None while off or after it fired, until the next input.
    pub idle_suspend_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Damage-tracking ids for the lock screen fill: [normal, locker died].
    pub lock_fill_ids: [Id; 2],

    pub config: Config,
    /// Monitor settings changed at runtime by tools like kanshi or wdisplays. They sit on top
    /// of `[[outputs]]` until the config's monitor settings change.
    pub output_overrides: Vec<crate::config::OutputConfig>,
    /// Clients managing monitors (wlr-output-management).
    pub output_management: crate::handlers::output_management::OutputManagement,
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
    /// The picture an app drags along under the pointer (a file, a tab...), while a drag is on.
    pub dnd_icon: Option<crate::handlers::DndIcon>,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    pub needs_redraw: bool,
    /// Tiles sliding into place, and the one being dragged.
    pub anims: crate::anim::Anims,
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
        // Any client may watch and set the clipboard, as on other wlroots-style desktops.
        let wlr_data_control_state = smithay::wayland::selection::wlr_data_control::DataControlState::new::<Self, _>(
            &dh,
            Some(&primary_selection_state),
            |_| true,
        );
        let ext_data_control_state = smithay::wayland::selection::ext_data_control::DataControlState::new::<Self, _>(
            &dh,
            Some(&primary_selection_state),
            |_| true,
        );
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);
        let idle_notifier = IdleNotifierState::new(&dh, loop_handle.clone());
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let pointer_constraints_state =
            smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state = smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(&dh);
        let activation_state = smithay::wayland::xdg_activation::XdgActivationState::new::<Self>(&dh);
        let shortcuts_inhibit_state =
            smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let cursor_shape_state = smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(&dh);
        let pointer_gestures_state = smithay::wayland::pointer_gestures::PointerGesturesState::new::<Self>(&dh);
        let clock = smithay::utils::Clock::<smithay::utils::Monotonic>::new();
        let presentation_state =
            smithay::wayland::presentation::PresentationState::new::<Self>(&dh, clock.id() as u32);
        Self::create_output_power_global(&dh);
        Self::create_screencopy_global(&dh);
        Self::create_tearing_control_global(&dh);
        Self::create_color_management_global(&dh);
        Self::create_output_management_global(&dh);
        let foreign_toplevels = crate::handlers::foreign_toplevel::ForeignToplevels::new(&dh);

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
        if kb.numlock {
            set_numlock(&seat.get_keyboard().unwrap());
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
            wlr_data_control_state,
            ext_data_control_state,
            seat,
            session_lock_state,
            idle_notifier,
            idle_inhibit_state,
            viewporter_state,
            fractional_scale_state,
            xwayland_shell_state,
            pointer_constraints_state,
            relative_pointer_state,
            cursor_shape_state,
            activation_state,
            shortcuts_inhibit_state,
            color: Default::default(),
            presentation_state,
            clock,
            keyboard_focus_moved: false,
            xwm: None,
            xdisplay: None,
            x11_props: None,
            x11_unmanaged: Vec::new(),
            startup_commands: Vec::new(),
            lock: LockState::Unlocked,
            suspend_when_locked: false,
            locker_started: None,
            lock_surfaces: Vec::new(),
            idle_inhibitors: Vec::new(),
            lock_fill_ids: [Id::new(), Id::new()],
            powered_off: Vec::new(),
            last_activity: Instant::now(),
            idle_power_timer: None,
            idle_suspend_timer: None,
            output_powers: Vec::new(),
            clipboard: Default::default(),
            foreign_toplevels,
            menu: None,
            switcher: None,
            bus: Default::default(),
            notifications: Default::default(),
            status: Default::default(),
            tray_menu_request: None,
            swipe: None,
            pointer_gestures_state,
            overview: None,
            volume: Default::default(),
            focus_history: Vec::new(),
            ui_text: None,
            screencopy_pending: Vec::new(),
            screencopy_watch: Vec::new(),
            session: crate::session::Session::new(config.session.restore, nested),
            config,
            output_overrides: Vec::new(),
            output_management: Default::default(),
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
            dnd_icon: None,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            needs_redraw: true,
            anims: Default::default(),
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
        let mut workspaces = if self.monitors.is_empty() {
            // The first monitor takes over windows kept while none was connected.
            let mut ws = std::mem::replace(&mut self.parked, make_workspaces(&self.config));
            let loc = output.current_location();
            shift_floating(&mut ws, (loc.x, loc.y));
            ws
        } else {
            make_workspaces(&self.config)
        };
        let layout = self.config.layout.default_for(&output.name());
        for ws in &mut workspaces {
            ws.layout = layout;
        }
        tracing::info!("monitor {} added", output.name());
        self.monitors.push(Monitor {
            output,
            workspaces,
            active_ws: 0,
        });
        self.restore_monitor(self.monitors.len() - 1);
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
        self.forget_screencopy_output(output);
        if self.menu.as_ref().is_some_and(|m| &m.output == output) {
            self.menu = None;
        }
        if self.switcher.as_ref().is_some_and(|s| s.on_output(output)) {
            self.switcher = None;
        }
        // Its cards are laid out for the old set of monitors.
        self.overview_cancel();
        if self.volume.osd.as_ref().is_some_and(|o| o.on_output(output)) {
            self.volume.osd = None;
        }

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
            match self.output_config(&o.name()).position {
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
        self.refresh_output_heads();
    }

    /// Settings for a monitor: a runtime change from a monitor tool, else the config's.
    pub fn output_config(&self, name: &str) -> crate::config::OutputConfig {
        self.output_overrides
            .iter()
            .find(|o| o.name.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| self.config.output(name))
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
        self.move_window_to_monitor(step(self.active_mon, n, dir));
    }

    /// Send the focused window to monitor `target`'s current workspace.
    pub fn move_window_to_monitor(&mut self, target: usize) {
        if target >= self.monitors.len() || target == self.active_mon {
            return;
        }
        let Some(w) = self.ws().focus.clone() else { return };
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
        let rule = self.rules_for(app_id.as_deref(), title.as_deref());
        // The launcher always floats in the middle of the screen.
        let mut floating = rule.floating.unwrap_or(false) || app_id.as_deref() == Some(LAUNCHER);
        let mut target_mon = self.active_mon;
        let mut target_ws = rule.workspace.map(|n| n - 1);
        if let Some(name) = &rule.output
            && let Some(i) = self
                .monitors
                .iter()
                .position(|m| m.output.name().eq_ignore_ascii_case(name))
        {
            target_mon = i;
        }
        if let Some(o) = rule.opacity {
            set_window_opacity(&window, o);
        }
        // Dialogs and fixed-size windows float.
        if !floating && let Some(t) = window.toplevel() {
            floating = t.parent().is_some();
        }
        if !floating && let Some(x) = window.x11_surface() {
            floating = x11_wants_float(x);
        }
        let is_dialog = window.toplevel().is_some_and(|t| t.parent().is_some());
        let restored = (!is_dialog)
            .then(|| self.restored_placement(&window, app_id.as_deref().unwrap_or(""), title.as_deref().unwrap_or("")))
            .flatten();
        if let Some(p) = &restored {
            if let Some(m) = p.mon {
                target_mon = m;
            }
            target_ws = Some(p.workspace);
            floating = p.floating.is_some();
        }

        let at_master = self.config.general.new_window == NewWindow::Master;
        // Wayland windows are centered once they pick a size (see on_window_commit). X11 windows
        // already have one.
        let area = self.monitors.get(target_mon).and(self.work_area(target_mon));
        let float_rect = floating.then(|| match (window.x11_surface(), area, rule.size) {
            // A size from a rule: placed now, and the client is told to use it.
            (_, Some(area), Some([w, h])) => place_floating(area, (w, h), rule.position),
            (Some(x), Some(area), None) => {
                let r = x11_float_rect(x, area);
                place_floating(area, (r.w, r.h), rule.position)
            }
            _ => Rect::new(0, 0, 0, 0),
        });
        // A Wayland window placed by a rule once it has picked its own size (see on_window_commit).
        if float_rect.is_some_and(|r| r.w == 0) && rule.position.is_some() {
            window
                .user_data()
                .insert_if_missing(|| RulePlacement(std::cell::Cell::new(rule.position)));
        }
        let float_rect = match &restored {
            Some(p) => p.floating,
            None => float_rect,
        };
        let (idx, visible) = match self.monitors.get(target_mon) {
            Some(m) => {
                let idx = target_ws.unwrap_or(m.active_ws);
                (idx, idx == m.active_ws)
            }
            None => (target_ws.unwrap_or(0), false),
        };
        // Back where it was last session: same slot in the tiling order, same size.
        let index = restored.as_ref().map(|p| {
            let ws = match self.monitors.get(target_mon) {
                Some(m) => &m.workspaces[idx],
                None => &self.parked[idx],
            };
            let entries: Vec<Window> = ws.entries.iter().map(|e| e.window.clone()).collect();
            self.restored_index(&entries, p.order)
        });
        let ws = match self.monitors.get_mut(target_mon) {
            Some(m) => &mut m.workspaces[idx],
            None => &mut self.parked[idx],
        };
        match (&restored, index) {
            (Some(p), Some(index)) => {
                let focus_before = ws.focus.clone();
                ws.add_at(window.clone(), index, float_rect, p.weight);
                if p.fullscreen {
                    ws.fullscreen = Some(window.clone());
                }
                if !p.focused && focus_before.is_some() {
                    ws.focus = focus_before;
                }
                if let (Some(t), Some(r)) = (window.toplevel(), float_rect) {
                    t.with_pending_state(|s| s.size = Some((r.w, r.h).into()));
                }
            }
            _ => {
                let focus_before = ws.focus.clone();
                ws.add(window.clone(), at_master, float_rect);
                if rule.fullscreen == Some(true) {
                    ws.fullscreen = Some(window.clone());
                }
                if rule.focus == Some(false) && focus_before.is_some() {
                    ws.focus = focus_before;
                }
                if let (Some(t), Some(r)) = (window.toplevel(), float_rect.filter(|r| r.w > 0)) {
                    t.with_pending_state(|s| s.size = Some((r.w, r.h).into()));
                }
            }
        }

        if let Some(t) = window.toplevel() {
            t.with_pending_state(|s| {
                s.decoration_mode = Some(
                    smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ServerSide,
                );
            });
        }

        // A rule sent it to another workspace or monitor: follow it there. Not for windows put
        // back by session restore, or rules with `focus = false`.
        let follow = restored.is_none()
            && (rule.workspace.is_some() || rule.output.is_some())
            && rule.focus != Some(false)
            && self.monitors.get(target_mon).is_some()
            && (target_mon != self.active_mon || !visible);

        if visible {
            self.arrange();
        } else if let Some(t) = window.toplevel() {
            // Not visible yet; give it a reasonable size so it can map, arrange() fixes it later.
            t.send_configure();
        }
        if follow {
            self.activate_window(&window);
        } else {
            self.refocus();
        }
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
                let position = window.user_data().get::<RulePlacement>().and_then(|p| p.0.take());
                *rect = place_floating(area, (size.w, size.h), position);
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

    /// Where the tiled windows of a monitor's workspace go, in tiling order (borders included).
    pub fn tile_rects(&self, mon: usize, area: Rectangle<i32, Logical>) -> Vec<Rect> {
        let ws = self.monitors[mon].ws();
        let mut params = layout::params(&self.config);
        params.master_ratio = ws.master_ratio;
        params.master_count = ws.master_count;
        layout::arrange(
            ws.layout,
            &ws.tiled_weights(),
            Rect::new(area.loc.x, area.loc.y, area.size.w, area.size.h),
            &params,
        )
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
        let rects = self.tile_rects(mon, area);
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

        // The overview takes the keys while it's open.
        if self.overview_holds_keyboard() {
            keyboard.set_focus(self, None, serial);
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
        // The compositor's own menu, switcher, overview and notifications are above everything.
        if self.over_menu(pos) || self.over_switcher(pos) || self.overview_open() || self.over_notification(pos) {
            return None;
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
        // Volume keys leave menus and the switcher open; they don't change what's on screen.
        if let Action::Volume(change) = action {
            return self.change_volume(change);
        }
        if let Action::Media(change) = action {
            return self.media_key(change);
        }
        self.close_menu();
        if !matches!(action, Action::WindowSwitcher(_)) {
            self.switcher_cancel();
        }
        if !matches!(action, Action::Overview) {
            self.overview_cancel();
        }
        match action {
            Action::Spawn(cmd) => self.spawn(&cmd),
            Action::Close => {
                if let Some(w) = self.ws().focus.clone() {
                    close_window(&w);
                }
            }
            Action::Quit => {
                self.save_session_final();
                self.loop_signal.stop();
            }
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
            Action::WindowSwitcher(dir) => self.window_switcher(dir),
            Action::Volume(change) => self.change_volume(change),
            Action::Overview => self.toggle_overview(),
            Action::Launcher => self.toggle_launcher(),
            Action::Lock => self.power(crate::power::PowerAction::Lock),
            Action::Media(change) => self.media_key(change),
            Action::ToggleShortcutsInhibit => self.toggle_shortcuts_inhibit(),
        }
    }

    fn media_key(&mut self, change: wt_config::config::MediaChange) {
        use crate::dbus::mpris::MediaCommand;
        use wt_config::config::MediaChange;
        let cmd = match change {
            MediaChange::PlayPause => MediaCommand::PlayPause,
            MediaChange::Next => MediaCommand::Next,
            MediaChange::Previous => MediaCommand::Previous,
        };
        self.bus_command(crate::dbus::Command::Media(cmd));
    }

    /// Show workspace `idx` on the focused monitor.
    pub fn switch_workspace(&mut self, idx: usize) {
        let Some(m) = self.monitors.get_mut(self.active_mon) else {
            return;
        };
        if idx >= m.workspaces.len() || idx == m.active_ws {
            return;
        }
        let anim = self.begin_workspace_switch(self.active_mon);
        self.monitors[self.active_mon].active_ws = idx;
        self.focused_layer = None;
        self.arrange();
        self.refocus();
        if let Some(a) = anim {
            self.finish_workspace_switch(a, idx);
        }
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

    pub fn spawn(&mut self, cmd: &str) {
        self.spawn_with(cmd, true);
    }

    /// `activate`: let the app bring its window forward. Off for apps started at login, so
    /// they don't pull the view away from the workspace the last session was on.
    pub fn spawn_with(&mut self, cmd: &str, activate: bool) {
        tracing::info!("spawn: {cmd}");
        let mut command = Command::new("sh");
        if activate {
            // Lets an app that's already running bring its window forward when launched again.
            let token = self.launch_token();
            command
                .env("XDG_ACTIVATION_TOKEN", &token)
                .env("DESKTOP_STARTUP_ID", &token);
        } else {
            command
                .env_remove("XDG_ACTIVATION_TOKEN")
                .env_remove("DESKTOP_STARTUP_ID");
        }
        command
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "wayland-time")
            .env("XDG_DATA_DIRS", xdg_data_dirs())
            .stdin(Stdio::null());
        if let Some(sock) = crate::ipc::socket_path(&self.socket_name.to_string_lossy()) {
            command.env("WAYLAND_TIME_SOCKET", sock);
        }
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
        // Painted from the old theme; reopening shows the new one.
        self.close_menu();
        self.switcher_cancel();
        self.hide_volume_osd();
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
            } else if kb.numlock {
                // A new keymap starts with every lock off.
                set_numlock(&keyboard);
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

        // Only reset what changed, so saving an unrelated setting (the settings app saves on
        // every edit) keeps the layouts and sizes picked per workspace.
        let (old, new) = (&self.config.layout, &config.layout);
        let layout_changed = old.default != new.default || old.per_output != new.per_output;
        let ratio_changed = old.master_ratio != new.master_ratio;
        let count_changed = old.master_count != new.master_count;
        for ws in self.all_workspaces_mut() {
            if layout_changed {
                ws.layout = config.layout.default;
            }
            if ratio_changed {
                ws.master_ratio = config.layout.master_ratio;
            }
            if count_changed {
                ws.master_count = config.layout.master_count;
            }
        }
        if config.workspace_labels != self.config.workspace_labels {
            let labels = &config.workspace_labels;
            for list in self.monitors.iter_mut().map(|m| &mut m.workspaces).chain([&mut self.parked]) {
                for (i, ws) in list.iter_mut().enumerate() {
                    ws.label = labels.get(i).cloned().unwrap_or_default();
                }
            }
        }
        if layout_changed {
            for m in &mut self.monitors {
                let layout = config.layout.default_for(&m.output.name());
                for ws in &mut m.workspaces {
                    ws.layout = layout;
                }
            }
        }

        if config.bar != self.config.bar {
            self.bar.reconfigure(&config.bar);
        }
        let outputs_changed = config.outputs != self.config.outputs;
        // Editing the monitors in the config file takes over from changes made by tools.
        if outputs_changed {
            self.output_overrides.clear();
        }
        let idle_changed = config.general.monitors_off_after != self.config.general.monitors_off_after;
        let suspend_changed = config.general.suspend_after != self.config.general.suspend_after;
        self.config = config;
        if idle_changed {
            self.arm_idle_power_off();
        }
        if suspend_changed {
            self.arm_idle_suspend();
        }
        if outputs_changed {
            self.apply_output_config();
        }
        self.arrange();
        self.refocus();
    }

    /// Run autostart and `--exec` commands, once.
    pub fn run_startup_commands(&mut self) {
        self.export_session_env();
        for cmd in std::mem::take(&mut self.startup_commands) {
            self.spawn_with(&cmd, false);
        }
        self.launch_session_apps();
    }

    /// Tell systemd and D-Bus our display, so services they start (keyring unlock prompt,
    /// desktop portals) can open windows here. Skipped when nested, to leave the host's alone.
    fn export_session_env(&self) {
        if self.nested {
            return;
        }
        let display = self.xdisplay.map(|n| format!(":{n}"));
        let mut vars = vec!["WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];
        let mut command = Command::new("dbus-update-activation-environment");
        command
            .arg("--systemd")
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_CURRENT_DESKTOP", "wayland-time")
            .env("XDG_SESSION_TYPE", "wayland");
        match &display {
            Some(d) => {
                command.env("DISPLAY", d);
                vars.push("DISPLAY");
            }
            // An old DISPLAY (say, from an earlier X session) would point at the wrong screen.
            None => {
                let _ = Command::new("systemctl")
                    .args(["--user", "unset-environment", "DISPLAY"])
                    .status();
            }
        }
        // Wait for it, so autostart programs see the new values.
        match command.args(&vars).stdin(Stdio::null()).status() {
            Ok(s) if s.success() => {
                tracing::info!("exported {} to systemd/D-Bus", vars.join(", "));
                restart_portals();
            }
            Ok(s) => tracing::warn!("dbus-update-activation-environment exited with {s}"),
            Err(e) => tracing::warn!("could not run dbus-update-activation-environment: {e}"),
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
        let mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE;
        if let Err(e) = inotify.watches().add(dir, mask) {
            return tracing::warn!("config hot reload disabled: cannot watch {}: {e}", dir.display());
        }
        // Saved themes: reload when the one in use changes. Created up front so it can be watched.
        let themes = wt_config::themes_dir(&path);
        let _ = std::fs::create_dir_all(&themes);
        let themes_watch = match inotify.watches().add(&themes, mask) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("theme hot reload disabled: cannot watch {}: {e}", themes.display());
                None
            }
        };

        let source = Generic::new(inotify, Interest::READ, Mode::Level);
        let result = self.loop_handle.insert_source(source, move |_, inotify, state| {
            let mut buf = [0u8; 4096];
            // SAFETY: we only read events; the inotify fd itself is never dropped or replaced.
            let inotify = unsafe { inotify.get_mut() };
            let mut hit = false;
            while let Ok(events) = inotify.read_events(&mut buf) {
                let mut any = false;
                let theme_file = state.config.theme.name.as_ref().map(|n| format!("{n}.toml"));
                for ev in events {
                    any = true;
                    hit |= if Some(&ev.wd) == themes_watch.as_ref() {
                        theme_file.is_some() && ev.name.and_then(|n| n.to_str()) == theme_file.as_deref()
                    } else {
                        ev.name == Some(name.as_os_str())
                    };
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

impl State {
    /// Every rule matching a window, merged in order: later rules win where they set something.
    fn rules_for(&self, app_id: Option<&str>, title: Option<&str>) -> crate::config::WindowRule {
        let mut out = crate::config::WindowRule::default();
        for r in self.config.rules.iter().filter(|r| r.matches(app_id, title)) {
            macro_rules! take {
                ($($f:ident),*) => { $( if r.$f.is_some() { out.$f = r.$f.clone(); } )* };
            }
            take!(floating, workspace, output, size, position, fullscreen, focus, opacity);
        }
        out
    }
}

/// The launcher's app id (and program name).
pub const LAUNCHER: &str = "wayland-time-launcher";

impl State {
    /// The `launcher` action: open it, or close it if it's open.
    pub fn toggle_launcher(&mut self) {
        let open = self
            .all_workspaces()
            .flat_map(|ws| ws.entries.iter())
            .find(|e| toplevel_ids(&e.window).0.as_deref() == Some(LAUNCHER))
            .map(|e| e.window.clone());
        match open {
            Some(w) => close_window(&w),
            None => self.spawn(LAUNCHER),
        }
    }

    /// The launcher has the keyboard: pointer movement shouldn't take it away.
    pub fn launcher_focused(&self) -> bool {
        self.ws().focus.as_ref().is_some_and(|w| toplevel_ids(w).0.as_deref() == Some(LAUNCHER))
    }
}

/// A rule's position for a floating Wayland window, used once it has picked its size.
struct RulePlacement(std::cell::Cell<Option<crate::config::RulePosition>>);

/// Where a floating window of `size` goes in `area`: at a rule's position, or centered.
/// The size is kept within the area.
fn place_floating(
    area: Rectangle<i32, Logical>,
    size: (i32, i32),
    position: Option<crate::config::RulePosition>,
) -> Rect {
    use crate::config::RulePosition;
    let (w, h) = (size.0.clamp(1, area.size.w.max(1)), size.1.clamp(1, area.size.h.max(1)));
    let (x, y) = match position {
        Some(RulePosition::At([x, y])) => (
            area.loc.x + x.clamp(0, (area.size.w - w).max(0)),
            area.loc.y + y.clamp(0, (area.size.h - h).max(0)),
        ),
        Some(RulePosition::Named(_)) | None => (
            area.loc.x + (area.size.w - w) / 2,
            area.loc.y + (area.size.h - h) / 2,
        ),
    };
    Rect::new(x, y, w, h)
}

/// How see-through a window is drawn, from a rule.
struct WindowOpacity(f32);

fn set_window_opacity(w: &Window, opacity: f32) {
    w.user_data().insert_if_missing(|| WindowOpacity(opacity.clamp(0.1, 1.0)));
}

/// 1.0 unless a rule made the window see-through.
pub fn window_opacity(w: &Window) -> f32 {
    w.user_data().get::<WindowOpacity>().map_or(1.0, |o| o.0)
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
        let mut ws = new_workspace(config);
        ws.label = config.workspace_labels.get(list.len()).cloned().unwrap_or_default();
        list.push(ws);
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
    (0..config.layout.workspaces)
        .map(|i| {
            let mut ws = new_workspace(config);
            ws.label = config.workspace_labels.get(i).cloned().unwrap_or_default();
            ws
        })
        .collect()
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

pub use wt_icons::xdg_data_dirs;

/// Portals that were already running (say, from an earlier login) keep the environment
/// they started with, so a stale XDG_CURRENT_DESKTOP means no screen sharing. Restart the
/// ones that are running; ones that aren't will start later with the new values.
/// --no-block: a slow portal must not hold up startup.
fn restart_portals() {
    let result = Command::new("systemctl")
        .args(["--user", "--no-block", "try-restart"])
        .args([
            "xdg-desktop-portal.service",
            "xdg-desktop-portal-wlr.service",
            "xdg-desktop-portal-gtk.service",
            "xdg-desktop-portal-gnome.service",
        ])
        .stdin(Stdio::null())
        .status();
    match result {
        Ok(s) if s.success() => tracing::info!("restarted running desktop portals"),
        Ok(s) => tracing::warn!("restarting desktop portals: systemctl exited with {s}"),
        Err(e) => tracing::warn!("could not restart desktop portals: {e}"),
    }
}

/// Turn Num Lock on, keeping the other modifiers as they are.
fn set_numlock(keyboard: &KeyboardHandle<State>) {
    let mut mods = keyboard.modifier_state();
    mods.num_lock = true;
    keyboard.set_modifier_state(mods);
}
