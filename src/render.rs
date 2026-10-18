//! Builds the list of things to draw for an output, front to back.

use std::{cell::RefCell, time::Duration};

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Id, Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            render_elements,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        utils::CommitCounter,
    },
    desktop::{Window, layer_map_for_output},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_server::Resource,
    utils::{Logical, Physical, Point, Rectangle, Scale},
    wayland::compositor::with_states,
    wayland::shell::wlr_layer::Layer,
};

use crate::{config::Color, state::State};

render_elements! {
    pub OutputElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
    Memory=MemoryRenderBufferRenderElement<R>,
}

/// What the pointer should look like this frame, for backends that draw it.
pub enum CursorDraw<'a> {
    None,
    /// A themed cursor image and its hotspot.
    Themed(&'a MemoryRenderBuffer, Point<i32, Logical>),
    /// The client supplied its own cursor surface.
    Surface,
}

/// Stable ids per window so the damage tracker only repaints borders that changed.
struct BorderIds {
    ids: [Id; 4],
    commit: CommitCounter,
    color: Color,
}

pub fn output_elements<R>(
    state: &State,
    renderer: &mut R,
    output: &Output,
    cursor: CursorDraw<'_>,
) -> Vec<OutputElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let mut out = Vec::new();
    let Some(output_geo) = state.space.output_geometry(output) else {
        return out;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    // The pointer is always on top.
    let pointer = state.seat.get_pointer().unwrap().current_location();
    if output_geo.to_f64().contains(pointer) {
        let rel = pointer - output_geo.loc.to_f64();
        match (cursor, &state.cursor_status) {
            (CursorDraw::Themed(buffer, hotspot), _) => {
                let loc = (rel - hotspot.to_f64()).to_physical(scale);
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    loc,
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(e) => out.push(OutputElement::Memory(e)),
                    Err(e) => tracing::warn!("cursor upload failed: {e:?}"),
                }
            }
            (CursorDraw::Surface, CursorImageStatus::Surface(surface)) if surface.is_alive() => {
                let hotspot = with_states(surface, |s| {
                    s.data_map
                        .get::<std::sync::Mutex<CursorImageAttributes>>()
                        .map(|a| a.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                let loc = (rel - hotspot.to_f64()).to_physical(scale).to_i32_round();
                out.extend(
                    render_elements_from_surface_tree(renderer, surface, loc, scale, 1.0, Kind::Cursor)
                        .into_iter()
                        .map(OutputElement::Surface),
                );
            }
            _ => {}
        }
    }
    let fullscreen = state.ws().fullscreen.is_some();

    let map = layer_map_for_output(output);
    let push_layers = |out: &mut Vec<OutputElement<R>>, renderer: &mut R, layer: Layer| {
        for surface in map.layers_on(layer).rev() {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            let loc = geo.loc.to_physical_precise_round(scale);
            out.extend(
                surface
                    .render_elements::<WaylandSurfaceRenderElement<R>>(renderer, loc, scale, 1.0)
                    .into_iter()
                    .map(OutputElement::Surface),
            );
        }
    };

    push_layers(&mut out, renderer, Layer::Overlay);
    if !fullscreen {
        push_layers(&mut out, renderer, Layer::Top);
    }

    let deco = &state.config.decoration;
    let focus = state.ws().focus.as_ref();
    for window in state.space.elements().rev() {
        let Some(loc) = state.space.element_location(window) else {
            continue;
        };
        let geo = window.geometry();
        let render_loc = (loc - geo.loc - output_geo.loc).to_physical_precise_round(scale);
        out.extend(
            window
                .render_elements::<WaylandSurfaceRenderElement<R>>(renderer, render_loc, scale, 1.0)
                .into_iter()
                .map(OutputElement::Surface),
        );

        if !fullscreen && deco.border_width > 0 && geo.size.w > 0 {
            let color = if Some(window) == focus {
                deco.border_active
            } else {
                deco.border_inactive
            };
            let rect = Rectangle::new(loc - output_geo.loc, geo.size);
            push_border(&mut out, window, rect, deco.border_width, color, scale);
        }
    }

    push_layers(&mut out, renderer, Layer::Bottom);
    push_layers(&mut out, renderer, Layer::Background);
    out
}

fn push_border<R>(
    out: &mut Vec<OutputElement<R>>,
    window: &Window,
    inner: Rectangle<i32, Logical>,
    width: i32,
    color: Color,
    scale: Scale<f64>,
) where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    window.user_data().insert_if_missing(|| {
        RefCell::new(BorderIds {
            ids: [Id::new(), Id::new(), Id::new(), Id::new()],
            commit: CommitCounter::default(),
            color,
        })
    });
    let mut ids = window.user_data().get::<RefCell<BorderIds>>().unwrap().borrow_mut();
    if ids.color != color {
        ids.color = color;
        ids.commit.increment();
    }

    let (x, y, w, h) = (inner.loc.x, inner.loc.y, inner.size.w, inner.size.h);
    let sides: [Rectangle<i32, Logical>; 4] = [
        Rectangle::new((x - width, y - width).into(), (w + 2 * width, width).into()), // top
        Rectangle::new((x - width, y + h).into(), (w + 2 * width, width).into()),     // bottom
        Rectangle::new((x - width, y).into(), (width, h).into()),                     // left
        Rectangle::new((x + w, y).into(), (width, h).into()),                         // right
    ];
    for (id, side) in ids.ids.iter().zip(sides) {
        let geo: Rectangle<i32, Physical> = side.to_physical_precise_round(scale);
        out.push(OutputElement::Solid(SolidColorRenderElement::new(
            id.clone(),
            geo,
            ids.commit,
            color,
            Kind::Unspecified,
        )));
    }
}

/// Tell clients they can draw their next frame, and do per-frame housekeeping.
pub fn send_frames(state: &mut State, output: &Output) {
    let time = state.start_time.elapsed();
    for window in state.space.elements() {
        window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
    }
    for layer in layer_map_for_output(output).layers() {
        layer.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
    }
    if let CursorImageStatus::Surface(surface) = &state.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    state.space.refresh();
    state.popups.cleanup();
}
