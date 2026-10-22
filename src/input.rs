use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent, GestureEndEvent,
        GesturePinchUpdateEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GrabStartData, MotionEvent, RelativeMotionEvent,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use smithay::input::pointer as gestures;

use crate::{
    bar::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT},
    config::{Action, ModKey, Mods, VolumeChange, normalize_keysym},
    grabs,
    handlers::pointer_lock::Constraint,
    menu::MenuKey,
    state::State,
};

/// How far (in touchpad units, about a pixel each) a swipe has to go to do something.
const SWIPE_DISTANCE: f64 = 70.0;

/// A touchpad swipe in progress.
pub struct Swipe {
    /// Three or more fingers with gestures on: the compositor's. Otherwise it goes to the app.
    ours: bool,
    delta: Point<f64, Logical>,
}

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
                let relative = (delta, event.delta_unaccel(), event.time());
                let pos = self.clamp_to_output(self.seat.get_pointer().unwrap().current_location() + delta);
                match self.pointer_constraint() {
                    // The app gets the movement, but the cursor stays put.
                    Constraint::Locked => return self.send_relative_only(relative),
                    Constraint::Confined {
                        surface,
                        origin,
                        region,
                    } => {
                        let stays = self.surface_under(pos).is_some_and(|(s, _)| s == surface)
                            && region.is_none_or(|r| r.contains((pos - origin).to_i32_round()));
                        if !stays {
                            return self.send_relative_only(relative);
                        }
                    }
                    Constraint::None => {}
                }
                self.on_pointer_moved(pos, event.time_msec(), Some(relative));
                self.maybe_activate_constraint();
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
                // No relative motion to give a locked app, so just hold still.
                if matches!(self.pointer_constraint(), Constraint::Locked) {
                    return;
                }
                self.on_pointer_moved(pos, event.time_msec(), None);
                self.maybe_activate_constraint();
            }
            InputEvent::PointerButton { event } => self.on_button::<I>(event),
            InputEvent::PointerAxis { event } => self.on_axis::<I>(event),
            InputEvent::GestureSwipeBegin { event } => self.on_swipe_begin(event.fingers(), event.time_msec()),
            InputEvent::GestureSwipeUpdate { event } => self.on_swipe_update(event.delta(), event.time_msec()),
            InputEvent::GestureSwipeEnd { event } => self.on_swipe_end(event.cancelled(), event.time_msec()),
            InputEvent::GesturePinchBegin { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_pinch_begin(
                    self,
                    &gestures::GesturePinchBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        fingers: event.fingers(),
                    },
                );
            }
            InputEvent::GesturePinchUpdate { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_pinch_update(
                    self,
                    &gestures::GesturePinchUpdateEvent {
                        time: event.time_msec(),
                        delta: event.delta(),
                        scale: event.scale(),
                        rotation: event.rotation(),
                    },
                );
            }
            InputEvent::GesturePinchEnd { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_pinch_end(
                    self,
                    &gestures::GesturePinchEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        cancelled: event.cancelled(),
                    },
                );
            }
            InputEvent::GestureHoldBegin { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_hold_begin(
                    self,
                    &gestures::GestureHoldBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        fingers: event.fingers(),
                    },
                );
            }
            InputEvent::GestureHoldEnd { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_hold_end(
                    self,
                    &gestures::GestureHoldEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        cancelled: event.cancelled(),
                    },
                );
            }
            _ => {}
        }
    }

    fn on_key<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        self.on_key_raw(event.key_code(), event.state(), Event::time_msec(&event));
    }

    /// A key went down or up (also used by `msg test-key`).
    pub fn on_key_raw(&mut self, code: smithay::input::keyboard::Keycode, key_state: KeyState, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();

        let mut mod_tapped = false;
        let mut menu_key = None;
        let mut switcher_key = None;
        let mut overview_key = None;
        let mut volume_repeat = None;
        let action =
            self.keyboard()
                .input::<Option<Action>, _>(self, code, key_state, serial, time, |state, mods, handle| {
                    // Swallow the release of any key whose press triggered a bind.
                    if key_state == KeyState::Released {
                        state.stop_volume_repeat_for(Some(code));
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
                    // Any other press cancels a pending Mod tap; a lone Mod press starts one,
                    // and stops a held volume key repeating.
                    state.mod_tap = None;
                    state.stop_volume_repeat_for(None);
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
                    // The overview takes plain keys it knows; Mod+Tab and the like still work.
                    if state.overview_holds_keyboard()
                        && !(mods.ctrl || mods.alt || mods.logo)
                        && let Some(k) = menu_key_for(raw)
                    {
                        state.suppressed_keys.push(code);
                        overview_key = Some(k);
                        return FilterResult::Intercept(None);
                    }
                    // The switcher takes its keys whatever modifiers are held (Alt is, usually).
                    if state.switcher.is_some()
                        && let Some(k) = menu_key_for(raw)
                    {
                        state.suppressed_keys.push(code);
                        // Shift+Tab goes back up.
                        let k = if raw == smithay::input::keyboard::keysyms::KEY_Tab && mods.shift {
                            MenuKey::Up
                        } else {
                            k
                        };
                        switcher_key = Some(k);
                        return FilterResult::Intercept(None);
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
                    let pressed = crate::config::mods_from_state(mods);
                    let mod_key = state.mod_key();
                    if is_mod_sym(sym, mod_key) && pressed == Mods::only_mod(mod_key) {
                        state.mod_tap = Some(code);
                        return FilterResult::Forward;
                    }
                    match state.config.binds.iter().find(|b| b.matches(pressed, sym, mod_key)) {
                        Some(bind) => {
                            state.suppressed_keys.push(code);
                            if let Action::Volume(change @ VolumeChange::Step(_)) = bind.action {
                                volume_repeat = Some(change);
                            }
                            FilterResult::Intercept(Some(bind.action.clone()))
                        }
                        None => FilterResult::Forward,
                    }
                });

        if let Some(Some(action)) = action {
            self.run_action(action);
        }
        if let Some(change) = volume_repeat {
            self.start_volume_repeat(code, change);
        }
        if let Some(k) = menu_key {
            self.menu_key(k);
        }
        if let Some(k) = switcher_key {
            self.switcher_key(k);
        }
        if let Some(k) = overview_key {
            self.overview_key(k);
        }
        // Letting go of Alt (or whatever opened it) switches.
        self.switcher_check_release();
        if mod_tapped && let Some(bind) = self.config.binds.iter().find(|b| b.key.is_none()) {
            self.run_action(bind.action.clone());
        }
    }

    pub fn on_pointer_moved(&mut self, pos: Point<f64, Logical>, time: u32, relative: Option<Relative>) {
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        let serial = SERIAL_COUNTER.next_serial();
        // The hardware backend draws the pointer, so moving it is a visible change. So does a
        // drag icon, which we draw on either backend.
        self.needs_redraw |= self.draws_cursor() || self.dnd_icon.is_some();
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
        if self.overview_open() {
            self.overview_motion(pos);
            self.cursor_status = CursorImageStatus::default_named();
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: pos,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            return;
        }
        self.bar_drag_motion(pos);
        self.bar_hover(pos);
        self.notification_motion(pos);
        self.menu_motion(pos);
        self.switcher_motion(pos);
        if (self.over_bar(pos) || self.over_menu(pos) || self.over_switcher(pos) || self.over_notification(pos))
            && !pointer.is_grabbed()
        {
            // Clients don't reset the pointer when it leaves them, so show the normal arrow here.
            self.cursor_status = CursorImageStatus::default_named();
        }
        if self.bar.drag.is_some_and(|d| d.moved) {
            self.cursor_status = CursorImageStatus::Named(CursorIcon::Grabbing);
        }

        // While a menu is open, hovering doesn't move focus away from what it's for.
        let follow = self.config.general.focus_follows_mouse
            && self.menu.is_none()
            && self.switcher.is_none()
            && !self.launcher_focused();
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

    /// Movement for an app that has locked or confined the pointer, without moving it.
    fn send_relative_only(&mut self, (delta, delta_unaccel, utime): Relative) {
        let pointer = self.seat.get_pointer().unwrap();
        let focus = self.surface_under(pointer.current_location());
        pointer.relative_motion(
            self,
            focus,
            &RelativeMotionEvent {
                delta,
                delta_unaccel,
                utime,
            },
        );
        pointer.frame(self);
    }

    fn on_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        self.on_button_raw(event.button_code(), event.state(), event.time_msec());
    }

    /// A pointer button went down or up (also used by `msg test-press`).
    pub fn on_button_raw(&mut self, button: u32, button_state: ButtonState, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
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
                    time,
                },
            );
            pointer.frame(self);
            return;
        }

        // The overview gets every click while it's open.
        if self.overview_open() && !pointer.is_grabbed() {
            match button_state {
                ButtonState::Pressed => self.overview_press(pos, button),
                ButtonState::Released => self.overview_release(pos),
            }
            return;
        }

        // While the switcher is open, a click picks a row or cancels; nothing else gets it.
        if self.switcher.is_some() && !pointer.is_grabbed() && button_state == ButtonState::Pressed {
            self.bar.held_buttons.push(button);
            self.switcher_press(pos);
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

        // Notification pop-ups take clicks on them, press and release.
        if button_state == ButtonState::Pressed && !pointer.is_grabbed() && self.notification_press(pos, button) {
            self.bar.held_buttons.push(button);
            return;
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
            if button == BTN_LEFT {
                self.bar.drag = None;
            }
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

            // Mod+middle-click: float or re-tile. Its release is swallowed too.
            if let Some(w) = window.clone().filter(|_| self.mod_held(&mods))
                && button == BTN_MIDDLE
            {
                self.bar.held_buttons.push(button);
                self.focus_window(&w);
                self.run_action(Action::ToggleFloating);
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
                time,
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
        if self.overview_open() {
            return;
        }
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

    // ---------- touchpad swipes ----------

    fn on_swipe_begin(&mut self, fingers: u32, time: u32) {
        let ours = fingers >= 3 && self.config.input.gestures && !self.is_locked();
        self.swipe = Some(Swipe {
            ours,
            delta: (0.0, 0.0).into(),
        });
        if !ours {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.gesture_swipe_begin(
                self,
                &gestures::GestureSwipeBeginEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                    fingers,
                },
            );
        }
    }

    fn on_swipe_update(&mut self, delta: Point<f64, Logical>, time: u32) {
        let Some(swipe) = &mut self.swipe else { return };
        swipe.delta += delta;
        if !swipe.ours {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.gesture_swipe_update(self, &gestures::GestureSwipeUpdateEvent { time, delta });
        }
    }

    /// The fingers lifted: a long enough swipe, mostly sideways or mostly up or down, acts.
    fn on_swipe_end(&mut self, cancelled: bool, time: u32) {
        let Some(swipe) = self.swipe.take() else { return };
        if !swipe.ours {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.gesture_swipe_end(
                self,
                &gestures::GestureSwipeEndEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                    cancelled,
                },
            );
            return;
        }
        if cancelled {
            return;
        }
        match swipe_action(swipe.delta) {
            Some(SwipeAction::Workspace(step)) => {
                if self.overview_open() {
                    return;
                }
                let Some(m) = self.monitors.get(self.active_mon) else { return };
                let target = m.active_ws as i32 + step;
                if (0..m.workspaces.len() as i32).contains(&target) {
                    self.switch_workspace(target as usize);
                }
            }
            Some(SwipeAction::Overview(open)) => {
                if open != self.overview_holds_keyboard() {
                    self.toggle_overview();
                }
            }
            None => {}
        }
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

#[derive(Debug, PartialEq)]
enum SwipeAction {
    /// +1: the next workspace (fingers moved left, like pushing the current one away).
    Workspace(i32),
    /// true: open (fingers moved up).
    Overview(bool),
}

/// What a finished swipe of `delta` does: nothing unless it went far enough, mostly in one
/// direction.
fn swipe_action(delta: Point<f64, Logical>) -> Option<SwipeAction> {
    let (ax, ay) = (delta.x.abs(), delta.y.abs());
    if ax >= SWIPE_DISTANCE && ax > ay * 1.5 {
        Some(SwipeAction::Workspace(if delta.x < 0.0 { 1 } else { -1 }))
    } else if ay >= SWIPE_DISTANCE && ay > ax * 1.5 {
        Some(SwipeAction::Overview(delta.y < 0.0))
    } else {
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swipes() {
        let d = |x, y| Point::<f64, Logical>::from((x, y));
        assert_eq!(swipe_action(d(-120.0, 10.0)), Some(SwipeAction::Workspace(1)));
        assert_eq!(swipe_action(d(120.0, -10.0)), Some(SwipeAction::Workspace(-1)));
        assert_eq!(swipe_action(d(5.0, -100.0)), Some(SwipeAction::Overview(true)));
        assert_eq!(swipe_action(d(0.0, 100.0)), Some(SwipeAction::Overview(false)));
        // Too short, or diagonal.
        assert_eq!(swipe_action(d(30.0, 0.0)), None);
        assert_eq!(swipe_action(d(100.0, 90.0)), None);
    }
}
