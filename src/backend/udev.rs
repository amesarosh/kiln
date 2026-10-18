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
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{LoopHandle, RegistrationToken},
        drm::control::{ModeTypeFlags, connector, crtc},
        input::{self as libinput, DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
    wayland::dmabuf::DmabufFeedbackBuilder,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use super::Backend;
use crate::{
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
    drm_output: DrmOutput<Allocator, Exporter, (), DrmDeviceFd>,
    global: GlobalId,
    /// A frame is queued and we're waiting for vblank before rendering again.
    waiting_vblank: bool,
    /// Something changed while waiting; render again right after vblank.
    redraw_after_vblank: bool,
}

pub fn init(state: &mut State, handle: &LoopHandle<'static, State>) -> Result<(), Box<dyn std::error::Error>> {
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
                    if let Err(e) = gpu.outputs.activate(false) {
                        tracing::error!("failed to reactivate GPU: {e}");
                    }
                    for s in gpu.surfaces.values_mut() {
                        s.waiting_vblank = false;
                        s.drm_output.reset_buffers();
                    }
                }
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
    let Some(gpu) = udev_data_mut(state).and_then(|u| u.gpu.as_mut()) else {
        return;
    };
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
    state.arrange();
    state.refocus();
}

fn connector_connected(state: &mut State, connector: connector::Info, crtc: crtc::Handle) {
    let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    let Some(&drm_mode) = connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
    else {
        return tracing::warn!("{name}: no usable modes");
    };
    let mode = WlMode::from(drm_mode);
    tracing::info!(
        "{name}: {}x{}@{}",
        mode.size.w,
        mode.size.h,
        mode.refresh as f32 / 1000.0
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

    // Place monitors left to right in the order they're found.
    let x: i32 = state
        .space
        .outputs()
        .filter_map(|o| state.space.output_geometry(o))
        .map(|g| g.loc.x + g.size.w)
        .max()
        .unwrap_or(0);
    output.set_preferred(mode);
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((x, 0).into()));
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
            drm_output,
            global,
            waiting_vblank: false,
            redraw_after_vblank: false,
        },
    );
    layer_map_for_output(&output).arrange();
    state.needs_redraw = true;
}

fn connector_disconnected(state: &mut State, connector: connector::Info, crtc: crtc::Handle) {
    let Some(gpu) = udev_data_mut(state).and_then(|u| u.gpu.as_mut()) else {
        return;
    };
    let Some(surface) = gpu.surfaces.remove(&crtc) else {
        return;
    };
    tracing::info!(
        "{}-{} disconnected",
        connector.interface().as_str(),
        connector.interface_id()
    );
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
            CursorImageStatus::Named(icon) => Some(u.cursor.get(*icon)),
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
