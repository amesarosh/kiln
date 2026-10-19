//! Pure tiling math. No Wayland types here so it stays easy to test and extend.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Shrink on every side by `by` pixels, never going below 1x1.
    pub fn inset(self, by: i32) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            w: (self.w - 2 * by).max(1),
            h: (self.h - 2 * by).max(1),
        }
    }

    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && y >= self.y as f64 && x < (self.x + self.w) as f64 && y < (self.y + self.h) as f64
    }
}

pub use wt_config::LayoutKind;

#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub master_ratio: f32,
    pub master_count: usize,
    pub gaps_inner: i32,
    pub gaps_outer: i32,
    pub smart_gaps: bool,
}

/// The tiling settings from the config, with negative gaps treated as none.
pub fn params(c: &wt_config::Config) -> Params {
    Params {
        master_ratio: c.layout.master_ratio,
        master_count: c.layout.master_count,
        gaps_inner: c.layout.gaps_inner.max(0),
        gaps_outer: c.layout.gaps_outer.max(0),
        smart_gaps: c.layout.smart_gaps,
    }
}

/// Compute one rectangle per tiled window, in order. `weights` has one entry per window:
/// its share of its column's height (all 1.0 means equal rows).
pub fn arrange(kind: LayoutKind, weights: &[f32], area: Rect, p: &Params) -> Vec<Rect> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let single = n == 1 || kind == LayoutKind::Monocle;
    let (outer, inner) = if p.smart_gaps && single {
        (0, 0)
    } else {
        (p.gaps_outer, p.gaps_inner)
    };
    let area = area.inset(outer);

    match kind {
        LayoutKind::Monocle => vec![area; n],
        LayoutKind::Tile => tile(weights, area, p, inner),
    }
}

fn tile(weights: &[f32], area: Rect, p: &Params, gap: i32) -> Vec<Rect> {
    let n = weights.len();
    let masters = p.master_count.min(n);
    let stack = n - masters;

    // Split the area into a master column and a stack column.
    let (master_col, stack_col) = if masters == 0 {
        (None, Some(area))
    } else if stack == 0 {
        (Some(area), None)
    } else {
        let ratio = p.master_ratio.clamp(0.05, 0.95);
        let mw = ((area.w - gap) as f32 * ratio).round() as i32;
        (
            Some(Rect::new(area.x, area.y, mw, area.h)),
            Some(Rect::new(area.x + mw + gap, area.y, area.w - mw - gap, area.h)),
        )
    };

    let mut out = Vec::with_capacity(n);
    if let Some(col) = master_col {
        split_vertical(col, &weights[..masters], gap, &mut out);
    }
    if let Some(col) = stack_col {
        split_vertical(col, &weights[masters..], gap, &mut out);
    }
    out
}

/// Split a column into rows sized by `weights`, separated by `gap`. The last row takes
/// whatever is left, so the column is filled exactly.
fn split_vertical(col: Rect, weights: &[f32], gap: i32, out: &mut Vec<Rect>) {
    if weights.is_empty() {
        return;
    }
    let count = weights.len() as i32;
    let usable = (col.h - gap * (count - 1)).max(count);
    let total: f32 = weights.iter().map(|w| w.max(0.0)).sum();
    let mut y = col.y;
    for (i, w) in weights.iter().enumerate() {
        let h = if i as i32 == count - 1 {
            col.y + col.h - y
        } else if total > 0.0 {
            (usable as f32 * w.max(0.0) / total).round() as i32
        } else {
            usable / count
        };
        let h = h.max(1);
        out.push(Rect::new(col.x, y, col.w, h));
        y += h + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            master_ratio: 0.5,
            master_count: 1,
            gaps_inner: 10,
            gaps_outer: 10,
            smart_gaps: false,
        }
    }

    const AREA: Rect = Rect::new(0, 0, 1000, 800);

    #[test]
    fn empty() {
        assert!(arrange(LayoutKind::Tile, &[1.0; 0], AREA, &params()).is_empty());
    }

    #[test]
    fn single_window_fills_area_minus_outer_gap() {
        let r = arrange(LayoutKind::Tile, &[1.0; 1], AREA, &params());
        assert_eq!(r, vec![Rect::new(10, 10, 980, 780)]);
    }

    #[test]
    fn smart_gaps_removes_gaps_for_single_window() {
        let p = Params {
            smart_gaps: true,
            ..params()
        };
        assert_eq!(arrange(LayoutKind::Tile, &[1.0; 1], AREA, &p), vec![AREA]);
    }

    #[test]
    fn master_and_stack() {
        let r = arrange(LayoutKind::Tile, &[1.0; 3], AREA, &params());
        // usable width 980, minus 10 gap = 970, half = 485
        assert_eq!(r[0], Rect::new(10, 10, 485, 780));
        assert_eq!(r[1], Rect::new(505, 10, 485, 385));
        assert_eq!(r[2], Rect::new(505, 405, 485, 385));
    }

    #[test]
    fn rows_fill_column_exactly() {
        let mut out = Vec::new();
        split_vertical(Rect::new(0, 0, 100, 101), &[1.0; 3], 5, &mut out);
        let last = out.last().unwrap();
        assert_eq!(last.y + last.h, 101);
    }

    #[test]
    fn zero_masters_means_all_stack() {
        let p = Params {
            master_count: 0,
            ..params()
        };
        let r = arrange(LayoutKind::Tile, &[1.0; 2], AREA, &p);
        assert_eq!(r[0].w, 980);
        assert_eq!(r[1].w, 980);
    }

    #[test]
    fn weights_share_a_column() {
        // Stack column: 3 rows, the first twice as tall. Usable height 780 - 2*10 = 760.
        let r = arrange(LayoutKind::Tile, &[1.0, 2.0, 1.0, 1.0], AREA, &params());
        assert_eq!(r[1].h, 380);
        assert_eq!(r[2].h, 190);
        assert_eq!(r[3].y + r[3].h, 790, "column still filled exactly");
        assert_eq!(r[2].y, r[1].y + r[1].h + 10);
    }

    #[test]
    fn monocle_gives_everyone_the_full_area() {
        let r = arrange(LayoutKind::Monocle, &[1.0; 3], AREA, &params());
        assert!(r.iter().all(|x| *x == Rect::new(10, 10, 980, 780)));
    }
}
