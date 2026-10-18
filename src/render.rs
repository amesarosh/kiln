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
        utils::{CommitCounter, with_renderer_surface_state},
    },
    desktop::{Window, layer_map_for_output},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Physical, Point, Rectangle, Scale, Transform},
    wayland::compositor::{SurfaceData, send_surface_state, with_states},
    wayland::fractional_scale::with_fractional_scale,
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
    // A turned-off monitor (only drawn when nested; on hardware it isn't rendered at all).
    if !state.monitor_powered(output) {
        push_fill(output, &state.lock_fill_ids[0], [0.0, 0.0, 0.0, 1.0], &mut out);
        return out;
    }
    if state.is_locked() {
        push_lock(state, renderer, output, scale, &mut out);
        return out;
    }

    let mon = state.monitor_index(output);
    let fullscreen = mon.is_some_and(|m| state.monitors[m].ws().fullscreen.is_some());

    // Each monitor has its own bar. Built before taking the layer map lock below, because
    // bar_rect() locks it too and the lock isn't re-entrant.
    let bar = mon.and_then(|m| {
        let rect = state.bar_rect(m)?;
        Some((rect, state.bar_buffer(m, output.current_scale().integer_scale())?))
    });

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

    if let Some((rect, buffer)) = bar {
        let loc = (rect.loc - output_geo.loc).to_f64().to_physical(scale);
        match MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, Kind::Unspecified)
        {
            Ok(e) => out.push(OutputElement::Memory(e)),
            Err(e) => tracing::warn!("bar upload failed: {e:?}"),
        }
    }
    if !fullscreen {
        push_layers(&mut out, renderer, Layer::Top);
    }

    let deco = &state.config.decoration;
    let focus = state.ws().focus.as_ref();
    for window in state.space.elements().rev() {
        let Some(loc) = state.space.element_location(window) else {
            continue;
        };
        // Windows on other monitors (a floating one may straddle two and is drawn on both).
        if !state.space.element_bbox(window).is_some_and(|b| b.overlaps(output_geo)) {
            continue;
        }
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

/// The lock screen: the locker's surface, or a plain fill until it has drawn one. The fill is
/// red when the locker died, so it's clear why the screen won't unlock.
fn push_lock<R>(state: &State, renderer: &mut R, output: &Output, scale: Scale<f64>, out: &mut Vec<OutputElement<R>>)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    if let Some(surface) = state.lock_surface(output)
        && with_renderer_surface_state(surface.wl_surface(), |s| s.buffer().is_some()).unwrap_or(false)
    {
        out.extend(
            render_elements_from_surface_tree(renderer, surface.wl_surface(), (0, 0), scale, 1.0, Kind::Unspecified)
                .into_iter()
                .map(OutputElement::Surface),
        );
        return;
    }
    let (id, color) = if state.locker_dead() {
        (&state.lock_fill_ids[1], state.config.theme.palette.lock_failed)
    } else {
        (&state.lock_fill_ids[0], state.config.theme.palette.locked)
    };
    push_fill(output, id, color, out);
}

/// Cover the whole output with one color.
fn push_fill<R>(output: &Output, id: &Id, color: Color, out: &mut Vec<OutputElement<R>>)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    let size = output.current_mode().map(|m| m.size).unwrap_or_default();
    let size = output.current_transform().transform_size(size);
    out.push(OutputElement::Solid(SolidColorRenderElement::new(
        id.clone(),
        Rectangle::from_size(size),
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )));
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

/// Tell clients on this output they can draw their next frame, and do per-frame housekeeping.
pub fn send_frames(state: &mut State, output: &Output) {
    let time = state.start_time.elapsed();
    let first_output = state.space.outputs().next() == Some(output);
    // Apps on this monitor render at its scale: the exact one if they support fractional scale,
    // otherwise rounded up and we shrink their buffers.
    let buffer_scale = output.current_scale().integer_scale();
    let fractional = output.current_scale().fractional_scale();
    let hint = |surface: &WlSurface, data: &SurfaceData| {
        send_surface_state(surface, data, buffer_scale, Transform::Normal);
        with_fractional_scale(data, |fs| fs.set_preferred_scale(fractional));
    };
    for window in state.space.elements() {
        // Paced by the monitor they're on. Windows on no monitor (not placed yet) use the first.
        let outputs = state.space.outputs_for_element(window);
        if outputs.contains(output) || (outputs.is_empty() && first_output) {
            window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }
        if main_output(state, window).as_ref() == Some(output) {
            window.with_surfaces(hint);
        }
    }
    if let Some(surface) = state.lock_surface(output) {
        let surface = surface.wl_surface();
        smithay::desktop::utils::send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
        smithay::desktop::utils::with_surfaces_surface_tree(surface, hint);
    }
    for layer in layer_map_for_output(output).layers() {
        layer.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
        layer.with_surfaces(hint);
    }
    if let CursorImageStatus::Surface(surface) = &state.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    state.space.refresh();
    state.popups.cleanup();
}

/// The monitor showing most of a window.
fn main_output(state: &State, window: &Window) -> Option<Output> {
    let geo = state.space.element_geometry(window)?;
    state.space.outputs_for_element(window).into_iter().max_by_key(|o| {
        state
            .space
            .output_geometry(o)
            .and_then(|g| g.intersection(geo))
            .map_or(0, |r| r.size.w as i64 * r.size.h as i64)
    })
}
