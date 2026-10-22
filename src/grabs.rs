//! Interactive move and resize. Tiled windows stay tiled: moving one swaps it with the tile
//! under the pointer (or, at the top or bottom edge of a tile in the other column, joins
//! that column; with just two tiles, flips them between side by side and stacked), and
//! resizing one moves the borders it shares with its neighbours.
//! Floating windows move and resize freely.

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

use smithay::wayland::seat::WaylandFocus;

use crate::{
    layout::{LayoutKind, Rect},
    state::State,
};

/// Rows (columns in the vertical layout) can't be dragged smaller than this.
const MIN_TILE_HEIGHT: i32 = 50;

/// How far into a tile, as a fraction of its height, its top and bottom drop zones reach.
/// Dropping there on a tile in the other column moves the window into that column.
const EDGE_ZONE: f64 = 0.25;

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

/// Start a move or resize asked for by an X11 window. X11 requests carry no serial, so the
/// pointer just has to be holding a button over the same app.
pub fn start_x11_grab(state: &mut State, seat: &Seat<State>, window: Window, resize: bool) {
    let Some(pointer) = seat.get_pointer() else { return };
    let Some(start) = pointer.grab_start_data() else { return };
    let same_client = match (start.focus.as_ref(), window.wl_surface()) {
        (Some((focus, _)), Some(surface)) => focus.id().same_client_as(&surface.id()),
        _ => false,
    };
    if !same_client {
        return;
    }
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    if resize {
        begin_resize(state, seat, window, start, serial);
    } else {
        begin_move(state, seat, window, start, serial);
    }
}

pub fn begin_move(state: &mut State, seat: &Seat<State>, window: Window, start: GrabStartData<State>, serial: Serial) {
    if is_tiled(state, &window) {
        state.focus_window(&window);
        state.start_tile_drag(&window, start.location);
        let grab = TileMoveGrab {
            start,
            window,
            settled_over: None,
        };
        if let Some(pointer) = seat.get_pointer() {
            pointer.set_grab(state, grab, serial, Focus::Clear);
        }
        return;
    }
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
    if let Some(tiled) = tiled_resize(state, &window, &start) {
        state.focus_window(&window);
        let grab = TileResizeGrab { start, tiled };
        if let Some(pointer) = seat.get_pointer() {
            pointer.set_grab(state, grab, serial, Focus::Clear);
        }
        return;
    }
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

/// Shown in the tiling right now (not floating, not hidden behind a fullscreen window).
fn is_tiled(state: &State, window: &Window) -> bool {
    state.monitor_showing(window).is_some_and(|mon| {
        let ws = state.monitors[mon].ws();
        ws.fullscreen.is_none() && !ws.is_floating(window)
    })
}

/// The tiled window whose slot is under a point, and that slot. Monocle has no slots side by side.
fn tile_at(state: &State, mon: usize, pos: Point<f64, Logical>) -> Option<(Window, Rect)> {
    let ws = state.monitors[mon].ws();
    if ws.layout == LayoutKind::Monocle || ws.fullscreen.is_some() {
        return None;
    }
    let rects = state.tile_rects(mon, state.work_area(mon)?);
    ws.tiled()
        .zip(rects)
        .find(|(_, r)| r.contains(pos.x, pos.y))
        .map(|(w, r)| (w.clone(), r))
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
    let surface = window.wl_surface()?;
    focus.id().same_client_as(&surface.id()).then_some(start)
}

/// What a drag on a tiled window can change, measured when the drag starts.
/// Written for the tile layout; the vertical layout is measured with x and y swapped.
struct TiledResize {
    window: Window,
    /// The vertical layout: pointer movement is read with x and y swapped.
    transposed: bool,
    /// Sideways: the master ratio at the start, and the pixel width it is a fraction of.
    split: Option<(f32, f64)>,
    /// Up and down: the border between two rows of one column.
    rows: Option<Rows>,
}

struct Rows {
    upper: Window,
    lower: Window,
    /// Heights in pixels and weights at the start.
    heights: (i32, i32),
    weights: (f32, f32),
}

/// Set up a tiled resize, or None if the window isn't tiled (then it floats as before).
/// Monocle has nothing to share space with, so it floats too.
fn tiled_resize(state: &State, window: &Window, start: &GrabStartData<State>) -> Option<TiledResize> {
    let mon = state.monitor_showing(window)?;
    let ws = state.monitors[mon].ws();
    if ws.layout == LayoutKind::Monocle || ws.fullscreen.is_some() || ws.is_floating(window) {
        return None;
    }
    let tiled: Vec<Window> = ws.tiled().cloned().collect();
    let i = tiled.iter().position(|w| w == window)?;
    let transposed = ws.layout == LayoutKind::Vertical;
    let mut rects = state.tile_rects(mon, state.work_area(mon)?);
    let mut grabbed = start.location;
    if transposed {
        rects.iter_mut().for_each(|r| *r = r.transpose());
        grabbed = (grabbed.y, grabbed.x).into();
    }
    let weights = ws.tiled_weights();
    let n = tiled.len();
    let masters = ws.master_count.min(n);

    // Master and stack column widths add up to the width the ratio divides.
    let split = (masters > 0 && masters < n).then(|| (ws.master_ratio, (rects[0].w + rects[masters].w) as f64));

    // Grabbing the top half moves the top border, the bottom half the bottom one; at the
    // end of a column, use the only border there is.
    let column = if i < masters { 0..masters } else { masters..n };
    let r = rects[i];
    let top_half = grabbed.y < (r.y + r.h / 2) as f64;
    let above = (i > column.start).then(|| i - 1);
    let below = (i + 1 < column.end).then_some(i + 1);
    let other = if top_half { above.or(below) } else { below.or(above) };
    let rows = other.map(|j| {
        let (a, b) = (i.min(j), i.max(j));
        Rows {
            upper: tiled[a].clone(),
            lower: tiled[b].clone(),
            heights: (rects[a].h, rects[b].h),
            weights: (weights[a], weights[b]),
        }
    });

    Some(TiledResize {
        window: window.clone(),
        transposed,
        split,
        rows,
    })
}

struct TileResizeGrab {
    start: GrabStartData<State>,
    tiled: TiledResize,
}

impl TileResizeGrab {
    fn on_unset(&mut self, _state: &mut State) {}

    /// The borders follow the pointer.
    fn on_motion(&mut self, state: &mut State, location: Point<f64, Logical>) {
        let mut delta = location - self.start.location;
        let t = &self.tiled;
        if t.transposed {
            delta = (delta.y, delta.x).into();
        }
        let Some(ws) = state.workspace_of_mut(&t.window) else {
            return;
        };
        let mut changed = false;
        if let Some((ratio, width)) = t.split
            && width > 0.0
        {
            let new = (ratio + (delta.x / width) as f32).clamp(0.05, 0.95);
            changed |= new != ws.master_ratio;
            ws.master_ratio = new;
        }
        if let Some(rows) = &t.rows {
            let (ha, hb) = rows.heights;
            let total = ha + hb;
            let upper_h = (ha + delta.y.round() as i32)
                .clamp(MIN_TILE_HEIGHT.min(total / 2), (total - MIN_TILE_HEIGHT).max(total / 2));
            let sum = rows.weights.0 + rows.weights.1;
            let wa = sum * upper_h as f32 / total.max(1) as f32;
            if let Some(e) = ws.entry_mut(&rows.upper) {
                changed |= e.weight != wa;
                e.weight = wa;
            }
            if let Some(e) = ws.entry_mut(&rows.lower) {
                e.weight = sum - wa;
            }
        }
        if changed {
            state.arrange();
        }
    }

    fn on_release(&mut self, _state: &mut State, _location: Point<f64, Logical>) {}
}

/// Dragging a tiled window: it stays tiled and trades places with whichever tile the
/// pointer enters. Near the top or bottom of a tile in the other column it joins that
/// column instead, above or below the tile.
struct TileMoveGrab {
    start: GrabStartData<State>,
    window: Window,
    /// The tile it just moved next to. That tile may still be under the pointer, so it's
    /// left alone until the pointer leaves it, or the window would bounce straight back.
    settled_over: Option<Window>,
}

impl TileMoveGrab {
    /// Covers grabs that end without a release, like a VT switch mid-drag.
    fn on_unset(&mut self, state: &mut State) {
        state.end_tile_drag();
    }

    fn on_motion(&mut self, state: &mut State, location: Point<f64, Logical>) {
        state.move_tile_drag(location);
        let Some(mon) = state.monitor_showing(&self.window) else {
            return;
        };
        if state.monitor_at(location) != Some(mon) {
            return;
        }
        let hovered = tile_at(state, mon, location);
        if hovered.as_ref().map(|(w, _)| w) == self.settled_over.as_ref() {
            return;
        }
        self.settled_over = None;
        let Some((other, r)) = hovered.filter(|(o, _)| *o != self.window) else {
            return;
        };
        let ws = state.monitors[mon].ws_mut();
        // Rows run across in the vertical layout, so its drop zones are the left and right edges.
        let (p, start, len) = if ws.layout == LayoutKind::Vertical {
            (location.x, r.x, r.w)
        } else {
            (location.y, r.y, r.h)
        };
        let f = (p - start as f64) / len.max(1) as f64;
        let edge = if f < EDGE_ZONE {
            Some(false)
        } else if f > 1.0 - EDGE_ZONE {
            Some(true)
        } else {
            None
        };
        if edge.is_some_and(|below| {
            ws.move_to_column(&self.window, &other, below) || ws.flip_pair(&self.window, &other, below)
        }) {
            self.settled_over = Some(other);
        } else {
            // After a swap the dragged window sits under the pointer, so this can't flip back.
            ws.swap_windows(&self.window, &other);
        }
        state.arrange_animated();
    }

    /// Dropped on another monitor: it moves there, into the slot it was dropped on.
    fn on_release(&mut self, state: &mut State, location: Point<f64, Logical>) {
        self.drop_on(state, location);
        state.end_tile_drag();
    }

    fn drop_on(&mut self, state: &mut State, location: Point<f64, Logical>) {
        let (Some(from), Some(to)) = (state.monitor_showing(&self.window), state.monitor_at(location)) else {
            return;
        };
        if from == to {
            return;
        }
        let target = tile_at(state, to, location).map(|(w, _)| w);
        let before: Vec<(Window, Point<f64, Logical>)> = state
            .space
            .elements()
            .filter_map(|w| Some((w.clone(), state.visual_location(w)?)))
            .collect();
        state.adopt_dropped_window(&self.window, location);
        if let Some(t) = target {
            state.monitors[to].ws_mut().move_before(&self.window, &t);
            state.arrange();
        }
        // Tiles on both monitors close up or make room.
        for (w, at) in before {
            state.slide_from(&w, at);
        }
    }
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
    fn on_unset(&mut self, _state: &mut State) {}

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
        // X11 apps place their menus from where they think they are.
        if let Some(x) = self.window.x11_surface() {
            let _ = x.configure(Rectangle::new(loc, x.geometry().size));
        }
        state.needs_redraw = true;
    }

    /// Dropping a window on another monitor moves it there.
    fn on_release(&mut self, state: &mut State, location: Point<f64, Logical>) {
        state.adopt_dropped_window(&self.window, location);
    }
}

impl ResizeGrab {
    fn on_unset(&mut self, _state: &mut State) {}

    fn on_motion(&mut self, state: &mut State, location: Point<f64, Logical>) {
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
        if let Some(x) = self.window.x11_surface() {
            let loc = state.space.element_location(&self.window).unwrap_or_default();
            let _ = x.configure(Rectangle::new(loc, size));
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

            fn unset(&mut self, data: &mut State) {
                self.on_unset(data);
            }
        }
    };
}

impl_grab!(MoveGrab);
impl_grab!(TileMoveGrab);
impl_grab!(ResizeGrab);
impl_grab!(TileResizeGrab);
