//! Window slide animations for dragging tiles around.
//!
//! The space always holds a window's real position, so input, clients and the layout never
//! wait on an animation. Only drawing is offset: a moved tile is drawn starting where it was
//! and glides to its new slot, and a tile being dragged is drawn under the pointer.

use std::time::{Duration, Instant};

use smithay::{
    desktop::Window,
    reexports::calloop::timer::{TimeoutAction, Timer},
    utils::{Logical, Point, Rectangle},
};

use crate::state::State;

/// How long a tile takes to slide into its new slot.
const DURATION: Duration = Duration::from_millis(180);
/// Redraw rate while something is sliding; vblank pacing still caps real frames.
const TICK: Duration = Duration::from_millis(7);

/// A window gliding from an old position to its real one.
pub struct Slide {
    window: Window,
    /// Where it was drawn when the slide began, minus where it really is now.
    offset: Point<f64, Logical>,
    start: Instant,
}

/// A tiled window held under the pointer by a Mod+drag.
pub struct DraggedTile {
    pub window: Window,
    /// Where on the window it was grabbed, as a fraction of its size, so it stays under the
    /// pointer as swaps resize it.
    pub grab: (f64, f64),
    pub pointer: Point<f64, Logical>,
}

#[derive(Default)]
pub struct Anims {
    slides: Vec<Slide>,
    pub dragged: Option<DraggedTile>,
    ticking: bool,
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

impl Slide {
    fn current(&self, now: Instant) -> Option<Point<f64, Logical>> {
        let t = now.saturating_duration_since(self.start).as_secs_f64() / DURATION.as_secs_f64();
        (t < 1.0).then(|| self.offset.upscale(1.0 - ease_out(t)))
    }
}

impl State {
    /// Where a window is drawn this frame (top-left of its geometry), which differs from its
    /// real position while it slides or is dragged.
    pub fn visual_location(&self, window: &Window) -> Option<Point<f64, Logical>> {
        if let Some(d) = self.anims.dragged.as_ref().filter(|d| d.window == *window) {
            let size = window.geometry().size.to_f64();
            return Some(d.pointer - Point::from((d.grab.0 * size.w, d.grab.1 * size.h)));
        }
        let loc = self.space.element_location(window)?.to_f64();
        let now = Instant::now();
        let offset = self
            .anims
            .slides
            .iter()
            .find(|s| s.window == *window)
            .and_then(|s| s.current(now))
            .unwrap_or_default();
        Some(loc + offset)
    }

    /// Arrange, and slide every window that moved from where it was drawn to its new place.
    pub fn arrange_animated(&mut self) {
        let before: Vec<(Window, Point<f64, Logical>)> = self
            .space
            .elements()
            .filter_map(|w| Some((w.clone(), self.visual_location(w)?)))
            .collect();
        self.arrange();
        for (w, from) in before {
            self.slide_from(&w, from);
        }
    }

    /// Slide a window from a drawn position to wherever it really is now.
    pub fn slide_from(&mut self, window: &Window, from: Point<f64, Logical>) {
        if self.anims.dragged.as_ref().is_some_and(|d| d.window == *window) {
            return;
        }
        let Some(to) = self.space.element_location(window) else {
            return;
        };
        let offset = from - to.to_f64();
        self.anims.slides.retain(|s| s.window != *window);
        if offset.x.abs() < 0.5 && offset.y.abs() < 0.5 {
            return;
        }
        self.anims.slides.push(Slide {
            window: window.clone(),
            offset,
            start: Instant::now(),
        });
        self.start_anim_tick();
    }

    /// Pick up a tiled window: from now on it's drawn under the pointer.
    pub fn start_tile_drag(&mut self, window: &Window, pointer: Point<f64, Logical>) {
        let Some(loc) = self.visual_location(window) else {
            return;
        };
        let size = window.geometry().size.to_f64();
        let frac = |p: f64, l: f64, s: f64| if s > 0.0 { ((p - l) / s).clamp(0.0, 1.0) } else { 0.5 };
        self.anims.slides.retain(|s| s.window != *window);
        self.anims.dragged = Some(DraggedTile {
            window: window.clone(),
            grab: (frac(pointer.x, loc.x, size.w), frac(pointer.y, loc.y, size.h)),
            pointer,
        });
        self.needs_redraw = true;
    }

    pub fn move_tile_drag(&mut self, pointer: Point<f64, Logical>) {
        if let Some(d) = &mut self.anims.dragged {
            d.pointer = pointer;
            self.needs_redraw = true;
        }
    }

    /// Let go of the dragged tile: it slides from the pointer into its slot.
    pub fn end_tile_drag(&mut self) {
        let Some(d) = &self.anims.dragged else { return };
        let window = d.window.clone();
        let from = self.visual_location(&window);
        self.anims.dragged = None;
        if let Some(from) = from {
            self.slide_from(&window, from);
        }
        self.needs_redraw = true;
    }

    /// The area a window covers this frame, for deciding which outputs draw it.
    pub fn visual_bbox(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let loc = self.visual_location(window)?;
        let real = self.space.element_location(window)?;
        let bbox = self.space.element_bbox(window)?;
        let shift = (loc - real.to_f64()).to_i32_round();
        Some(Rectangle::new(bbox.loc + shift, bbox.size))
    }

    /// Keep redrawing until every slide has finished.
    fn start_anim_tick(&mut self) {
        if self.anims.ticking {
            return;
        }
        self.anims.ticking = true;
        let timer = Timer::from_duration(TICK);
        let _ = self.loop_handle.insert_source(timer, |_, _, state| {
            let now = Instant::now();
            // Drop slides that finished or whose window went away.
            let alive: Vec<Window> = state.space.elements().cloned().collect();
            state
                .anims
                .slides
                .retain(|s| s.current(now).is_some() && alive.contains(&s.window));
            state.needs_redraw = true;
            if state.anims.slides.is_empty() {
                state.anims.ticking = false;
                TimeoutAction::Drop
            } else {
                TimeoutAction::ToDuration(TICK)
            }
        });
    }
}
