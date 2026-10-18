//! Nested backend: runs the compositor as a window inside an existing X11 or Wayland session.

use std::time::Duration;

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::LoopHandle,
    utils::Transform,
};

use crate::{render, state::State};

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage_tracker: OutputDamageTracker,
}

impl WinitData {
    pub fn request_redraw(&self) {
        self.backend.window().request_redraw();
    }
}

pub fn init(state: &mut State, handle: &LoopHandle<'static, State>) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, winit_loop) = winit::init::<GlesRenderer>()?;
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

    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.winit = Some(WinitData {
        backend,
        output,
        damage_tracker,
    });

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
    let Some(mut data) = state.winit.take() else { return };
    let age = data.backend.buffer_age().unwrap_or(0);
    let clear = state.config.decoration.background;

    let rendered = match data.backend.bind() {
        Ok((renderer, mut framebuffer)) => {
            let elements = render::output_elements(state, renderer, &data.output);
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

    send_frames(state, &data.output);
    state.winit = Some(data);
}

/// Tell clients they can draw their next frame.
fn send_frames(state: &mut State, output: &Output) {
    let time = state.start_time.elapsed();
    for window in state.space.elements() {
        window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
    }
    for layer in layer_map_for_output(output).layers() {
        layer.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
    }
    state.space.refresh();
    state.popups.cleanup();
}
