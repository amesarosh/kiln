//! Files dragged in from other apps. winit (under eframe) doesn't take Wayland drops, so a
//! thread of our own listens for them on the window's Wayland connection, the way egui's
//! clipboard does: it accepts drags that carry a `text/uri-list`, reports where the pointer
//! is while one hovers over the window, and hands over the list when it's dropped.

use std::{
    ffi::c_void,
    io::Read,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::Backend;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_callback::{self, WlCallback},
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
    },
};
use wt_ui::{eframe, egui};

const URI_LIST: &str = "text/uri-list";

/// A finished drop.
pub struct Dropped {
    /// The `text/uri-list` the other app sent.
    pub uris: String,
    /// The other app agreed to a move rather than a copy.
    pub moved: bool,
}

#[derive(Default)]
struct Shared {
    /// Where a drag carrying files is over the window, if one is.
    hover: Option<(f64, f64)>,
    drops: Vec<Dropped>,
}

pub struct Dnd {
    shared: Arc<Mutex<Shared>>,
    /// Whether the place shown takes files; drags are refused when it doesn't.
    can_drop: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    conn: Connection,
    wake: QueueHandle<State>,
    done: mpsc::Receiver<()>,
}

impl Dnd {
    /// Start listening, if the window is on Wayland.
    pub fn start(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = cc.display_handle().ok()?.as_raw() else {
            return None;
        };
        let RawWindowHandle::Wayland(window) = cc.window_handle().ok()?.as_raw() else {
            return None;
        };
        // Safety: the display lives as long as the window, and we stop before it goes (Drop).
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) = registry_queue_init::<State>(&conn)
            .inspect_err(|e| tracing::warn!("no drag and drop: {e}"))
            .ok()?;
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).ok()?;
        let manager: WlDataDeviceManager = globals
            .bind(&qh, 1..=3, ())
            .inspect_err(|e| tracing::warn!("no drag and drop: {e}"))
            .ok()?;
        let device = manager.get_data_device(&seat, &qh, ());

        let shared = Arc::new(Mutex::new(Shared::default()));
        let can_drop = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = State {
            conn: conn.clone(),
            surface: window.surface.as_ptr(),
            version: device.version(),
            shared: shared.clone(),
            can_drop: can_drop.clone(),
            ctx: cc.egui_ctx.clone(),
            current: None,
        };
        let (done_tx, done) = mpsc::channel();
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("drag and drop".into())
            .spawn(move || {
                let _keep = (seat, manager, device);
                while !thread_stop.load(Ordering::Relaxed) {
                    if let Err(e) = queue.blocking_dispatch(&mut state) {
                        tracing::warn!("drag and drop stopped: {e}");
                        break;
                    }
                }
                let _ = done_tx.send(());
            })
            .ok()?;
        Some(Self {
            shared,
            can_drop,
            stop,
            conn,
            wake: qh,
            done,
        })
    }

    /// Say whether drops are taken where we are.
    pub fn set_can_drop(&self, yes: bool) {
        self.can_drop.store(yes, Ordering::Relaxed);
    }

    /// Where a drag is hovering (window coordinates), and the drops since the last call.
    pub fn poll(&self) -> (Option<(f64, f64)>, Vec<Dropped>) {
        let mut s = self.shared.lock().unwrap();
        (s.hover, std::mem::take(&mut s.drops))
    }
}

impl Drop for Dnd {
    fn drop(&mut self) {
        // Wake the thread with a round trip so it sees `stop`, and let it finish before the
        // window's connection closes.
        self.stop.store(true, Ordering::Relaxed);
        self.conn.display().sync(&self.wake, ());
        let _ = self.conn.flush();
        let _ = self.done.recv_timeout(Duration::from_millis(500));
    }
}

/// The drag over the window now.
struct Current {
    offer: WlDataOffer,
    serial: u32,
    /// Whether we told the other app we'd take it.
    accepted: bool,
    /// The action agreed on (copy or move).
    action: DndAction,
}

struct State {
    conn: Connection,
    /// Our window's wl_surface.
    surface: *mut c_void,
    version: u32,
    shared: Arc<Mutex<Shared>>,
    can_drop: Arc<AtomicBool>,
    ctx: egui::Context,
    current: Option<Current>,
}

// Safety: `surface` is only compared, never used through.
unsafe impl Send for State {}

impl State {
    /// Accept or refuse the drag, as the place shown allows.
    fn update_accept(&mut self) {
        let want = self.can_drop.load(Ordering::Relaxed);
        let Some(c) = self.current.as_mut().filter(|c| c.accepted != want) else {
            return;
        };
        c.offer.accept(c.serial, want.then(|| URI_LIST.to_string()));
        c.accepted = want;
    }

    fn set_hover(&self, at: Option<(f64, f64)>) {
        self.shared.lock().unwrap().hover = at;
        self.ctx.request_repaint();
    }

    /// The drag was dropped on us: fetch the list.
    fn dropped(&mut self) {
        let Some(c) = self.current.take() else { return };
        let uris = if c.accepted { self.receive(&c.offer) } else { None };
        if uris.is_some() && self.version >= 3 {
            c.offer.finish();
        }
        c.offer.destroy();
        // The hover ends as the drop arrives (not before), so the window keeps the folder row
        // it was over until it takes the drop.
        let mut shared = self.shared.lock().unwrap();
        shared.hover = None;
        if let Some(uris) = uris {
            shared.drops.push(Dropped {
                uris,
                moved: c.action == DndAction::Move,
            });
        }
        self.ctx.request_repaint();
    }

    fn receive(&self, offer: &WlDataOffer) -> Option<String> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return None;
        }
        // Safety: pipe2 just made these and nothing else owns them.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        offer.receive(URI_LIST.into(), write.as_fd());
        drop(write);
        self.conn.flush().ok()?;
        // Don't wait forever on an app that never sends.
        let mut out = Vec::new();
        let mut file = std::fs::File::from(read);
        let mut buf = [0u8; 8192];
        loop {
            let mut pfd = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&file),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 5000) } <= 0 {
                tracing::warn!("a dropped file list never arrived");
                return None;
            }
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        String::from_utf8(out).ok()
    }
}

/// The types an offer comes in.
#[derive(Default)]
struct Mimes(Mutex<Vec<String>>);

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                if let Some(old) = state.current.take() {
                    old.offer.destroy();
                }
                let Some(offer) = id else { return };
                let files = offer
                    .data::<Mimes>()
                    .is_some_and(|m| m.0.lock().unwrap().iter().any(|t| t == URI_LIST));
                if !files || surface.id().as_ptr().cast() != state.surface {
                    offer.accept(serial, None);
                    return offer.destroy();
                }
                if state.version >= 3 {
                    // Copy unless the other app only allows a move: nothing goes missing from
                    // where it was.
                    offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                }
                state.current = Some(Current {
                    offer,
                    serial,
                    accepted: false,
                    action: DndAction::Copy,
                });
                state.update_accept();
                state.set_hover(Some((x, y)));
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if state.current.is_some() {
                    state.update_accept();
                    state.set_hover(Some((x, y)));
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(c) = state.current.take() {
                    c.offer.destroy();
                }
                state.set_hover(None);
            }
            wl_data_device::Event::Drop => state.dropped(),
            // The clipboard: egui has its own listener for that.
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, Mimes::default()),
    ]);
}

impl Dispatch<WlDataOffer, Mimes> for State {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        mimes: &Mimes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => mimes.0.lock().unwrap().push(mime_type),
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => {
                if let Some(c) = state.current.as_mut().filter(|c| c.offer == *offer) {
                    c.action = action;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlDataDeviceManager,
        _: <WlDataDeviceManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(_: &mut Self, _: &WlCallback, _: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
