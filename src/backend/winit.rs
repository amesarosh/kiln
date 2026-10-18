//! Nested backend: runs the compositor as a window inside an existing X11 or Wayland session.

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{ImportDma, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::LoopHandle,
    utils::Transform,
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use super::Backend;
use crate::{
    render::{self, CursorDraw},
    state::State,
};

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage_tracker: OutputDamageTracker,
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

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "wayland-time".into(),
            model: "nested".into(),
        },
    );
    let _global = output.create_global::<State>(&state.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

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

    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.backend = Some(Backend::Winit(WinitData {
        backend,
        output,
        damage_tracker,
    }));

    handle.insert_source(winit_loop, |event, _, state| match event {
        WinitEvent::Resized { size, .. } => {
            if let Some(output) = state.output() {
                output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
                layer_map_for_output(&output).arrange();
            }
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
            let elements = render::output_elements(state, renderer, &data.output, CursorDraw::None);
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

    let output = data.output.clone();
    state.backend = Some(Backend::Winit(data));
    render::send_frames(state, &output);
}
