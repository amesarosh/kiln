use smithay::{
    delegate_layer_shell,
    desktop::{LayerSurface, PopupKind, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::shell::{
        wlr_layer::{Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState},
        xdg::PopupSurface,
    },
};

use crate::state::State;

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let Some(output) = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.output())
        else {
            surface.send_close();
            return;
        };
        let mut map = layer_map_for_output(&output);
        if let Err(e) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("failed to map layer surface: {e}");
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(popup)) {
            tracing::warn!("failed to track layer popup: {e}");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let Some(output) = self.output() else { return };
        {
            let mut map = layer_map_for_output(&output);
            let layer = map.layers().find(|l| l.layer_surface() == &surface).cloned();
            if let Some(layer) = layer {
                map.unmap_layer(&layer);
            }
        }
        if self.focused_layer.as_ref() == Some(surface.wl_surface()) {
            self.focused_layer = None;
        }
        self.arrange();
        self.refocus();
    }
}
delegate_layer_shell!(State);
