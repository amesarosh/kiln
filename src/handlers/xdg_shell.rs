use smithay::{
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window, WindowSurfaceType,
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
    },
    input::{Seat, pointer::Focus},
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode, shell::server::xdg_toplevel,
        },
        wayland_server::protocol::{wl_output::WlOutput, wl_seat},
    },
    utils::Serial,
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        decoration::XdgDecorationHandler,
    },
};

use crate::{grabs, state::State};

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Managed on first commit, once app_id and title are set.
        self.pending_windows.push(Window::new_wayland_window(surface));
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        // Before the first commit, manage_window sees the parent itself.
        if let Some(window) = self.window_for_surface(surface.wl_surface())
            && !self.pending_windows.contains(&window)
        {
            State::parent_changed(self, &window);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.unmanage_window(&window);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("failed to track popup: {e}");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<State>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(mut grab) = self.popups.grab_popup(root.into(), kind, &seat, serial) else {
            return;
        };

        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial) || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        if let (Some(seat), Some(window)) = (
            Seat::from_resource(&seat),
            self.window_for_surface(surface.wl_surface()),
        ) {
            grabs::start_move(self, &seat, window, serial);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        if let (Some(seat), Some(window)) = (
            Seat::from_resource(&seat),
            self.window_for_surface(surface.wl_surface()),
        ) {
            grabs::start_resize(self, &seat, window, serial);
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        let window = self.window_for_surface(surface.wl_surface());
        match window.and_then(|w| Some((self.monitor_showing(&w)?, w))) {
            Some((mon, w)) => {
                self.monitors[mon].ws_mut().fullscreen = Some(w);
                self.arrange();
            }
            // Protocol requires a configure in reply even when we decline.
            _ => {
                if surface.is_initial_configure_sent() {
                    surface.send_configure();
                }
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let window = self.window_for_surface(surface.wl_surface());
        let ws = window.as_ref().and_then(|w| self.workspace_of_mut(w));
        if let Some(ws) = ws
            && ws.fullscreen == window
        {
            ws.fullscreen = None;
            self.arrange();
        } else if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(w) = self.window_for_surface(surface.wl_surface()) {
            self.minimize_window(&w);
        }
    }

    // Tiling decides window sizes, so maximize is acknowledged but ignored.
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }
}
delegate_xdg_shell!(State);

impl State {
    /// Keep popups (menus, tooltips) inside the screen.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        // Parent may be a window or a layer surface (e.g. a bar's menu). Keep the popup on the
        // parent's monitor.
        let (output, parent_loc) = if let Some(window) = self.window_for_surface(&root) {
            let Some(g) = self.space.element_geometry(&window) else {
                return;
            };
            let Some(output) = self
                .monitor_showing(&window)
                .map(|m| self.monitors[m].output.clone())
                .or_else(|| self.output())
            else {
                return;
            };
            (output, g.loc)
        } else {
            let found = self.space.outputs().find_map(|o| {
                let map = layer_map_for_output(o);
                let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
                Some((o.clone(), map.layer_geometry(layer)?.loc))
            });
            let Some((output, loc)) = found else { return };
            let Some(geo) = self.space.output_geometry(&output) else {
                return;
            };
            (output, loc + geo.loc)
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= parent_loc;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

impl XdgDecorationHandler for State {
    // Ask clients not to draw their own title bars; borders are drawn by the compositor.
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}
delegate_xdg_decoration!(State);
