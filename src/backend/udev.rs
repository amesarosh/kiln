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
    config::ModeSpec,
    cursor::Cursor,
    render::{self, CursorDraw, FrameElement},
    state::State,
};

/// 8-bit formats are universally supported.
const COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Abgr8888, Fourcc::Argb8888];
/// HDR monitors get 10 bits per colour, so PQ doesn't band; 8-bit is the fallback.
const HDR_COLOR_FORMATS: [Fourcc; 6] = [
    Fourcc::Xrgb2101010,
    Fourcc::Xbgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
/// Carried with each queued frame, and answered when it reaches the screen.
type Feedback = Option<smithay::desktop::utils::OutputPresentationFeedback>;

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
    outputs: DrmOutputManager<Allocator, Exporter, Feedback, DrmDeviceFd>,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
    _drm_token: RegistrationToken,
}

struct Surface {
    output: Output,
    connector: connector::Handle,
    mode: drm::control::Mode,
    drm_output: DrmOutput<Allocator, Exporter, Feedback, DrmDeviceFd>,
    global: GlobalId,
    /// A frame is queued and we're waiting for vblank before rendering again.
    waiting_vblank: bool,
    /// Something changed while waiting; render again right after vblank.
    redraw_after_vblank: bool,
    /// The queued frame was rendered while locked; once it's on screen the lock can be confirmed.
    lock_frame_queued: bool,
    /// Frames may flip without waiting for vblank (a fullscreen game asked to tear).
    tearing: bool,
    /// The queued frame was flipped without waiting for vblank.
    torn: bool,
    /// What the monitor's EDID says about its colours and brightness.
    display: crate::color::DisplayColor,
    /// Driven in HDR mode: everything is composited in half-float, then PQ-encoded.
    hdr: Option<HdrState>,
}

struct HdrState {
    params: crate::color::HdrOutputParams,
    pipeline: Option<crate::hdr::HdrPipeline>,
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
            // Nothing is on screen now, so a pending lock is safe to confirm.
            state.confirm_lock_if_ready();
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
                        s.lock_frame_queued = false;
                        s.drm_output.reset_buffers();
                        // Whoever had the screens may have changed their HDR signalling.
                        if s.hdr.is_some()
                            && let Err(e) = set_connector_hdr(gpu.outputs.device(), s.connector, Some(&s.display))
                        {
                            tracing::warn!("{}: could not turn HDR back on: {e}", s.output.name());
                        }
                        if let Some(hdr) = &mut s.hdr {
                            hdr.pipeline = None;
                        }
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

    let token = handle.insert_source(notifier, move |event, metadata, state| match event {
        DrmEvent::VBlank(crtc) => frame_finished(state, crtc, event_time(metadata)),
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
fn wants_enabled(state: &State, name: &str) -> bool {
    let Some(u) = udev_data(state) else { return false };
    if !u.only_outputs.is_empty() && !u.only_outputs.iter().any(|n| n.eq_ignore_ascii_case(name)) {
        return false;
    }
    if state.output_config(name).enabled {
        return true;
    }
    let Some(gpu) = &u.gpu else { return false };
    let mut connected: Vec<String> = gpu.scanner.crtcs().map(|(info, _)| connector_name(info)).collect();
    connected.sort();
    let all_off = connected.iter().all(|n| !state.output_config(n).enabled);
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

/// Every connected monitor, lit or not, for monitor setup tools.
pub fn heads(state: &State) -> Vec<crate::handlers::output_management::HeadInfo> {
    use crate::handlers::output_management::{HeadInfo, HeadMode};
    let Some(gpu) = udev_data(state).and_then(|u| u.gpu.as_ref()) else {
        return Vec::new();
    };
    let mut heads: Vec<HeadInfo> = gpu
        .scanner
        .crtcs()
        .map(|(info, crtc)| {
            let name = connector_name(info);
            // Monitors list some modes more than once (with different flags); show each once.
            let mut modes: Vec<(HeadMode, drm::control::Mode)> = Vec::new();
            for m in info.modes() {
                let wl = WlMode::from(*m);
                let hm = HeadMode {
                    width: wl.size.w,
                    height: wl.size.h,
                    refresh: wl.refresh,
                    preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
                };
                match modes
                    .iter_mut()
                    .find(|(x, _)| (x.width, x.height, x.refresh) == (hm.width, hm.height, hm.refresh))
                {
                    Some((x, _)) => x.preferred |= hm.preferred,
                    None => modes.push((hm, *m)),
                }
            }
            let surface = gpu.surfaces.get(&crtc);
            let current = surface.and_then(|s| {
                let wl = WlMode::from(s.mode);
                modes
                    .iter()
                    .position(|(x, _)| (x.width, x.height, x.refresh) == (wl.size.w, wl.size.h, wl.refresh))
            });
            let (w, h) = info.size().unwrap_or((0, 0));
            let (position, transform, scale) = match surface {
                Some(s) => {
                    let loc = s.output.current_location();
                    (
                        (loc.x, loc.y),
                        s.output.current_transform(),
                        s.output.current_scale().fractional_scale(),
                    )
                }
                None => {
                    let cfg = state.output_config(&name);
                    (
                        cfg.position.unwrap_or_default(),
                        crate::config::smithay_transform(cfg.transform),
                        cfg.scale,
                    )
                }
            };
            HeadInfo {
                description: if w > 0 {
                    format!("{name} ({w}x{h} mm)")
                } else {
                    name.clone()
                },
                name,
                physical_size: (w as i32, h as i32),
                modes: modes.into_iter().map(|(m, _)| m).collect(),
                enabled: surface.is_some(),
                current,
                position,
                transform,
                scale,
            }
        })
        .collect();
    heads.sort_by(|a, b| a.name.cmp(&b.name));
    heads
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
fn apply_scale_transform(output: &Output, cfg: &crate::config::OutputConfig) {
    output.change_current_state(
        None,
        Some(crate::config::smithay_transform(cfg.transform)),
        Some(Scale::Fractional(cfg.scale)),
        None,
    );
}

/// Re-apply `[[outputs]]` after a config reload.
pub fn apply_output_config(state: &mut State) {
    let Some(u) = udev_data(state) else { return };
    if !u.session.is_active() {
        return; // resume rescans
    }
    let Some(gpu) = &u.gpu else { return };
    let lit: Vec<(crtc::Handle, String)> = gpu.surfaces.iter().map(|(crtc, s)| (*crtc, s.output.name())).collect();
    let dark: Vec<(connector::Info, crtc::Handle)> = gpu
        .scanner
        .crtcs()
        .filter(|(_, crtc)| !gpu.surfaces.contains_key(crtc))
        .map(|(info, crtc)| (info.clone(), crtc))
        .collect();

    for (crtc, name) in lit {
        let cfg = state.output_config(&name);
        if !wants_enabled(state, &name) {
            tracing::info!("{name}: disabled in config");
            remove_surface(state, crtc);
            continue;
        }
        // Switching HDR on or off needs a modeset and other buffers: light the monitor again.
        let gpu = udev_data_mut(state).unwrap().gpu.as_mut().unwrap();
        if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
            let want_hdr = cfg.hdr && hdr_capable(&name, &surface.display);
            if want_hdr != surface.hdr.is_some() {
                let info = gpu.scanner.connectors().get(&surface.connector).cloned();
                match info {
                    Some(info) => relight_surface(state, info, crtc, want_hdr),
                    None => remove_surface(state, crtc),
                }
                continue;
            }
            if let Some(hdr) = &mut surface.hdr {
                hdr.params = hdr_params(&cfg, &surface.display);
            }
        }
        let gpu = udev_data_mut(state).unwrap().gpu.as_mut().unwrap();
        let Some(info) = gpu
            .surfaces
            .get(&crtc)
            .and_then(|s| gpu.scanner.connectors().get(&s.connector))
            .cloned()
        else {
            continue;
        };
        let surface = gpu.surfaces.get_mut(&crtc).unwrap();
        if let Some(mode) = pick_mode(&info, cfg.mode)
            && mode != surface.mode
        {
            let wl = WlMode::from(mode);
            match surface.drm_output.use_mode::<_, FrameElement>(
                mode,
                &mut gpu.renderer,
                &DrmOutputRenderElements::default(),
            ) {
                Ok(()) => {
                    tracing::info!(
                        "{name}: mode {}x{}@{}",
                        wl.size.w,
                        wl.size.h,
                        wl.refresh as f32 / 1000.0
                    );
                    surface.mode = mode;
                    surface.output.set_preferred(wl);
                    surface.output.change_current_state(Some(wl), None, None, None);
                }
                Err(e) => tracing::warn!("{name}: could not switch mode: {e}"),
            }
        }
        let output = surface.output.clone();
        apply_scale_transform(&output, &cfg);
    }

    for (info, crtc) in dark {
        let enable = wants_enabled(state, &connector_name(&info));
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
    if !wants_enabled(state, &name) {
        return tracing::info!("{name}: left off (disabled in config or not listed in --output)");
    }
    let cfg = state.output_config(&name);
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

    let display = udev_data(state)
        .and_then(|u| u.gpu.as_ref())
        .map(|g| read_display_color(g.outputs.device(), connector.handle()))
        .unwrap_or_default();
    let want_hdr = cfg.hdr && hdr_capable(&name, &display);
    if let Some(gpu) = udev_data(state).and_then(|u| u.gpu.as_ref()) {
        // Set (or clear, after an earlier HDR session) the monitor's HDR signalling before the
        // first modeset, which picks it up.
        match set_connector_hdr(gpu.outputs.device(), connector.handle(), want_hdr.then_some(&display)) {
            Ok(()) if want_hdr => {
                let peak = display.peak();
                tracing::info!(
                    "{name}: HDR on (peak {peak:.0} nits, SDR white {} nits)",
                    cfg.sdr_brightness
                );
            }
            Ok(()) => {}
            Err(e) if want_hdr => tracing::warn!("{name}: could not turn HDR on: {e}"),
            Err(e) => tracing::debug!("{name}: could not reset HDR properties: {e}"),
        }
    }

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
    // List every mode the monitor supports, so tools (like the settings app) can offer them.
    for m in connector.modes() {
        output.add_mode(WlMode::from(*m));
    }
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
        Some(crate::config::smithay_transform(cfg.transform)),
        Some(Scale::Fractional(cfg.scale)),
        Some((x, 0).into()),
    );
    state.space.map_output(&output, (x, 0));

    let Some(gpu) = udev_data_mut(state).and_then(|u| u.gpu.as_mut()) else {
        return;
    };
    if want_hdr {
        gpu.outputs.set_color_formats(HDR_COLOR_FORMATS);
    } else {
        gpu.outputs.set_color_formats(COLOR_FORMATS);
    }
    let drm_output = match gpu.outputs.initialize_output::<_, FrameElement>(
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
            if want_hdr {
                let _ = set_connector_hdr(gpu.outputs.device(), connector.handle(), None);
            }
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
            lock_frame_queued: false,
            tearing: false,
            torn: false,
            hdr: want_hdr.then(|| HdrState {
                params: hdr_params(&cfg, &display),
                pipeline: None,
            }),
            display,
        },
    );
    if want_hdr && let Some(s) = gpu.surfaces.get(&crtc) {
        let format = s.drm_output.format();
        if !matches!(
            format,
            Fourcc::Xrgb2101010 | Fourcc::Xbgr2101010 | Fourcc::Argb2101010 | Fourcc::Abgr2101010
        ) {
            tracing::warn!("{name}: no 10-bit buffers ({format:?}); HDR may show banding");
        }
    }
    layer_map_for_output(&output).arrange();
    state.add_monitor(output);
}

/// Light a monitor again with HDR switched on or off. Only the DRM side is rebuilt: the
/// Output, its global and its workspaces stay, so windows don't move to another monitor.
fn relight_surface(state: &mut State, connector: connector::Info, crtc: crtc::Handle, want_hdr: bool) {
    let name = connector_name(&connector);
    let cfg = state.output_config(&name);
    let Some(gpu) = udev_data_mut(state).and_then(|u| u.gpu.as_mut()) else {
        return;
    };
    let Some(old) = gpu.surfaces.remove(&crtc) else {
        return;
    };
    let Some(drm_mode) = pick_mode(&connector, cfg.mode) else {
        gpu.surfaces.insert(crtc, old);
        return tracing::warn!("{name}: no usable modes");
    };
    let Surface {
        output,
        connector: conn,
        global,
        display,
        drm_output,
        ..
    } = old;
    if let Err(e) = drm_output.with_compositor(|c| c.clear()) {
        tracing::debug!("could not turn off {name}: {e}");
    }
    // Free the CRTC before setting it up again.
    drop(drm_output);

    match set_connector_hdr(gpu.outputs.device(), conn, want_hdr.then_some(&display)) {
        Ok(()) if want_hdr => {
            let peak = display.peak();
            tracing::info!(
                "{name}: HDR on (peak {peak:.0} nits, SDR white {} nits)",
                cfg.sdr_brightness
            );
        }
        Ok(()) => tracing::info!("{name}: HDR off"),
        Err(e) if want_hdr => tracing::warn!("{name}: could not turn HDR on: {e}"),
        Err(e) => tracing::debug!("{name}: could not reset HDR properties: {e}"),
    }
    if want_hdr {
        gpu.outputs.set_color_formats(HDR_COLOR_FORMATS);
    } else {
        gpu.outputs.set_color_formats(COLOR_FORMATS);
    }
    let drm_output = match gpu.outputs.initialize_output::<_, FrameElement>(
        crtc,
        drm_mode,
        &[conn],
        &output,
        None,
        &mut gpu.renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("{name}: failed to initialize output: {e}");
            if want_hdr {
                let _ = set_connector_hdr(gpu.outputs.device(), conn, None);
            }
            state.remove_monitor(&output);
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            state.needs_redraw = true;
            return;
        }
    };
    if want_hdr {
        let format = drm_output.format();
        if !matches!(
            format,
            Fourcc::Xrgb2101010 | Fourcc::Xbgr2101010 | Fourcc::Argb2101010 | Fourcc::Abgr2101010
        ) {
            tracing::warn!("{name}: no 10-bit buffers ({format:?}); HDR may show banding");
        }
    }
    let wl = WlMode::from(drm_mode);
    output.set_preferred(wl);
    output.change_current_state(Some(wl), None, None, None);
    gpu.surfaces.insert(
        crtc,
        Surface {
            output,
            connector: conn,
            mode: drm_mode,
            drm_output,
            global,
            waiting_vblank: false,
            redraw_after_vblank: false,
            lock_frame_queued: false,
            tearing: false,
            torn: false,
            hdr: want_hdr.then(|| HdrState {
                params: hdr_params(&cfg, &display),
                pipeline: None,
            }),
            display,
        },
    );
    state.needs_redraw = true;
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

/// Turn a monitor off (DPMS) or on. Off, the CRTC is disabled and nothing is rendered for it;
/// the next queued frame turns it back on.
pub fn set_power(state: &mut State, output: &Output, on: bool) {
    let Some(u) = udev_data_mut(state) else { return };
    let active = u.session.is_active();
    let Some(gpu) = &mut u.gpu else { return };
    let Some(surface) = gpu.surfaces.values_mut().find(|s| &s.output == output) else {
        return;
    };
    surface.waiting_vblank = false;
    surface.redraw_after_vblank = false;
    surface.lock_frame_queued = false;
    if on {
        // Everything has to be redrawn: the screen lost what it showed.
        surface.drm_output.reset_buffers();
    } else if active && let Err(e) = surface.drm_output.with_compositor(|c| c.clear()) {
        tracing::warn!("could not turn off {}: {e}", output.name());
    }
    state.needs_redraw = true;
}

/// Render every output that isn't already waiting for its vblank.
pub fn render_all(state: &mut State) {
    let powered_off = state.powered_off.clone();
    let Some(u) = udev_data_mut(state) else { return };
    if !u.session.is_active() {
        return;
    }
    let Some(gpu) = &mut u.gpu else { return };
    let mut ready = Vec::new();
    for (crtc, s) in gpu
        .surfaces
        .iter_mut()
        .filter(|(_, s)| !powered_off.contains(&s.output))
    {
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

/// When a frame reached the screen and its sequence number, from the kernel if it says, else now.
fn event_time(metadata: &mut Option<smithay::backend::drm::DrmEventMetadata>) -> (Option<std::time::Duration>, u64) {
    match metadata {
        Some(m) => match m.time {
            smithay::backend::drm::DrmEventTime::Monotonic(t) => (Some(t), m.sequence as u64),
            smithay::backend::drm::DrmEventTime::Realtime(_) => (None, m.sequence as u64),
        },
        None => (None, 0),
    }
}

fn frame_finished(state: &mut State, crtc: crtc::Handle, (when, seq): (Option<std::time::Duration>, u64)) {
    let now = std::time::Duration::from(state.clock.now());
    let Some(surface) = udev_data_mut(state)
        .and_then(|u| u.gpu.as_mut())
        .and_then(|g| g.surfaces.get_mut(&crtc))
    else {
        return;
    };
    match surface.drm_output.frame_submitted() {
        Ok(Some(Some(mut feedback))) => {
            use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
            // A kernel timestamp is the moment of the flip, taken by the hardware.
            let (time, mut flags) = match when {
                Some(t) => (t, Kind::Vsync | Kind::HwClock | Kind::HwCompletion),
                None => (now, Kind::Vsync),
            };
            // A torn frame went up mid-refresh, not in sync with it.
            if surface.torn {
                flags.remove(Kind::Vsync);
            }
            let refresh = render::refresh_of(&surface.output);
            feedback.presented::<_, smithay::utils::Monotonic>(time, refresh, seq, flags);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("frame submit error: {e}"),
    }
    surface.waiting_vblank = false;
    let redraw = std::mem::take(&mut surface.redraw_after_vblank);
    if std::mem::take(&mut surface.lock_frame_queued) {
        let output = surface.output.clone();
        state.lock_frame_shown(&output);
    }
    if redraw {
        render_surface(state, crtc);
    }
}

fn render_surface(state: &mut State, crtc: crtc::Handle) {
    let off = udev_data(state)
        .and_then(|u| u.gpu.as_ref())
        .and_then(|g| g.surfaces.get(&crtc))
        .is_some_and(|s| !state.monitor_powered(&s.output));
    if off {
        return;
    }
    // Taken out of `state` so the renderer can borrow mutably while elements borrow `state`.
    let Some(Backend::Udev(mut u)) = state.backend.take() else {
        return;
    };
    let output = u
        .gpu
        .as_mut()
        .and_then(|gpu| gpu.surfaces.get(&crtc))
        .map(|s| s.output.clone());

    let mut lock_shown = false;
    let mut damaged = false;
    if let (Some(output), Some(gpu)) = (&output, u.gpu.as_mut()) {
        let themed = themed_cursor(state, &mut u.cursor, output);
        let cursor = cursor_draw(state, &themed);
        let locked = state.is_locked();
        let tearing = state.output_wants_tearing(output);
        let clear = state.config.decoration.background;
        let hdr_on = gpu.surfaces.get(&crtc).is_some_and(|s| s.hdr.is_some());
        let target = if hdr_on {
            crate::color::Target::HdrIntermediate
        } else {
            crate::color::Target::Sdr
        };
        let mut elements = render::frame_elements(state, &mut gpu.renderer, output, cursor, target);
        // HDR: composite into the half-float buffer, then hand the monitor just the encoded frame.
        let mut hdr_states = None;
        if let Some(hdr) = gpu.surfaces.get_mut(&crtc).and_then(|s| s.hdr.as_mut()) {
            if hdr.pipeline.as_ref().is_none_or(|p| !p.matches(output, hdr.params)) {
                hdr.pipeline = match crate::hdr::HdrPipeline::new(&mut gpu.renderer, output, hdr.params) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        tracing::warn!("{}: HDR compositing unavailable: {e}", output.name());
                        None
                    }
                };
            }
            if let Some(pipeline) = &mut hdr.pipeline {
                match pipeline.render(&mut gpu.renderer, &elements, clear) {
                    Ok((states, _)) => {
                        hdr_states = Some(states);
                        elements = pipeline
                            .element(&mut gpu.renderer)
                            .map(FrameElement::Encode)
                            .into_iter()
                            .collect();
                    }
                    Err(e) => tracing::warn!("{}: HDR compositing failed: {e}", output.name()),
                }
            }
        }

        if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
            if surface.tearing != tearing {
                surface.tearing = tearing;
                tracing::info!(
                    "{}: tearing {} (fullscreen app asked for it)",
                    output.name(),
                    if tearing { "on" } else { "off" }
                );
                surface.drm_output.with_compositor(|c| c.set_tearing(tearing));
            }
            match surface
                .drm_output
                .render_frame(&mut gpu.renderer, &elements, clear, FrameFlags::DEFAULT)
            {
                // Only flip when the frame differs; an idle screen stays untouched.
                Ok(result) if !result.is_empty => match surface.drm_output.queue_frame(Some(
                    render::take_presentation_feedback(state, output, hdr_states.as_ref().unwrap_or(&result.states)),
                )) {
                    Ok(()) => {
                        damaged = true;
                        surface.waiting_vblank = true;
                        surface.lock_frame_queued = locked;
                        surface.torn = tearing && surface.drm_output.with_compositor(|c| c.last_flip_torn());
                    }
                    Err(e) => tracing::warn!("failed to queue frame: {e}"),
                },
                // Nothing changed, so the screen already shows the locked frame.
                Ok(_) => lock_shown = locked,
                Err(e) => tracing::warn!("render failed: {e:?}"),
            }
        }
    }

    state.backend = Some(Backend::Udev(u));
    if let Some(output) = output {
        if damaged {
            state.screencopy_output_damaged(&output);
        }
        if lock_shown {
            state.lock_frame_shown(&output);
        }
        render::send_frames(state, &output);
    }
}

type ThemedCursor = Option<(
    smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    smithay::utils::Point<i32, smithay::utils::Logical>,
)>;

/// The themed cursor image for `output`, when the pointer shows a named cursor.
fn themed_cursor(state: &State, cursor: &mut Cursor, output: &Output) -> ThemedCursor {
    match &state.cursor_status {
        CursorImageStatus::Named(icon) => Some(cursor.get(*icon, output.current_scale().integer_scale())),
        _ => None,
    }
}

fn cursor_draw<'a>(state: &State, themed: &'a ThemedCursor) -> CursorDraw<'a> {
    match (&state.cursor_status, themed) {
        (_, Some((buffer, hotspot))) => CursorDraw::Themed(buffer, *hotspot),
        (CursorImageStatus::Surface(_), _) => CursorDraw::Surface,
        _ => CursorDraw::None,
    }
}

/// See [`State::capture_output`].
pub fn capture<T>(
    state: &mut State,
    output: &Output,
    with_cursor: bool,
    f: impl FnOnce(&mut GlesRenderer, &[FrameElement]) -> T,
) -> Option<T> {
    let Some(Backend::Udev(mut u)) = state.backend.take() else {
        return None;
    };
    let mut result = None;
    if u.session.is_active()
        && let Some(gpu) = u.gpu.as_mut()
    {
        let themed = if with_cursor {
            themed_cursor(state, &mut u.cursor, output)
        } else {
            None
        };
        let cursor = if with_cursor {
            cursor_draw(state, &themed)
        } else {
            CursorDraw::None
        };
        let elements = render::frame_elements(state, &mut gpu.renderer, output, cursor, crate::color::Target::Sdr);
        result = Some(f(&mut gpu.renderer, &elements));
    }
    state.backend = Some(Backend::Udev(u));
    result
}

impl UdevData {
    pub fn session_active(&self) -> bool {
        self.session.is_active()
    }

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

    pub fn output_hdr(&self, output: &Output) -> Option<(crate::color::HdrOutputParams, crate::color::DisplayColor)> {
        let s = self.gpu.as_ref()?.surfaces.values().find(|s| &s.output == output)?;
        s.hdr.as_ref().map(|h| (h.params, s.display.clone()))
    }

    /// On exit: leave no monitor in HDR mode for whatever shows next (a console, the login screen).
    pub fn reset_hdr(&self) {
        let Some(gpu) = &self.gpu else { return };
        if !self.session.is_active() {
            return;
        }
        for s in gpu.surfaces.values().filter(|s| s.hdr.is_some()) {
            if let Err(e) = set_connector_hdr(gpu.outputs.device(), s.connector, None) {
                tracing::warn!("{}: could not switch HDR off: {e}", s.output.name());
            }
        }
    }

    pub fn reapply_input_config(&mut self, config: &crate::config::InputConfig) {
        for dev in &mut self.pointers {
            apply_input_config(dev, config);
        }
    }
}

/// The monitor's colour information, from its EDID property.
fn read_display_color(dev: &DrmDevice, connector: connector::Handle) -> crate::color::DisplayColor {
    use drm::control::Device as _;
    let edid = find_property(dev, connector, "EDID")
        .and_then(|(_, _, blob)| (blob != 0).then_some(blob))
        .and_then(|blob| dev.get_property_blob(blob).ok());
    edid.map(|e| crate::color::DisplayColor::from_edid(&e))
        .unwrap_or_default()
}

/// Whether to drive this monitor in HDR: it must say it understands HDR10.
fn hdr_capable(name: &str, display: &crate::color::DisplayColor) -> bool {
    if !display.pq {
        tracing::warn!("{name}: hdr = true, but the monitor doesn't say it supports HDR10; staying SDR");
    }
    display.pq
}

fn hdr_params(
    cfg: &crate::config::OutputConfig,
    display: &crate::color::DisplayColor,
) -> crate::color::HdrOutputParams {
    crate::color::HdrOutputParams {
        sdr_white: cfg.sdr_brightness as f64,
        peak: display.peak(),
    }
}

/// A connector property's handle, info and current value, by name.
fn find_property(
    dev: &DrmDevice,
    connector: connector::Handle,
    name: &str,
) -> Option<(drm::control::property::Handle, drm::control::property::Info, u64)> {
    use drm::control::Device as _;
    let props = dev.get_properties(connector).ok()?;
    let (handles, values) = props.as_props_and_values();
    handles.iter().zip(values).find_map(|(h, v)| {
        let info = dev.get_property(*h).ok()?;
        (info.name().to_bytes() == name.as_bytes()).then_some((*h, info, *v))
    })
}

/// Turn a monitor's HDR signalling on (HDR10 metadata, BT.2020 colours, 10 bits per colour) or
/// back to the defaults. Needs a modeset, so it's done while the monitor isn't being driven.
fn set_connector_hdr(
    dev: &DrmDevice,
    connector: connector::Handle,
    hdr: Option<&crate::color::DisplayColor>,
) -> Result<(), String> {
    use drm::control::{
        AtomicCommitFlags, Device as _,
        atomic::AtomicModeReq,
        property::{Value, ValueType},
    };
    let mut req = AtomicModeReq::new();
    let mut blob = None;
    match find_property(dev, connector, "HDR_OUTPUT_METADATA") {
        Some((h, _, _)) => {
            let value = match hdr {
                Some(d) => {
                    let v = dev
                        .create_property_blob(&crate::hdr::hdr_output_metadata(d))
                        .map_err(|e| format!("HDR metadata: {e}"))?;
                    if let Value::Blob(id) = v {
                        blob = Some(id);
                    }
                    v
                }
                None => Value::Blob(0),
            };
            req.add_property(connector, h, value);
        }
        None if hdr.is_some() => return Err("the driver can't send HDR metadata to this monitor".into()),
        None => return Ok(()),
    }
    if let Some((h, info, _)) = find_property(dev, connector, "Colorspace")
        && let ValueType::Enum(e) = info.value_type()
    {
        let want = if hdr.is_some() { "BT2020_RGB" } else { "Default" };
        let (raws, entries) = e.values();
        if let Some(raw) = raws
            .iter()
            .zip(entries)
            .find(|(_, en)| en.name().to_bytes() == want.as_bytes())
            .map(|(r, _)| *r)
        {
            req.add_property(connector, h, Value::Unknown(raw));
        }
    }
    if hdr.is_some()
        && let Some((h, info, _)) = find_property(dev, connector, "max bpc")
        && let ValueType::UnsignedRange(min, max) = info.value_type()
    {
        req.add_property(connector, h, Value::UnsignedRange(10u64.clamp(min, max)));
    }
    let result = dev
        .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        .map_err(|e| format!("commit: {e}"));
    if let Some(id) = blob {
        // The kernel keeps its own reference while the property uses it.
        let _ = dev.destroy_property_blob(id);
    }
    result
}
