use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{
    bar::{BTN_LEFT, BTN_RIGHT},
    config::{Action, ModKey, Mods, normalize_keysym},
    grabs,
    menu::MenuKey,
    state::State,
};

/// Relative motion details: (accelerated delta, raw delta, timestamp in µs).
type Relative = (Point<f64, Logical>, Point<f64, Logical>, u64);

const XF86_SWITCH_VT_1: u32 = 0x1008_FE01;
const XF86_SWITCH_VT_12: u32 = 0x1008_FE0C;
const XKB_BACKSPACE: u32 = 0xFF08;
const XKB_TERMINATE_SERVER: u32 = 0xFED5;

/// What a key does while a menu is open.
fn menu_key_for(raw: u32) -> Option<MenuKey> {
    use smithay::input::keyboard::keysyms as k;
    Some(match raw {
        k::KEY_Escape => MenuKey::Close,
        k::KEY_Up | k::KEY_ISO_Left_Tab => MenuKey::Up,
        k::KEY_Down | k::KEY_Tab => MenuKey::Down,
        k::KEY_Left => MenuKey::Left,
        k::KEY_Right => MenuKey::Right,
        k::KEY_Return | k::KEY_KP_Enter | k::KEY_space => MenuKey::Pick,
        k::KEY_1..=k::KEY_9 => MenuKey::Workspace((raw - k::KEY_1) as usize),
        _ => return None,
    })
}

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Any input resets the idle timers (swayidle and friends).
        let seat = self.seat.clone();
        self.idle_notifier.notify_activity(&seat);
        // Deliberate input turns monitors back on. Key and button releases don't count, or
        // letting go of the keys of a power-off bind would wake them straight away.
        let wakes = match &event {
            InputEvent::Keyboard { event } => event.state() == KeyState::Pressed,
            InputEvent::PointerButton { event } => event.state() == ButtonState::Pressed,
            InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. }
            | InputEvent::PointerAxis { .. } => true,
            _ => false,
        };
        self.note_activity(wakes);
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
                // Tablets and the nested window span all monitors.
                let Some(geo) = self
                    .space
                    .outputs()
                    .filter_map(|o| self.space.output_geometry(o))
                    .reduce(|a, b| a.merge(b))
                else {
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

        let mut mod_tapped = false;
        let mut menu_key = None;
        let action =
            self.keyboard()
                .input::<Option<Action>, _>(self, code, key_state, serial, time, |state, mods, handle| {
                    // Swallow the release of any key whose press triggered a bind.
                    if key_state == KeyState::Released {
                        // Still forwarded, so clients don't think Mod is stuck down.
                        if state.mod_tap == Some(code) {
                            state.mod_tap = None;
                            mod_tapped = true;
                        }
                        if let Some(i) = state.suppressed_keys.iter().position(|k| *k == code) {
                            state.suppressed_keys.swap_remove(i);
                            return FilterResult::Intercept(None);
                        }
                        return FilterResult::Forward;
                    }
                    // Any other press cancels a pending Mod tap; a lone Mod press starts one.
                    state.mod_tap = None;
                    // Ctrl+Alt+Fn produces XF86Switch_VT_n; always handled so you can't get stuck.
                    let modified = handle.modified_sym().raw();
                    if (XF86_SWITCH_VT_1..=XF86_SWITCH_VT_12).contains(&modified) {
                        state.suppressed_keys.push(code);
                        let vt = (modified - XF86_SWITCH_VT_1 + 1) as i32;
                        return FilterResult::Intercept(Some(Action::SwitchVt(vt)));
                    }
                    // Locked: every key goes to the locker. No binds, not even quit, which
                    // would drop back to the shell that started us.
                    if state.is_locked() {
                        return FilterResult::Forward;
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
                    // An open menu takes the keys it knows; the rest work as usual.
                    if state.menu.is_some()
                        && !(mods.ctrl || mods.alt || mods.logo)
                        && let Some(k) = menu_key_for(raw)
                    {
                        state.suppressed_keys.push(code);
                        menu_key = Some(k);
                        return FilterResult::Intercept(None);
                    }
                    let sym = normalize_keysym(sym);
                    let pressed = Mods::from_state(mods);
                    let mod_key = state.mod_key();
                    if is_mod_sym(sym, mod_key) && pressed == Mods::only_mod(mod_key) {
                        state.mod_tap = Some(code);
                        return FilterResult::Forward;
                    }
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
        if let Some(k) = menu_key {
            self.menu_key(k);
        }
        if mod_tapped && let Some(bind) = self.config.binds.iter().find(|b| b.key.is_none()) {
            self.run_action(bind.action.clone());
        }
    }

    fn on_pointer_moved(&mut self, pos: Point<f64, Logical>, time: u32, relative: Option<Relative>) {
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        let serial = SERIAL_COUNTER.next_serial();
        // The hardware backend draws the pointer, so moving it is a visible change.
        self.needs_redraw |= self.draws_cursor();
        if self.is_locked() {
            pointer.motion(
                self,
                under,
                &MotionEvent {
                    location: pos,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            return;
        }
        self.bar_hover(pos);
        self.menu_motion(pos);
        if (self.over_bar(pos) || self.over_menu(pos)) && !pointer.is_grabbed() {
            // Clients don't reset the pointer when it leaves them, so show the normal arrow here.
            self.cursor_status = CursorImageStatus::default_named();
        }

        // While a menu is open, hovering doesn't move focus away from what it's for.
        let follow = self.config.general.focus_follows_mouse && self.menu.is_none();
        if follow && !pointer.is_grabbed() && self.focused_layer.is_none() {
            self.focus_monitor_at(pos);
        }
        if follow
            && !pointer.is_grabbed()
            && !self.over_bar(pos)
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
        // Mod+click (move/resize) shouldn't count as a Mod tap.
        if button_state == ButtonState::Pressed {
            self.mod_tap = None;
        }

        if self.is_locked() {
            // Clicking a monitor's lock surface gives it the keyboard.
            if button_state == ButtonState::Pressed
                && let Some(mon) = self.monitor_at(pos)
            {
                self.active_mon = mon;
                let output = self.monitors[mon].output.clone();
                if let Some(surface) = self.lock_surface(&output).map(|s| s.wl_surface().clone()) {
                    self.keyboard().set_focus(self, Some(surface.into()), serial);
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
            return;
        }

        // An open menu sees clicks first. A press it uses (or one that just closes it) goes no
        // further, and neither does its release.
        if self.menu.is_some() && !pointer.is_grabbed() {
            match button_state {
                ButtonState::Pressed => {
                    if self.menu_press(pos) {
                        self.bar.held_buttons.push(button);
                        return;
                    }
                }
                ButtonState::Released => self.menu_release(pos),
            }
        }

        // Bar clicks are handled here and never reach clients, press or release.
        if button_state == ButtonState::Pressed && !pointer.is_grabbed() && self.over_bar(pos) {
            self.bar.held_buttons.push(button);
            if let Some((mon, part)) = self.bar_part_at(pos) {
                self.bar_click(mon, part, button);
            }
            return;
        }
        if button_state == ButtonState::Released
            && let Some(i) = self.bar.held_buttons.iter().position(|b| *b == button)
        {
            self.bar.held_buttons.swap_remove(i);
            return;
        }

        if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
            // Clicking anywhere on a monitor focuses it, even empty desktop.
            self.focus_monitor_at(pos);
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
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        if !pointer.is_grabbed() && !self.is_locked() {
            let vertical = amount(Axis::Vertical);
            if vertical != 0.0
                && self.bar_scroll(pos, source, Axis::Vertical, vertical, event.amount_v120(Axis::Vertical))
            {
                return;
            }
        }
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

    /// Keep the pointer on a monitor. Monitors of different heights leave gaps in their
    /// bounding box, so clamp to the nearest monitor rather than the box.
    fn clamp_to_output(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let rects: Vec<_> = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .collect();
        rects
            .iter()
            .map(|geo| clamp_into(pos, geo.to_f64()))
            .min_by(|a, b| {
                let d = |p: &Point<f64, Logical>| (p.x - pos.x).powi(2) + (p.y - pos.y).powi(2);
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(pos)
    }
}

/// Whether `sym` is one of the physical keys for `mod_key` (left or right).
fn is_mod_sym(sym: Keysym, mod_key: ModKey) -> bool {
    match mod_key {
        ModKey::Super | ModKey::Auto => matches!(sym, Keysym::Super_L | Keysym::Super_R),
        ModKey::Alt => matches!(sym, Keysym::Alt_L | Keysym::Alt_R | Keysym::Meta_L | Keysym::Meta_R),
        ModKey::Ctrl => matches!(sym, Keysym::Control_L | Keysym::Control_R),
    }
}

fn clamp_into(pos: Point<f64, Logical>, geo: smithay::utils::Rectangle<f64, Logical>) -> Point<f64, Logical> {
    let max_x = geo.loc.x + geo.size.w - 1.0;
    let max_y = geo.loc.y + geo.size.h - 1.0;
    (pos.x.clamp(geo.loc.x, max_x), pos.y.clamp(geo.loc.y, max_y)).into()
}
