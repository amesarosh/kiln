//! What can hold keyboard focus: a Wayland surface, or an X11 window.
//!
//! X11 windows need their own variant because Xwayland only routes keys to the window the X
//! server thinks is focused; smithay's `X11Surface` tells it (via the window manager connection)
//! as well as sending the usual Wayland keyboard enter on the window's surface.

use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::PopupKind,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::state::State;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Surface(WlSurface),
    /// An X11 window and its Wayland surface. Only built once Xwayland has linked the two.
    X11(Box<X11Surface>, WlSurface),
}

impl KeyboardFocusTarget {
    /// Focus for a window's main surface.
    pub fn for_window(w: &smithay::desktop::Window) -> Option<Self> {
        match w.x11_surface() {
            Some(x) => x.wl_surface().map(|s| Self::X11(Box::new(x.clone()), s)),
            None => w.toplevel().map(|t| Self::Surface(t.wl_surface().clone())),
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(s: WlSurface) -> Self {
        Self::Surface(s)
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(p: PopupKind) -> Self {
        Self::Surface(p.wl_surface().clone())
    }
}

// Popup grabs hand keyboard focus to the pointer; the pointer only deals in Wayland surfaces.
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(f: KeyboardFocusTarget) -> Self {
        match f {
            KeyboardFocusTarget::Surface(s) | KeyboardFocusTarget::X11(_, s) => s,
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(s) => s.alive(),
            Self::X11(x, _) => x.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Surface(s) | Self::X11(_, s) => Some(Cow::Borrowed(s)),
        }
    }
}

impl KeyboardTarget<State> for KeyboardFocusTarget {
    fn enter(&self, seat: &Seat<State>, data: &mut State, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(x, _) => KeyboardTarget::enter(&**x, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(x, _) => KeyboardTarget::leave(&**x, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Surface(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(x, _) => KeyboardTarget::key(&**x, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(&self, seat: &Seat<State>, data: &mut State, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(x, _) => KeyboardTarget::modifiers(&**x, seat, data, modifiers, serial),
        }
    }
}
