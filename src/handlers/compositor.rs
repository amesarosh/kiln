use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    desktop::{PopupKind, WindowSurfaceType, layer_map_for_output},
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface, with_states,
        },
        shell::{wlr_layer::LayerSurfaceData, xdg::XdgToplevelSurfaceData},
        shm::{ShmHandler, ShmState},
    },
};

use crate::state::{ClientState, State};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client has ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.needs_redraw = true;

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
            }
        }

        if let Some(window) = self.window_for_surface(surface) {
            if self.pending_windows.contains(&window) {
                // First commit: app_id/title are known now, so rules can apply.
                self.manage_window(window.clone());
                let sent = with_states(surface, |s| {
                    s.data_map
                        .get::<XdgToplevelSurfaceData>()
                        .is_some_and(|d| d.lock().unwrap().initial_configure_sent)
                });
                if !sent && let Some(t) = window.toplevel() {
                    t.send_configure();
                }
            } else {
                self.on_window_commit(&window);
            }
        }

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            // The initial configure is always allowed, so this cannot fail.
            let _ = popup.send_configure();
        }

        self.layer_commit(surface);
    }
}

impl State {
    fn layer_commit(&mut self, surface: &WlSurface) {
        let Some(output) = self.space.outputs().find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        }) else {
            return;
        };
        let output = output.clone();

        let initial_sent = with_states(surface, |s| {
            s.data_map
                .get::<LayerSurfaceData>()
                .is_some_and(|d| d.lock().unwrap().initial_configure_sent)
        });

        let changed = {
            let mut map = layer_map_for_output(&output);
            let changed = map.arrange();
            if !initial_sent && let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
                layer.layer_surface().send_configure();
            }
            changed
        };

        // A bar appearing or changing its exclusive zone resizes the tiling area.
        if changed {
            self.arrange();
        }
        self.refocus();
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(State);
delegate_shm!(State);
