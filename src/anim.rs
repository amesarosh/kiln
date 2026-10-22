//! Animations: tiles sliding into place, the tile being dragged, and workspace switches.
//!
//! The space always holds a window's real position, so input, clients and the layout never
//! wait on an animation. Only drawing is offset: a moved tile is drawn starting where it was
//! and glides to its new slot, and a tile being dragged is drawn under the pointer.
//!
//! A workspace switch keeps a list of the windows that were showing and where they were drawn.
//! Those are drawn sliding (or fading) out while the new workspace's windows come in.

use std::time::{Duration, Instant};

use smithay::{
    desktop::Window,
    output::Output,
    reexports::calloop::timer::{TimeoutAction, Timer},
    utils::{Logical, Point, Rectangle},
};

use crate::{config::WorkspaceAnimation, state::State};
/// Redraw rate while something is sliding; vblank pacing still caps real frames.
const TICK: Duration = Duration::from_millis(7);

/// A window gliding from an old position to its real one.
pub struct Slide {
    window: Window,
    /// Where it was drawn when the slide began, minus where it really is now.
    offset: Point<f64, Logical>,
    start: Instant,
    duration: Duration,
}

/// A monitor changing workspace.
pub struct WsSwitch {
    output: Output,
    /// The windows of the workspace being left, bottom to top, and where they were drawn.
    old: Vec<(Window, Point<f64, Logical>)>,
    /// 1.0: the new workspace comes in from the right (a higher number); -1.0 from the left.
    dir: f64,
    /// How far to slide: the monitor's width.
    width: f64,
    style: WorkspaceAnimation,
    start: Instant,
    duration: Duration,
}

impl WsSwitch {
    /// How far along it is, eased, or None once it's done.
    fn progress(&self, now: Instant) -> Option<f64> {
        let t = now.saturating_duration_since(self.start).as_secs_f64() / self.duration.as_secs_f64().max(1e-3);
        (t < 1.0).then(|| ease_out(t))
    }

    /// (offset, opacity) for the new workspace's windows (`incoming`) or the old one's.
    fn transform(&self, now: Instant, incoming: bool) -> (Point<f64, Logical>, f32) {
        let Some(p) = self.progress(now) else {
            return ((0.0, 0.0).into(), if incoming { 1.0 } else { 0.0 });
        };
        match self.style {
            WorkspaceAnimation::Slide => {
                let x = if incoming {
                    self.dir * self.width * (1.0 - p)
                } else {
                    -self.dir * self.width * p
                };
                ((x, 0.0).into(), 1.0)
            }
            WorkspaceAnimation::Fade | WorkspaceAnimation::None => {
                ((0.0, 0.0).into(), if incoming { p as f32 } else { 1.0 - p as f32 })
            }
        }
    }
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
    ws_switch: Option<WsSwitch>,
    /// Something else animating (the overview) keeps frames coming until then.
    busy_until: Option<Instant>,
    ticking: bool,
}

impl Anims {
    /// Whether anything is still moving, so frames keep coming.
    fn running(&self, now: Instant) -> bool {
        !self.slides.is_empty()
            || self.ws_switch.as_ref().is_some_and(|w| w.progress(now).is_some())
            || self.busy_until.is_some_and(|t| now <= t)
    }
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

impl Slide {
    fn current(&self, now: Instant) -> Option<Point<f64, Logical>> {
        let t = now.saturating_duration_since(self.start).as_secs_f64() / self.duration.as_secs_f64().max(1e-3);
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
        let switch = self.switch_transform(window, now).map_or_else(Default::default, |(o, _)| o);
        Some(loc + offset + switch)
    }

    /// How a window coming in with a workspace switch is moved and faded right now.
    fn switch_transform(&self, window: &Window, now: Instant) -> Option<(Point<f64, Logical>, f32)> {
        let sw = self.anims.ws_switch.as_ref()?;
        sw.progress(now)?;
        let mon = self.monitor_index(&sw.output)?;
        self.monitors[mon].ws().contains(window).then(|| sw.transform(now, true))
    }

    /// How opaque a window is drawn this frame (fading in with a workspace switch).
    pub fn visual_alpha(&self, window: &Window) -> f32 {
        self.switch_transform(window, Instant::now()).map_or(1.0, |(_, a)| a)
    }

    /// The monitor a window is being switched onto, if it is: it's drawn there and nowhere else
    /// while it slides in, even when the slide takes it past the monitor's edge.
    pub fn switching_onto(&self, window: &Window) -> Option<&Output> {
        let sw = self.anims.ws_switch.as_ref()?;
        sw.progress(Instant::now())?;
        let mon = self.monitor_index(&sw.output)?;
        self.monitors[mon].ws().contains(window).then_some(&sw.output)
    }

    /// The windows of a workspace being left on `output`, top first, with where to draw them
    /// and how opaque. Empty when that monitor isn't switching.
    pub fn outgoing_windows(&self, output: &Output) -> Vec<(Window, Point<f64, Logical>, f32)> {
        let now = Instant::now();
        let Some(sw) = self.anims.ws_switch.as_ref().filter(|s| &s.output == output) else {
            return Vec::new();
        };
        if sw.progress(now).is_none() {
            return Vec::new();
        }
        let (offset, alpha) = sw.transform(now, false);
        sw.old.iter().rev().map(|(w, loc)| (w.clone(), *loc + offset, alpha)).collect()
    }

    /// Note what monitor `mon` shows before its workspace changes, to animate from it.
    pub fn begin_workspace_switch(&mut self, mon: usize) -> Option<WsSwitchStart> {
        let cfg = &self.config.animations;
        let duration = cfg.duration();
        if duration.is_zero() || cfg.workspace == WorkspaceAnimation::None {
            return None;
        }
        let m = self.monitors.get(mon)?;
        let width = self.space.output_geometry(&m.output)?.size.w as f64;
        // Windows already sliding out of an earlier switch stop where they are.
        let old = self
            .space
            .elements()
            .filter(|w| m.ws().contains(w))
            .filter_map(|w| Some((w.clone(), self.visual_location(w)?)))
            .collect();
        Some(WsSwitchStart {
            output: m.output.clone(),
            old,
            from: m.active_ws,
            width,
            style: cfg.workspace,
            duration,
        })
    }

    /// The workspace has changed: start animating from what was noted.
    pub fn finish_workspace_switch(&mut self, start: WsSwitchStart, to: usize) {
        if start.from == to {
            return;
        }
        self.anims.ws_switch = Some(WsSwitch {
            output: start.output,
            old: start.old,
            dir: if to > start.from { 1.0 } else { -1.0 },
            width: start.width,
            style: start.style,
            start: Instant::now(),
            duration: start.duration,
        });
        self.start_anim_tick();
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
        let duration = self.config.animations.duration();
        if (offset.x.abs() < 0.5 && offset.y.abs() < 0.5) || duration.is_zero() {
            return;
        }
        self.anims.slides.push(Slide {
            window: window.clone(),
            offset,
            start: Instant::now(),
            // Tiles settle a little quicker than whole workspaces move.
            duration: duration.mul_f32(0.8),
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

    /// Keep frames coming for `duration`, for an animation drawn elsewhere (the overview).
    pub fn keep_animating(&mut self, duration: Duration) {
        // One extra frame at the end, so the last one drawn is the finished state.
        let until = Instant::now() + duration + TICK;
        self.anims.busy_until = Some(self.anims.busy_until.map_or(until, |t| t.max(until)));
        self.needs_redraw = true;
        self.start_anim_tick();
    }

    /// Keep redrawing until every animation has finished.
    pub fn start_anim_tick(&mut self) {
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
            if state.anims.ws_switch.as_ref().is_some_and(|w| w.progress(now).is_none()) {
                state.anims.ws_switch = None;
            }
            state.overview_tick();
            state.needs_redraw = true;
            if state.anims.running(now) {
                TimeoutAction::ToDuration(TICK)
            } else {
                state.anims.ticking = false;
                TimeoutAction::Drop
            }
        });
    }
}

/// What a monitor showed just before switching workspace; see `begin_workspace_switch`.
pub struct WsSwitchStart {
    output: Output,
    old: Vec<(Window, Point<f64, Logical>)>,
    from: usize,
    width: f64,
    style: WorkspaceAnimation,
    duration: Duration,
}
