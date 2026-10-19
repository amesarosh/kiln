//! Per-workspace window bookkeeping: order, focus, floating and fullscreen state.
//!
//! Generic over the window type so the logic can be tested without a compositor.

use crate::{
    config::Dir,
    layout::{LayoutKind, Rect},
};

#[derive(Debug, Clone)]
pub struct Entry<W> {
    pub window: W,
    /// `Some` when floating. Size of zero means "not placed yet, center once the client picks a size".
    pub floating: Option<Rect>,
    /// Share of its tiling column's height; 1.0 is an equal share. Changed by Mod+right-drag.
    pub weight: f32,
}

#[derive(Debug, Clone)]
pub struct Workspace<W> {
    /// Tiling order: the first tiled entry is the master.
    pub entries: Vec<Entry<W>>,
    pub focus: Option<W>,
    pub fullscreen: Option<W>,
    pub layout: LayoutKind,
    pub master_ratio: f32,
    pub master_count: usize,
}

impl<W: Clone + PartialEq> Workspace<W> {
    pub fn new(layout: LayoutKind, master_ratio: f32, master_count: usize) -> Self {
        Self {
            entries: Vec::new(),
            focus: None,
            fullscreen: None,
            layout,
            master_ratio,
            master_count,
        }
    }

    pub fn contains(&self, w: &W) -> bool {
        self.index_of(w).is_some()
    }

    fn index_of(&self, w: &W) -> Option<usize> {
        self.entries.iter().position(|e| &e.window == w)
    }

    pub fn entry(&self, w: &W) -> Option<&Entry<W>> {
        self.entries.iter().find(|e| &e.window == w)
    }

    pub fn entry_mut(&mut self, w: &W) -> Option<&mut Entry<W>> {
        self.entries.iter_mut().find(|e| &e.window == w)
    }

    pub fn is_floating(&self, w: &W) -> bool {
        self.entry(w).is_some_and(|e| e.floating.is_some())
    }

    /// Height shares of the tiled windows, in tiling order.
    pub fn tiled_weights(&self) -> Vec<f32> {
        self.entries
            .iter()
            .filter(|e| e.floating.is_none())
            .map(|e| e.weight)
            .collect()
    }

    pub fn tiled(&self) -> impl Iterator<Item = &W> {
        self.entries.iter().filter(|e| e.floating.is_none()).map(|e| &e.window)
    }

    pub fn floating(&self) -> impl Iterator<Item = (&W, Rect)> {
        self.entries.iter().filter_map(|e| e.floating.map(|r| (&e.window, r)))
    }

    pub fn add(&mut self, window: W, at_master: bool, floating: Option<Rect>) {
        let entry = Entry {
            window: window.clone(),
            floating,
            weight: 1.0,
        };
        if at_master {
            self.entries.insert(0, entry);
        } else {
            self.entries.push(entry);
        }
        self.focus = Some(window);
    }

    /// Insert a window at a spot in the list (clamped to the end) and focus it.
    pub fn add_at(&mut self, window: W, index: usize, floating: Option<Rect>, weight: f32) {
        let entry = Entry {
            window: window.clone(),
            floating,
            weight,
        };
        self.entries.insert(index.min(self.entries.len()), entry);
        self.focus = Some(window);
    }

    /// Remove a window, moving focus to a neighbour. Returns false if it wasn't here.
    pub fn remove(&mut self, w: &W) -> bool {
        let Some(idx) = self.index_of(w) else { return false };
        self.entries.remove(idx);
        if self.fullscreen.as_ref() == Some(w) {
            self.fullscreen = None;
        }
        if self.focus.as_ref() == Some(w) {
            let next = idx.min(self.entries.len().saturating_sub(1));
            self.focus = self.entries.get(next).map(|e| e.window.clone());
        }
        true
    }

    pub fn focus_dir(&mut self, dir: Dir) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        let cur = self.focus.as_ref().and_then(|f| self.index_of(f)).unwrap_or(0);
        let next = step(cur, n, dir);
        self.focus = Some(self.entries[next].window.clone());
    }

    /// Move the focused tiled window one slot through the tiled order (wrapping).
    pub fn swap_dir(&mut self, dir: Dir) {
        let Some(focus) = self.focus.clone() else { return };
        let tiled: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].floating.is_none())
            .collect();
        let Some(pos) = tiled.iter().position(|&i| self.entries[i].window == focus) else {
            return;
        };
        if tiled.len() < 2 {
            return;
        }
        let other = tiled[step(pos, tiled.len(), dir)];
        self.entries.swap(tiled[pos], other);
    }

    /// Two windows trade places. The slots keep their own height shares, so the layout
    /// doesn't change size, only which window sits where.
    pub fn swap_windows(&mut self, a: &W, b: &W) {
        let (Some(i), Some(j)) = (self.index_of(a), self.index_of(b)) else {
            return;
        };
        let wa = self.entries[i].window.clone();
        self.entries[i].window = self.entries[j].window.clone();
        self.entries[j].window = wa;
    }

    /// Move a window to just before `target` in the order.
    pub fn move_before(&mut self, w: &W, target: &W) {
        if w == target || !self.contains(target) {
            return;
        }
        let Some(i) = self.index_of(w) else { return };
        let entry = self.entries.remove(i);
        let j = self.index_of(target).unwrap_or(self.entries.len());
        self.entries.insert(j, entry);
    }

    /// Swap the focused tiled window with the master. If it already is the master, swap with the next one.
    pub fn zoom(&mut self) {
        let Some(focus) = self.focus.clone() else { return };
        let tiled: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].floating.is_none())
            .collect();
        let Some(pos) = tiled.iter().position(|&i| self.entries[i].window == focus) else {
            return;
        };
        let target = match pos {
            0 if tiled.len() > 1 => tiled[1],
            0 => return,
            _ => tiled[0],
        };
        self.entries.swap(tiled[pos], target);
        // Keep focus on the window that is now master.
        self.focus = Some(self.entries[tiled[0]].window.clone());
    }
}

/// Move every window from `from` into the same-numbered workspace of `into` (extras go to the
/// last one), e.g. when a monitor is unplugged. Placed floating windows are shifted by `offset`.
pub fn merge_workspaces<W: Clone + PartialEq>(into: &mut [Workspace<W>], from: Vec<Workspace<W>>, offset: (i32, i32)) {
    let Some(last) = into.len().checked_sub(1) else { return };
    for (i, ws) in from.into_iter().enumerate() {
        let target = &mut into[i.min(last)];
        let focus_before = target.focus.clone();
        for mut e in ws.entries {
            if let Some(r) = &mut e.floating
                && r.w > 0
            {
                r.x += offset.0;
                r.y += offset.1;
            }
            target.add(e.window, false, e.floating);
        }
        // Keep what the target had focused; otherwise use what was focused on the old monitor.
        target.focus = focus_before
            .or(ws.focus)
            .or_else(|| target.entries.first().map(|e| e.window.clone()));
    }
}

/// Index one step from `cur` in a list of `n`, wrapping around.
pub fn step(cur: usize, n: usize, dir: Dir) -> usize {
    match dir {
        Dir::Next => (cur + 1) % n,
        Dir::Prev => (cur + n - 1) % n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(ids: &[u32]) -> Workspace<u32> {
        let mut w = Workspace::new(LayoutKind::Tile, 0.5, 1);
        for &id in ids {
            w.add(id, false, None);
        }
        w
    }

    fn order(w: &Workspace<u32>) -> Vec<u32> {
        w.entries.iter().map(|e| e.window).collect()
    }

    #[test]
    fn add_at_master_and_stack() {
        let mut w = ws(&[1, 2]);
        w.add(3, true, None);
        assert_eq!(order(&w), vec![3, 1, 2]);
        assert_eq!(w.focus, Some(3));
    }

    #[test]
    fn remove_moves_focus_to_neighbour() {
        let mut w = ws(&[1, 2, 3]);
        w.focus = Some(3);
        w.remove(&3);
        assert_eq!(w.focus, Some(2));
        w.focus = Some(1);
        w.remove(&1);
        assert_eq!(w.focus, Some(2));
        w.remove(&2);
        assert_eq!(w.focus, None);
        assert!(!w.remove(&99));
    }

    #[test]
    fn focus_wraps() {
        let mut w = ws(&[1, 2, 3]);
        w.focus = Some(3);
        w.focus_dir(Dir::Next);
        assert_eq!(w.focus, Some(1));
        w.focus_dir(Dir::Prev);
        assert_eq!(w.focus, Some(3));
    }

    #[test]
    fn swap_skips_floating() {
        let mut w = ws(&[1, 2, 3]);
        w.entry_mut(&2).unwrap().floating = Some(Rect::new(0, 0, 10, 10));
        w.focus = Some(1);
        w.swap_dir(Dir::Next);
        assert_eq!(order(&w), vec![3, 2, 1]);
        assert_eq!(w.focus, Some(1));
    }

    #[test]
    fn zoom_promotes_and_toggles() {
        let mut w = ws(&[1, 2, 3]);
        w.focus = Some(3);
        w.zoom();
        assert_eq!(order(&w), vec![3, 2, 1]);
        assert_eq!(w.focus, Some(3));
        w.zoom();
        assert_eq!(order(&w), vec![2, 3, 1]);
        assert_eq!(w.focus, Some(2));
    }

    #[test]
    fn swap_windows_keeps_slot_weights() {
        let mut w = ws(&[1, 2, 3]);
        w.entry_mut(&1).unwrap().weight = 2.0;
        w.swap_windows(&1, &3);
        assert_eq!(order(&w), vec![3, 2, 1]);
        assert_eq!(w.tiled_weights(), vec![2.0, 1.0, 1.0]);
    }

    #[test]
    fn move_before_inserts() {
        let mut w = ws(&[1, 2, 3, 4]);
        w.move_before(&4, &2);
        assert_eq!(order(&w), vec![1, 4, 2, 3]);
        w.move_before(&1, &3);
        assert_eq!(order(&w), vec![4, 2, 1, 3]);
        w.move_before(&1, &99);
        assert_eq!(order(&w), vec![4, 2, 1, 3]);
    }

    #[test]
    fn removing_fullscreen_clears_it() {
        let mut w = ws(&[1, 2]);
        w.fullscreen = Some(2);
        w.remove(&2);
        assert_eq!(w.fullscreen, None);
    }

    #[test]
    fn merge_keeps_numbers_and_shifts_floats() {
        let mut into = vec![ws(&[1]), ws(&[])];
        let mut b = ws(&[3]);
        b.add(4, false, Some(Rect::new(10, 10, 100, 100)));
        b.add(5, false, Some(Rect::new(0, 0, 0, 0)));
        b.focus = Some(3);
        let from = vec![ws(&[2]), b, ws(&[6])];
        merge_workspaces(&mut into, from, (-2560, 0));

        assert_eq!(order(&into[0]), vec![1, 2]);
        // Workspace 3 doesn't exist on the target, so its window lands on the last one.
        assert_eq!(order(&into[1]), vec![3, 4, 5, 6]);
        assert_eq!(into[0].focus, Some(1), "target keeps its focus");
        assert_eq!(into[1].focus, Some(3), "empty target takes the old focus");
        assert_eq!(
            into[1].entry(&4).unwrap().floating,
            Some(Rect::new(-2550, 10, 100, 100))
        );
        // Unplaced floats stay unplaced so they still get centered.
        assert_eq!(into[1].entry(&5).unwrap().floating, Some(Rect::new(0, 0, 0, 0)));
    }
}
