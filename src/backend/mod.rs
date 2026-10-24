pub mod headless;
pub mod udev;
pub mod winit;

use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma},
    delegate_dmabuf,
    input::keyboard::LedState,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use smithay::backend::renderer::gles::GlesRenderer;

use crate::{render::FrameElement, state::State};

pub enum Backend {
    /// A window inside another desktop.
    Winit(winit::WinitData),
    /// Real hardware from a TTY.
    Udev(udev::UdevData),
    /// No screen at all, for testing.
    Headless(headless::HeadlessData),
}

impl State {
    /// Called once per event-loop iteration when something changed on screen.
    pub fn schedule_redraw(&mut self) {
        match &self.backend {
            Some(Backend::Winit(w)) => w.request_redraw(),
            Some(Backend::Udev(_)) => udev::render_all(self),
            // Draws on its own steady timer.
            Some(Backend::Headless(_)) | None => {}
        }
    }

    /// Build `output`'s scene and hand it to `f` with the renderer, for drawing offscreen
    /// (screenshots). None when there's no GPU to draw with, e.g. while on another VT.
    pub fn capture_output<T>(
        &mut self,
        output: &smithay::output::Output,
        with_cursor: bool,
        f: impl FnOnce(&mut GlesRenderer, &[FrameElement]) -> T,
    ) -> Option<T> {
        match &self.backend {
            Some(Backend::Winit(_)) => winit::capture(self, output, f),
            Some(Backend::Udev(_)) => udev::capture(self, output, with_cursor, f),
            Some(Backend::Headless(_)) => headless::capture(self, output, f),
            None => None,
        }
    }

    /// Whether `output` is driven in HDR, and with what. Headless outputs with `hdr = true`
    /// act like an HDR400 monitor, for tests.
    pub fn output_hdr(
        &self,
        output: &smithay::output::Output,
    ) -> Option<(crate::color::HdrOutputParams, crate::color::DisplayColor)> {
        match &self.backend {
            Some(Backend::Udev(u)) => u.output_hdr(output),
            Some(Backend::Headless(_)) => {
                let cfg = self.output_config(&output.name());
                cfg.hdr.then(|| {
                    (
                        crate::color::HdrOutputParams {
                            sdr_white: cfg.sdr_brightness as f64,
                            peak: 400.0,
                        },
                        crate::color::DisplayColor {
                            pq: true,
                            bt2020: true,
                            max_luminance: Some(400.0),
                            ..Default::default()
                        },
                    )
                })
            }
            _ => None,
        }
    }

    /// Whether our VT is in front. Nested, always.
    pub fn session_active(&self) -> bool {
        match &self.backend {
            Some(Backend::Udev(u)) => u.session_active(),
            _ => true,
        }
    }

    /// Whether the compositor draws the pointer itself (the nested backend uses the host cursor).
    pub fn draws_cursor(&self) -> bool {
        matches!(self.backend, Some(Backend::Udev(_)))
    }

    /// The `[[outputs]]` config changed: re-apply modes, scales, positions and on/off.
    pub fn apply_output_config(&mut self) {
        match &self.backend {
            Some(Backend::Winit(_)) => winit::apply_output_config(self),
            Some(Backend::Udev(_)) => udev::apply_output_config(self),
            Some(Backend::Headless(_)) => headless::apply_output_config(self),
            None => {}
        }
    }

    /// Every connected monitor, lit or not, for monitor setup tools.
    pub fn output_heads(&self) -> Vec<crate::handlers::output_management::HeadInfo> {
        match &self.backend {
            Some(Backend::Winit(w)) => w.heads(self),
            Some(Backend::Udev(_)) => udev::heads(self),
            Some(Backend::Headless(h)) => h.heads(self),
            None => Vec::new(),
        }
    }

    /// Turn a monitor's display off or back on. Nested, the window slice just goes black.
    pub fn backend_set_power(&mut self, output: &smithay::output::Output, on: bool) {
        if let Some(Backend::Udev(_)) = &self.backend {
            udev::set_power(self, output, on);
        }
    }

    pub fn switch_vt(&mut self, vt: i32) {
        if let Some(Backend::Udev(u)) = &mut self.backend {
            u.switch_vt(vt);
        }
    }

    pub fn update_leds(&mut self, leds: LedState) {
        if let Some(Backend::Udev(u)) = &mut self.backend {
            u.update_leds(leds);
        }
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// Clients rendering on the GPU hand us buffers; check the renderer can actually use them.
    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let ok = match &mut self.backend {
            Some(Backend::Winit(w)) => w.renderer().import_dmabuf(&dmabuf, None).is_ok(),
            Some(Backend::Udev(u)) => u.import_dmabuf(&dmabuf),
            Some(Backend::Headless(h)) => h.renderer().import_dmabuf(&dmabuf, None).is_ok(),
            None => false,
        };
        if ok {
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}
delegate_dmabuf!(State);
