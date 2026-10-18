use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{
    config::{Action, ModKey, Mods, normalize_keysym},
    grabs,
    state::State,
};

/// Relative motion details: (accelerated delta, raw delta, timestamp in µs).
type Relative = (Point<f64, Logical>, Point<f64, Logical>, u64);

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const XF86_SWITCH_VT_1: u32 = 0x1008_FE01;
const XF86_SWITCH_VT_12: u32 = 0x1008_FE0C;
const XKB_BACKSPACE: u32 = 0xFF08;
const XKB_TERMINATE_SERVER: u32 = 0xFED5;

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => self.on_key::<I>(event),
            InputEvent::PointerMotion { event } => {
                let delta = event.delta();
                let pos = self.clamp_to_output(self.seat.get_pointer().unwrap().current_location() + delta);
                self.on_pointer_moved(
                    pos,
                    event.time_msec(),
                    Some((delta, event.delta_unaccel(), event.time())),
                );
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(output) = self.output() else { return };
                let Some(geo) = self.space.output_geometry(&output) else {
                    return;
                };
                let pos = event.position_transformed(geo.size) + geo.loc.to_f64();
                self.on_pointer_moved(pos, event.time_msec(), None);
            }
            InputEvent::PointerButton { event } => self.on_button::<I>(event),
            InputEvent::PointerAxis { event } => self.on_axis::<I>(event),
            _ => {}
        }
    }

    fn on_key<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let code = event.key_code();
        let key_state = event.state();

        let action =
            self.keyboard()
                .input::<Option<Action>, _>(self, code, key_state, serial, time, |state, mods, handle| {
                    // Swallow the release of any key whose press triggered a bind.
                    if key_state == KeyState::Released {
                        if let Some(i) = state.suppressed_keys.iter().position(|k| *k == code) {
                            state.suppressed_keys.swap_remove(i);
                            return FilterResult::Intercept(None);
                        }
                        return FilterResult::Forward;
                    }
                    // Ctrl+Alt+Fn produces XF86Switch_VT_n; always handled so you can't get stuck.
                    let modified = handle.modified_sym().raw();
                    if (XF86_SWITCH_VT_1..=XF86_SWITCH_VT_12).contains(&modified) {
                        state.suppressed_keys.push(code);
                        let vt = (modified - XF86_SWITCH_VT_1 + 1) as i32;
                        return FilterResult::Intercept(Some(Action::SwitchVt(vt)));
                    }
                    // Match on the unshifted, layout-independent sym so "Mod+Shift+1" works.
                    let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() else {
                        return FilterResult::Forward;
                    };
                    // Ctrl+Alt+Backspace always quits, so a broken [binds] table can't trap you.
                    let raw = sym.raw();
                    if raw == XKB_TERMINATE_SERVER || (raw == XKB_BACKSPACE && mods.ctrl && mods.alt) {
                        state.suppressed_keys.push(code);
                        return FilterResult::Intercept(Some(Action::Quit));
                    }
                    let sym = normalize_keysym(sym);
                    let pressed = Mods::from_state(mods);
                    let mod_key = state.mod_key();
                    match state.config.binds.iter().find(|b| b.matches(pressed, sym, mod_key)) {
                        Some(bind) => {
                            state.suppressed_keys.push(code);
                            FilterResult::Intercept(Some(bind.action.clone()))
                        }
                        None => FilterResult::Forward,
                    }
                });

        if let Some(Some(action)) = action {
            self.run_action(action);
        }
    }

    fn on_pointer_moved(
        &mut self,
        pos: Point<f64, Logical>,
        time: u32,
        relative: Option<Relative>,
    ) {
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        let serial = SERIAL_COUNTER.next_serial();
        // The hardware backend draws the pointer, so moving it is a visible change.
        self.needs_redraw |= self.draws_cursor();

        if self.config.general.focus_follows_mouse
            && !pointer.is_grabbed()
            && self.layer_under(pos).is_none()
            && let Some(window) = self.space.element_under(pos).map(|(w, _)| w.clone())
            && self.ws().focus.as_ref() != Some(&window)
            && self.focused_layer.is_none()
        {
            self.focus_window(&window);
        }

        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        if let Some((delta, delta_unaccel, utime)) = relative {
            pointer.relative_motion(
                self,
                under,
                &RelativeMotionEvent {
                    delta,
                    delta_unaccel,
                    utime,
                },
            );
        }
        pointer.frame(self);
    }

    fn on_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();
        let pos = pointer.current_location();

        if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
            let mods = self.keyboard().modifier_state();
            let window = match self.layer_under(pos) {
                Some(_) => None,
                None => self.space.element_under(pos).map(|(w, _)| w.clone()),
            };

            // Mod+drag: move with left button, resize with right.
            if let Some(w) = window.clone().filter(|_| self.mod_held(&mods))
                && (button == BTN_LEFT || button == BTN_RIGHT)
            {
                let start = GrabStartData {
                    focus: None,
                    button,
                    location: pos,
                };
                if button == BTN_LEFT {
                    grabs::begin_move(self, &self.seat.clone(), w, start, serial);
                } else {
                    grabs::begin_resize(self, &self.seat.clone(), w, start, serial);
                }
                return;
            }

            match (window, self.layer_under(pos)) {
                (Some(w), _) => {
                    self.focused_layer = None;
                    self.focus_window(&w);
                }
                (None, Some(layer)) => self.focus_layer(&layer),
                (None, None) => {}
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn on_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let source = event.source();
        let amount = |axis| {
            event
                .amount(axis)
                .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0)
        };
        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let value = amount(axis);
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                frame = frame.stop(axis);
            }
        }
        let pointer = self.seat.get_pointer().unwrap();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    fn mod_held(&self, mods: &ModifiersState) -> bool {
        match self.mod_key() {
            ModKey::Alt => mods.alt,
            ModKey::Ctrl => mods.ctrl,
            ModKey::Super | ModKey::Auto => mods.logo,
        }
    }

    fn clamp_to_output(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        // Bounding box of all monitors, so the pointer can cross between them.
        let Some(geo) = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .reduce(|a, b| a.merge(b))
        else {
            return pos;
        };
        let max_x = (geo.loc.x + geo.size.w - 1) as f64;
        let max_y = (geo.loc.y + geo.size.h - 1) as f64;
        (
            pos.x.clamp(geo.loc.x as f64, max_x),
            pos.y.clamp(geo.loc.y as f64, max_y),
        )
            .into()
    }
}
