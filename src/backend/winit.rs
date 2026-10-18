//! Nested backend: runs the compositor as a window inside an existing X11 or Wayland session.
//!
//! For testing multi-monitor behaviour without hardware, `WAYLAND_TIME_NESTED_OUTPUTS=N` splits
//! the window into N side-by-side monitors named winit-1..winit-N.

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{
            ImportDma, Renderer,
            damage::OutputDamageTracker,
            element::{
                Element, Id, Kind, RenderElement, UnderlyingStorage,
                utils::{Relocate, RelocateRenderElement},
            },
            gles::GlesRenderer,
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::LoopHandle,
    utils::{Buffer, Physical, Point, Rectangle, Size, Transform},
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use super::Backend;
use crate::{
    render::{self, CursorDraw, OutputElement},
    state::State,
};

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    /// Left to right, splitting the window evenly.
    outputs: Vec<Output>,
    /// Where each output's slice starts in the window, in window pixels.
    offsets: Vec<Point<i32, Physical>>,
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
/// Only `scale` from the outputs config applies here; mode and position follow the window.
/// Returns where each slice starts, in window pixels.
fn layout_outputs(state: &mut State, outputs: &[Output], window: Size<i32, Physical>) -> Vec<Point<i32, Physical>> {
    let n = outputs.len() as i32;
    let w = window.w / n;
    let mut offsets = Vec::new();
    let mut logical_x = 0;
    for (i, output) in outputs.iter().enumerate() {
        let i = i as i32;
        let width = if i == n - 1 { window.w - w * (n - 1) } else { w };
        let mode = Mode {
            size: (width, window.h).into(),
            refresh: 60_000,
        };
        let scale = state.config.output(&output.name()).scale;
        output.change_current_state(Some(mode), None, Some(Scale::Fractional(scale)), None);
        output.set_preferred(mode);
        state.move_monitor(output, (logical_x, 0).into());
        logical_x += state.space.output_geometry(output).map_or(0, |g| g.size.w);
        offsets.push((i * w, 0).into());
    }
    offsets
}

/// Re-apply output scales after a config reload.
pub fn apply_output_config(state: &mut State) {
    let Some(Backend::Winit(data)) = &state.backend else {
        return;
    };
    let (outputs, size) = (data.outputs.clone(), data.backend.window_size());
    let offsets = layout_outputs(state, &outputs, size);
    if let Some(Backend::Winit(data)) = &mut state.backend {
        data.offsets = offsets;
    }
    state.monitors_changed();
}

/// The whole window is drawn by one damage tracker at scale 1, but each fake monitor has its
/// own scale. This makes an element ignore the tracker's scale and use its monitor's.
struct AtScale<E> {
    inner: E,
    scale: smithay::utils::Scale<f64>,
}

impl<E: Element> Element for AtScale<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn geometry(&self, _: smithay::utils::Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(self.scale)
    }
    fn location(&self, _: smithay::utils::Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(self.scale)
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn damage_since(&self, _: smithay::utils::Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(self.scale, commit)
    }
    fn opaque_regions(&self, _: smithay::utils::Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(self.scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for AtScale<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        self.inner.draw(frame, src, dst, damage, opaque_regions)
    }
    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
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
    let offsets = layout_outputs(state, &outputs, window_size);
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
        offsets,
        damage_tracker,
    }));

    handle.insert_source(winit_loop, |event, _, state| match event {
        WinitEvent::Resized { size, .. } => {
            let Some(Backend::Winit(data)) = &mut state.backend else {
                return;
            };
            data.damage_tracker = OutputDamageTracker::new(size, 1.0, WINDOW_TRANSFORM);
            let outputs = data.outputs.clone();
            let offsets = layout_outputs(state, &outputs, size);
            if let Some(Backend::Winit(data)) = &mut state.backend {
                data.offsets = offsets;
            }
            state.monitors_changed();
        }
        WinitEvent::Input(event) => state.process_input_event(event),
        WinitEvent::Redraw => render(state),
        WinitEvent::CloseRequested => {
            state.save_session_final();
            state.loop_signal.stop();
        }
        WinitEvent::Focus(_) => {}
    })?;

    Ok(())
}

/// See [`State::capture_output`]. The host draws the pointer, so there's no cursor to add.
pub fn capture<T>(
    state: &mut State,
    output: &Output,
    f: impl FnOnce(&mut GlesRenderer, &[OutputElement<GlesRenderer>]) -> T,
) -> Option<T> {
    let Some(Backend::Winit(mut data)) = state.backend.take() else {
        return None;
    };
    let renderer = data.renderer();
    let elements = render::output_elements(state, renderer, output, CursorDraw::None);
    let result = f(renderer, &elements);
    state.backend = Some(Backend::Winit(data));
    Some(result)
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
            for (output, &offset) in data.outputs.iter().zip(&data.offsets) {
                let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
                elements.extend(
                    render::output_elements(state, renderer, output, CursorDraw::None)
                        .into_iter()
                        .map(|e| {
                            RelocateRenderElement::from_element(AtScale { inner: e, scale }, offset, Relocate::Relative)
                        }),
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
    if let Some(damage) = &rendered
        && let Err(e) = data.backend.submit(Some(damage))
    {
        tracing::warn!("failed to submit frame: {e}");
    }

    let outputs = data.outputs.clone();
    state.backend = Some(Backend::Winit(data));
    if rendered.is_some() {
        for output in &outputs {
            state.screencopy_output_damaged(output);
        }
    }
    let locked = state.is_locked();
    for output in &outputs {
        if locked {
            state.lock_frame_shown(output);
        }
        // Like on hardware, apps on a turned-off monitor get no frame callbacks.
        if state.monitor_powered(output) {
            render::send_frames(state, output);
        }
    }
}
