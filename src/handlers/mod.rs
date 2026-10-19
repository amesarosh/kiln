mod activation;
mod compositor;
pub mod foreign_toplevel;
mod idle;
mod layer_shell;
pub mod lock;
mod output_power;
pub mod pointer_lock;
pub mod screencopy;
mod xdg_shell;
pub mod xwayland;

use smithay::{
    delegate_data_device, delegate_fractional_scale, delegate_output, delegate_primary_selection, delegate_seat,
    delegate_viewporter,
    input::{Seat, SeatHandler, SeatState, keyboard::LedState, pointer::CursorImageStatus},
    reexports::wayland_server::{
        Resource,
        protocol::{wl_data_source::WlDataSource, wl_surface::WlSurface},
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{SurfaceAttributes, with_states},
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState, set_primary_focus},
        },
    },
};

use std::os::fd::OwnedFd;

use smithay::wayland::{
    seat::WaylandFocus,
    selection::{SelectionSource, SelectionTarget},
};

use crate::{focus::KeyboardFocusTarget, state::State};

impl SeatHandler for State {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    // The nested backend shows the host cursor; the hardware backend draws this.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.needs_redraw |= self.draws_cursor();
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, leds: LedState) {
        self.update_leds(leds);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocusTarget>) {
        tracing::debug!(
            "keyboard focus: {}",
            match focused {
                None => "nothing".to_string(),
                Some(KeyboardFocusTarget::Surface(_)) => "Wayland surface".to_string(),
                Some(KeyboardFocusTarget::X11(x, _)) => format!("X11 window \"{}\"", x.title()),
            }
        );
        self.keyboard_focus_moved = true;
        let dh = &self.display_handle;
        let client = focused
            .and_then(|f| f.wl_surface())
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}
delegate_seat!(State);

// Selections are shared with Xwayland so copy and paste works between X11 and Wayland apps.
impl SelectionHandler for State {
    type SelectionUserData = crate::clipboard::SelectionData;

    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        if ty == SelectionTarget::Clipboard {
            match &source {
                Some(s) => self.keep_clipboard(s.mime_types(), false),
                None => self.stop_keeping(),
            }
        }
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(e) = xwm.new_selection(ty, source.map(|s| s.mime_types())) {
                tracing::warn!("could not pass the selection to X11: {e}");
            }
            // new_selection() doesn't flush, so X11 apps wouldn't hear about the copy until
            // something else did. This call changes nothing with an empty list, but always
            // flushes, and (unlike calls that wait for a reply) can't block on Xwayland.
            let _ = xwm.update_stacking_order_upwards(std::iter::empty::<&smithay::xwayland::X11Surface>());
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &crate::clipboard::SelectionData,
    ) {
        match user_data {
            crate::clipboard::SelectionData::Kept(kept) => crate::clipboard::serve_kept(kept, &mime_type, fd),
            crate::clipboard::SelectionData::X11 => {
                if let Some(xwm) = self.xwm.as_mut()
                    && let Err(e) = xwm.send_selection(ty, mime_type, fd, self.loop_handle.clone())
                {
                    tracing::warn!("could not read the selection from X11: {e}");
                }
            }
        }
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

/// A drag icon: drawn at the pointer, moved by `offset` (set by the app through surface offsets).
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

impl DndIcon {
    /// Take the offset change from the icon's latest commit.
    pub fn take_offset(&mut self) {
        self.offset += take_buffer_delta(&self.surface);
    }
}

fn take_buffer_delta(surface: &WlSurface) -> Point<i32, Logical> {
    with_states(surface, |s| {
        s.cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer_delta
            .take()
            .unwrap_or_default()
    })
}

impl ClientDndGrabHandler for State {
    fn started(&mut self, _source: Option<WlDataSource>, icon: Option<WlSurface>, _seat: Seat<Self>) {
        // The app may have set the icon's offset before starting the drag, so count it too.
        self.dnd_icon = icon.map(|surface| DndIcon {
            offset: take_buffer_delta(&surface),
            surface,
        });
        self.needs_redraw = true;
    }

    fn dropped(&mut self, _target: Option<WlSurface>, _validated: bool, _seat: Seat<Self>) {
        self.dnd_icon = None;
        self.needs_redraw = true;
    }
}
impl ServerDndGrabHandler for State {}
delegate_data_device!(State);

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(State);

// Clipboard managers. Both protocol versions, since tools differ in which they speak.
impl smithay::wayland::selection::wlr_data_control::DataControlHandler for State {
    fn data_control_state(&self) -> &smithay::wayland::selection::wlr_data_control::DataControlState {
        &self.wlr_data_control_state
    }
}
smithay::delegate_data_control!(State);

impl smithay::wayland::selection::ext_data_control::DataControlHandler for State {
    fn data_control_state(&self) -> &smithay::wayland::selection::ext_data_control::DataControlState {
        &self.ext_data_control_state
    }
}
smithay::delegate_ext_data_control!(State);

impl OutputHandler for State {
    fn output_bound(
        &mut self,
        output: smithay::output::Output,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        self.foreign_toplevel_output_bound(&output, &wl_output);
    }
}
delegate_output!(State);

// Viewporter lets clients pick the logical size of a buffer; fractional scale tells them the
// exact scale to render at. Together they give sharp output at scales like 1.5.
delegate_viewporter!(State);

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // A first guess so the client can draw right away; send_frames keeps it updated.
        let Some(scale) = self.output().map(|o| o.current_scale().fractional_scale()) else {
            return;
        };
        with_states(&surface, |s| {
            with_fractional_scale(s, |fs| fs.set_preferred_scale(scale))
        });
    }
}
delegate_fractional_scale!(State);
