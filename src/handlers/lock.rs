//! Screen locking (ext-session-lock-v1), used by lockers like swaylock.
//!
//! While locked, only lock surfaces are drawn and get input. If the locker crashes, the screen
//! stays locked (and turns red) until a new locker takes over, e.g. one started from another VT
//! with `WAYLAND_DISPLAY=wayland-1 swaylock`.

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::{
            ext_session_lock_manager_v1::ExtSessionLockManagerV1,
            ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
            ext_session_lock_v1::{self, ExtSessionLockV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, Resource, delegate_dispatch, delegate_global_dispatch,
            protocol::wl_output::WlOutput,
        },
    },
    utils::SERIAL_COUNTER,
    wayland::session_lock::{
        ExtLockSurfaceUserData, LockSurface, SessionLockHandler, SessionLockManagerGlobalData, SessionLockManagerState,
        SessionLockState, SessionLocker,
    },
};

use crate::state::State;

#[derive(Default)]
pub enum LockState {
    #[default]
    Unlocked,
    /// A locker asked to lock. Nothing but lock surfaces is drawn already; the locker is told
    /// the session is locked once every monitor in `waiting` has shown such a frame.
    Locking {
        locker: SessionLocker,
        waiting: Vec<Output>,
    },
    Locked {
        lock: ExtSessionLockV1,
    },
}

impl LockState {
    /// The lock object of the locker currently in charge.
    fn lock(&self) -> Option<&ExtSessionLockV1> {
        match self {
            LockState::Unlocked => None,
            LockState::Locking { locker, .. } => Some(locker.ext_session_lock()),
            LockState::Locked { lock } => Some(lock),
        }
    }
}

impl State {
    pub fn is_locked(&self) -> bool {
        !matches!(self.lock, LockState::Unlocked)
    }

    /// Locked, but the locker is gone, so nothing can unlock until a new one connects.
    pub fn locker_dead(&self) -> bool {
        self.lock.lock().is_some_and(|l| !l.is_alive())
    }

    pub fn lock_surface(&self, output: &Output) -> Option<&LockSurface> {
        self.lock_surfaces
            .iter()
            .find(|(o, s)| o == output && s.alive())
            .map(|(_, s)| s)
    }

    /// A monitor showed a frame rendered while locked.
    pub fn lock_frame_shown(&mut self, output: &Output) {
        if let LockState::Locking { waiting, .. } = &mut self.lock {
            waiting.retain(|o| o != output);
        }
        self.confirm_lock_if_ready();
    }

    /// Tell the locker the session is locked once no monitor can still show old content.
    pub fn confirm_lock_if_ready(&mut self) {
        let LockState::Locking { waiting, .. } = &mut self.lock else {
            return;
        };
        // Monitors that are gone or turned off show nothing, so there's nothing to wait for.
        waiting.retain(|o| self.monitors.iter().any(|m| &m.output == o) && !self.powered_off.contains(o));
        if !waiting.is_empty() && self.session_active() {
            return;
        }
        let LockState::Locking { locker, .. } = std::mem::take(&mut self.lock) else {
            unreachable!()
        };
        let lock = locker.ext_session_lock().clone();
        locker.lock();
        self.lock = LockState::Locked { lock };
        tracing::info!("session locked");
        self.locked_for_suspend();
    }

    /// Monitors changed size or scale: lock surfaces must match their monitor exactly.
    pub fn configure_lock_surfaces(&mut self) {
        self.lock_surfaces.retain(|(_, s)| s.alive());
        for (output, surface) in &self.lock_surfaces {
            if let Some(geo) = self.space.output_geometry(output) {
                surface.with_pending_state(|s| s.size = Some((geo.size.w as u32, geo.size.h as u32).into()));
                surface.send_configure();
            }
        }
    }

    /// Locking or unlocking changes what is under the pointer and who has the keyboard.
    fn lock_changed(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            let time = self.start_time.elapsed().as_millis() as u32;
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
        }
        self.focused_layer = None;
        self.refresh_pointer_focus();
        self.refocus();
        self.update_idle_inhibit();
        self.needs_redraw = true;
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, locker: SessionLocker) {
        if self.lock.lock().is_some_and(|l| l.is_alive()) {
            // Another locker is in charge; dropping `locker` tells this one it failed.
            tracing::warn!("refused a second screen locker");
            return;
        }
        if self.is_locked() {
            // The previous locker died. The screen never unlocked, so take over at once.
            tracing::info!("new screen locker took over");
            let lock = locker.ext_session_lock().clone();
            locker.lock();
            self.lock = LockState::Locked { lock };
            self.lock_surfaces.retain(|(_, s)| s.alive());
            self.needs_redraw = true;
            return;
        }
        tracing::info!("locking session");
        let waiting = self.monitors.iter().map(|m| m.output.clone()).collect();
        // A menu open over the desktop mustn't survive into the lock screen.
        self.menu = None;
        self.switcher = None;
        self.lock = LockState::Locking { locker, waiting };
        self.lock_changed();
        self.confirm_lock_if_ready();
    }

    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.lock = LockState::Unlocked;
        self.lock_surfaces.clear();
        self.lock_changed();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        // smithay sends the first configure right after this, with the size set here.
        if let Some(geo) = self.space.output_geometry(&output) {
            surface.with_pending_state(|s| s.size = Some((geo.size.w as u32, geo.size.h as u32).into()));
        }
        self.lock_surfaces.retain(|(o, s)| s.alive() && o != &output);
        self.lock_surfaces.push((output, surface));
        self.refocus();
        self.needs_redraw = true;
    }
}

delegate_global_dispatch!(State: [ExtSessionLockManagerV1: SessionLockManagerGlobalData] => SessionLockManagerState);
delegate_dispatch!(State: [ExtSessionLockManagerV1: ()] => SessionLockManagerState);
delegate_dispatch!(State: [ExtSessionLockSurfaceV1: ExtLockSurfaceUserData] => SessionLockManagerState);

/// Forwards to smithay, except that only the locker in charge may unlock. smithay posts an
/// error for an unconfirmed lock's unlock request but then unlocks anyway, so a client whose
/// lock was refused could otherwise unlock the screen.
impl Dispatch<ExtSessionLockV1, SessionLockState> for State {
    fn request(
        state: &mut State,
        client: &Client,
        resource: &ExtSessionLockV1,
        request: ext_session_lock_v1::Request,
        data: &SessionLockState,
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        if matches!(request, ext_session_lock_v1::Request::UnlockAndDestroy)
            && !matches!(&state.lock, LockState::Locked { lock } if lock == resource)
        {
            resource.post_error(ext_session_lock_v1::Error::InvalidUnlock, "this lock is not active");
            return;
        }
        <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, State>>::request(
            state, client, resource, request, data, dh, data_init,
        );
    }
}
