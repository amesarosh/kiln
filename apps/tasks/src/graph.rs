//! History graphs for the Performance page and the sidebar, drawn with the theme's colors.

use std::collections::VecDeque;

use kiln_config::theme::Palette;
use kiln_ui::{
    color,
    egui::{self, Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Sense, Stroke, Ui, pos2},
};

/// How many samples a graph keeps.
pub const HISTORY: usize = 60;

/// A fixed-length series of samples, oldest first.
#[derive(Clone, Default)]
pub struct Series(VecDeque<f32>);

impl Series {
    pub fn push(&mut self, v: f32) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(v);
    }

    pub fn max(&self) -> f32 {
        self.0.iter().copied().fold(0.0, f32::max)
    }
}

/// One line on a graph and how it's drawn.
pub struct Line<'a> {
    pub series: &'a Series,
    pub color: Color32,
    /// Shade the area under the line.
    pub fill: bool,
}

/// The corner rounding of a graph box: the theme's, but not so round that the lines poke
/// out of the corners.
pub fn radius(ui: &Ui) -> CornerRadius {
    let r = ui.visuals().widgets.noninteractive.corner_radius;
    CornerRadius::same(r.nw.min(10))
}

/// Draw `lines` in a framed box of `size`, from 0 at the bottom to `top` at the top, with the
/// newest sample at the right edge.
pub fn draw(ui: &mut Ui, p: &Palette, size: egui::Vec2, top: f32, lines: &[Line<'_>]) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    paint(ui.painter(), p, rect, radius(ui), top, lines, true);
    response
}

/// Draw `lines` into `rect`. `framed` adds the dark box, quarter lines and border; without it
/// the lines sit straight on whatever is behind (a sparkline).
pub fn paint(
    painter: &Painter,
    p: &Palette,
    rect: Rect,
    radius: CornerRadius,
    top: f32,
    lines: &[Line<'_>],
    framed: bool,
) {
    let painter = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    if framed {
        painter.rect_filled(rect, radius, color(p.base));
        // Quarter lines, so values can be judged by eye.
        for i in 1..4 {
            let y = rect.top() + rect.height() * i as f32 / 4.0;
            painter.line_segment(
                [pos2(rect.left(), y), pos2(rect.right(), y)],
                Stroke::new(1.0, color(p.separator).gamma_multiply(0.6)),
            );
        }
    }
    // Keep a zero line visible above the bottom edge.
    let plot = rect.shrink2(egui::vec2(0.0, 2.0));
    let top = top.max(f32::EPSILON);
    for line in lines {
        let points = points(plot, top, line.series);
        if points.len() < 2 {
            continue;
        }
        if line.fill {
            painter.add(area_mesh(&points, plot, line.color));
        }
        let last = *points.last().unwrap();
        painter.line(points, Stroke::new(if framed { 2.0 } else { 1.5 }, line.color));
        if framed {
            painter.circle_filled(last - egui::vec2(1.5, 0.0), 3.0, line.color);
        }
    }
    if framed {
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0, color(p.separator)),
            egui::StrokeKind::Inside,
        );
    }
}

fn points(rect: Rect, top: f32, series: &Series) -> Vec<Pos2> {
    let step = rect.width() / (HISTORY - 1) as f32;
    let start = HISTORY - series.0.len();
    series
        .0
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + (start + i) as f32 * step;
            let y = rect.bottom() - (v / top).clamp(0.0, 1.0) * rect.height();
            pos2(x, y)
        })
        .collect()
}

/// The area between the line and the bottom, as a strip of quads (the shape isn't convex),
/// fading out towards the bottom.
fn area_mesh(points: &[Pos2], plot: Rect, line: Color32) -> Mesh {
    let shade = |y: f32| {
        let up = ((plot.bottom() - y) / plot.height().max(1.0)).clamp(0.0, 1.0);
        line.gamma_multiply(0.06 + 0.34 * up)
    };
    let mut mesh = Mesh::default();
    for p in points {
        mesh.colored_vertex(*p, shade(p.y));
        mesh.colored_vertex(pos2(p.x, plot.bottom() + 2.0), shade(plot.bottom()));
    }
    for i in 0..points.len() as u32 - 1 {
        let (a, b, c, d) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
        mesh.add_triangle(a, b, c);
        mesh.add_triangle(b, d, c);
    }
    mesh
}
