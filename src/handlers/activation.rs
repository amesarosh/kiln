//! xdg-activation: lets an app bring a window forward, e.g. a browser when you click a link
//! in another app, or an already-running app when you launch it again.
//!
//! To stop background apps stealing focus, only tokens made in response to a recent click
//! or key press (or handed out by us when launching a program) can activate a window.

use std::time::Duration;

use smithay::{
    delegate_xdg_activation,
    input::Seat,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData},
};

use crate::state::State;

/// How long a token stays usable. Slow app startups (a cold browser) need a while.
const TOKEN_LIFETIME: Duration = Duration::from_secs(10);

impl State {
    /// A token for a program we're about to launch, so it can bring its window forward.
    pub fn launch_token(&mut self) -> String {
        let state = &mut self.activation_state;
        state.retain_tokens(|_, data| data.timestamp.elapsed() < TOKEN_LIFETIME);
        let (token, _) = state.create_external_token(None);
        token.as_str().to_owned()
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    /// Valid only if made for an input event on our seat no older than the keyboard's last
    /// focus change, i.e. the app had focus when the user clicked or typed.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        let Some((serial, seat)) = data.serial else {
            return false;
        };
        Seat::from_resource(&seat).as_ref() == Some(&self.seat)
            && self
                .keyboard()
                .last_enter()
                .is_some_and(|enter| serial.is_no_older_than(&enter))
    }

    fn request_activation(&mut self, token: XdgActivationToken, data: XdgActivationTokenData, surface: WlSurface) {
        // One use per token, so a stale one can't be replayed later.
        self.activation_state.remove_token(&token);
        if data.timestamp.elapsed() >= TOKEN_LIFETIME || self.is_locked() {
            return;
        }
        // Windows that haven't mapped yet get focus when they do.
        if let Some(window) = self.window_for_surface(&surface) {
            tracing::debug!("activation request granted");
            self.activate_window(&window);
        }
    }
}
delegate_xdg_activation!(State);
