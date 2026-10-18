//! Nested backend: runs the compositor as a window inside an existing X11 or Wayland session.
//!
//! For testing multi-monitor behaviour without hardware, `WAYLAND_TIME_NESTED_OUTPUTS=N` splits
//! the window into N side-by-side monitors named winit-1..winit-N.

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{
            ImportDma,
            damage::OutputDamageTracker,
            element::utils::{Relocate, RelocateRenderElement},
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::LoopHandle,
    utils::{Physical, Size, Transform},
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use super::Backend;
use crate::{
    render::{self, CursorDraw},
    state::State,
};

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    /// Left to right, splitting the window evenly.
    outputs: Vec<Output>,
    /// Covers the whole window; each output's elements are shifted into its slice.
    damage_tracker: OutputDamageTracker,
}

/// The window is rendered upside down relative to GL, like smithay's winit examples.
const WINDOW_TRANSFORM: Transform = Transform::Flipped180;

fn nested_output_count() -> usize {
    std::env::var("WAYLAND_TIME_NESTED_OUTPUTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Give each output its slice of the window. The last one takes any leftover pixels.
fn layout_outputs(state: &mut State, outputs: &[Output], window: Size<i32, Physical>) {
    let n = outputs.len() as i32;
    let w = window.w / n;
    for (i, output) in outputs.iter().enumerate() {
        let i = i as i32;
        let width = if i == n - 1 { window.w - w * (n - 1) } else { w };
        let mode = Mode {
            size: (width, window.h).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((i * w, 0).into()));
        output.set_preferred(mode);
        state.space.map_output(output, (i * w, 0));
        layer_map_for_output(output).arrange();
    }
}

impl WinitData {
    pub fn request_redraw(&self) {
        self.backend.window().request_redraw();
    }

    pub fn renderer(&mut self) -> &mut GlesRenderer {
        self.backend.renderer()
    }
}

pub fn init(state: &mut State, handle: &LoopHandle<'static, State>) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit_loop) = winit::init::<GlesRenderer>()?;
    backend.window().set_title("wayland-time");

    let count = nested_output_count();
    let outputs: Vec<Output> = (1..=count)
        .map(|i| {
            let name = if count == 1 {
                "winit".to_string()
            } else {
                format!("winit-{i}")
            };
            let output = Output::new(
                name,
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "wayland-time".into(),
                    model: "nested".into(),
                },
            );
            // Lives as long as the compositor, so the global handle isn't needed.
            let _global = output.create_global::<State>(&state.display_handle);
            output.change_current_state(None, Some(WINDOW_TRANSFORM), None, None);
            output
        })
        .collect();
    let window_size = backend.window_size();
    layout_outputs(state, &outputs, window_size);
    for output in &outputs {
        state.add_monitor(output.clone());
    }

    // Let GPU clients share buffers with us instead of falling back to software rendering.
    let formats = backend.renderer().dmabuf_formats();
    let render_node =
        EGLDevice::device_for_display(backend.renderer().egl_context().display()).and_then(|d| d.try_get_render_node());
    match render_node
        .map_err(|e| e.to_string())
        .and_then(|n| n.ok_or_else(|| "no render node".to_string()))
        .and_then(|n| {
            DmabufFeedbackBuilder::new(n.dev_id(), formats)
                .build()
                .map_err(|e| e.to_string())
        }) {
        Ok(feedback) => {
            let global = state
                .dmabuf_state
                .create_global_with_default_feedback::<State>(&state.display_handle, &feedback);
            state.dmabuf_global = Some(global);
        }
        Err(e) => tracing::warn!("linux-dmabuf disabled: {e}"),
    }

    let damage_tracker = OutputDamageTracker::new(window_size, 1.0, WINDOW_TRANSFORM);
    state.backend = Some(Backend::Winit(WinitData {
        backend,
        outputs,
        damage_tracker,
    }));

    handle.insert_source(winit_loop, |event, _, state| match event {
        WinitEvent::Resized { size, .. } => {
            let Some(Backend::Winit(data)) = &mut state.backend else {
                return;
            };
            data.damage_tracker = OutputDamageTracker::new(size, 1.0, WINDOW_TRANSFORM);
            let outputs = data.outputs.clone();
            layout_outputs(state, &outputs, size);
            state.arrange();
        }
        WinitEvent::Input(event) => state.process_input_event(event),
        WinitEvent::Redraw => render(state),
        WinitEvent::CloseRequested => state.loop_signal.stop(),
        WinitEvent::Focus(_) => {}
    })?;

    Ok(())
}

fn render(state: &mut State) {
    // Taken out of `state` so the renderer can borrow mutably while elements borrow `state`.
    let Some(Backend::Winit(mut data)) = state.backend.take() else {
        return;
    };
    let age = data.backend.buffer_age().unwrap_or(0);
    let clear = state.config.decoration.background;

    let rendered = match data.backend.bind() {
        Ok((renderer, mut framebuffer)) => {
            // Each output renders its own elements, shifted to its slice of the window.
            let mut elements = Vec::new();
            for output in &data.outputs {
                let offset = state
                    .space
                    .output_geometry(output)
                    .map(|g| g.loc.to_physical(1))
                    .unwrap_or_default();
                elements.extend(
                    render::output_elements(state, renderer, output, CursorDraw::None)
                        .into_iter()
                        .map(|e| RelocateRenderElement::from_element(e, offset, Relocate::Relative)),
                );
            }
            match data
                .damage_tracker
                .render_output(renderer, &mut framebuffer, age, &elements, clear)
            {
                Ok(result) => result.damage.cloned(),
                Err(e) => {
                    tracing::warn!("render failed: {e:?}");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!("failed to bind winit buffer: {e}");
            None
        }
    };

    // Nothing changed on screen means nothing to swap; avoids burning GPU while idle.
    if let Some(damage) = rendered
        && let Err(e) = data.backend.submit(Some(&damage))
    {
        tracing::warn!("failed to submit frame: {e}");
    }

    let outputs = data.outputs.clone();
    state.backend = Some(Backend::Winit(data));
    for output in &outputs {
        render::send_frames(state, output);
    }
}
