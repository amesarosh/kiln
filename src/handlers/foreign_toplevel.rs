//! Telling other programs which windows are open, so taskbars (waybar's wlr/taskbar), window
//! switchers (rofi) and scripts (`wlrctl`, `lswt`) can list, focus and close them.
//!
//! Two protocols: wlr-foreign-toplevel-management, which most tools use and which also lets them
//! focus, close and fullscreen windows; and the newer ext-foreign-toplevel-list, which only lists.
//!
//! Rather than hooking every change, `refresh_foreign_toplevels` runs once per event-loop pass,
//! compares each window with what clients were last told, and sends only the differences.

use smithay::{
    desktop::Window,
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::ClientId,
            protocol::wl_output::WlOutput,
        },
    },
    wayland::foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState},
};

use crate::{
    config::Action,
    layout::LayoutKind,
    state::{State, close_window, toplevel_ids},
};

/// What clients were last told about one window.
pub struct Tracked {
    id: u64,
    window: Window,
    info: Info,
    wlr: Vec<ZwlrForeignToplevelHandleV1>,
    ext: ForeignToplevelHandle,
}

#[derive(Clone, PartialEq, Default)]
struct Info {
    title: String,
    app_id: String,
    activated: bool,
    fullscreen: bool,
    /// The monitor the window belongs to (shown or on a hidden workspace).
    output: Option<Output>,
}

pub struct ForeignToplevels {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    list: ForeignToplevelListState,
    windows: Vec<Tracked>,
    next_id: u64,
}

impl ForeignToplevels {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ZwlrForeignToplevelManagerV1, _>(3, ());
        Self {
            managers: Vec::new(),
            list: ForeignToplevelListState::new::<State>(dh),
            windows: Vec::new(),
            next_id: 1,
        }
    }
}

impl State {
    /// Bring clients' view of the window list up to date.
    pub fn refresh_foreign_toplevels(&mut self) {
        let current = self.toplevel_infos();
        let dh = self.display_handle.clone();
        let ft = &mut self.foreign_toplevels;

        // Gone windows.
        ft.windows.retain(|t| {
            let alive = current.iter().any(|(w, _)| *w == t.window);
            if !alive {
                for h in &t.wlr {
                    h.closed();
                }
                t.ext.send_closed();
            }
            alive
        });
        ft.list.cleanup_closed_handles();

        for (window, info) in current {
            match ft.windows.iter_mut().find(|t| t.window == window) {
                Some(t) => {
                    if t.info != info {
                        t.wlr.retain(|h| h.is_alive());
                        for h in &t.wlr {
                            send_changes(h, &t.info, &info);
                        }
                        if t.info.title != info.title || t.info.app_id != info.app_id {
                            t.ext.send_title(&info.title);
                            t.ext.send_app_id(&info.app_id);
                            t.ext.send_done();
                        }
                        t.info = info;
                    }
                }
                None => {
                    let id = ft.next_id;
                    ft.next_id += 1;
                    let ext = ft.list.new_toplevel::<State>(&info.title, &info.app_id);
                    let mut t = Tracked {
                        id,
                        window,
                        info,
                        wlr: Vec::new(),
                        ext,
                    };
                    for m in &ft.managers {
                        announce(&dh, m, &mut t);
                    }
                    ft.windows.push(t);
                }
            }
        }
    }

    /// Every managed window and how it looks to a taskbar right now.
    fn toplevel_infos(&self) -> Vec<(Window, Info)> {
        let focused = (self.focused_layer.is_none() && !self.is_locked())
            .then(|| self.monitors.get(self.active_mon).and_then(|m| m.ws().focus.clone()))
            .flatten();
        let mut out = Vec::new();
        for m in &self.monitors {
            for ws in &m.workspaces {
                for e in &ws.entries {
                    let (app_id, title) = toplevel_ids(&e.window);
                    out.push((
                        e.window.clone(),
                        Info {
                            title: title.unwrap_or_default(),
                            app_id: app_id.unwrap_or_default(),
                            activated: focused.as_ref() == Some(&e.window),
                            fullscreen: ws.fullscreen.as_ref() == Some(&e.window),
                            output: Some(m.output.clone()),
                        },
                    ));
                }
            }
        }
        out
    }

    /// A client bound a monitor after hearing about windows on it: say which ones are there.
    pub fn foreign_toplevel_output_bound(&mut self, output: &Output, wl_output: &WlOutput) {
        let Some(client) = wl_output.client() else { return };
        for t in &self.foreign_toplevels.windows {
            if t.info.output.as_ref() != Some(output) {
                continue;
            }
            for h in t.wlr.iter().filter(|h| h.client().as_ref() == Some(&client)) {
                h.output_enter(wl_output);
                h.done();
            }
        }
    }

    fn tracked_window(&self, id: u64) -> Option<Window> {
        self.foreign_toplevels
            .windows
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.window.clone())
    }

    /// Show a window and give it focus, switching to its workspace and monitor if needed.
    pub fn activate_window(&mut self, w: &Window) {
        let Some(mon) = self.monitor_of(w) else { return };
        let Some(ws) = self.monitors[mon].workspaces.iter().position(|ws| ws.contains(w)) else {
            return;
        };
        self.close_menu();
        self.active_mon = mon;
        if self.monitors[mon].active_ws != ws {
            self.switch_workspace(ws);
        }
        self.focused_layer = None;
        self.focus_window(w);
        if self.ws().layout == LayoutKind::Monocle {
            self.arrange();
        }
        self.needs_redraw = true;
    }
}

/// Create a handle for `t` on manager `m` and send everything about it.
fn announce(dh: &DisplayHandle, m: &ZwlrForeignToplevelManagerV1, t: &mut Tracked) {
    let Some(client) = m.client() else { return };
    let Ok(h) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, State>(dh, m.version(), t.id) else {
        return;
    };
    m.toplevel(&h);
    // Everything differs from a blank Info, so this sends the lot, state included.
    send_changes(&h, &Info::default(), &t.info);
    t.wlr.push(h);
}

/// Send what changed from `old` to `new`, then `done`.
fn send_changes(h: &ZwlrForeignToplevelHandleV1, old: &Info, new: &Info) {
    if old.title != new.title {
        h.title(new.title.clone());
    }
    if old.app_id != new.app_id {
        h.app_id(new.app_id.clone());
    }
    if old.output != new.output {
        let client = h.client();
        let wl_outputs = |o: &Option<Output>| -> Vec<WlOutput> {
            match (o, &client) {
                (Some(o), Some(c)) => o.client_outputs(c).collect(),
                _ => Vec::new(),
            }
        };
        for wl in wl_outputs(&old.output) {
            h.output_leave(&wl);
        }
        for wl in wl_outputs(&new.output) {
            h.output_enter(&wl);
        }
    }
    if old.activated != new.activated || old.fullscreen != new.fullscreen || old == &Info::default() {
        h.state(state_bytes(new));
    }
    h.done();
}

/// The `state` array: native-endian u32 values.
fn state_bytes(info: &Info) -> Vec<u8> {
    use zwlr_foreign_toplevel_handle_v1::State as S;
    let mut states = Vec::new();
    if info.activated {
        states.push(S::Activated as u32);
    }
    if info.fullscreen {
        states.push(S::Fullscreen as u32);
    }
    states.into_iter().flat_map(u32::to_ne_bytes).collect()
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn bind(
        state: &mut State,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        let manager = data_init.init(resource, ());
        for t in &mut state.foreign_toplevels.windows {
            announce(dh, &manager, t);
        }
        state.foreign_toplevels.managers.push(manager);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            state.foreign_toplevels.managers.retain(|m| m != manager);
            manager.finished();
        }
    }

    fn destroyed(state: &mut State, _client: ClientId, manager: &ZwlrForeignToplevelManagerV1, _data: &()) {
        state.foreign_toplevels.managers.retain(|m| m != manager);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, u64> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        id: &u64,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let Some(w) = state.tracked_window(*id) else { return };
        match request {
            Request::Activate { .. } => state.activate_window(&w),
            Request::Close => close_window(&w),
            Request::SetFullscreen { .. } | Request::UnsetFullscreen => {
                let want = matches!(request, Request::SetFullscreen { .. });
                let is = state.monitor_of(&w).is_some_and(|m| {
                    state.monitors[m]
                        .workspaces
                        .iter()
                        .any(|ws| ws.fullscreen.as_ref() == Some(&w))
                });
                if want != is {
                    state.activate_window(&w);
                    state.run_action(Action::ToggleFullscreen);
                }
            }
            // Tiled windows can't be maximized or minimized here; taskbars cope with that.
            _ => {}
        }
    }

    fn destroyed(state: &mut State, _client: ClientId, handle: &ZwlrForeignToplevelHandleV1, _id: &u64) {
        for t in &mut state.foreign_toplevels.windows {
            t.wlr.retain(|h| h != handle);
        }
    }
}

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevels.list
    }
}
smithay::delegate_foreign_toplevel_list!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_array_lists_flags_in_order() {
        let mut info = Info::default();
        assert!(state_bytes(&info).is_empty());
        info.activated = true;
        info.fullscreen = true;
        let values: Vec<u32> = state_bytes(&info)
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(values, [2, 3]);
    }
}
