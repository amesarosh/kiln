//! Files dragged in from other apps. winit (under eframe) doesn't take Wayland drops, so a
//! thread of our own listens for them on the window's Wayland connection, the way egui's
//! clipboard does: it accepts drags that carry a `text/uri-list`, reports where the pointer
//! is while one hovers over the window, and hands over the list when it's dropped. It also
//! starts drags of our own files out to other apps (and back into this window).

use std::{
    ffi::c_void,
    io::Read,
    io::Write,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback::{self, WlCallback},
        wl_compositor::{self, WlCompositor},
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::{self, WlShmPool},
        wl_surface::{self, WlSurface},
    },
};
use kiln_files::fs;
use kiln_ui::{eframe, egui};

const URI_LIST: &str = "text/uri-list";
/// Also offered when dragging out, for apps that only take text (the paths, one per line).
const TEXT: &str = "text/plain;charset=utf-8";

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
    /// The serial of the last button press on the window: a drag has to name it.
    press_serial: Option<u32>,
    /// A drag of ours under way: its source, and its icon (surface and buffer).
    out: Option<(WlDataSource, Option<(WlSurface, WlBuffer)>)>,
    /// Its `text/uri-list`, for when it's dropped on our own window: this thread would be both
    /// ends of the pipe, waiting on itself.
    out_uris: Option<String>,
}

impl Shared {
    /// Our drag ended (dropped and done, or cancelled).
    fn end_out(&mut self) {
        self.out_uris = None;
        if let Some((source, icon)) = self.out.take() {
            source.destroy();
            if let Some((surface, buffer)) = icon {
                surface.destroy();
                buffer.destroy();
            }
        }
    }
}

pub struct Dnd {
    shared: Arc<Mutex<Shared>>,
    /// Whether the place shown takes files; drags are refused when it doesn't.
    can_drop: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    conn: Connection,
    wake: QueueHandle<State>,
    done: mpsc::Receiver<()>,
    manager: WlDataDeviceManager,
    device: WlDataDevice,
    /// For drag icons.
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    /// Our window's surface, where drags out start.
    window: WlSurface,
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
        // Version 4 at most: from 5 a buffer's offset can't be given when attaching it.
        let compositor: Option<WlCompositor> = globals.bind(&qh, 1..=4, ()).ok();
        let shm: Option<WlShm> = globals.bind(&qh, 1..=1, ()).ok();
        // Safety: winit's surface, alive as long as the window (we stop first, see Drop).
        let window_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), window.surface.as_ptr().cast()) }.ok()?;
        let window_surface = WlSurface::from_id(&conn, window_id).ok()?;

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
            pointer: None,
        };
        let (done_tx, done) = mpsc::channel();
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("drag and drop".into())
            .spawn(move || {
                let _keep = seat;
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
            manager,
            device,
            compositor,
            shm,
            window: window_surface,
        })
    }

    /// Start dragging `paths` out, from the button press under way, with `icon` (premultiplied
    /// RGBA, `scale` pixels per window unit) under the pointer. False if no press to start from.
    pub fn start_drag(&self, paths: &[PathBuf], icon: Option<&kiln_icons::Icon>, scale: i32) -> bool {
        let mut shared = self.shared.lock().unwrap();
        let Some(serial) = shared.press_serial else {
            return false;
        };
        shared.end_out();
        let uris: String = paths.iter().map(|p| fs::path_to_uri(p) + "\r\n").collect();
        shared.out_uris = Some(uris.clone());
        let text: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let source = self.manager.create_data_source(
            &self.wake,
            Outgoing {
                uris,
                text: text.join("\n"),
            },
        );
        source.offer(URI_LIST.into());
        source.offer(TEXT.into());
        if self.device.version() >= 3 {
            source.set_actions(DndAction::Copy | DndAction::Move);
        }
        let icon = icon.and_then(|i| self.icon_surface(i, scale));
        self.device
            .start_drag(Some(&source), &self.window, icon.as_ref().map(|(s, _)| s), serial);
        shared.out = Some((source, icon));
        drop(shared);
        let _ = self.conn.flush();
        true
    }

    /// A surface showing `icon`, centered on the pointer.
    fn icon_surface(&self, icon: &kiln_icons::Icon, scale: i32) -> Option<(WlSurface, WlBuffer)> {
        let (compositor, shm) = (self.compositor.as_ref()?, self.shm.as_ref()?);
        let side = icon.size as i32;
        let len = (side * side * 4) as usize;
        // wl_shm's ARGB8888 is premultiplied B, G, R, A in memory.
        let mut bgra = Vec::with_capacity(len);
        for px in icon.rgba.chunks_exact(4) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        let fd = unsafe { libc::memfd_create(c"drag-icon".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // Safety: memfd_create just made it.
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.write_all(&bgra).ok()?;
        let pool = shm.create_pool(file.as_fd(), len as i32, &self.wake, ());
        let buffer = pool.create_buffer(0, side, side, side * 4, wl_shm::Format::Argb8888, &self.wake, ());
        pool.destroy();
        let surface = compositor.create_surface(&self.wake, ());
        let scale = if compositor.version() >= 3 { scale.max(1) } else { 1 };
        if scale > 1 {
            surface.set_buffer_scale(scale);
        }
        let half = side / scale / 2;
        surface.attach(Some(&buffer), -half, -half);
        surface.commit();
        Some((surface, buffer))
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
    pointer: Option<WlPointer>,
}

/// What a drag of ours carries.
struct Outgoing {
    uris: String,
    text: String,
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
        let own = self.shared.lock().unwrap().out_uris.clone();
        let uris = if !c.accepted {
            None
        } else if own.is_some() {
            own
        } else {
            self.receive(&c.offer)
        };
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
                    // where it was. Our own files dragged within the window move, like
                    // rearranging them.
                    let ours = state.shared.lock().unwrap().out.is_some();
                    let preferred = if ours { DndAction::Move } else { DndAction::Copy };
                    offer.set_actions(DndAction::Copy | DndAction::Move, preferred);
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
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        // A pointer of our own, only for the serials of button presses (winit keeps its own).
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_pointer::Event::Button {
            serial,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            state.shared.lock().unwrap().press_serial = Some(serial);
        }
    }
}

impl Dispatch<WlDataSource, Outgoing> for State {
    fn event(
        state: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        data: &Outgoing,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let bytes = if mime_type == URI_LIST { &data.uris } else { &data.text }
                    .clone()
                    .into_bytes();
                // On a thread of its own, so an app slow to read can't hold up the others.
                std::thread::spawn(move || {
                    let _ = std::fs::File::from(fd).write_all(&bytes);
                });
            }
            // Nothing to do on our side for a move: the app that took the files moves them.
            wl_data_source::Event::DndFinished | wl_data_source::Event::Cancelled => {
                state.shared.lock().unwrap().end_out();
                state.ctx.request_repaint();
            }
            _ => {}
        }
    }
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

impl Dispatch<WlCompositor, ()> for State {
    fn event(_: &mut Self, _: &WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSurface, ()> for State {
    fn event(_: &mut Self, _: &WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
