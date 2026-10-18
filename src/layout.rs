//! Pure tiling math. No Wayland types here so it stays easy to test and extend.

use serde::Deserialize;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LayoutKind {
    /// Master area on the left, remaining windows stacked on the right.
    #[default]
    Tile,
    /// Every window takes the full area; only the focused one is visible.
    Monocle,
}

impl LayoutKind {
    pub fn next(self) -> Self {
        match self {
            Self::Tile => Self::Monocle,
            Self::Monocle => Self::Tile,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub master_ratio: f32,
    pub master_count: usize,
    pub gaps_inner: i32,
    pub gaps_outer: i32,
    pub smart_gaps: bool,
}

/// Compute one rectangle per tiled window, in order.
pub fn arrange(kind: LayoutKind, n: usize, area: Rect, p: &Params) -> Vec<Rect> {
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
        LayoutKind::Tile => tile(n, area, p, inner),
    }
}

fn tile(n: usize, area: Rect, p: &Params, gap: i32) -> Vec<Rect> {
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
        split_vertical(col, masters, gap, &mut out);
    }
    if let Some(col) = stack_col {
        split_vertical(col, stack, gap, &mut out);
    }
    out
}

/// Split a column into `count` rows separated by `gap`, giving leftover pixels to the last row.
fn split_vertical(col: Rect, count: usize, gap: i32, out: &mut Vec<Rect>) {
    if count == 0 {
        return;
    }
    let count_i = count as i32;
    let usable = (col.h - gap * (count_i - 1)).max(count_i);
    let each = usable / count_i;
    let mut y = col.y;
    for i in 0..count_i {
        let h = if i == count_i - 1 { col.y + col.h - y } else { each };
        out.push(Rect::new(col.x, y, col.w, h.max(1)));
        y += each + gap;
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
        assert!(arrange(LayoutKind::Tile, 0, AREA, &params()).is_empty());
    }

    #[test]
    fn single_window_fills_area_minus_outer_gap() {
        let r = arrange(LayoutKind::Tile, 1, AREA, &params());
        assert_eq!(r, vec![Rect::new(10, 10, 980, 780)]);
    }

    #[test]
    fn smart_gaps_removes_gaps_for_single_window() {
        let p = Params {
            smart_gaps: true,
            ..params()
        };
        assert_eq!(arrange(LayoutKind::Tile, 1, AREA, &p), vec![AREA]);
    }

    #[test]
    fn master_and_stack() {
        let r = arrange(LayoutKind::Tile, 3, AREA, &params());
        // usable width 980, minus 10 gap = 970, half = 485
        assert_eq!(r[0], Rect::new(10, 10, 485, 780));
        assert_eq!(r[1], Rect::new(505, 10, 485, 385));
        assert_eq!(r[2], Rect::new(505, 405, 485, 385));
    }

    #[test]
    fn rows_fill_column_exactly() {
        let mut out = Vec::new();
        split_vertical(Rect::new(0, 0, 100, 101), 3, 5, &mut out);
        let last = out.last().unwrap();
        assert_eq!(last.y + last.h, 101);
    }

    #[test]
    fn zero_masters_means_all_stack() {
        let p = Params {
            master_count: 0,
            ..params()
        };
        let r = arrange(LayoutKind::Tile, 2, AREA, &p);
        assert_eq!(r[0].w, 980);
        assert_eq!(r[1].w, 980);
    }

    #[test]
    fn monocle_gives_everyone_the_full_area() {
        let r = arrange(LayoutKind::Monocle, 3, AREA, &params());
        assert!(r.iter().all(|x| *x == Rect::new(10, 10, 980, 780)));
    }
}
