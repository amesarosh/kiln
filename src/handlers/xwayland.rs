//! Xwayland: runs X11 apps (Steam, games, older programs) as windows like any other.
//!
//! Xwayland is started with the compositor and its display (`:N`) is set as `DISPLAY` for
//! everything we launch. Normal X11 windows are tiled and focused like Wayland windows.
//! Override-redirect windows (X11 menus, tooltips, drag icons) place themselves; they are only
//! drawn, on top of other windows, at the position they ask for.

use std::{os::fd::OwnedFd, process::Stdio, time::Duration};

use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    reexports::calloop::timer::{TimeoutAction, Timer},
    utils::{Logical, Rectangle},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{clear_primary_selection, request_primary_client_selection, set_primary_selection},
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, WmWindowType, XwmId},
    },
};

use x11rb::{
    protocol::xproto::{AtomEnum, ConnectionExt, PropMode},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::{clipboard::SelectionData, cursor::Cursor, focus::KeyboardFocusTarget, grabs, layout::Rect, state::State};

/// If Xwayland hasn't reported in by then, run startup commands without it.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

impl State {
    /// Start Xwayland. Startup commands run once it's ready (or failed), so they get `DISPLAY`.
    pub fn start_xwayland(&mut self) {
        let spawned = XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        );
        let (xwayland, client) = match spawned {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("could not start Xwayland, X11 apps won't run: {e}");
                self.run_startup_commands();
                return;
            }
        };

        let inserted = self
            .loop_handle
            .insert_source(xwayland, move |event, _, state| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    match X11Wm::start_wm(state.loop_handle.clone(), x11_socket, client.clone()) {
                        Ok(mut wm) => {
                            // The cursor X11 shows over windows that don't set their own.
                            if let Some((pixels, size, hotspot)) = Cursor::new().default_image() {
                                let _ = wm.set_cursor(&pixels, size.into(), hotspot.into());
                            }
                            state.xwm = Some(wm);
                            state.xdisplay = Some(display_number);
                            state.x11_props = X11Props::connect(display_number);
                            tracing::info!("Xwayland ready on DISPLAY=:{display_number}");
                        }
                        Err(e) => tracing::warn!("Xwayland started but its window manager failed: {e}"),
                    }
                    state.run_startup_commands();
                }
                XWaylandEvent::Error => {
                    tracing::warn!("Xwayland exited during startup; X11 apps won't run");
                    state.run_startup_commands();
                }
            });
        if let Err(e) = inserted {
            tracing::warn!("could not watch Xwayland: {e}");
            self.run_startup_commands();
            return;
        }

        let _ = self
            .loop_handle
            .insert_source(Timer::from_duration(STARTUP_TIMEOUT), |_, _, state| {
                if !state.startup_commands.is_empty() {
                    tracing::warn!("Xwayland is slow to start; running startup commands without it");
                    state.run_startup_commands();
                }
                TimeoutAction::Drop
            });
    }

    /// The managed window for an X11 surface.
    pub fn window_for_x11(&self, surface: &X11Surface) -> Option<Window> {
        self.all_workspaces()
            .flat_map(|ws| ws.entries.iter().map(|e| &e.window))
            .find(|w| w.x11_surface() == Some(surface))
            .cloned()
    }

    fn forget_x11(&mut self, surface: &X11Surface) {
        self.x11_unmanaged.retain(|s| s != surface);
        if let Some(w) = self.window_for_x11(surface) {
            self.unmanage_window(&w);
        }
        self.needs_redraw = true;
    }

    /// Whether the keyboard is on an X11 window, so X11 may read the clipboard.
    fn x11_has_focus(&self) -> bool {
        matches!(self.keyboard().current_focus(), Some(KeyboardFocusTarget::X11(..)))
    }
}

/// A second connection to Xwayland, for window properties smithay's window manager doesn't set.
pub struct X11Props {
    conn: RustConnection,
    frame_extents: u32,
}

impl X11Props {
    fn connect(display: u32) -> Option<Self> {
        let result = (|| -> Result<Self, Box<dyn std::error::Error>> {
            let (conn, _) = RustConnection::connect(Some(&format!(":{display}")))?;
            let frame_extents = conn.intern_atom(false, b"_NET_FRAME_EXTENTS")?.reply()?.atom;
            Ok(Self { conn, frame_extents })
        })();
        result
            .inspect_err(|e| tracing::warn!("could not open a second Xwayland connection: {e}"))
            .ok()
    }

    /// Say the window has no frame. Java waits for this before it draws: without it, it guesses
    /// at title-bar insets and ignores every resize that arrives with the reparent, so the
    /// window stays blank. Done before mapping, which reparents.
    fn set_no_frame(&self, window: u32) {
        let set = self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.frame_extents,
            AtomEnum::CARDINAL,
            &[0; 4],
        );
        // Waits for it, so the property is there before the window manager reparents.
        if let Err(e) = set
            .map_err(|e| e.to_string())
            .and_then(|c| c.check().map_err(|e| e.to_string()))
        {
            tracing::debug!("could not set _NET_FRAME_EXTENTS: {e}");
        }
    }
}

/// Dialogs, menus and fixed-size windows float instead of tiling.
pub fn x11_wants_float(s: &X11Surface) -> bool {
    let fixed = matches!((s.min_size(), s.max_size()), (Some(a), Some(b)) if a == b && a.w > 0);
    s.is_transient_for().is_some()
        || s.is_popup()
        || fixed
        || !matches!(s.window_type(), None | Some(WmWindowType::Normal))
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("X11 events only arrive while the window manager runs")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(props) = &self.x11_props {
            props.set_no_frame(window.window_id());
        }
        if let Err(e) = window.set_mapped(true) {
            tracing::warn!("could not map X11 window \"{}\": {e}", window.title());
            return;
        }
        if self.window_for_x11(&window).is_none() {
            self.manage_window(Window::new_x11_window(window.clone()));
        }
        // Always tell a newly mapped window its geometry, even if unchanged: X11 requires it,
        // and Java apps that reopen at their old tile's exact size stay blank without it.
        let _ = window.configure(None);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if !self.x11_unmanaged.contains(&window) {
            self.x11_unmanaged.push(window);
        }
        self.needs_redraw = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let Some(managed) = self.window_for_x11(&window) else {
            // Not shown yet: it may pick its own size and position.
            let mut geo = window.geometry();
            geo.loc.x = x.unwrap_or(geo.loc.x);
            geo.loc.y = y.unwrap_or(geo.loc.y);
            geo.size.w = w.map_or(geo.size.w, |w| w as i32);
            geo.size.h = h.map_or(geo.size.h, |h| h as i32);
            let _ = window.configure(geo);
            return;
        };
        // Floating windows may resize themselves; tiled ones keep their tile. Either way the
        // client is told its current geometry, which X11 requires as an answer.
        if let Some(r) = self
            .workspace_of_mut(&managed)
            .and_then(|ws| ws.entry_mut(&managed))
            .and_then(|e| e.floating.as_mut())
        {
            r.w = w.map_or(r.w, |w| w as i32);
            r.h = h.map_or(r.h, |h| h as i32);
        }
        if self.monitor_showing(&managed).is_some() {
            self.arrange();
        }
        // arrange() only configures windows whose geometry changed; this always answers.
        let _ = window.configure(None);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Override-redirect windows move themselves; their geometry is read when drawing.
        if window.is_override_redirect() {
            self.needs_redraw = true;
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, _window: X11Surface, property: WmWindowProperty) {
        // The bar shows titles.
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class) {
            self.needs_redraw = true;
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(w) = self.window_for_x11(&window)
            && let Some(mon) = self.monitor_showing(&w)
        {
            self.monitors[mon].ws_mut().fullscreen = Some(w);
            self.arrange();
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(w) = self.window_for_x11(&window)
            && let Some(ws) = self.workspace_of_mut(&w)
            && ws.fullscreen.as_ref() == Some(&w)
        {
            ws.fullscreen = None;
            self.arrange();
        }
    }

    // Tiling decides sizes; answer with the current geometry.
    fn maximize_request(&mut self, _xwm: XwmId, _window: X11Surface) {
        self.arrange();
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, _window: X11Surface) {
        self.arrange();
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        if let Some(w) = self.window_for_x11(&window) {
            let seat = self.seat.clone();
            grabs::start_x11_grab(self, &seat, w, false);
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, _edge: ResizeEdge) {
        if let Some(w) = self.window_for_x11(&window) {
            let seat = self.seat.clone();
            grabs::start_x11_grab(self, &seat, w, true);
        }
    }

    // ---------- clipboard between X11 and Wayland apps ----------

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        self.x11_has_focus()
    }

    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        // Our own copy of the clipboard (its app has closed, or it's about to).
        if selection == SelectionTarget::Clipboard
            && let Some(kept) = self.kept_clipboard()
        {
            crate::clipboard::serve_kept(&kept, &mime_type, fd);
            return;
        }
        let result = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd).map_err(|e| format!("{e:?}"))
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.seat, mime_type, fd).map_err(|e| format!("{e:?}"))
            }
        };
        if let Err(e) = result {
            tracing::debug!("X11 asked for a selection that isn't available: {e}");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        let dh = self.display_handle.clone();
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&dh, &self.seat, mime_types.clone(), SelectionData::X11);
                self.keep_clipboard(mime_types, true);
            }
            SelectionTarget::Primary => set_primary_selection(&dh, &self.seat, mime_types, SelectionData::X11),
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        let dh = self.display_handle.clone();
        match selection {
            // The X11 app that copied went away, but we kept a copy: offer it to X11 apps too.
            SelectionTarget::Clipboard if self.kept_clipboard().is_some() => {
                self.loop_handle.insert_idle(|state| {
                    let Some(kept) = state.kept_clipboard() else { return };
                    if let Some(xwm) = state.xwm.as_mut() {
                        let mime_types = kept.iter().map(|(m, _)| m.clone()).collect();
                        let _ = xwm.new_selection(SelectionTarget::Clipboard, Some(mime_types));
                        // See SelectionHandler::new_selection: this flushes without waiting.
                        let _ = xwm.update_stacking_order_upwards(std::iter::empty::<&X11Surface>());
                    }
                });
            }
            SelectionTarget::Clipboard => {
                self.stop_keeping();
                clear_data_device_selection(&dh, &self.seat);
            }
            SelectionTarget::Primary => clear_primary_selection(&dh, &self.seat),
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        tracing::warn!("lost the connection to Xwayland; X11 apps were closed");
        self.xwm = None;
        self.xdisplay = None;
        let gone: Vec<Window> = self
            .all_workspaces()
            .flat_map(|ws| ws.entries.iter().map(|e| e.window.clone()))
            .filter(|w| w.is_x11())
            .collect();
        for w in gone {
            self.unmanage_window(&w);
        }
        self.x11_unmanaged.clear();
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
delegate_xwayland_shell!(State);

/// Where a new floating X11 window goes: the size it asked for, centered in `area`.
pub fn x11_float_rect(s: &X11Surface, area: Rectangle<i32, Logical>) -> Rect {
    let size = s.geometry().size;
    let (w, h) = (size.w.clamp(1, area.size.w.max(1)), size.h.clamp(1, area.size.h.max(1)));
    Rect::new(
        area.loc.x + (area.size.w - w) / 2,
        area.loc.y + (area.size.h - h) / 2,
        w,
        h,
    )
}
