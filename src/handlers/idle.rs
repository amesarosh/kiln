//! Idle reporting (ext-idle-notify-v1, used by swayidle) and idle inhibit (used by video
//! players to keep the screen on).

use smithay::{
    delegate_idle_inhibit, delegate_idle_notify,
    desktop::{WindowSurfaceType, layer_map_for_output},
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::{
        compositor::get_parent,
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
    },
};

use smithay::wayland::seat::WaylandFocus;

use crate::state::State;

impl State {
    /// Idle is held off while a surface asking for it is on screen. Hidden windows (other
    /// workspaces, or anything behind the lock screen) don't count.
    pub fn update_idle_inhibit(&mut self) {
        self.idle_inhibitors.retain(|s| s.is_alive());
        let inhibited = !self.is_locked() && self.idle_inhibitors.iter().any(|s| self.surface_visible(s));
        if inhibited != self.idle_notifier.is_inhibited() {
            tracing::debug!("idle inhibited: {inhibited}");
            self.idle_notifier.set_is_inhibited(inhibited);
        }
    }

    fn surface_visible(&self, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.space
            .elements()
            .any(|w| w.wl_surface().is_some_and(|s| *s == root))
            || self.space.outputs().any(|o| {
                layer_map_for_output(o)
                    .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
    }
}

impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier
    }
}
delegate_idle_notify!(State);

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.push(surface);
        self.update_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.retain(|s| s != &surface);
        self.update_idle_inhibit();
    }
}
delegate_idle_inhibit!(State);
