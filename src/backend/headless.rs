//! Headless backend, for testing: no window and no screen. Outputs are drawn offscreen on the
//! GPU's render node at a steady 60 Hz, so clients get frame callbacks and screenshots (grim)
//! work even when nothing is visible. Pair it with `kiln msg` to drive it.
//!
//! `KILN_HEADLESS_OUTPUTS=N` makes N outputs of 1280x800, named headless-1..N.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::{Fourcc, gbm::GbmDevice},
        egl::{EGLContext, EGLDisplay},
        renderer::{
            Bind, ImportDma, Offscreen,
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::{
        LoopHandle,
        timer::{TimeoutAction, Timer},
    },
    utils::Transform,
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use super::Backend;
use crate::{
    render::{self, CursorDraw, FrameElement},
    state::State,
};

const SIZE: (i32, i32) = (1280, 800);
const FRAME: Duration = Duration::from_micros(16_667);

pub struct HeadlessData {
    renderer: GlesRenderer,
    outputs: Vec<(Output, OutputDamageTracker, GlesTexture)>,
    /// Outputs with `hdr = true` in the config are drawn like an HDR monitor would be, so the
    /// HDR path can be tested (see `msg test-dump`).
    hdr: std::collections::HashMap<String, crate::hdr::HdrPipeline>,
}

impl HeadlessData {
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    pub fn heads(&self, state: &State) -> Vec<crate::handlers::output_management::HeadInfo> {
        use crate::handlers::output_management::{HeadInfo, HeadMode};
        self.outputs
            .iter()
            .map(|(o, _, _)| {
                let mode = HeadMode {
                    width: SIZE.0,
                    height: SIZE.1,
                    refresh: 60_000,
                    preferred: true,
                };
                let loc = state.space.output_geometry(o).map(|g| g.loc).unwrap_or_default();
                HeadInfo {
                    name: o.name(),
                    description: "kiln headless output".into(),
                    physical_size: (0, 0),
                    modes: vec![mode],
                    enabled: true,
                    current: Some(0),
                    position: (loc.x, loc.y),
                    transform: o.current_transform(),
                    scale: o.current_scale().fractional_scale(),
                }
            })
            .collect()
    }
}

fn open_renderer() -> Result<(GlesRenderer, Option<u64>), Box<dyn std::error::Error>> {
    let mut last_err: Box<dyn std::error::Error> = "no render node in /dev/dri".into();
    for n in 128..136 {
        let path = format!("/dev/dri/renderD{n}");
        let file = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                last_err = format!("{path}: {e}").into();
                continue;
            }
        };
        let dev_id = rustix::fs::fstat(&file).ok().map(|s| s.st_rdev);
        let gbm = GbmDevice::new(file)?;
        // SAFETY: the GBM device lives as long as the display, which owns a clone of it.
        let display = match unsafe { EGLDisplay::new(gbm) } {
            Ok(d) => d,
            Err(e) => {
                last_err = format!("{path}: {e}").into();
                continue;
            }
        };
        let context = EGLContext::new(&display)?;
        // SAFETY: the context is only made current on this thread.
        let renderer = unsafe { GlesRenderer::new(context)? };
        tracing::info!("headless: rendering on {path}");
        return Ok((renderer, dev_id));
    }
    Err(last_err)
}

pub fn init(state: &mut State, handle: &LoopHandle<'static, State>) -> Result<(), Box<dyn std::error::Error>> {
    let (mut renderer, dev_id) = open_renderer()?;
    state
        .shm_state
        .update_formats(smithay::backend::renderer::ImportMemWl::shm_formats(&renderer));
    if let Some(dev) = dev_id
        && let Ok(feedback) = DmabufFeedbackBuilder::new(dev, renderer.dmabuf_formats()).build()
    {
        let global = state
            .dmabuf_state
            .create_global_with_default_feedback::<State>(&state.display_handle, &feedback);
        state.dmabuf_global = Some(global);
    }

    let count = std::env::var("KILN_HEADLESS_OUTPUTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1usize)
        .clamp(1, 4);
    let mut outputs = Vec::new();
    for i in 1..=count {
        let output = Output::new(
            format!("headless-{i}"),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "kiln".into(),
                model: "headless".into(),
            },
        );
        let _global = output.create_global::<State>(&state.display_handle);
        let mode = Mode {
            size: SIZE.into(),
            refresh: 60_000,
        };
        output.add_mode(mode);
        output.set_preferred(mode);
        let scale = state.output_config(&output.name()).scale;
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(scale)),
            Some(((i as i32 - 1) * SIZE.0, 0).into()),
        );
        state.space.map_output(&output, ((i as i32 - 1) * SIZE.0, 0));
        let texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, SIZE.into())?;
        let tracker = OutputDamageTracker::from_output(&output);
        outputs.push((output, tracker, texture));
    }
    let list: Vec<Output> = outputs.iter().map(|(o, _, _)| o.clone()).collect();
    state.backend = Some(Backend::Headless(HeadlessData {
        renderer,
        outputs,
        hdr: Default::default(),
    }));
    state.layout_monitors();
    for o in list {
        state.add_monitor(o);
    }

    handle.insert_source(Timer::from_duration(FRAME), |_, _, state| {
        render(state);
        TimeoutAction::ToDuration(FRAME)
    })?;
    Ok(())
}

/// Re-apply output scales after a config change.
pub fn apply_output_config(state: &mut State) {
    let Some(Backend::Headless(data)) = &state.backend else {
        return;
    };
    let outputs: Vec<Output> = data.outputs.iter().map(|(o, _, _)| o.clone()).collect();
    for o in &outputs {
        let cfg = state.output_config(&o.name());
        o.change_current_state(
            None,
            Some(crate::config::smithay_transform(cfg.transform)),
            Some(Scale::Fractional(cfg.scale)),
            None,
        );
    }
    state.layout_monitors();
    state.monitors_changed();
}

/// Draw every output offscreen and answer frame callbacks, like a 60 Hz screen would.
fn render(state: &mut State) {
    let Some(Backend::Headless(mut data)) = state.backend.take() else {
        return;
    };
    let clear = state.config.decoration.background;
    let mut feedback = Vec::new();
    let mut damaged = Vec::new();
    let HeadlessData { renderer, outputs, hdr } = &mut data;
    for (output, tracker, texture) in outputs.iter_mut() {
        let cfg = state.output_config(&output.name());
        let mut hdr_states = None;
        let elements = if cfg.hdr {
            // Pretend to be an HDR400 monitor.
            let params = crate::color::HdrOutputParams {
                sdr_white: cfg.sdr_brightness as f64,
                peak: 400.0,
            };
            if hdr.get(&output.name()).is_none_or(|p| !p.matches(output, params)) {
                match crate::hdr::HdrPipeline::new(renderer, output, params) {
                    Ok(p) => {
                        hdr.insert(output.name(), p);
                    }
                    Err(e) => tracing::warn!("headless HDR: {e}"),
                }
            }
            let scene = render::frame_elements(
                state,
                renderer,
                output,
                CursorDraw::None,
                crate::color::Target::HdrIntermediate,
            );
            match hdr
                .get_mut(&output.name())
                .map(|p| (p.render(renderer, &scene, clear), p))
            {
                Some((Ok((states, _)), p)) => {
                    hdr_states = Some(states);
                    p.element(renderer).map(FrameElement::Encode).into_iter().collect()
                }
                Some((Err(e), _)) => {
                    tracing::warn!("headless HDR render failed: {e}");
                    Vec::new()
                }
                None => scene,
            }
        } else {
            hdr.remove(&output.name());
            render::frame_elements(state, renderer, output, CursorDraw::None, crate::color::Target::Sdr)
        };
        let Ok(mut fb) = renderer.bind(texture) else { continue };
        match tracker.render_output(renderer, &mut fb, 0, &elements, clear) {
            Ok(result) => {
                if result.damage.is_some_and(|d| !d.is_empty()) {
                    damaged.push(output.clone());
                }
                let states = hdr_states.as_ref().unwrap_or(&result.states);
                feedback.push(render::take_presentation_feedback(state, output, states));
            }
            Err(e) => tracing::warn!("headless render failed: {e:?}"),
        }
    }
    let outputs: Vec<Output> = data.outputs.iter().map(|(o, _, _)| o.clone()).collect();
    state.backend = Some(Backend::Headless(data));

    use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
    let now = state.clock.now();
    for mut f in feedback {
        let refresh = f
            .output()
            .map_or(smithay::wayland::presentation::Refresh::Unknown, |o| {
                render::refresh_of(&o)
            });
        f.presented(now, refresh, 0, Kind::Vsync);
    }
    for o in &damaged {
        state.screencopy_output_damaged(o);
    }
    let locked = state.is_locked();
    for o in &outputs {
        if locked {
            state.lock_frame_shown(o);
        }
        if state.monitor_powered(o) {
            render::send_frames(state, o);
        }
    }
    state.needs_redraw = false;
}

/// See [`State::capture_output`].
pub fn capture<T>(
    state: &mut State,
    output: &Output,
    f: impl FnOnce(&mut GlesRenderer, &[FrameElement]) -> T,
) -> Option<T> {
    let Some(Backend::Headless(mut data)) = state.backend.take() else {
        return None;
    };
    let elements = render::frame_elements(
        state,
        &mut data.renderer,
        output,
        CursorDraw::None,
        crate::color::Target::Sdr,
    );
    let result = f(&mut data.renderer, &elements);
    state.backend = Some(Backend::Headless(data));
    Some(result)
}

/// Save what `output` last showed (what a monitor would be sent: PQ-encoded on an HDR output)
/// as a binary PPM, for tests.
pub fn dump(state: &mut State, name: &str, path: &str) -> Result<(), String> {
    use smithay::backend::renderer::{ExportMem, Texture};
    let Some(Backend::Headless(data)) = state.backend.as_mut() else {
        return Err("not the headless backend".into());
    };
    let HeadlessData { renderer, outputs, .. } = data;
    let (_, _, texture) = outputs
        .iter_mut()
        .find(|(o, _, _)| o.name() == name)
        .ok_or_else(|| format!("no output {name}"))?;
    let size = texture.size();
    let fb = renderer.bind(texture).map_err(|e| e.to_string())?;
    let mapping = renderer
        .copy_framebuffer(&fb, smithay::utils::Rectangle::from_size(size), Fourcc::Abgr8888)
        .map_err(|e| e.to_string())?;
    let pixels = renderer.map_texture(&mapping).map_err(|e| e.to_string())?;
    let mut ppm = format!("P6\n{} {}\n255\n", size.w, size.h).into_bytes();
    for px in pixels.chunks_exact(4) {
        ppm.extend_from_slice(&px[..3]);
    }
    std::fs::write(path, ppm).map_err(|e| e.to_string())
}
