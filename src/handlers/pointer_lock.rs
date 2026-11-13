//! Pointer lock and confinement (games, Blender, VMs), plus relative motion to go with it.
//!
//! A constraint is only active while its window has keyboard focus too, so Alt+Tab or a
//! click elsewhere always frees the mouse.

use smithay::{
    delegate_pointer_constraints, delegate_relative_pointer,
    input::pointer::PointerHandle,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
    wayland::{
        compositor::get_parent,
        pointer_constraints::{PointerConstraint, PointerConstraintsHandler, with_pointer_constraint},
        seat::WaylandFocus,
    },
};

use crate::state::State;

/// What the pointer's surface has asked for, at the pointer's current position.
pub enum Constraint {
    None,
    Locked,
    /// Stay inside this surface (whose origin is given) and, if set, its region.
    Confined {
        surface: WlSurface,
        origin: Point<f64, Logical>,
        region: Option<smithay::wayland::compositor::RegionAttributes>,
    },
}

fn root_of(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

impl State {
    /// Whether `surface` belongs to the window (or layer, or X11 window) with keyboard focus.
    fn has_keyboard_focus(&self, surface: &WlSurface) -> bool {
        self.keyboard()
            .current_focus()
            .and_then(|f| f.wl_surface().map(|s| root_of(&s)))
            .is_some_and(|focused| focused == root_of(surface))
    }

    /// The active constraint on the surface under the pointer, if it applies right here.
    pub fn pointer_constraint(&self) -> Constraint {
        let pointer = self.pointer();
        let pos = pointer.current_location();
        let Some((surface, origin)) = self.surface_under(pos) else {
            return Constraint::None;
        };
        if pointer.current_focus().as_ref() != Some(&surface) {
            return Constraint::None;
        }
        with_pointer_constraint(&surface, &pointer, |c| match c {
            Some(c) if c.is_active() => {
                // Outside its region the constraint doesn't hold.
                let inside = c.region().is_none_or(|r| r.contains((pos - origin).to_i32_round()));
                match &*c {
                    _ if !inside => Constraint::None,
                    PointerConstraint::Locked(_) => Constraint::Locked,
                    PointerConstraint::Confined(confined) => Constraint::Confined {
                        surface: surface.clone(),
                        origin,
                        region: confined.region().cloned(),
                    },
                }
            }
            _ => Constraint::None,
        })
    }

    /// Turn on a waiting constraint once the pointer is inside it and its window has focus.
    pub fn maybe_activate_constraint(&mut self) {
        let pointer = self.pointer();
        let pos = pointer.current_location();
        let Some((surface, origin)) = self.surface_under(pos) else {
            return;
        };
        if pointer.current_focus().as_ref() != Some(&surface) || !self.has_keyboard_focus(&surface) {
            return;
        }
        with_pointer_constraint(&surface, &pointer, |c| {
            if let Some(c) = c
                && !c.is_active()
                && c.region().is_none_or(|r| r.contains((pos - origin).to_i32_round()))
            {
                c.activate();
            }
        });
    }

    /// Keyboard focus moved: release the mouse if its window lost focus, or lock it again
    /// if focus came back. Runs from the main loop, not from SeatHandler::focus_changed,
    /// which smithay calls with the keyboard lock held.
    pub fn sync_constraint_focus(&mut self) {
        if !std::mem::take(&mut self.keyboard_focus_moved) {
            return;
        }
        let pointer = self.pointer();
        let Some(surface) = pointer.current_focus() else { return };
        if self.has_keyboard_focus(&surface) {
            // Focus came (back) to the window under the pointer.
            self.maybe_activate_constraint();
            return;
        }
        with_pointer_constraint(&surface, &pointer, |c| {
            if let Some(c) = c
                && c.is_active()
            {
                c.deactivate();
            }
        });
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        self.maybe_activate_constraint();
    }

    /// A locked client says where it drew the cursor (Xwayland uses this for pointer warps).
    /// Move ours there without telling clients, so the cursor shows up in the right place.
    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active = with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active()));
        if !active {
            return;
        }
        let Some((under, origin)) = self.surface_under(pointer.current_location()) else {
            return;
        };
        if under == *surface {
            pointer.set_location(origin + location);
            self.needs_redraw |= self.draws_cursor();
        }
    }
}
delegate_pointer_constraints!(State);
delegate_relative_pointer!(State);
