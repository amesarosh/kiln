//! Interactive move and resize. Both make a tiled window floating, like dwm.

use smithay::{
    desktop::Window,
    input::{
        Seat,
        pointer::{
            AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle, Serial, Size},
};

use crate::{layout::Rect, state::State};

/// Start a move from a client request (title bar drag). Validates the client actually holds a click.
pub fn start_move(state: &mut State, seat: &Seat<State>, window: Window, serial: Serial) {
    if let Some(start) = client_grab_start(seat, &window, serial) {
        begin_move(state, seat, window, start, serial);
    }
}

pub fn start_resize(state: &mut State, seat: &Seat<State>, window: Window, serial: Serial) {
    if let Some(start) = client_grab_start(seat, &window, serial) {
        begin_resize(state, seat, window, start, serial);
    }
}

pub fn begin_move(state: &mut State, seat: &Seat<State>, window: Window, start: GrabStartData<State>, serial: Serial) {
    let Some(geo) = float_window(state, &window) else {
        return;
    };
    let grab = MoveGrab {
        start,
        window,
        initial: geo.loc,
    };
    if let Some(pointer) = seat.get_pointer() {
        pointer.set_grab(state, grab, serial, Focus::Clear);
    }
}

pub fn begin_resize(
    state: &mut State,
    seat: &Seat<State>,
    window: Window,
    start: GrabStartData<State>,
    serial: Serial,
) {
    let Some(geo) = float_window(state, &window) else {
        return;
    };
    if let Some(t) = window.toplevel() {
        t.with_pending_state(|s| s.states.set(xdg_toplevel::State::Resizing));
        t.send_pending_configure();
    }
    let grab = ResizeGrab {
        start,
        window,
        initial: geo.size,
    };
    if let Some(pointer) = seat.get_pointer() {
        pointer.set_grab(state, grab, serial, Focus::Clear);
    }
}

/// Make the window floating at its current geometry and return that geometry.
fn float_window(state: &mut State, window: &Window) -> Option<Rectangle<i32, Logical>> {
    let ws = state.workspace_of_mut(window)?;
    if ws.fullscreen.as_ref() == Some(window) {
        return None;
    }
    let geo = state.space.element_geometry(window)?;
    let entry = state.workspace_of_mut(window)?.entry_mut(window)?;
    if entry.floating.is_none() {
        entry.floating = Some(Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h));
    }
    state.focus_window(window);
    state.arrange();
    Some(geo)
}

fn client_grab_start(seat: &Seat<State>, window: &Window, serial: Serial) -> Option<GrabStartData<State>> {
    let pointer = seat.get_pointer()?;
    if !pointer.has_grab(serial) {
        return None;
    }
    let start = pointer.grab_start_data()?;
    let (focus, _) = start.focus.as_ref()?;
    let surface = window.toplevel()?.wl_surface();
    focus.id().same_client_as(&surface.id()).then_some(start)
}

struct MoveGrab {
    start: GrabStartData<State>,
    window: Window,
    initial: Point<i32, Logical>,
}

struct ResizeGrab {
    start: GrabStartData<State>,
    window: Window,
    initial: Size<i32, Logical>,
}

impl MoveGrab {
    fn on_motion(&mut self, state: &mut State, location: Point<f64, Logical>) {
        let loc = (self.initial.to_f64() + (location - self.start.location)).to_i32_round();
        if let Some(Some(r)) = state
            .workspace_of_mut(&self.window)
            .and_then(|ws| ws.entry_mut(&self.window))
            .map(|e| e.floating.as_mut())
        {
            r.x = loc.x;
            r.y = loc.y;
        }
        state.space.map_element(self.window.clone(), loc, false);
        state.needs_redraw = true;
    }

    /// Dropping a window on another monitor moves it there.
    fn on_release(&mut self, state: &mut State, location: Point<f64, Logical>) {
        state.adopt_dropped_window(&self.window, location);
    }
}

impl ResizeGrab {
    fn on_motion(&mut self, _state: &mut State, location: Point<f64, Logical>) {
        let delta = location - self.start.location;
        let size: Size<i32, Logical> = (
            (self.initial.w as f64 + delta.x).max(50.0) as i32,
            (self.initial.h as f64 + delta.y).max(50.0) as i32,
        )
            .into();
        if let Some(t) = self.window.toplevel() {
            t.with_pending_state(|s| s.size = Some(size));
            t.send_pending_configure();
        }
    }

    fn on_release(&mut self, _state: &mut State, _location: Point<f64, Logical>) {
        if let Some(t) = self.window.toplevel() {
            t.with_pending_state(|s| s.states.unset(xdg_toplevel::State::Resizing));
            t.send_pending_configure();
        }
    }
}

/// Both grabs share everything except motion and release handling.
macro_rules! impl_grab {
    ($ty:ty) => {
        impl PointerGrab<State> for $ty {
            fn motion(
                &mut self,
                data: &mut State,
                handle: &mut PointerInnerHandle<'_, State>,
                _focus: Option<(WlSurface, Point<f64, Logical>)>,
                event: &MotionEvent,
            ) {
                handle.motion(data, None, event);
                self.on_motion(data, event.location);
            }

            fn relative_motion(
                &mut self,
                data: &mut State,
                handle: &mut PointerInnerHandle<'_, State>,
                focus: Option<(WlSurface, Point<f64, Logical>)>,
                event: &RelativeMotionEvent,
            ) {
                handle.relative_motion(data, focus, event);
            }

            fn button(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &ButtonEvent) {
                handle.button(data, event);
                if handle.current_pressed().is_empty() {
                    // From the handle: the pointer is locked while a grab runs, so asking
                    // the seat for its location here would deadlock.
                    let location = handle.current_location();
                    self.on_release(data, location);
                    handle.unset_grab(self, data, event.serial, event.time, true);
                }
            }

            fn axis(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, details: AxisFrame) {
                handle.axis(data, details)
            }

            fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
                handle.frame(data);
            }

            fn gesture_swipe_begin(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GestureSwipeBeginEvent,
            ) {
                h.gesture_swipe_begin(d, e)
            }
            fn gesture_swipe_update(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GestureSwipeUpdateEvent,
            ) {
                h.gesture_swipe_update(d, e)
            }
            fn gesture_swipe_end(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GestureSwipeEndEvent,
            ) {
                h.gesture_swipe_end(d, e)
            }
            fn gesture_pinch_begin(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GesturePinchBeginEvent,
            ) {
                h.gesture_pinch_begin(d, e)
            }
            fn gesture_pinch_update(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GesturePinchUpdateEvent,
            ) {
                h.gesture_pinch_update(d, e)
            }
            fn gesture_pinch_end(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GesturePinchEndEvent,
            ) {
                h.gesture_pinch_end(d, e)
            }
            fn gesture_hold_begin(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GestureHoldBeginEvent,
            ) {
                h.gesture_hold_begin(d, e)
            }
            fn gesture_hold_end(
                &mut self,
                d: &mut State,
                h: &mut PointerInnerHandle<'_, State>,
                e: &GestureHoldEndEvent,
            ) {
                h.gesture_hold_end(d, e)
            }

            fn start_data(&self) -> &GrabStartData<State> {
                &self.start
            }

            fn unset(&mut self, _data: &mut State) {}
        }
    };
}

impl_grab!(MoveGrab);
impl_grab!(ResizeGrab);
