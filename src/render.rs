//! Builds the list of things to draw for an output, front to back.

use std::cell::RefCell;

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Id, Kind, render_elements, solid::SolidColorRenderElement,
            surface::WaylandSurfaceRenderElement,
        },
        utils::CommitCounter,
    },
    desktop::{Window, layer_map_for_output},
    output::Output,
    utils::{Logical, Physical, Rectangle, Scale},
    wayland::shell::wlr_layer::Layer,
};

use crate::{config::Color, state::State};

render_elements! {
    pub OutputElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
}

/// Stable ids per window so the damage tracker only repaints borders that changed.
struct BorderIds {
    ids: [Id; 4],
    commit: CommitCounter,
    color: Color,
}

pub fn output_elements<R>(state: &State, renderer: &mut R, output: &Output) -> Vec<OutputElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    let mut out = Vec::new();
    let Some(output_geo) = state.space.output_geometry(output) else {
        return out;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());
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
