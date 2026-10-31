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
    /// Hidden until it's focused again; it keeps its tab and its place in the order.
    pub minimized: bool,
}

impl<W> Entry<W> {
    fn is_tiled(&self) -> bool {
        self.floating.is_none() && !self.minimized
    }
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
    /// Name and icon from the config, shown on the bar instead of the number. It belongs to
    /// the workspace, so it moves along when the workspace is dragged to another spot.
    pub label: crate::config::WorkspaceLabel,
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
            label: Default::default(),
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

    pub fn is_minimized(&self, w: &W) -> bool {
        self.entry(w).is_some_and(|e| e.minimized)
    }

    pub fn is_floating(&self, w: &W) -> bool {
        self.entry(w).is_some_and(|e| e.floating.is_some())
    }

    /// Height shares of the tiled windows, in tiling order.
    pub fn tiled_weights(&self) -> Vec<f32> {
        self.entries
            .iter()
            .filter(|e| e.is_tiled())
            .map(|e| e.weight)
            .collect()
    }

    pub fn tiled(&self) -> impl Iterator<Item = &W> {
        self.entries.iter().filter(|e| e.is_tiled()).map(|e| &e.window)
    }

    pub fn floating(&self) -> impl Iterator<Item = (&W, Rect)> {
        self.entries
            .iter()
            .filter(|e| !e.minimized)
            .filter_map(|e| e.floating.map(|r| (&e.window, r)))
    }

    pub fn add(&mut self, window: W, at_master: bool, floating: Option<Rect>) {
        let entry = Entry {
            window: window.clone(),
            floating,
            weight: 1.0,
            minimized: false,
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
            minimized: false,
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
            self.focus = self.shown_near(idx);
        }
        true
    }

    /// The shown (not minimized) window at `idx`, or the nearest one after it, or before it.
    fn shown_near(&self, idx: usize) -> Option<W> {
        let after = self.entries.iter().skip(idx).find(|e| !e.minimized);
        let before = || self.entries.iter().take(idx).rev().find(|e| !e.minimized);
        after.or_else(before).map(|e| e.window.clone())
    }

    /// Hide a window, moving focus to a shown neighbour. Returns false if it wasn't here or
    /// was already minimized.
    pub fn minimize(&mut self, w: &W) -> bool {
        let Some(idx) = self.index_of(w) else { return false };
        if self.entries[idx].minimized {
            return false;
        }
        self.entries[idx].minimized = true;
        if self.fullscreen.as_ref() == Some(w) {
            self.fullscreen = None;
        }
        if self.focus.as_ref() == Some(w) {
            self.focus = self.shown_near(idx);
        }
        true
    }

    /// Show a minimized window again, in its old place, and focus it. Returns false if it
    /// wasn't minimized.
    pub fn restore(&mut self, w: &W) -> bool {
        let Some(e) = self.entry_mut(w) else { return false };
        if !e.minimized {
            return false;
        }
        e.minimized = false;
        self.focus = Some(w.clone());
        true
    }

    /// Focus the next or previous shown window; minimized ones are skipped.
    pub fn focus_dir(&mut self, dir: Dir) {
        let shown: Vec<usize> = (0..self.entries.len()).filter(|&i| !self.entries[i].minimized).collect();
        if shown.is_empty() {
            return;
        }
        let cur = self
            .focus
            .as_ref()
            .and_then(|f| shown.iter().position(|&i| &self.entries[i].window == f))
            .unwrap_or(0);
        let next = shown[step(cur, shown.len(), dir)];
        self.focus = Some(self.entries[next].window.clone());
    }

    /// Move the focused tiled window one slot through the tiled order (wrapping).
    pub fn swap_dir(&mut self, dir: Dir) {
        let Some(focus) = self.focus.clone() else { return };
        let tiled: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].is_tiled())
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

    /// Move a tiled window into the other column (master or stack), just above or below
    /// `target`. The master count changes so the window ends up in `target`'s column.
    /// Does nothing and returns false if both are already in the same column, or if
    /// moving would leave the window's own column empty.
    pub fn move_to_column(&mut self, w: &W, target: &W, below: bool) -> bool {
        let tiled: Vec<&W> = self.tiled().collect();
        let n = tiled.len();
        let (Some(i), Some(j)) = (
            tiled.iter().position(|t| *t == w),
            tiled.iter().position(|t| *t == target),
        ) else {
            return false;
        };
        let masters = self.master_count.min(n);
        let to_master = j < masters;
        let from_len = if i < masters { masters } else { n - masters };
        if (i < masters) == to_master || from_len < 2 {
            return false;
        }
        // Take an average share of the new column, not the size it had in the old one.
        let col = if to_master { 0..masters } else { masters..n };
        let weights = self.tiled_weights();
        let weight = weights[col.clone()].iter().sum::<f32>() / col.len() as f32;

        let Some(k) = self.index_of(w) else { return false };
        let mut entry = self.entries.remove(k);
        entry.weight = weight;
        let Some(t) = self.index_of(target) else { return false };
        self.entries.insert(t + usize::from(below), entry);
        self.master_count = if to_master { masters + 1 } else { masters - 1 };
        true
    }

    /// With exactly two tiles, one per column, flip between side by side and stacked: the
    /// window lands just above or below `target` (left or right of it when going from the
    /// vertical layout back to the tile layout). Returns false when it doesn't apply.
    pub fn flip_pair(&mut self, w: &W, target: &W, below: bool) -> bool {
        let tiled: Vec<W> = self.tiled().cloned().collect();
        if tiled.len() != 2 || self.master_count != 1 || w == target || !tiled.contains(w) || !tiled.contains(target) {
            return false;
        }
        self.layout = match self.layout {
            LayoutKind::Tile => LayoutKind::Vertical,
            LayoutKind::Vertical => LayoutKind::Tile,
            LayoutKind::Monocle => return false,
        };
        let is_first = &tiled[0] == w;
        if is_first == below {
            self.swap_windows(w, target);
        }
        true
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
            .filter(|&i| self.entries[i].is_tiled())
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
            let minimized = e.minimized;
            target.add(e.window.clone(), false, e.floating);
            if minimized {
                target.minimize(&e.window);
            }
        }
        // Keep what the target had focused; otherwise use what was focused on the old monitor.
        target.focus = focus_before
            .or(ws.focus)
            .or_else(|| target.shown_near(0));
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
    fn move_to_column_joins_the_master_column() {
        // One master on the left, 2 and 3 stacked on the right.
        let mut w = ws(&[1, 2, 3]);
        w.master_count = 1;
        w.entry_mut(&3).unwrap().weight = 1.6;
        assert!(w.move_to_column(&3, &1, true));
        assert_eq!(order(&w), vec![1, 3, 2]);
        assert_eq!(w.master_count, 2);
        assert_eq!(w.tiled_weights(), vec![1.0, 1.0, 1.0]);
        // And back out to the stack, above 2.
        assert!(w.move_to_column(&1, &2, false));
        assert_eq!(order(&w), vec![3, 1, 2]);
        assert_eq!(w.master_count, 1);
    }

    #[test]
    fn move_to_column_refuses_same_column_or_emptying_one() {
        let mut w = ws(&[1, 2, 3]);
        w.master_count = 1;
        assert!(!w.move_to_column(&2, &3, true), "same column");
        assert!(!w.move_to_column(&1, &2, true), "would leave no master");
        let mut two = ws(&[1, 2]);
        two.master_count = 1;
        assert!(!two.move_to_column(&2, &1, true), "would leave no stack");
        assert_eq!(order(&w), vec![1, 2, 3]);
    }

    #[test]
    fn flip_pair_switches_between_side_by_side_and_stacked() {
        let mut w = ws(&[1, 2]);
        w.master_count = 1;
        // 1 | 2, drop 2 below 1: 1 over 2.
        assert!(w.flip_pair(&2, &1, true));
        assert_eq!(w.layout, LayoutKind::Vertical);
        assert_eq!(order(&w), vec![1, 2]);
        // Drop 2 left of 1: back to side by side, 2 first.
        assert!(w.flip_pair(&2, &1, false));
        assert_eq!(w.layout, LayoutKind::Tile);
        assert_eq!(order(&w), vec![2, 1]);
        // Drop 2 below 1 again: 1 on top.
        assert!(w.flip_pair(&2, &1, true));
        assert_eq!(order(&w), vec![1, 2]);
        let mut three = ws(&[1, 2, 3]);
        three.master_count = 1;
        assert!(!three.flip_pair(&1, &2, true), "only for two tiles");
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
    fn minimized_windows_leave_the_tiling_and_focus() {
        let mut w = ws(&[1, 2, 3]);
        w.focus = Some(2);
        w.fullscreen = Some(2);
        assert!(w.minimize(&2));
        assert!(!w.minimize(&2), "already minimized");
        assert_eq!(w.focus, Some(3));
        assert_eq!(w.fullscreen, None);
        assert_eq!(w.tiled().copied().collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(w.tiled_weights().len(), 2);
        // Focus cycling skips it.
        w.focus_dir(Dir::Next);
        assert_eq!(w.focus, Some(1));
        w.focus_dir(Dir::Next);
        assert_eq!(w.focus, Some(3));
        // Removing the last shown neighbour falls back to one before it.
        w.minimize(&3);
        assert_eq!(w.focus, Some(1));
        w.minimize(&1);
        assert_eq!(w.focus, None);
        w.focus_dir(Dir::Next);
        assert_eq!(w.focus, None);
        // Restoring puts it back in its old slot, focused.
        assert!(w.restore(&2));
        assert!(!w.restore(&2));
        assert_eq!(w.focus, Some(2));
        assert_eq!(order(&w), vec![1, 2, 3]);
        assert_eq!(w.tiled().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn minimized_floating_windows_are_hidden() {
        let mut w = ws(&[1]);
        w.add(2, false, Some(Rect::new(0, 0, 10, 10)));
        w.minimize(&2);
        assert_eq!(w.floating().count(), 0);
        assert_eq!(w.focus, Some(1));
    }

    #[test]
    fn removing_focus_skips_minimized_neighbours() {
        let mut w = ws(&[1, 2, 3]);
        w.minimize(&3);
        w.focus = Some(2);
        w.remove(&2);
        assert_eq!(w.focus, Some(1));
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
