//! keyboard-shortcuts-inhibit: a virtual machine, remote desktop or game can ask for every key,
//! so Super+key and other binds reach it instead of us. Only honoured while that window has
//! keyboard focus. A bind to `toggle-shortcuts-inhibit` always works, to take the keys back.

use smithay::{
    delegate_keyboard_shortcuts_inhibit,
    input::Seat,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::{
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
            KeyboardShortcutsInhibitorSeat,
        },
        seat::WaylandFocus,
    },
};

use crate::state::State;

impl KeyboardShortcutsInhibitHandler for State {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        if self.config.general.allow_shortcuts_inhibit {
            tracing::debug!("an app took every key (keyboard shortcuts inhibited)");
            inhibitor.activate();
        }
    }
}
delegate_keyboard_shortcuts_inhibit!(State);

impl State {
    /// The window with keyboard focus holds every key, so binds don't apply.
    pub fn shortcuts_inhibited(&self) -> bool {
        self.focused_inhibitor().is_some_and(|i| i.is_active())
    }

    fn focused_inhibitor(&self) -> Option<KeyboardShortcutsInhibitor> {
        let surface: WlSurface = self.keyboard().current_focus()?.wl_surface()?.into_owned();
        Seat::keyboard_shortcuts_inhibitor_for_surface(&self.seat, &surface)
    }

    /// Take the keys back from the focused window, or hand them back.
    pub fn toggle_shortcuts_inhibit(&mut self) {
        let Some(inhibitor) = self.focused_inhibitor() else {
            return;
        };
        if inhibitor.is_active() {
            tracing::info!("took the keys back from the focused app");
            inhibitor.inactivate();
        } else {
            tracing::info!("handed every key back to the focused app");
            inhibitor.activate();
        }
    }
}
