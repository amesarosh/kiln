//! Hardware backend: DRM/KMS for display, libinput for input, libseat for device access.
//!
//! Single GPU for now: outputs on the primary GPU are driven, others are ignored.
//! Frames are rendered only when something changed, and paced by vblank.

use std::{collections::HashMap, path::Path};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            dmabuf::Dmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{ImportDma, ImportMemWl, gles::GlesRenderer},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::layer_map_for_output,
    input::{keyboard::LedState, pointer::CursorImageStatus},
    output::{Mode as WlMode, Output, PhysicalProperties, Scale},
    reexports::{
        calloop::{LoopHandle, RegistrationToken},
        drm::{
            self,
            control::{ModeTypeFlags, connector, crtc},
        },
        input::{self as libinput, DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::DeviceFd,
    wayland::dmabuf::DmabufFeedbackBuilder,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use super::Backend;
use crate::{
    config::{Config, ModeSpec},
    cursor::Cursor,
    render::{self, CursorDraw, OutputElement},
    state::State,
};

/// 8-bit formats are universally supported; 10-bit can be added later behind a config flag.
const COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Abgr8888, Fourcc::Argb8888];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;

pub struct UdevData {
    session: LibSeatSession,
    libinput: Libinput,
    gpu: Option<Gpu>,
    /// From --output: only these connectors are lit. Empty means all of them.
    only_outputs: Vec<String>,
    cursor: Cursor,
    keyboards: Vec<libinput::Device>,
    /// Pointer devices, kept so input settings can be re-applied on config reload.
    pointers: Vec<libinput::Device>,
}

struct Gpu {
    node: DrmNode,
    renderer: GlesRenderer,
    outputs: DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
    _drm_token: RegistrationToken,
}

struct Surface {
    output: Output,
    connector: connector::Handle,
    mode: drm::control::Mode,
    drm_output: DrmOutput<Allocator, Exporter, (), DrmDeviceFd>,
    global: GlobalId,
    /// A frame is queued and we're waiting for vblank before rendering again.
    waiting_vblank: bool,
    /// Something changed while waiting; render again right after vblank.
    redraw_after_vblank: bool,
}

pub fn init(
    state: &mut State,
    handle: &LoopHandle<'static, State>,
    only_outputs: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // libseat tries seatd before logind. If both run, seatd and logind fight over
    // the GPU and VTs on every switch, which can hang the machine. Prefer logind
    // when it's managing the system, unless the user picked a backend.
    if std::env::var_os("LIBSEAT_BACKEND").is_none() && Path::new("/run/systemd/seats").exists() {
        // SAFETY: called during startup before any other threads exist.
        unsafe { std::env::set_var("LIBSEAT_BACKEND", "logind") };
    }
    let (session, notifier) = LibSeatSession::new().map_err(|e| format!("could not start a seat session: {e}"))?;
    let seat = session.seat();
    tracing::info!("seat: {seat}");

    let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|_| "failed to assign libinput to the seat")?;

    state.backend = Some(Backend::Udev(UdevData {
        session,
        libinput: libinput.clone(),
        gpu: None,
        only_outputs,
        cursor: Cursor::new(),
        keyboards: Vec::new(),
        pointers: Vec::new(),
    }));

    // Pick the GPU the system marks as primary (boot_vga), or the first one found.
    let primary = primary_gpu(&seat)?
        .or_else(|| all_gpus(&seat).ok()?.into_iter().next())
        .ok_or("no GPU found")?;
    let primary_node = DrmNode::from_path(&primary)?;
    tracing::info!("primary GPU: {}", primary.display());

    let udev = UdevBackend::new(&seat)?;
    let devices: Vec<_> = udev.device_list().map(|(id, p)| (id, p.to_path_buf())).collect();
    for (dev_id, path) in devices {
        if dev_id == primary_node.dev_id() {
            device_added(state, handle, DrmNode::from_dev_id(dev_id)?, &path)?;
        } else {
            tracing::info!(
                "ignoring secondary GPU {} (multi-GPU not supported yet)",
                path.display()
            );
        }
    }
    if udev_data(state).is_none_or(|u| u.gpu.is_none()) {
        return Err("could not initialize the primary GPU".into());
    }
    // A typo in --output would otherwise leave every screen dark.
    if let Some(u) = udev_data(state)
        && !u.only_outputs.is_empty()
        && u.gpu.as_ref().is_some_and(|g| g.surfaces.is_empty())
    {
        return Err(format!(
            "none of --output {} matched a connected monitor",
            u.only_outputs.join(", ")
        )
        .into());
    }

    handle.insert_source(udev, move |event, _, state| match event {
        UdevEvent::Changed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                scan_connectors(state, node);
            }
        }
        UdevEvent::Added { .. } | UdevEvent::Removed { .. } => {}
    })?;

    handle.insert_source(LibinputInputBackend::new(libinput), |mut event, _, state| {
        match &mut event {
            InputEvent::DeviceAdded { device } => device_added_input(state, device),
            InputEvent::DeviceRemoved { device } => {
                if let Some(u) = udev_data_mut(state) {
                    u.keyboards.retain(|d| d != device);
                    u.pointers.retain(|d| d != device);
                }
            }
            _ => {}
        }
        state.process_input_event(event);
    })?;

    handle.insert_source(notifier, |event, _, state| match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused (VT switched away)");
            if let Some(u) = udev_data_mut(state) {
                u.libinput.suspend();
                if let Some(gpu) = &mut u.gpu {
                    gpu.outputs.pause();
                }
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("session resumed");
            if let Some(u) = udev_data_mut(state) {
                if u.libinput.resume().is_err() {
                    tracing::error!("failed to resume libinput");
                }
                if let Some(gpu) = &mut u.gpu {
                    // Whoever had the GPU meanwhile (Xorg, GDM) may have moved monitors to other
                    // CRTCs. `true` turns everything off first so our old layout doesn't conflict.
                    if let Err(e) = gpu.outputs.activate(true) {
                        tracing::error!("failed to reactivate GPU: {e}");
                    }
                    for s in gpu.surfaces.values_mut() {
                        s.waiting_vblank = false;
                        s.redraw_after_vblank = false;
                        s.drm_output.reset_buffers();
                    }
                }
            }
            // Hotplug events were ignored while paused; catch up on monitor changes.
            if let Some(node) = udev_data(state).and_then(|u| u.gpu.as_ref()).map(|g| g.node) {
                scan_connectors(state, node);
            }
            state.needs_redraw = true;
        }
    })?;

    Ok(())
}

fn udev_data(state: &State) -> Option<&UdevData> {
    match &state.backend {
        Some(Backend::Udev(u)) => Some(u),
        _ => None,
    }
}

fn udev_data_mut(state: &mut State) -> Option<&mut UdevData> {
    match &mut state.backend {
        Some(Backend::Udev(u)) => Some(u),
        _ => None,
    }
}

fn device_added(
    state: &mut State,
    handle: &LoopHandle<'static, State>,
    node: DrmNode,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let u = udev_data_mut(state).ok_or("udev backend missing")?;
    let fd = u
        .session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;

    // SAFETY: the GBM device outlives the display; both are owned by this GPU entry.
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let context = EGLContext::new(&display)?;
    // SAFETY: the context is current only on this thread.
    let renderer = unsafe { GlesRenderer::new(context)? };

    let render_node = node.node_with_type(NodeType::Render).and_then(Result::ok);
    let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let exporter = GbmFramebufferExporter::new(gbm.clone(), render_node);
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    let outputs = DrmOutputManager::new(drm, allocator, exporter, Some(gbm), COLOR_FORMATS, render_formats);

    state.shm_state.update_formats(renderer.shm_formats());

    // GPU clients share buffers with us through linux-dmabuf.
    let feedback = DmabufFeedbackBuilder::new(node.dev_id(), renderer.dmabuf_formats()).build()?;
    let global = state
        .dmabuf_state
        .create_global_with_default_feedback::<State>(&state.display_handle, &feedback);
    state.dmabuf_global = Some(global);

    let token = handle.insert_source(notifier, move |event, _, state| match event {
        DrmEvent::VBlank(crtc) => frame_finished(state, crtc),
        DrmEvent::Error(e) => tracing::error!("DRM error: {e}"),
    })?;

    udev_data_mut(state).ok_or("udev backend missing")?.gpu = Some(Gpu {
        node,
        renderer,
        outputs,
        scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
        _drm_token: token,
    });

    scan_connectors(state, node);
    Ok(())
}

/// Light up newly connected monitors and drop disconnected ones.
fn scan_connectors(state: &mut State, node: DrmNode) {
    let Some(u) = udev_data_mut(state) else { return };
    // While paused another program owns the screens and every modeset it does sends a udev
    // change event. Scanning force-probes each monitor (EDID reads) underneath it, so wait for
    // resume, which rescans.
    if !u.session.is_active() {
        return;
    }
    let Some(gpu) = u.gpu.as_mut() else { return };
    if gpu.node != node {
        return;
    }
    let result = match gpu.scanner.scan_connectors(gpu.outputs.device()) {
        Ok(r) => r,
        Err(e) => return tracing::warn!("failed to scan connectors: {e}"),
    };
    for event in result {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(state, connector, crtc),
            DrmScanEvent::Disconnected {
                connector,
                crtc: Some(crtc),
            } => connector_disconnected(state, connector, crtc),
            _ => {}
        }
    }
    state.layout_monitors();
    state.monitors_changed();
}

fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

/// Whether a connected monitor should be lit. Never all of them off: if the config disables
/// every connected monitor, the first one by name stays on.
fn wants_enabled(u: &UdevData, config: &Config, name: &str) -> bool {
    if !u.only_outputs.is_empty() && !u.only_outputs.iter().any(|n| n.eq_ignore_ascii_case(name)) {
        return false;
    }
    if config.output(name).enabled {
        return true;
    }
    let Some(gpu) = &u.gpu else { return false };
    let mut connected: Vec<String> = gpu.scanner.crtcs().map(|(info, _)| connector_name(info)).collect();
    connected.sort();
    let all_off = connected.iter().all(|n| !config.output(n).enabled);
    all_off && connected.first().is_some_and(|n| n == name)
}

/// The configured mode if the monitor has it, else its preferred one.
fn pick_mode(info: &connector::Info, want: Option<ModeSpec>) -> Option<drm::control::Mode> {
    let modes = info.modes();
    let preferred = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())
        .copied();
    let Some(want) = want else { return preferred };
    let refresh = |m: &drm::control::Mode| WlMode::from(*m).refresh as f64 / 1000.0;
    let same_size = modes.iter().filter(|m| {
        let (w, h) = m.size();
        (w as i32, h as i32) == (want.width, want.height)
    });
    let best = match want.refresh {
        Some(hz) => same_size.min_by(|a, b| (refresh(a) - hz).abs().total_cmp(&(refresh(b) - hz).abs())),
        None => same_size.max_by(|a, b| refresh(a).total_cmp(&refresh(b))),
    };
    let name = connector_name(info);
    match best {
        Some(m) => {
            if let Some(hz) = want.refresh
                && (refresh(m) - hz).abs() > 0.5
            {
                tracing::warn!("{name}: no {hz} Hz mode, using {:.3} Hz", refresh(m));
            }
            Some(*m)
        }
        None => {
            tracing::warn!(
                "{name}: no {}x{} mode, using the preferred one",
                want.width,
                want.height
            );
            preferred
        }
    }
}

fn describe_modes(info: &connector::Info) -> String {
    let mut seen = Vec::new();
    for m in info.modes() {
        let (w, h) = m.size();
        let s = format!("{w}x{h}@{:.2}", WlMode::from(*m).refresh as f64 / 1000.0);
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    seen.join(", ")
}

/// Push scale and rotation from the config onto an output.
fn apply_scale_transform(output: &Output, config: &Config) {
    let cfg = config.output(&output.name());
    output.change_current_state(None, Some(cfg.transform), Some(Scale::Fractional(cfg.scale)), None);
}

/// Re-apply `[[outputs]]` after a config reload.
pub fn apply_output_config(state: &mut State) {
    let Some(u) = udev_data(state) else { return };
    if !u.session.is_active() {
        return; // resume rescans
    }
    let Some(gpu) = &u.gpu else { return };
    let lit: Vec<(crtc::Handle, String)> = gpu
        .surfaces
        .iter()
        .map(|(crtc, s)| (*crtc, s.output.name()))
        .collect();
    let dark: Vec<(connector::Info, crtc::Handle)> = gpu
        .scanner
        .crtcs()
        .filter(|(_, crtc)| !gpu.surfaces.contains_key(crtc))
        .map(|(info, crtc)| (info.clone(), crtc))
        .collect();

    for (crtc, name) in lit {
        let config = state.config.clone();
        let u = udev_data_mut(state).unwrap();
        if !wants_enabled(u, &config, &name) {
            tracing::info!("{name}: disabled in config");
            remove_surface(state, crtc);
            continue;
        }
        let gpu = u.gpu.as_mut().unwrap();
        let Some(info) = gpu
            .surfaces
            .get(&crtc)
            .and_then(|s| gpu.scanner.connectors().get(&s.connector))
            .cloned()
        else {
            continue;
        };
        let surface = gpu.surfaces.get_mut(&crtc).unwrap();
        if let Some(mode) = pick_mode(&info, config.output(&name).mode)
            && mode != surface.mode
        {
            let wl = WlMode::from(mode);
            match surface.drm_output.use_mode::<_, OutputElement<GlesRenderer>>(
                mode,
                &mut gpu.renderer,
                &DrmOutputRenderElements::default(),
            ) {
                Ok(()) => {
                    tracing::info!("{name}: mode {}x{}@{}", wl.size.w, wl.size.h, wl.refresh as f32 / 1000.0);
                    surface.mode = mode;
                    surface.output.set_preferred(wl);
                    surface.output.change_current_state(Some(wl), None, None, None);
                }
                Err(e) => tracing::warn!("{name}: could not switch mode: {e}"),
            }
        }
        let output = surface.output.clone();
        apply_scale_transform(&output, &config);
    }

    for (info, crtc) in dark {
        let enable = udev_data(state).is_some_and(|u| wants_enabled(u, &state.config, &connector_name(&info)));
        if enable {
            connector_connected(state, info, crtc);
        }
    }
    state.layout_monitors();
    state.monitors_changed();
}

fn connector_connected(state: &mut State, connector: connector::Info, crtc: crtc::Handle) {
    let name = connector_name(&connector);
    tracing::info!("{name}: connected, modes: {}", describe_modes(&connector));
    if !udev_data(state).is_some_and(|u| wants_enabled(u, &state.config, &name)) {
        return tracing::info!("{name}: left off (disabled in config or not listed in --output)");
    }
    let cfg = state.config.output(&name);
    let Some(drm_mode) = pick_mode(&connector, cfg.mode) else {
        return tracing::warn!("{name}: no usable modes");
    };
    let mode = WlMode::from(drm_mode);
    tracing::info!(
        "{name}: using {}x{}@{}, scale {}",
        mode.size.w,
        mode.size.h,
        mode.refresh as f32 / 1000.0,
        cfg.scale
    );

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make: "Unknown".into(),
            model: "Unknown".into(),
        },
    );
    let global = output.create_global::<State>(&state.display_handle);

    // Start right of the others; layout_monitors() puts it in its final place.
    let x: i32 = state
        .space
        .outputs()
        .filter_map(|o| state.space.output_geometry(o))
        .map(|g| g.loc.x + g.size.w)
        .max()
        .unwrap_or(0);
    output.set_preferred(mode);
    output.change_current_state(
        Some(mode),
        Some(cfg.transform),
        Some(Scale::Fractional(cfg.scale)),
        Some((x, 0).into()),
    );
    state.space.map_output(&output, (x, 0));

    let Some(gpu) = udev_data_mut(state).and_then(|u| u.gpu.as_mut()) else {
        return;
    };
    let drm_output = match gpu.outputs.initialize_output::<_, OutputElement<GlesRenderer>>(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        None,
        &mut gpu.renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("{name}: failed to initialize output: {e}");
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };
    gpu.surfaces.insert(
        crtc,
        Surface {
            output: output.clone(),
            connector: connector.handle(),
            mode: drm_mode,
            drm_output,
            global,
            waiting_vblank: false,
            redraw_after_vblank: false,
        },
    );
    layer_map_for_output(&output).arrange();
    state.add_monitor(output);
}

fn connector_disconnected(state: &mut State, connector: connector::Info, crtc: crtc::Handle) {
    tracing::info!("{} disconnected", connector_name(&connector));
    remove_surface(state, crtc);
}

/// Stop driving a monitor and forget it.
fn remove_surface(state: &mut State, crtc: crtc::Handle) {
    let Some(gpu) = udev_data_mut(state).and_then(|u| u.gpu.as_mut()) else {
        return;
    };
    let Some(surface) = gpu.surfaces.remove(&crtc) else {
        return;
    };
    // Dropping a DrmOutput doesn't turn the screen off; clear the CRTC so it goes dark.
    if let Err(e) = surface.drm_output.with_compositor(|c| c.clear()) {
        tracing::debug!("could not turn off {}: {e}", surface.output.name());
    }
    state.remove_monitor(&surface.output);
    state.space.unmap_output(&surface.output);
    state.display_handle.remove_global::<State>(surface.global);
    state.needs_redraw = true;
}

fn device_added_input(state: &mut State, device: &mut libinput::Device) {
    let leds = state.keyboard().led_state();
    let config = state.config.input.clone();
    let Some(u) = udev_data_mut(state) else { return };
    if device.has_capability(DeviceCapability::Keyboard) {
        device.led_update(leds.into());
        u.keyboards.push(device.clone());
    }
    if device.has_capability(DeviceCapability::Pointer) {
        apply_input_config(device, &config);
        u.pointers.push(device.clone());
    }
}

fn apply_input_config(device: &mut libinput::Device, config: &crate::config::InputConfig) {
    if device.config_tap_finger_count() > 0 {
        let _ = device.config_tap_set_enabled(config.tap_to_click);
    }
    if device.config_scroll_has_natural_scroll() {
        let _ = device.config_scroll_set_natural_scroll_enabled(config.natural_scroll);
    }
    if device.config_accel_is_available() {
        let _ = device.config_accel_set_speed(config.pointer_speed.clamp(-1.0, 1.0));
    }
}

/// Render every output that isn't already waiting for its vblank.
pub fn render_all(state: &mut State) {
    let Some(u) = udev_data_mut(state) else { return };
    if !u.session.is_active() {
        return;
    }
    let Some(gpu) = &mut u.gpu else { return };
    let mut ready = Vec::new();
    for (crtc, s) in gpu.surfaces.iter_mut() {
        if s.waiting_vblank {
            s.redraw_after_vblank = true;
        } else {
            ready.push(*crtc);
        }
    }
    for crtc in ready {
        render_surface(state, crtc);
    }
}

fn frame_finished(state: &mut State, crtc: crtc::Handle) {
    let Some(surface) = udev_data_mut(state)
        .and_then(|u| u.gpu.as_mut())
        .and_then(|g| g.surfaces.get_mut(&crtc))
    else {
        return;
    };
    if let Err(e) = surface.drm_output.frame_submitted() {
        tracing::warn!("frame submit error: {e}");
    }
    surface.waiting_vblank = false;
    if std::mem::take(&mut surface.redraw_after_vblank) {
        render_surface(state, crtc);
    }
}

fn render_surface(state: &mut State, crtc: crtc::Handle) {
    // Taken out of `state` so the renderer can borrow mutably while elements borrow `state`.
    let Some(Backend::Udev(mut u)) = state.backend.take() else {
        return;
    };
    let output = u
        .gpu
        .as_mut()
        .and_then(|gpu| gpu.surfaces.get(&crtc))
        .map(|s| s.output.clone());

    if let (Some(output), Some(gpu)) = (&output, u.gpu.as_mut()) {
        let themed = match &state.cursor_status {
            CursorImageStatus::Named(icon) => Some(u.cursor.get(*icon, output.current_scale().integer_scale())),
            CursorImageStatus::Surface(_) => None,
            CursorImageStatus::Hidden => None,
        };
        let cursor = match (&state.cursor_status, &themed) {
            (_, Some((buffer, hotspot))) => CursorDraw::Themed(buffer, *hotspot),
            (CursorImageStatus::Surface(_), _) => CursorDraw::Surface,
            _ => CursorDraw::None,
        };
        let elements = render::output_elements(state, &mut gpu.renderer, output, cursor);
        let clear = state.config.decoration.background;

        if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
            match surface
                .drm_output
                .render_frame(&mut gpu.renderer, &elements, clear, FrameFlags::DEFAULT)
            {
                // Only flip when the frame differs; an idle screen stays untouched.
                Ok(result) if !result.is_empty => match surface.drm_output.queue_frame(()) {
                    Ok(()) => surface.waiting_vblank = true,
                    Err(e) => tracing::warn!("failed to queue frame: {e}"),
                },
                Ok(_) => {}
                Err(e) => tracing::warn!("render failed: {e:?}"),
            }
        }
    }

    state.backend = Some(Backend::Udev(u));
    if let Some(output) = output {
        render::send_frames(state, &output);
    }
}

impl UdevData {
    pub fn switch_vt(&mut self, vt: i32) {
        if let Err(e) = self.session.change_vt(vt) {
            tracing::warn!("failed to switch to VT {vt}: {e}");
        }
    }

    pub fn update_leds(&mut self, leds: LedState) {
        for kb in &mut self.keyboards {
            kb.led_update(leds.into());
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.gpu
            .as_mut()
            .is_some_and(|g| g.renderer.import_dmabuf(dmabuf, None).is_ok())
    }

    pub fn reapply_input_config(&mut self, config: &crate::config::InputConfig) {
        for dev in &mut self.pointers {
            apply_input_config(dev, config);
        }
    }
}
