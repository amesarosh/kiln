pub mod udev;
pub mod winit;

use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma},
    delegate_dmabuf,
    input::keyboard::LedState,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use crate::state::State;

pub enum Backend {
    /// A window inside another desktop.
    Winit(winit::WinitData),
    /// Real hardware from a TTY.
    Udev(udev::UdevData),
}

impl State {
    /// Called once per event-loop iteration when something changed on screen.
    pub fn schedule_redraw(&mut self) {
        match &self.backend {
            Some(Backend::Winit(w)) => w.request_redraw(),
            Some(Backend::Udev(_)) => udev::render_all(self),
            None => {}
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
            None => {}
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
