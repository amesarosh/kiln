//! kiln-lock: the screen locker.
//!
//! Locks with the standard session lock protocol (ext-session-lock-v1), so the compositor
//! keeps the screen locked even if this program crashes. Draws one lock screen per monitor
//! in the theme's colors and font, and unlocks once PAM accepts the password.

mod auth;
mod draw;
mod password;

use std::{
    io::{Read, Write},
    os::fd::{FromRawFd, OwnedFd},
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat, delegate_session_lock,
    delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle,
            channel::{self, Channel, Sender},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        protocols::wp::{
            fractional_scale::v1::client::{
                wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
                wp_fractional_scale_v1::{self, WpFractionalScaleV1},
            },
            viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_seat::WlSeat, wl_shm, wl_surface::WlSurface},
};

use draw::{Look, Ui};
use password::Password;

const HELP: &str = "\
kiln-lock — lock the screen

USAGE:
    kiln-lock [-f]

OPTIONS:
    -f, --daemonize   return once the screen is locked, and keep running in the background
                      (for swayidle's before-sleep)

Type your password and press Enter to unlock. Escape clears what you typed.
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let mut daemonize = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => return print!("{HELP}"),
            "-f" | "--daemonize" => daemonize = true,
            other => {
                eprintln!("unknown option \"{other}\"\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    // No core dumps: they could hold the password.
    // SAFETY: setrlimit with a valid struct.
    unsafe {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
        libc::setlocale(libc::LC_TIME, c"".as_ptr());
    }
    let parent_pipe = daemonize.then(fork_until_locked);
    std::process::exit(match run(parent_pipe) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e}");
            1
        }
    });
}

/// Fork; the parent waits until the child says the screen is locked, then exits. Returns the
/// child's end of the pipe.
fn fork_until_locked() -> OwnedFd {
    let mut fds = [0; 2];
    // SAFETY: plain pipe and fork while single-threaded; each side keeps the end it uses.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            eprintln!("pipe: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        let (read, write) = (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]));
        match libc::fork() {
            -1 => {
                eprintln!("fork: {}", std::io::Error::last_os_error());
                std::process::exit(1);
            }
            0 => {
                drop(read);
                libc::setsid();
                write
            }
            _ => {
                drop(write);
                let mut byte = [0u8];
                let locked = std::fs::File::from(read).read(&mut byte).is_ok_and(|n| n == 1);
                std::process::exit(if locked { 0 } else { 1 });
            }
        }
    }
}

/// One monitor's lock screen.
struct Screen {
    output: WlOutput,
    surface: SessionLockSurface,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Logical size from the compositor; (0, 0) until the first configure.
    size: (u32, u32),
    /// Scale from fractional-scale, in 120ths.
    scale120: Option<u32>,
    /// Whole-number scale, when fractional scale isn't available.
    int_scale: i32,
    /// Buffer size of the last full frame: the next frame at this size only damages the card.
    drawn: Option<(i32, i32)>,
    dirty: bool,
}

impl Screen {
    fn scale(&self) -> f64 {
        match (self.scale120, &self.viewport) {
            (Some(n), Some(_)) => n as f64 / 120.0,
            _ => self.int_scale as f64,
        }
    }
}

struct Locker {
    conn: Connection,
    qh: QueueHandle<Self>,
    loop_handle: LoopHandle<'static, Self>,
    registry: RegistryState,
    compositor: CompositorState,
    outputs: OutputState,
    seats: SeatState,
    shm: Shm,
    pool: SlotPool,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    lock: Option<SessionLock>,
    locked: bool,
    screens: Vec<Screen>,
    keyboard: Option<WlKeyboard>,
    ctrl: bool,
    look: Look,
    ui: Ui,
    user: String,
    checked: Sender<auth::Outcome>,
    /// Tell the parent of `-f` that the screen is locked.
    parent_pipe: Option<OwnedFd>,
    exit: Option<i32>,
}

fn run(parent_pipe: Option<OwnedFd>) -> Result<i32, Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env().map_err(|e| format!("can't connect to the compositor: {e}"))?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<Locker> = EventLoop::try_new()?;

    let config = kiln_config::Config::default_path().and_then(|p| kiln_config::Config::read(&p).ok());
    let theme = config
        .map(|c| c.theme)
        .unwrap_or_else(|| kiln_config::Config::default().theme);
    let (user, user_label) = user();
    let look = Look {
        text: kiln_draw::Text::load(&theme.font),
        theme,
        user_label,
    };

    let (checked, results): (Sender<auth::Outcome>, Channel<auth::Outcome>) = channel::channel();
    event_loop
        .handle()
        .insert_source(results, |event, _, me| {
            if let channel::Event::Msg(outcome) = event {
                me.checked(outcome);
            }
        })
        .map_err(|e| format!("event loop: {e}"))?;

    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new(4096, &shm)?;
    let lock_state = SessionLockState::new(&globals, &qh);
    let mut me = Locker {
        conn: conn.clone(),
        qh: qh.clone(),
        loop_handle: event_loop.handle(),
        registry: RegistryState::new(&globals),
        compositor: CompositorState::bind(&globals, &qh)?,
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        shm,
        pool,
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional: globals.bind(&qh, 1..=1, ()).ok(),
        lock: None,
        locked: false,
        screens: Vec::new(),
        keyboard: None,
        ctrl: false,
        look,
        ui: Ui {
            password: Password::new(),
            checking: false,
            denied: None,
            failures: 0,
            caps_lock: false,
        },
        user,
        checked,
        parent_pipe,
        exit: None,
    };

    me.lock = Some(
        lock_state
            .lock(&qh)
            .map_err(|_| "the compositor doesn't support screen locking (ext-session-lock-v1)")?,
    );
    WaylandSource::new(conn, queue)
        .insert(event_loop.handle())
        .map_err(|e| format!("event loop: {e}"))?;
    // Monitors already known get their lock screen now; later ones in `new_output`.
    for output in me.outputs.outputs().collect::<Vec<_>>() {
        me.add_screen(output);
    }

    event_loop
        .handle()
        .insert_source(Timer::from_duration(draw::until_next_minute()), |_, _, me| {
            me.screens.iter_mut().for_each(|s| s.dirty = true);
            me.redraw();
            TimeoutAction::ToDuration(draw::until_next_minute())
        })
        .map_err(|e| format!("event loop: {e}"))?;

    while me.exit.is_none() {
        event_loop.dispatch(None, &mut me)?;
    }
    // Keep `lock_state` alive until here: it owns the lock manager.
    drop(lock_state);
    Ok(me.exit.unwrap_or(0))
}

/// (login name, name to show): the full name from the user database if there is one.
fn user() -> (String, String) {
    // SAFETY: getpwuid's result is read before any other call could replace it.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            let name = std::env::var("USER").unwrap_or_default();
            return (name.clone(), name);
        }
        let login = std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        let gecos = if (*pw).pw_gecos.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr((*pw).pw_gecos).to_string_lossy().into_owned()
        };
        let full = gecos.split(',').next().unwrap_or("").trim().to_string();
        let label = if full.is_empty() { login.clone() } else { full };
        (login, label)
    }
}

impl Locker {
    fn add_screen(&mut self, output: WlOutput) {
        let Some(lock) = &self.lock else { return };
        if self.screens.iter().any(|s| s.output == output) {
            return;
        }
        let wl_surface = self.compositor.create_surface(&self.qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&wl_surface, &self.qh, ()));
        let fractional = self
            .fractional
            .as_ref()
            .map(|f| f.get_fractional_scale(&wl_surface, &self.qh, ()));
        let int_scale = self.outputs.info(&output).map_or(1, |i| i.scale_factor.max(1));
        let surface = lock.create_lock_surface(wl_surface, &output, &self.qh);
        self.screens.push(Screen {
            output,
            surface,
            viewport,
            fractional,
            size: (0, 0),
            scale120: None,
            int_scale,
            drawn: None,
            dirty: true,
        });
    }

    /// Draw every screen that changed.
    fn redraw(&mut self) {
        for i in 0..self.screens.len() {
            let screen = &self.screens[i];
            if !screen.dirty || screen.size.0 == 0 || screen.size.1 == 0 {
                continue;
            }
            if let Err(e) = self.draw(i) {
                tracing::warn!("drawing the lock screen: {e}");
            }
        }
    }

    fn draw(&mut self, i: usize) -> Result<(), Box<dyn std::error::Error>> {
        let screen = &mut self.screens[i];
        screen.dirty = false;
        let scale = screen.scale();
        let (lw, lh) = screen.size;
        let w = (lw as f64 * scale).round() as i32;
        let h = (lh as f64 * scale).round() as i32;
        let stride = w as usize * 4;
        let (buffer, pixels) = self.pool.create_buffer(w, h, stride as i32, wl_shm::Format::Argb8888)?;

        let bg = draw::background(&self.look.theme);
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        let fill = [
            to_u8(bg[2] * bg[3]),
            to_u8(bg[1] * bg[3]),
            to_u8(bg[0] * bg[3]),
            to_u8(bg[3]),
        ];
        for px in pixels.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        let (cx, cy, cw, ch) = draw::card_rect(&self.look.theme, scale, w, h);
        draw::card(&self.look, &self.ui, scale, cw, ch).write_argb8888(pixels, w, h, stride, cx, cy);

        let surface = screen.surface.wl_surface();
        if screen.drawn == Some((w, h)) {
            surface.damage_buffer(cx, cy, cw, ch);
        } else {
            surface.damage_buffer(0, 0, w, h);
        }
        screen.drawn = Some((w, h));
        match &screen.viewport {
            Some(v) => v.set_destination(lw as i32, lh as i32),
            None => surface.set_buffer_scale(screen.int_scale),
        }
        buffer.attach_to(surface)?;
        surface.commit();
        Ok(())
    }

    fn mark_dirty(&mut self) {
        self.screens.iter_mut().for_each(|s| s.dirty = true);
        self.redraw();
    }

    fn key(&mut self, event: KeyEvent) {
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::BackSpace if self.ctrl => self.ui.password.clear(),
            Keysym::BackSpace => self.ui.password.pop(),
            Keysym::Escape => self.ui.password.clear(),
            Keysym::u if self.ctrl => self.ui.password.clear(),
            _ => match &event.utf8 {
                Some(t) if !self.ctrl && !t.is_empty() && !t.chars().any(char::is_control) => {
                    self.ui.password.push(t);
                }
                _ => return,
            },
        }
        self.mark_dirty();
    }

    /// Check the typed password on another thread; the answer comes back in `checked`.
    fn submit(&mut self) {
        if self.ui.checking || self.ui.password.is_empty() {
            return;
        }
        self.ui.checking = true;
        let password = self.ui.password.take();
        let user = self.user.clone();
        let reply = self.checked.clone();
        std::thread::spawn(move || {
            let _ = reply.send(auth::check(&user, password));
        });
    }

    fn checked(&mut self, outcome: auth::Outcome) {
        self.ui.checking = false;
        match outcome {
            auth::Outcome::Unlock => {
                tracing::info!("unlocking");
                if let Some(lock) = self.lock.take() {
                    lock.unlock();
                }
                // Make sure the compositor got the unlock before exiting.
                let _ = self.conn.roundtrip();
                self.exit = Some(0);
            }
            auth::Outcome::Denied(said) => {
                self.ui.failures += 1;
                self.ui.denied = Some(said);
                // Whatever was typed while checking belongs to the next try; start clean.
                self.ui.password.clear();
                self.mark_dirty();
            }
        }
    }
}

impl SessionLockHandler for Locker {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        tracing::info!("screen locked");
        self.locked = true;
        if let Some(fd) = self.parent_pipe.take() {
            let _ = std::fs::File::from(fd).write_all(&[1]);
        }
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        if self.locked {
            tracing::info!("the compositor ended the lock");
            self.exit = Some(0);
        } else {
            tracing::error!("the compositor refused to lock (is another locker running?)");
            self.exit = Some(1);
        }
        // Must not unlock a lock that's over; just forget it.
        self.lock = None;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        if let Some(s) = self
            .screens
            .iter_mut()
            .find(|s| s.surface.wl_surface() == surface.wl_surface())
        {
            s.size = configure.new_size;
            s.dirty = true;
        }
        self.redraw();
    }
}

impl CompositorHandler for Locker {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, factor: i32) {
        if let Some(s) = self.screens.iter_mut().find(|s| s.surface.wl_surface() == surface) {
            s.int_scale = factor.max(1);
            s.dirty = true;
        }
        self.redraw();
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {}

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
}

impl OutputHandler for Locker {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.add_screen(output);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.screens.retain(|s| s.output != output);
    }
}

impl SeatHandler for Locker {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let repeat = Box::new(|me: &mut Locker, _: &WlKeyboard, event: KeyEvent| me.key(event));
            match self
                .seats
                .get_keyboard_with_repeat(qh, &seat, None, self.loop_handle.clone(), repeat)
            {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => tracing::error!("no keyboard: {e}"),
            }
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for Locker {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32) {}

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, event: KeyEvent) {
        self.key(event);
    }

    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, event: KeyEvent) {
        self.key(event);
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, _: KeyEvent) {}

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.ctrl = modifiers.ctrl;
        if self.ui.caps_lock != modifiers.caps_lock {
            self.ui.caps_lock = modifiers.caps_lock;
            self.mark_dirty();
        }
    }
}

impl ShmHandler for Locker {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Locker {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<WpFractionalScaleV1, ()> for Locker {
    fn event(
        me: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(s) = me.screens.iter_mut().find(|s| s.fractional.as_ref() == Some(proxy))
        {
            s.scale120 = Some(scale);
            s.dirty = true;
            me.redraw();
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
    }
}

delegate_compositor!(Locker);
delegate_output!(Locker);
delegate_seat!(Locker);
delegate_keyboard!(Locker);
delegate_shm!(Locker);
delegate_session_lock!(Locker);
delegate_registry!(Locker);
wayland_client::delegate_noop!(Locker: ignore WpViewporter);
wayland_client::delegate_noop!(Locker: ignore WpViewport);
wayland_client::delegate_noop!(Locker: ignore WpFractionalScaleManagerV1);
