//! What the power menu does: lock, suspend, log out, restart and shut down, and suspending
//! after `[general] suspend_after` seconds without input.
//!
//! Suspend locks the screen first and waits until the lock is in place, so the desktop isn't
//! on show for a moment when the machine wakes up. Restart and shut down save the session
//! first, so apps come back at the next login.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::{config::Action, state::State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Lock,
    Suspend,
    Logout,
    Restart,
    Shutdown,
}

/// Lockers tried in order when `[general] locker` is empty, as (program, command).
const LOCKERS: &[(&str, &str)] = &[
    ("wayland-time-lock", "wayland-time-lock"),
    ("hyprlock", "hyprlock"),
    ("swaylock", "swaylock -f"),
    ("gtklock", "gtklock -d"),
    ("waylock", "waylock"),
];

/// Shortest time between starting lockers, so one has time to connect.
const LOCKER_RESTART: Duration = Duration::from_secs(3);

/// How long Suspend waits for the locker before suspending anyway.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The configured locker, or the first one installed.
fn locker_command(configured: &str) -> Option<String> {
    if !configured.trim().is_empty() {
        return Some(configured.to_string());
    }
    let path = std::env::var_os("PATH")?;
    LOCKERS
        .iter()
        .find(|(program, _)| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .map(|(_, cmd)| cmd.to_string())
}

impl State {
    pub fn power(&mut self, action: PowerAction) {
        match action {
            PowerAction::Lock => {
                self.lock_screen();
            }
            PowerAction::Suspend => {
                if self.is_locked() || !self.lock_screen() {
                    return self.suspend_now();
                }
                self.suspend_when_locked = true;
                let result = self
                    .loop_handle
                    .insert_source(Timer::from_duration(LOCK_WAIT), |_, _, state| {
                        if std::mem::take(&mut state.suspend_when_locked) {
                            tracing::warn!("the locker didn't lock within {LOCK_WAIT:?}; suspending anyway");
                            state.suspend_now();
                        }
                        TimeoutAction::Drop
                    });
                if let Err(e) = result {
                    tracing::warn!("suspend timer: {e}");
                    self.suspend_when_locked = false;
                    self.suspend_now();
                }
            }
            PowerAction::Logout => self.run_action(Action::Quit),
            PowerAction::Restart => {
                self.save_session_now();
                self.spawn("systemctl reboot");
            }
            PowerAction::Shutdown => {
                self.save_session_now();
                self.spawn("systemctl poweroff");
            }
        }
    }

    /// Start the locker unless the screen is already locked, or start it again if it crashed
    /// while locked. False if there's no locker.
    pub fn lock_screen(&mut self) -> bool {
        if self.is_locked() && !self.locker_dead() {
            return true;
        }
        // A locker started moments ago is still connecting.
        if self.locker_started.is_some_and(|t| t.elapsed() < LOCKER_RESTART) {
            return true;
        }
        match locker_command(&self.config.general.locker) {
            Some(cmd) => {
                self.locker_started = Some(Instant::now());
                self.spawn(&cmd);
                true
            }
            None => {
                tracing::warn!("no screen locker found: install wayland-time-lock, or set [general] locker");
                false
            }
        }
    }

    fn suspend_now(&mut self) {
        self.spawn("systemctl suspend");
    }

    /// (Re)start the `suspend_after` countdown, timed from the last input like
    /// `arm_idle_power_off`. Video players and other idle inhibitors hold it off.
    pub fn arm_idle_suspend(&mut self) {
        if let Some(token) = self.idle_suspend_timer.take() {
            self.loop_handle.remove(token);
        }
        let secs = self.config.general.suspend_after;
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
            // Re-armed by the next input, so it doesn't fire again straight after waking.
            state.idle_suspend_timer = None;
            if state.nested {
                // Suspending would put the host machine to sleep.
                tracing::info!("idle for {timeout:?}: would suspend, but running nested");
            } else {
                tracing::info!("idle for {timeout:?}: suspending");
                state.power(PowerAction::Suspend);
            }
            TimeoutAction::Drop
        });
        match token {
            Ok(t) => self.idle_suspend_timer = Some(t),
            Err(e) => tracing::warn!("idle suspend disabled: {e}"),
        }
    }

    /// The screen just locked: suspend if that's what the lock was for.
    pub fn locked_for_suspend(&mut self) {
        if std::mem::take(&mut self.suspend_when_locked) {
            self.suspend_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_locker_wins() {
        assert_eq!(locker_command("mylock --now").as_deref(), Some("mylock --now"));
    }
}
