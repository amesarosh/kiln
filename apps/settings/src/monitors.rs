//! Asking the compositor which monitors are connected, so they can be picked instead of typed.

use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{wl_output, wl_registry},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    /// In mHz.
    pub refresh: i32,
    pub current: bool,
}

impl Mode {
    /// As written in the config: "2560x1440@143.91".
    pub fn spec(&self) -> String {
        let hz = format!("{:.3}", self.refresh as f64 / 1000.0);
        let hz = hz.trim_end_matches('0').trim_end_matches('.');
        format!("{}x{}@{hz}", self.width, self.height)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Monitor {
    /// Connector name, e.g. "DP-1".
    pub name: String,
    pub description: String,
    pub modes: Vec<Mode>,
    /// Where it is on the desktop, for listing monitors left to right.
    pub position: (i32, i32),
}

#[derive(Default)]
struct State {
    monitors: Vec<Monitor>,
}

/// The connected, switched-on monitors. Empty if the compositor can't be reached.
pub fn detect() -> Vec<Monitor> {
    let run = || -> Result<Vec<Monitor>, Box<dyn std::error::Error>> {
        let conn = Connection::connect_to_env()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        // First round trip lists the globals; the second gets each output's details.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        Ok(state.monitors)
    };
    let mut monitors = run().unwrap_or_else(|e| {
        tracing::warn!("couldn't list monitors: {e}");
        Vec::new()
    });
    for m in &mut monitors {
        // Biggest and fastest first, and each mode once.
        m.modes
            .sort_by(|a, b| (b.width * b.height, b.refresh).cmp(&(a.width * a.height, a.refresh)));
        m.modes.dedup_by(|a, b| a.spec() == b.spec());
    }
    monitors.sort_by_key(|m| m.position);
    monitors
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "wl_output"
            // Version 4 tells us the connector name.
            && version >= 4
        {
            registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, state.monitors.len());
            state.monitors.push(Monitor::default());
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let m = &mut state.monitors[*index];
        match event {
            wl_output::Event::Name { name } => m.name = name,
            // The compositor fills in what it knows, e.g. "Dell Inc. - U2720Q - DP-1"; drop the
            // parts it doesn't know and the connector name, which is shown anyway.
            wl_output::Event::Description { description } => {
                m.description = description
                    .split(" - ")
                    .filter(|p| *p != "Unknown" && *p != m.name)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            wl_output::Event::Geometry { x, y, .. } => m.position = (x, y),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                let current = matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
                m.modes.push(Mode {
                    width,
                    height,
                    refresh,
                    current,
                });
            }
            _ => {}
        }
    }
}
