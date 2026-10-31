//! The box drawn while dragging with the left button on the empty desktop. It doesn't select
//! anything; it's there because desktops have one.

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{Id, Kind, solid::SolidColorRenderElement},
        utils::CommitCounter,
    },
    output::Output,
    utils::{Logical, Physical, Point, Rectangle, Scale},
};

use crate::{config::Color, render::OutputElement, state::State};

pub struct DragBox {
    start: Point<f64, Logical>,
    end: Point<f64, Logical>,
    /// The monitor it started on; the box stays inside it.
    bounds: Rectangle<f64, Logical>,
    /// Damage-tracking ids: [inside, top, bottom, left, right].
    ids: [Id; 5],
}

impl DragBox {
    /// The box in global coordinates.
    fn rect(&self) -> Rectangle<f64, Logical> {
        let (x0, x1) = (self.start.x.min(self.end.x), self.start.x.max(self.end.x));
        let (y0, y1) = (self.start.y.min(self.end.y), self.start.y.max(self.end.y));
        Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())
    }
}

impl State {
    /// Start a box at `pos`, which should be on empty desktop.
    pub fn drag_box_start(&mut self, pos: Point<f64, Logical>) {
        let Some(bounds) = self
            .monitor_at(pos)
            .and_then(|m| self.space.output_geometry(&self.monitors[m].output))
        else {
            return;
        };
        self.drag_box = Some(DragBox {
            start: pos,
            end: pos,
            bounds: bounds.to_f64(),
            ids: std::array::from_fn(|_| Id::new()),
        });
    }

    /// Stretch the box to the pointer. False when there's no box.
    pub fn drag_box_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(b) = &mut self.drag_box else {
            return false;
        };
        let max = b.bounds.loc + b.bounds.size.to_point();
        b.end = (pos.x.clamp(b.bounds.loc.x, max.x), pos.y.clamp(b.bounds.loc.y, max.y)).into();
        self.needs_redraw = true;
        true
    }

    /// Drop the box. False when there wasn't one.
    pub fn drag_box_end(&mut self) -> bool {
        let had = self.drag_box.take().is_some();
        self.needs_redraw |= had;
        had
    }
}

/// Draw the box, if there is one on `output`: its edge, then its inside (front to back).
pub fn push_elements<R>(state: &State, output: &Output, scale: Scale<f64>, out: &mut Vec<OutputElement<R>>)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    let Some(b) = &state.drag_box else { return };
    let Some(output_geo) = state.space.output_geometry(output) else {
        return;
    };
    let rect = b.rect();
    if rect.size.w < 1.0 || rect.size.h < 1.0 || !rect.overlaps(output_geo.to_f64()) {
        return;
    }
    let rel = Rectangle::new(rect.loc - output_geo.loc.to_f64(), rect.size);
    let r: Rectangle<i32, Physical> = rel.to_physical(scale).to_i32_round();
    // A one logical pixel edge, at least one physical pixel wide.
    let e = (scale.x.round() as i32).max(1);
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    let edges = [
        Rectangle::new((x, y).into(), (w, e).into()),
        Rectangle::new((x, y + h - e).into(), (w, e).into()),
        Rectangle::new((x, y).into(), (e, h).into()),
        Rectangle::new((x + w - e, y).into(), (e, h).into()),
    ];
    let palette = &state.config.theme.palette;
    let solid = |id: &Id, geo, color| {
        OutputElement::Solid(SolidColorRenderElement::new(
            id.clone(),
            geo,
            CommitCounter::default(),
            premultiplied(color),
            Kind::Unspecified,
        ))
    };
    for (id, geo) in b.ids[1..].iter().zip(edges) {
        out.push(solid(id, geo, palette.drag_box_border));
    }
    out.push(solid(&b.ids[0], r, palette.drag_box));
}

/// Theme colors are plain RGBA; solid elements want the color already multiplied by its alpha.
fn premultiplied([r, g, b, a]: Color) -> Color {
    [r * a, g * a, b * a, a]
}
