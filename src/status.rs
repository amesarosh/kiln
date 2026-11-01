//! Battery, network and screen brightness for the bar, read from sysfs every few seconds.
//! Reading these small files takes microseconds, so it's done on the main loop.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::State;

const POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    /// Plugged in and full.
    pub full: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Network {
    #[default]
    Offline,
    Wired,
    /// Signal strength, 0 to 100.
    Wireless(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brightness {
    /// The backlight's name, e.g. "intel_backlight".
    pub device: String,
    pub value: u32,
    pub max: u32,
}

impl Brightness {
    pub fn percent(&self) -> u8 {
        ((self.value as u64 * 100 + self.max as u64 / 2) / self.max.max(1) as u64).min(100) as u8
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub battery: Option<Battery>,
    pub network: Network,
    pub brightness: Option<Brightness>,
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn entries(dir: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// The system's batteries together. Batteries of mice and controllers (scope "Device") don't
/// count.
pub fn battery() -> Option<Battery> {
    let mut sum = 0u32;
    let mut n = 0u32;
    let (mut charging, mut all_full) = (false, true);
    for dev in entries("/sys/class/power_supply") {
        if read(&dev.join("type")).as_deref() != Some("Battery") || read(&dev.join("scope")).as_deref() == Some("Device") {
            continue;
        }
        let Some(cap) = read(&dev.join("capacity")).and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        let status = read(&dev.join("status")).unwrap_or_default();
        charging |= status == "Charging";
        all_full &= status == "Full" || (status == "Not charging" && cap >= 95);
        sum += cap.min(100);
        n += 1;
    }
    (n > 0).then(|| Battery {
        percent: (sum / n) as u8,
        charging,
        full: all_full,
    })
}

/// The best connection that's up: wired beats wireless. Virtual devices (docker, VPN
/// tunnels) don't count.
pub fn network() -> Network {
    let wireless_quality = std::fs::read_to_string("/proc/net/wireless").unwrap_or_default();
    let mut best = Network::Offline;
    for dev in entries("/sys/class/net") {
        // Only real hardware has a `device` link.
        if !dev.join("device").exists() || read(&dev.join("operstate")).as_deref() != Some("up") {
            continue;
        }
        if dev.join("wireless").is_dir() || dev.join("phy80211").exists() {
            let name = dev.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            // "wlan0: 0000   54.  -56.  ..." – link quality out of 70.
            let quality = wireless_quality
                .lines()
                .find_map(|l| l.trim().strip_prefix(&format!("{name}:")))
                .and_then(|rest| rest.split_whitespace().nth(1))
                .and_then(|q| q.trim_end_matches('.').parse::<f32>().ok())
                .map_or(0, |q| ((q / 70.0) * 100.0).clamp(0.0, 100.0) as u8);
            if best == Network::Offline {
                best = Network::Wireless(quality);
            }
        } else {
            best = Network::Wired;
        }
    }
    best
}

/// The first backlight, if the machine has one (laptops; not desktop monitors).
pub fn brightness() -> Option<Brightness> {
    let dev = entries("/sys/class/backlight").into_iter().next()?;
    Some(Brightness {
        device: dev.file_name()?.to_string_lossy().to_string(),
        value: read(&dev.join("brightness"))?.parse().ok()?,
        max: read(&dev.join("max_brightness"))?.parse().ok()?,
    })
}

impl State {
    /// Read the status now and every few seconds after.
    pub fn start_status_timer(&mut self) {
        self.refresh_status();
        let result = self.loop_handle.insert_source(Timer::from_duration(POLL), |_, _, state| {
            state.refresh_status();
            TimeoutAction::ToDuration(POLL)
        });
        if let Err(e) = result {
            tracing::warn!("status timer: {e}");
        }
    }

    pub fn refresh_status(&mut self) {
        use crate::config::Widget;
        if self.config.bar.has(Widget::Bluetooth) {
            self.start_bluetooth();
        }
        if self.config.bar.has(Widget::Sound) {
            self.start_sound();
        }
        let bar = &self.config.bar;
        let status = Status {
            battery: bar.has(Widget::Battery).then(battery).flatten(),
            network: if bar.has(Widget::Network) { network() } else { Network::Offline },
            brightness: bar.has(Widget::Brightness).then(brightness).flatten(),
        };
        if status != self.status {
            self.status = status;
            self.needs_redraw = true;
        }
    }

    /// Scroll on the brightness widget: change it by `steps` of 5%. Done through logind, which
    /// lets the logged-in user set the backlight without root.
    pub fn change_brightness(&mut self, steps: i32) {
        let Some(b) = self.status.brightness.clone().filter(|b| b.max > 0) else {
            return;
        };
        let step = (b.max / 20).max(1) as i64;
        // Never all the way to 0: some screens go fully dark.
        let value = (b.value as i64 + steps as i64 * step).clamp((b.max / 100).max(1) as i64, b.max as i64) as u32;
        if value == b.value {
            return;
        }
        self.bus_command(crate::dbus::Command::SetBrightness {
            device: b.device.clone(),
            value,
        });
        self.status.brightness = Some(Brightness { value, ..b });
        self.needs_redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_percent() {
        let b = Brightness {
            device: "x".into(),
            value: 120,
            max: 240,
        };
        assert_eq!(b.percent(), 50);
        assert_eq!(Brightness { max: 0, ..b }.percent(), 100);
    }

    /// Reports what this machine has. `cargo test status_here -- --nocapture`
    #[test]
    fn status_here() {
        eprintln!("battery {:?} network {:?} brightness {:?}", battery(), network(), brightness());
    }
}
