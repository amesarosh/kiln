//! History graphs for the Performance page, drawn with the theme's colors.

use std::collections::VecDeque;

use wt_config::theme::Palette;
use wt_ui::{
    color,
    egui::{self, Color32, Mesh, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2},
};

/// How many samples a graph keeps.
pub const HISTORY: usize = 60;

/// A fixed-length series of samples, oldest first.
#[derive(Default)]
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

/// Draw `lines` in a box of `size`, from 0 at the bottom to `top` at the top, with the newest
/// sample at the right edge.
pub fn draw(ui: &mut Ui, p: &Palette, size: egui::Vec2, top: f32, lines: &[Line<'_>]) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let radius = ui.visuals().widgets.noninteractive.corner_radius;
    painter.rect_filled(rect, radius, color(p.base));
    // Quarter lines, so values can be judged by eye.
    for i in 1..4 {
        let y = rect.top() + rect.height() * i as f32 / 4.0;
        painter.line_segment(
            [pos2(rect.left(), y), pos2(rect.right(), y)],
            Stroke::new(1.0, color(p.separator).gamma_multiply(0.5)),
        );
    }
    let top = top.max(f32::EPSILON);
    for line in lines {
        let points = points(rect, top, line.series);
        if points.len() < 2 {
            continue;
        }
        if line.fill {
            painter.add(area_mesh(&points, rect.bottom(), line.color.gamma_multiply(0.25)));
        }
        painter.line(points, Stroke::new(1.5, line.color));
    }
    painter.rect_stroke(rect, radius, Stroke::new(1.0, color(p.separator)), egui::StrokeKind::Inside);
    response
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
            let y = rect.bottom() - (v / top).clamp(0.0, 1.0) * (rect.height() - 1.0);
            pos2(x, y)
        })
        .collect()
}

/// The area between the line and the bottom, as a strip of quads (the shape isn't convex).
fn area_mesh(points: &[Pos2], bottom: f32, fill: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    for p in points {
        mesh.colored_vertex(*p, fill);
        mesh.colored_vertex(pos2(p.x, bottom), fill);
    }
    for i in 0..points.len() as u32 - 1 {
        let (a, b, c, d) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
        mesh.add_triangle(a, b, c);
        mesh.add_triangle(b, d, c);
    }
    mesh
}

/// A small filled bar showing `frac` (0-1), for per-core use.
pub fn meter(ui: &mut Ui, p: &Palette, width: f32, frac: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, color(p.base));
    let mut filled = rect;
    filled.set_width(rect.width() * frac.clamp(0.0, 1.0));
    painter.rect_filled(filled, 3.0, color(p.accent));
}
