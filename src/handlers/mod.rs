mod compositor;
mod idle;
mod layer_shell;
pub mod lock;
mod output_power;
mod xdg_shell;

use smithay::{
    delegate_data_device, delegate_fractional_scale, delegate_output, delegate_primary_selection, delegate_seat,
    delegate_viewporter,
    input::{Seat, SeatHandler, SeatState, keyboard::LedState, pointer::CursorImageStatus},
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::{
        compositor::with_states,
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

use crate::state::State;

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
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

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}
delegate_seat!(State);

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}
delegate_data_device!(State);

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(State);

impl OutputHandler for State {}
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
