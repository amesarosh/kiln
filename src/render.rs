//! Builds the list of things to draw for an output, front to back.

use std::{cell::RefCell, time::Duration};

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Element, Id, Kind,
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
    /// Windows drawn smaller, as overview thumbnails.
    Rescaled=smithay::backend::renderer::element::utils::RescaleRenderElement<WaylandSurfaceRenderElement<R>>,
}

render_elements! {
    /// What a backend actually draws: the scene, with colour-managed surfaces converted, and on
    /// an HDR monitor the encoded frame.
    pub FrameElement<=smithay::backend::renderer::gles::GlesRenderer>;
    Plain=OutputElement<smithay::backend::renderer::gles::GlesRenderer>,
    Color=crate::hdr::ColorElement,
    Encode=crate::hdr::HdrEncodeElement,
}

/// The scene for `output`, ready to draw onto `target`: surfaces that described their colours
/// are converted for it.
pub fn frame_elements(
    state: &State,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    output: &Output,
    cursor: CursorDraw<'_>,
    target: crate::color::Target,
) -> Vec<FrameElement> {
    let elements = output_elements(state, renderer, output, cursor);
    let described = state.color_described_surfaces();
    if described.is_empty() {
        return elements.into_iter().map(FrameElement::Plain).collect();
    }
    let hdr = target == crate::color::Target::HdrIntermediate;
    elements
        .into_iter()
        .map(|e| {
            let Some((_, desc)) = described.iter().find(|(id, _)| id == e.id()) else {
                return FrameElement::Plain(e);
            };
            let params = crate::color::ConvertParams::new(desc, target);
            crate::hdr::ColorElement::wrap(renderer, e, &params, hdr)
        })
        .collect()
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
    let pointer = state.pointer().current_location();
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
    // A drag icon, just under the pointer. Drawn on every monitor it may overlap.
    if let Some(icon) = state.dnd_icon.as_ref().filter(|i| i.surface.is_alive()) {
        let loc = (pointer - output_geo.loc.to_f64() + icon.offset.to_f64())
            .to_physical(scale)
            .to_i32_round();
        out.extend(
            render_elements_from_surface_tree(renderer, &icon.surface, loc, scale, 1.0, Kind::Unspecified)
                .into_iter()
                .map(OutputElement::Surface),
        );
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

    // Pop-up menus, above everything but the pointer.
    if let Some((loc, buffer)) = state.menu_buffer(output, output.current_scale().integer_scale()) {
        let loc = (loc - output_geo.loc).to_f64().to_physical(scale);
        match MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, Kind::Unspecified)
        {
            Ok(e) => out.push(OutputElement::Memory(e)),
            Err(e) => tracing::warn!("menu upload failed: {e:?}"),
        }
    }

    if let Some((loc, buffer)) = state.switcher_buffer(output, output.current_scale().integer_scale()) {
        let loc = (loc - output_geo.loc).to_f64().to_physical(scale);
        match MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, Kind::Unspecified)
        {
            Ok(e) => out.push(OutputElement::Memory(e)),
            Err(e) => tracing::warn!("switcher upload failed: {e:?}"),
        }
    }

    if let Some((loc, buffer, alpha)) = state.volume_osd_buffer(output, output.current_scale().integer_scale()) {
        let loc = (loc - output_geo.loc).to_f64().to_physical(scale);
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            loc,
            &buffer,
            Some(alpha),
            None,
            None,
            Kind::Unspecified,
        ) {
            Ok(e) => out.push(OutputElement::Memory(e)),
            Err(e) => tracing::warn!("volume panel upload failed: {e:?}"),
        }
    }

    // The overview covers the monitor; its backdrop goes over the bar and panels, and the
    // windows are drawn as its thumbnails instead.
    let in_overview = crate::overview::push_elements(state, renderer, output, &mut out);

    for (loc, buffer) in state.notification_buffers(output, output.current_scale().integer_scale()) {
        let loc = (loc - output_geo.loc).to_f64().to_physical(scale);
        match MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, Kind::Unspecified)
        {
            Ok(e) => out.push(OutputElement::Memory(e)),
            Err(e) => tracing::warn!("notification upload failed: {e:?}"),
        }
    }

    let mon = state.monitor_index(output);
    let fullscreen = mon.is_some_and(|m| state.monitors[m].ws().fullscreen.is_some()) && !in_overview;

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

    // X11 menus and tooltips, topmost last in the list.
    for x in state.x11_unmanaged.iter().rev() {
        let geo = x.geometry();
        if !geo.overlaps(output_geo) {
            continue;
        }
        let loc = (geo.loc - output_geo.loc).to_physical_precise_round(scale);
        out.extend(
            x.render_elements::<WaylandSurfaceRenderElement<R>>(renderer, loc, scale, 1.0)
                .into_iter()
                .map(OutputElement::Surface),
        );
    }

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
    // A dragged tile is drawn above everything else.
    let dragged = state.anims.dragged.as_ref().map(|d| &d.window);
    for window in dragged
        .into_iter()
        .chain(state.space.elements().rev().filter(|w| Some(*w) != dragged))
        .filter(|_| !in_overview)
    {
        // Where it's drawn this frame, which differs from where it is while sliding.
        let Some(loc) = state.visual_location(window) else {
            continue;
        };
        // Windows on other monitors (a floating one may straddle two and is drawn on both).
        // One sliding in with a workspace switch is only drawn on its own monitor.
        match state.switching_onto(window) {
            Some(o) if o != output => continue,
            Some(_) => {}
            None if !state.visual_bbox(window).is_some_and(|b| b.overlaps(output_geo)) => continue,
            None => {}
        }
        let alpha = state.visual_alpha(window);
        let geo = window.geometry();
        let render_loc = (loc - geo.loc.to_f64() - output_geo.loc.to_f64()).to_physical_precise_round(scale);
        out.extend(
            window
                .render_elements::<WaylandSurfaceRenderElement<R>>(
                    renderer,
                    render_loc,
                    scale,
                    crate::state::window_opacity(window) * alpha,
                )
                .into_iter()
                .map(OutputElement::Surface),
        );

        if !fullscreen && deco.border_width > 0 && geo.size.w > 0 {
            let color = if Some(window) == focus {
                deco.border_active
            } else {
                deco.border_inactive
            };
            let rect = Rectangle::new(loc.to_i32_round() - output_geo.loc, geo.size);
            push_border(&mut out, window, rect, deco.border_width, faded(color, alpha), scale);
        }
    }

    // The workspace being left, sliding or fading out, below the one coming in.
    for (window, loc, alpha) in state.outgoing_windows(output).into_iter().filter(|_| !in_overview) {
        let geo = window.geometry();
        let render_loc = (loc - geo.loc.to_f64() - output_geo.loc.to_f64()).to_physical_precise_round(scale);
        out.extend(
            window
                .render_elements::<WaylandSurfaceRenderElement<R>>(
                    renderer,
                    render_loc,
                    scale,
                    crate::state::window_opacity(&window) * alpha,
                )
                .into_iter()
                .map(OutputElement::Surface),
        );
        if deco.border_width > 0 && geo.size.w > 0 {
            let rect = Rectangle::new(loc.to_i32_round() - output_geo.loc, geo.size);
            push_border(
                &mut out,
                &window,
                rect,
                deco.border_width,
                faded(deco.border_inactive, alpha),
                scale,
            );
        }
    }

    // On the desktop, under the windows.
    if !in_overview {
        crate::drag_box::push_elements(state, output, scale, &mut out);
    }
    push_layers(&mut out, renderer, Layer::Bottom);
    push_layers(&mut out, renderer, Layer::Background);
    push_wallpaper(state, renderer, output, output_geo.size, &mut out);
    out
}

/// The wallpaper, under everything, covering the monitor.
fn push_wallpaper<R>(
    state: &State,
    renderer: &mut R,
    output: &Output,
    size: smithay::utils::Size<i32, Logical>,
    out: &mut Vec<OutputElement<R>>,
) where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let names: Vec<String> = state.space.outputs().map(|o| o.name()).collect();
    state.wallpapers.retain_outputs(&names);
    let want = crate::wallpaper::Want::for_output(state, output);
    let Some(buffer) = state.wallpapers.buffer(&state.loop_handle, &output.name(), want) else {
        return;
    };
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (0.0, 0.0),
        &buffer,
        None,
        None,
        Some(size),
        Kind::Unspecified,
    ) {
        Ok(e) => out.push(OutputElement::Memory(e)),
        Err(e) => tracing::warn!("wallpaper upload failed: {e:?}"),
    }
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

/// A color made more see-through, premultiplied the way solid elements are drawn.
fn faded(c: Color, alpha: f32) -> Color {
    [c[0] * alpha, c[1] * alpha, c[2] * alpha, c[3] * alpha]
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
    // Thumbnails in the overview stay live, even of workspaces not shown.
    for window in state.overview_windows_on(output) {
        if state.space.element_location(&window).is_none() {
            window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
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
    // X11 menus and tooltips. Xwayland holds back new content until told to draw, so without
    // this a menu stays stuck on its first (often blank) frame.
    let output_geo = state.space.output_geometry(output);
    for x in &state.x11_unmanaged {
        let Some(surface) = x.wl_surface() else {
            continue;
        };
        if output_geo.is_some_and(|o| o.overlaps(x.geometry())) {
            smithay::desktop::utils::send_frames_surface_tree(&surface, output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
            smithay::desktop::utils::with_surfaces_surface_tree(&surface, hint);
        }
    }
    if let Some(icon) = &state.dnd_icon {
        smithay::desktop::utils::send_frames_surface_tree(&icon.surface, output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    state.space.refresh();
    state.popups.cleanup();
}

/// Collect the presentation feedback requests of everything drawn on `output` this frame, to
/// be answered once the frame is on screen. `states` says which surfaces went straight to the
/// display without a copy.
pub fn take_presentation_feedback(
    state: &State,
    output: &Output,
    states: &smithay::backend::renderer::element::RenderElementStates,
) -> smithay::desktop::utils::OutputPresentationFeedback {
    use smithay::desktop::utils::{
        OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
        take_presentation_feedback_surface_tree,
    };
    let mut feedback = OutputPresentationFeedback::new(output);
    let on_output = |_: &WlSurface, _: &SurfaceData| Some(output.clone());
    let flags = |s: &WlSurface, _: &SurfaceData| surface_presentation_feedback_flags_from_states(s, states);
    // A window on two monitors is answered by the one showing most of it.
    for window in state.space.elements() {
        if main_output(state, window).as_ref() == Some(output) {
            window.take_presentation_feedback(&mut feedback, on_output, flags);
        }
    }
    for layer in layer_map_for_output(output).layers() {
        layer.take_presentation_feedback(&mut feedback, on_output, flags);
    }
    if let Some(lock) = state.lock_surface(output) {
        take_presentation_feedback_surface_tree(lock.wl_surface(), &mut feedback, on_output, flags);
    }
    feedback
}

/// How long one frame lasts on `output`, for presentation feedback.
pub fn refresh_of(output: &Output) -> smithay::wayland::presentation::Refresh {
    match output.current_mode().map(|m| m.refresh) {
        Some(mhz) if mhz > 0 => {
            smithay::wayland::presentation::Refresh::fixed(Duration::from_nanos(1_000_000_000_000 / mhz as u64))
        }
        _ => smithay::wayland::presentation::Refresh::Unknown,
    }
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
