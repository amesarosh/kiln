//! tearing-control: a game can say it prefers its frames on screen right away, even if that
//! tears, over waiting for the next refresh. We honour it for a fullscreen window when
//! `[general] allow_tearing` is on. The hint is double-buffered: it applies on the next commit.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::wp::tearing_control::v1::server::{
            wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
            wp_tearing_control_v1::{self, PresentationHint, WpTearingControlV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak,
            protocol::wl_surface::WlSurface,
        },
    },
    wayland::{
        compositor::{Cacheable, with_states},
        seat::WaylandFocus,
    },
};

use crate::state::State;

/// Whether the surface asked for tearing, as of its last commit.
#[derive(Debug, Clone, Copy, Default)]
pub struct TearingHint {
    pub tearing: bool,
}

impl Cacheable for TearingHint {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// One tearing-control object per surface at most.
#[derive(Default)]
struct HasTearingControl(AtomicBool);

pub struct TearingControlData(Mutex<Weak<WlSurface>>);

impl State {
    pub fn create_tearing_control_global(dh: &DisplayHandle) {
        dh.create_global::<State, WpTearingControlManagerV1, _>(1, ());
    }

    /// Whether `output` should flip without waiting for vblank: its workspace shows a fullscreen
    /// window that asked for it.
    pub fn output_wants_tearing(&self, output: &Output) -> bool {
        if !self.config.general.allow_tearing || self.is_locked() || self.overview.is_some() {
            return false;
        }
        let Some(ws) = self.monitor_index(output).map(|i| self.monitors[i].ws()) else {
            return false;
        };
        let Some(window) = ws.fullscreen.as_ref() else {
            return false;
        };
        window.wl_surface().is_some_and(|s| surface_wants_tearing(&s))
    }
}

pub fn surface_wants_tearing(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states.cached_state.get::<TearingHint>().current().tearing
    })
}

impl GlobalDispatch<WpTearingControlManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpTearingControlManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpTearingControlManagerV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        manager: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        if let wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } = request {
            let taken = with_states(&surface, |states| {
                states.data_map.insert_if_missing_threadsafe(HasTearingControl::default);
                let has = states.data_map.get::<HasTearingControl>().unwrap();
                has.0.swap(true, Ordering::AcqRel)
            });
            if taken {
                manager.post_error(
                    wp_tearing_control_manager_v1::Error::TearingControlExists,
                    "the surface already has a tearing control object",
                );
                return;
            }
            data_init.init(id, TearingControlData(Mutex::new(surface.downgrade())));
        }
    }
}

impl Dispatch<WpTearingControlV1, TearingControlData> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        data: &TearingControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        let Ok(surface) = data.0.lock().unwrap().upgrade() else {
            return;
        };
        let tearing = match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                matches!(hint, WEnum::Value(PresentationHint::Async))
            }
            // Destroying goes back to vsync, on the next commit like any other change.
            wp_tearing_control_v1::Request::Destroy => {
                with_states(&surface, |states| {
                    if let Some(has) = states.data_map.get::<HasTearingControl>() {
                        has.0.store(false, Ordering::Release);
                    }
                });
                false
            }
            _ => return,
        };
        with_states(&surface, |states| {
            states.cached_state.get::<TearingHint>().pending().tearing = tearing;
        });
    }
}
