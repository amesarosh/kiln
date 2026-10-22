//! Monitor setup tools (kanshi, wdisplays, wlr-randr): wlr-output-management.
//!
//! Each connected monitor is a "head", lit or not, with the modes it supports. A tool sends a
//! whole new arrangement; it's checked, then kept as runtime overrides on top of `[[outputs]]`
//! (see `State::output_config`) and applied the same way a config change is. Editing the
//! monitors in the config file clears the overrides again.

use std::sync::{Arc, Mutex};

use smithay::{
    reexports::{
        wayland_protocols_wlr::output_management::v1::server::{
            zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
            zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
            zwlr_output_head_v1::{self, AdaptiveSyncState, ZwlrOutputHeadV1},
            zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
            zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, backend::ClientId,
            protocol::wl_output,
        },
    },
    utils::Transform,
};

use crate::{
    config::{ModeSpec, OutputConfig},
    state::State,
};

/// One mode a monitor supports. `refresh` is in mHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadMode {
    pub width: i32,
    pub height: i32,
    pub refresh: i32,
    pub preferred: bool,
}

/// A connected monitor as the backend sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadInfo {
    pub name: String,
    pub description: String,
    /// Millimetres.
    pub physical_size: (i32, i32),
    pub modes: Vec<HeadMode>,
    pub enabled: bool,
    /// Index into `modes`, while lit.
    pub current: Option<usize>,
    pub position: (i32, i32),
    pub transform: Transform,
    pub scale: f64,
}

/// What one client has been told about one head.
struct ClientHead {
    head: ZwlrOutputHeadV1,
    modes: Vec<ZwlrOutputModeV1>,
    info: HeadInfo,
}

struct ClientManager {
    manager: ZwlrOutputManagerV1,
    heads: Vec<ClientHead>,
}

#[derive(Default)]
pub struct OutputManagement {
    /// Goes up whenever the heads change; a configuration made for an older serial is stale.
    serial: u32,
    /// The heads as last sent to clients.
    heads: Vec<HeadInfo>,
    clients: Vec<ClientManager>,
}

/// What a client asked for one head in a configuration.
#[derive(Debug, Default)]
pub struct PendingHead {
    mode: Option<ModeSpec>,
    position: Option<(i32, i32)>,
    transform: Option<Transform>,
    scale: Option<f64>,
}

/// A configuration being built: each head is enabled (with its settings) or disabled.
#[derive(Default)]
pub struct PendingConfig {
    serial: u32,
    used: bool,
    heads: Vec<(String, Option<Arc<Mutex<PendingHead>>>)>,
}

/// The global's version. 4 adds adaptive sync, which is reported as off.
const VERSION: u32 = 4;

impl State {
    pub fn create_output_management_global(dh: &DisplayHandle) {
        dh.create_global::<State, ZwlrOutputManagerV1, _>(VERSION, ());
    }

    /// Monitors changed: tell every monitor tool what's different.
    pub fn refresh_output_heads(&mut self) {
        let heads = self.output_heads();
        if heads == self.output_management.heads {
            return;
        }
        let om = &mut self.output_management;
        om.serial = om.serial.wrapping_add(1);
        om.heads = heads;
        let dh = self.display_handle.clone();
        for client in &mut om.clients {
            update_client(&dh, client, &om.heads);
            client.manager.done(om.serial);
        }
    }

    /// Check a configuration and turn it into monitor settings, or say why it can't be used.
    fn configuration_settings(&self, config: &PendingConfig) -> Result<Vec<OutputConfig>, String> {
        let heads = &self.output_management.heads;
        let mut out = Vec::new();
        for info in heads {
            let Some((_, pending)) = config.heads.iter().find(|(n, _)| *n == info.name) else {
                continue;
            };
            let mut cfg = self.output_config(&info.name);
            cfg.name = info.name.clone();
            match pending {
                None => cfg.enabled = false,
                Some(p) => {
                    let p = p.lock().unwrap();
                    cfg.enabled = true;
                    if let Some(mode) = p.mode {
                        cfg.mode = Some(mode);
                    } else if let Some(m) = info.current.and_then(|i| info.modes.get(i)) {
                        cfg.mode = Some(mode_spec(m));
                    }
                    if let Some(pos) = p.position {
                        cfg.position = Some(pos);
                    } else if info.enabled {
                        cfg.position = Some(info.position);
                    }
                    if let Some(t) = p.transform {
                        cfg.transform = config_transform(t);
                    }
                    if let Some(s) = p.scale {
                        if !(0.25..=8.0).contains(&s) {
                            return Err(format!("{}: scale {s} is out of range", info.name));
                        }
                        cfg.scale = s.clamp(0.5, 4.0);
                    }
                }
            }
            out.push(cfg);
        }
        if !out.iter().any(|c| c.enabled) {
            return Err("every monitor would be off".into());
        }
        Ok(out)
    }

    fn finish_configuration(&mut self, resource: &ZwlrOutputConfigurationV1, apply: bool) {
        let Some(data) = resource.data::<Mutex<PendingConfig>>() else {
            return;
        };
        let mut config = data.lock().unwrap();
        if config.used {
            resource.post_error(
                zwlr_output_configuration_v1::Error::AlreadyUsed,
                "configuration was already applied or tested",
            );
            return;
        }
        config.used = true;
        if config.serial != self.output_management.serial {
            resource.cancelled();
            return;
        }
        if let Some(missing) = self
            .output_management
            .heads
            .iter()
            .find(|h| !config.heads.iter().any(|(n, _)| *n == h.name))
        {
            resource.post_error(
                zwlr_output_configuration_v1::Error::UnconfiguredHead,
                format!("head {} has not been configured", missing.name),
            );
            return;
        }
        match self.configuration_settings(&config) {
            Err(e) => {
                tracing::info!("monitor change refused: {e}");
                resource.failed();
            }
            Ok(_) if !apply => resource.succeeded(),
            Ok(settings) => {
                drop(config);
                tracing::info!("monitor change from a client: {settings:?}");
                for s in settings {
                    self.output_overrides.retain(|o| !o.name.eq_ignore_ascii_case(&s.name));
                    self.output_overrides.push(s);
                }
                self.apply_output_config();
                resource.succeeded();
                // apply_output_config ends in monitors_changed, which sends the new state; make
                // sure it's sent even if nothing visible changed.
                self.refresh_output_heads();
            }
        }
    }
}

fn mode_spec(m: &HeadMode) -> ModeSpec {
    ModeSpec {
        width: m.width,
        height: m.height,
        refresh: (m.refresh > 0).then(|| m.refresh as f64 / 1000.0),
    }
}

fn config_transform(t: Transform) -> crate::config::Transform {
    use crate::config::Transform as T;
    match t {
        Transform::Normal => T::Normal,
        Transform::_90 => T::_90,
        Transform::_180 => T::_180,
        Transform::_270 => T::_270,
        Transform::Flipped => T::Flipped,
        Transform::Flipped90 => T::Flipped90,
        Transform::Flipped180 => T::Flipped180,
        Transform::Flipped270 => T::Flipped270,
    }
}

/// Create a head (and its modes) for a client and send everything about it.
fn send_new_head(dh: &DisplayHandle, manager: &ZwlrOutputManagerV1, info: &HeadInfo) -> Option<ClientHead> {
    let client = manager.client()?;
    let head = client
        .create_resource::<ZwlrOutputHeadV1, _, State>(dh, manager.version(), info.name.clone())
        .ok()?;
    manager.head(&head);
    head.name(info.name.clone());
    head.description(info.description.clone());
    if info.physical_size.0 > 0 && info.physical_size.1 > 0 {
        head.physical_size(info.physical_size.0, info.physical_size.1);
    }
    let modes = send_modes(dh, &client, &head, info);
    send_state(&head, &modes, info, None);
    if head.version() >= 4 {
        head.adaptive_sync(AdaptiveSyncState::Disabled);
    }
    Some(ClientHead {
        head,
        modes,
        info: info.clone(),
    })
}

fn send_modes(dh: &DisplayHandle, client: &Client, head: &ZwlrOutputHeadV1, info: &HeadInfo) -> Vec<ZwlrOutputModeV1> {
    let version = head.version().min(ZwlrOutputModeV1::interface().version);
    info.modes
        .iter()
        .filter_map(|m| {
            let mode = client
                .create_resource::<ZwlrOutputModeV1, _, State>(dh, version, (info.name.clone(), *m))
                .ok()?;
            head.mode(&mode);
            mode.size(m.width, m.height);
            if m.refresh > 0 {
                mode.refresh(m.refresh);
            }
            if m.preferred {
                mode.preferred();
            }
            Some(mode)
        })
        .collect()
}

/// Send the parts of a head's state that differ from `old` (everything without one).
fn send_state(head: &ZwlrOutputHeadV1, modes: &[ZwlrOutputModeV1], info: &HeadInfo, old: Option<&HeadInfo>) {
    if old.is_none_or(|o| o.enabled != info.enabled) {
        head.enabled(info.enabled as i32);
    }
    if !info.enabled {
        return;
    }
    let was_off = old.is_some_and(|o| !o.enabled);
    if (was_off || old.is_none_or(|o| o.current != info.current || o.modes != info.modes))
        && let Some(mode) = info.current.and_then(|i| modes.get(i))
    {
        head.current_mode(mode);
    }
    if was_off || old.is_none_or(|o| o.position != info.position) {
        head.position(info.position.0, info.position.1);
    }
    if was_off || old.is_none_or(|o| o.transform != info.transform) {
        head.transform(wl_output::Transform::from(info.transform));
    }
    if was_off || old.is_none_or(|o| o.scale != info.scale) {
        head.scale(info.scale);
    }
}

/// Bring one client up to date with `heads`.
fn update_client(dh: &DisplayHandle, client: &mut ClientManager, heads: &[HeadInfo]) {
    // Gone monitors.
    client.heads.retain(|ch| {
        let keep = heads.iter().any(|h| h.name == ch.info.name);
        if !keep {
            for m in &ch.modes {
                m.finished();
            }
            ch.head.finished();
        }
        keep
    });
    for info in heads {
        match client.heads.iter_mut().find(|ch| ch.info.name == info.name) {
            Some(ch) => {
                if ch.info.modes != info.modes {
                    for m in &ch.modes {
                        m.finished();
                    }
                    match ch.head.client() {
                        Some(c) => ch.modes = send_modes(dh, &c, &ch.head, info),
                        None => ch.modes.clear(),
                    }
                }
                send_state(&ch.head, &ch.modes, info, Some(&ch.info));
                ch.info = info.clone();
            }
            None => {
                if let Some(ch) = send_new_head(dh, &client.manager, info) {
                    client.heads.push(ch);
                }
            }
        }
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for State {
    fn bind(
        state: &mut State,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        let manager = data_init.init(resource, ());
        if state.output_management.heads.is_empty() {
            state.output_management.heads = state.output_heads();
        }
        let om = &mut state.output_management;
        let heads = om.heads.iter().filter_map(|h| send_new_head(dh, &manager, h)).collect();
        manager.done(om.serial);
        om.clients.push(ClientManager { manager, heads });
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    Mutex::new(PendingConfig {
                        serial,
                        ..Default::default()
                    }),
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                state.output_management.clients.retain(|c| &c.manager != resource);
                resource.finished();
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut State, _client: ClientId, resource: &ZwlrOutputManagerV1, _data: &()) {
        state.output_management.clients.retain(|c| &c.manager != resource);
    }
}

impl Dispatch<ZwlrOutputHeadV1, String> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &String,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        // Only `release`, a destructor.
    }

    fn destroyed(state: &mut State, _client: ClientId, resource: &ZwlrOutputHeadV1, _data: &String) {
        for c in &mut state.output_management.clients {
            c.heads.retain(|h| &h.head != resource);
        }
    }
}

impl Dispatch<ZwlrOutputModeV1, (String, HeadMode)> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &(String, HeadMode),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        // Only `release`, a destructor.
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, Mutex<PendingConfig>> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &Mutex<PendingConfig>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_output_configuration_v1::{Error, Request};
        let head_name = |head: &ZwlrOutputHeadV1| head.data::<String>().cloned().unwrap_or_default();
        let used = |r: &ZwlrOutputConfigurationV1| {
            r.post_error(Error::AlreadyUsed, "configuration was already applied or tested");
        };
        match request {
            Request::EnableHead { id, head } => {
                let name = head_name(&head);
                let pending = Arc::new(Mutex::new(PendingHead::default()));
                // New objects must be set up even when the request is an error.
                data_init.init(id, pending.clone());
                let mut config = data.lock().unwrap();
                if config.used {
                    return used(resource);
                }
                if config.heads.iter().any(|(n, _)| *n == name) {
                    resource.post_error(Error::AlreadyConfiguredHead, format!("head {name} configured twice"));
                    return;
                }
                config.heads.push((name, Some(pending)));
            }
            Request::DisableHead { head } => {
                let name = head_name(&head);
                let mut config = data.lock().unwrap();
                if config.used {
                    return used(resource);
                }
                if config.heads.iter().any(|(n, _)| *n == name) {
                    resource.post_error(Error::AlreadyConfiguredHead, format!("head {name} configured twice"));
                    return;
                }
                config.heads.push((name, None));
            }
            Request::Apply => state.finish_configuration(resource, true),
            Request::Test => state.finish_configuration(resource, false),
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, Arc<Mutex<PendingHead>>> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &Arc<Mutex<PendingHead>>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_output_configuration_head_v1::{Error, Request};
        let mut p = data.lock().unwrap();
        let already = |r: &ZwlrOutputConfigurationHeadV1, what: &str| {
            r.post_error(Error::AlreadySet, format!("{what} was already set"));
        };
        match request {
            Request::SetMode { mode } => {
                if p.mode.is_some() {
                    return already(resource, "mode");
                }
                match mode.data::<(String, HeadMode)>() {
                    Some((_, m)) => p.mode = Some(mode_spec(m)),
                    None => resource.post_error(Error::InvalidMode, "unknown mode"),
                }
            }
            Request::SetCustomMode { width, height, refresh } => {
                if p.mode.is_some() {
                    return already(resource, "mode");
                }
                if width <= 0 || height <= 0 || refresh < 0 {
                    return resource.post_error(Error::InvalidCustomMode, "mode size must be positive");
                }
                // Only the monitor's own modes can be used; the closest one is picked.
                p.mode = Some(mode_spec(&HeadMode {
                    width,
                    height,
                    refresh,
                    preferred: false,
                }));
            }
            Request::SetPosition { x, y } => {
                if p.position.is_some() {
                    return already(resource, "position");
                }
                p.position = Some((x, y));
            }
            Request::SetTransform { transform } => {
                if p.transform.is_some() {
                    return already(resource, "transform");
                }
                match transform {
                    WEnum::Value(t) => p.transform = Some(t.into()),
                    WEnum::Unknown(_) => resource.post_error(Error::InvalidTransform, "unknown transform"),
                }
            }
            Request::SetScale { scale } => {
                if p.scale.is_some() {
                    return already(resource, "scale");
                }
                if scale <= 0.0 {
                    return resource.post_error(Error::InvalidScale, "scale must be positive");
                }
                p.scale = Some(scale);
            }
            // Adaptive sync isn't supported; asking for it changes nothing.
            Request::SetAdaptiveSync { .. } => {}
            _ => {}
        }
    }
}
