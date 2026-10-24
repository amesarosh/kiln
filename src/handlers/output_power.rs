//! Turning monitors off and on: wlr-output-power-management (used by `wlopm` and swayidle
//! setups) and the `power-off-monitors` action. Any key press, click or pointer movement turns
//! them back on.

use std::time::{Duration, Instant};

use smithay::{
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_protocols_wlr::output_power_management::v1::server::{
            zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
            zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, backend::ClientId,
        },
    },
};

use crate::state::State;

impl State {
    pub fn create_output_power_global(dh: &DisplayHandle) {
        dh.create_global::<State, ZwlrOutputPowerManagerV1, _>(1, ());
    }

    pub fn monitor_powered(&self, output: &Output) -> bool {
        !self.powered_off.contains(output)
    }

    pub fn any_monitor_off(&self) -> bool {
        !self.powered_off.is_empty()
    }

    pub fn set_monitor_power(&mut self, output: &Output, on: bool) {
        if self.monitor_powered(output) == on || self.monitor_index(output).is_none() {
            return;
        }
        tracing::info!("monitor {} {}", output.name(), if on { "on" } else { "off" });
        if on {
            self.powered_off.retain(|o| o != output);
        } else {
            self.powered_off.push(output.clone());
        }
        self.backend_set_power(output, on);
        let mode = if on { Mode::On } else { Mode::Off };
        self.output_powers.retain(|(p, _)| p.is_alive());
        for (power, o) in &self.output_powers {
            if o == output {
                power.mode(mode);
            }
        }
        // A lock waiting for this monitor to show a locked frame needn't wait: it's dark.
        self.confirm_lock_if_ready();
        self.needs_redraw = true;
        if on {
            // Whatever turned it on counts as activity; the countdown starts over.
            self.last_activity = Instant::now();
            if self.idle_power_timer.is_none() {
                self.arm_idle_power_off();
            }
        }
    }

    /// Turn every monitor off because of idle or the `power-off-monitors` action, locking
    /// first if `lock_when_monitors_off` is set.
    pub fn blank_monitors(&mut self) {
        if self.config.general.lock_when_monitors_off {
            self.lock_screen();
        }
        self.set_all_monitors_power(false);
    }

    pub fn set_all_monitors_power(&mut self, on: bool) {
        let outputs: Vec<Output> = self.monitors.iter().map(|m| m.output.clone()).collect();
        for o in &outputs {
            self.set_monitor_power(o, on);
        }
    }

    /// Input happened: monitors wake up, and the idle power-off countdown restarts.
    pub fn note_activity(&mut self, wakes: bool) {
        self.last_activity = Instant::now();
        // The suspend countdown stops once it fires; input after waking starts it again.
        if self.idle_suspend_timer.is_none() && self.config.general.suspend_after > 0 {
            self.arm_idle_suspend();
        }
        if wakes && self.any_monitor_off() {
            self.set_all_monitors_power(true);
        }
    }

    /// (Re)start the `monitors_off_after` countdown. The timer doesn't move on every input
    /// event: when it fires it checks how long ago the last input was and sleeps the rest.
    pub fn arm_idle_power_off(&mut self) {
        if let Some(token) = self.idle_power_timer.take() {
            self.loop_handle.remove(token);
        }
        let secs = self.config.general.monitors_off_after;
        if secs == 0 {
            return;
        }
        let timeout = Duration::from_secs(secs);
        let timer = Timer::from_deadline(self.last_activity + timeout);
        let token = self.loop_handle.insert_source(timer, move |_, _, state| {
            let deadline = state.last_activity + timeout;
            if Instant::now() < deadline {
                return TimeoutAction::ToInstant(deadline);
            }
            if state.idle_notifier.is_inhibited() {
                return TimeoutAction::ToDuration(timeout);
            }
            state.blank_monitors();
            // Re-armed when input wakes the monitors.
            state.idle_power_timer = None;
            TimeoutAction::Drop
        });
        match token {
            Ok(t) => self.idle_power_timer = Some(t),
            Err(e) => tracing::warn!("idle power-off disabled: {e}"),
        }
    }

    /// A monitor went away: its power controls are no longer valid.
    pub fn forget_monitor_power(&mut self, output: &Output) {
        self.powered_off.retain(|o| o != output);
        self.output_powers.retain(|(power, o)| {
            if o == output {
                power.failed();
                false
            } else {
                true
            }
        });
    }
}

impl GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        if let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request {
            let power = data_init.init(id, ());
            match Output::from_resource(&output).filter(|o| state.monitor_index(o).is_some()) {
                Some(output) => {
                    power.mode(if state.monitor_powered(&output) {
                        Mode::On
                    } else {
                        Mode::Off
                    });
                    state.output_powers.push((power, output));
                }
                None => power.failed(),
            }
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        if let zwlr_output_power_v1::Request::SetMode { mode } = request {
            let on = match mode {
                WEnum::Value(Mode::On) => true,
                WEnum::Value(Mode::Off) => false,
                _ => {
                    resource.post_error(zwlr_output_power_v1::Error::InvalidMode, "unknown power mode");
                    return;
                }
            };
            let Some(output) = state
                .output_powers
                .iter()
                .find(|(p, _)| p == resource)
                .map(|(_, o)| o.clone())
            else {
                return;
            };
            state.set_monitor_power(&output, on);
        }
    }

    fn destroyed(state: &mut State, _client: ClientId, resource: &ZwlrOutputPowerV1, _data: &()) {
        state.output_powers.retain(|(p, _)| p != resource);
    }
}
