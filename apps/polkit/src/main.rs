//! kiln-polkit: asks for your password when an app needs admin rights.
//!
//! Registers with polkitd as this login session's authentication agent. When an app asks for
//! something that needs a password (installing software, mounting a disk, `pkexec`), polkitd
//! asks us; we dim the screen and show a dialog in the theme's colors and font, and pass what
//! is typed to polkit's own helper, which checks it and tells polkitd the outcome.
//!
//! One dialog at a time; other requests wait their turn. The program runs for the whole
//! session and quits with the compositor.

mod agent;
mod draw;
mod helper;
#[path = "../../lock/src/password.rs"]
mod password;

use std::collections::VecDeque;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle,
            channel::{self, Sender},
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
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{
        wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_seat::WlSeat, wl_shm,
        wl_surface::WlSurface,
    },
};

use agent::{AgentError, Begin, Request};
use draw::{Hit, Look};
use helper::{Event, Helper};
use password::Password;

const HELP: &str = "\
kiln-polkit — ask for passwords when apps need admin rights

USAGE:
    kiln-polkit

Runs for the whole session as its polkit authentication agent. The compositor starts it at
login (see `polkit_agent` under [general] in the config). Only one agent can run per session.

In the dialog: Enter to authenticate, Escape to cancel, Tab to pick another user when there's
a choice.
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    if let Some(arg) = std::env::args().nth(1) {
        if arg == "-h" || arg == "--help" {
            return print!("{HELP}");
        }
        eprintln!("unknown option \"{arg}\"\n\n{HELP}");
        std::process::exit(2);
    }
    // No core dumps: they could hold a password.
    // SAFETY: setrlimit with a valid struct.
    unsafe {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
    }
    if let Err(e) = run() {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

/// The dialog on screen, and the conversation with the helper behind it.
struct Dialog {
    begin: Begin,
    /// Index into `begin.users`.
    chosen: usize,
    helper: Option<Helper>,
    /// Which helper conversation is current; events from older ones are ignored.
    helper_id: u64,
    /// The helper is waiting for an answer: its prompt, and whether the answer is shown.
    prompt: Option<(String, bool)>,
    /// Enter was pressed before the helper was ready.
    submit_when_ready: bool,
    /// An answer was sent in the current conversation.
    answered: bool,
    /// The helper ended without asking anything; Enter tries again.
    broken: bool,
    input: Password,
    checking: bool,
    status: Option<(String, bool)>,
    /// What PAM said during the current conversation, to show if it fails.
    pam_error: Option<String>,
    failures: u32,

    surface: LayerSurface,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Logical size from the compositor; (0, 0) until the first configure.
    size: (u32, u32),
    scale120: Option<u32>,
    int_scale: i32,
    /// Buttons in buffer pixels, from the last drawing.
    hits: Vec<(Hit, draw::Rect)>,
    hover: Option<Hit>,
    pointer: (f64, f64),
}

impl Dialog {
    fn scale(&self) -> f64 {
        match (self.scale120, &self.viewport) {
            (Some(n), Some(_)) => n as f64 / 120.0,
            _ => self.int_scale as f64,
        }
    }

    fn hit_at(&self, (x, y): (f64, f64)) -> Option<Hit> {
        let s = self.scale();
        let (x, y) = ((x * s) as i32, (y * s) as i32);
        self.hits
            .iter()
            .find(|(_, r)| x >= r.0 && y >= r.1 && x < r.0 + r.2 && y < r.1 + r.3)
            .map(|(h, _)| *h)
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
    }
}

struct App {
    qh: QueueHandle<Self>,
    loop_handle: LoopHandle<'static, Self>,
    registry: RegistryState,
    compositor: CompositorState,
    outputs: OutputState,
    seats: SeatState,
    shm: Shm,
    pool: SlotPool,
    layer_shell: LayerShell,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    ctrl: bool,
    caps_lock: bool,
    look: Look,
    waiting: VecDeque<Begin>,
    dialog: Option<Dialog>,
    helper_events: Sender<(u64, Event)>,
    next_helper_id: u64,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env().map_err(|e| format!("can't connect to the compositor: {e}"))?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;

    let (requests, request_rx) = channel::channel::<Request>();
    event_loop
        .handle()
        .insert_source(request_rx, |event, _, app| {
            if let channel::Event::Msg(r) = event {
                app.request(r);
            }
        })
        .map_err(|e| format!("event loop: {e}"))?;
    let (helper_events, helper_rx) = channel::channel::<(u64, Event)>();
    event_loop
        .handle()
        .insert_source(helper_rx, |event, _, app| {
            if let channel::Event::Msg((id, e)) = event {
                app.helper_event(id, e);
            }
        })
        .map_err(|e| format!("event loop: {e}"))?;

    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new(4096, &shm)?;
    let mut app = App {
        qh: qh.clone(),
        loop_handle: event_loop.handle(),
        registry: RegistryState::new(&globals),
        compositor: CompositorState::bind(&globals, &qh)?,
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        shm,
        pool,
        layer_shell: LayerShell::bind(&globals, &qh).map_err(|_| "the compositor has no layer shell")?,
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional: globals.bind(&qh, 1..=1, ()).ok(),
        keyboard: None,
        pointer: None,
        ctrl: false,
        caps_lock: false,
        look: load_look(),
        waiting: VecDeque::new(),
        dialog: None,
        helper_events,
        next_helper_id: 0,
    };
    WaylandSource::new(conn, queue)
        .insert(event_loop.handle())
        .map_err(|e| format!("event loop: {e}"))?;

    // Only once Wayland works: no point taking requests we can't show.
    let _bus = agent::register(requests).map_err(|e| format!("can't become the password agent: {e}"))?;

    loop {
        // An error here means the compositor is gone; polkitd sees us leave the bus.
        if let Err(e) = event_loop.dispatch(None, &mut app) {
            tracing::info!("the compositor went away ({e}); exiting");
            return Ok(());
        }
    }
}

fn load_look() -> Look {
    let config = kiln_config::Config::default_path().and_then(|p| kiln_config::Config::read(&p).ok());
    let theme = config
        .map(|c| c.theme)
        .unwrap_or_else(|| kiln_config::Config::default().theme);
    Look {
        text: kiln_draw::Text::load(&theme.font),
        theme,
    }
}

impl App {
    fn request(&mut self, r: Request) {
        match r {
            Request::Begin(b) => {
                self.waiting.push_back(b);
                if self.dialog.is_none() {
                    self.open_next();
                }
            }
            Request::Cancel(cookie) => {
                if self.dialog.as_ref().is_some_and(|d| d.begin.cookie == cookie) {
                    tracing::info!("the request was withdrawn");
                    self.finish(Err(AgentError::Cancelled("withdrawn".into())));
                } else if let Some(i) = self.waiting.iter().position(|b| b.cookie == cookie)
                    && let Some(b) = self.waiting.remove(i)
                {
                    let _ = b.reply.try_send(Err(AgentError::Cancelled("withdrawn".into())));
                }
            }
        }
    }

    fn open_next(&mut self) {
        let Some(begin) = self.waiting.pop_front() else { return };
        // The theme may have changed since the last dialog.
        self.look = load_look();
        let wl_surface = self.compositor.create_surface(&self.qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&wl_surface, &self.qh, ()));
        let fractional = self
            .fractional
            .as_ref()
            .map(|f| f.get_fractional_scale(&wl_surface, &self.qh, ()));
        // No output given: the compositor puts it on the monitor in use.
        let surface = self.layer_shell.create_layer_surface(
            &self.qh,
            wl_surface,
            Layer::Overlay,
            Some("kiln-polkit"),
            None,
        );
        surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        surface.set_exclusive_zone(-1);
        surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        self.dialog = Some(Dialog {
            begin,
            chosen: 0,
            helper: None,
            helper_id: 0,
            prompt: None,
            submit_when_ready: false,
            answered: false,
            broken: false,
            input: Password::new(),
            checking: false,
            status: None,
            pam_error: None,
            failures: 0,
            surface,
            viewport,
            fractional,
            size: (0, 0),
            scale120: None,
            int_scale: 1,
            hits: Vec::new(),
            hover: None,
            pointer: (0.0, 0.0),
        });
        self.start_helper();
    }

    /// Start a new conversation with the helper for the chosen user.
    fn start_helper(&mut self) {
        self.next_helper_id += 1;
        let id = self.next_helper_id;
        let events = self.helper_events.clone();
        let Some(d) = &mut self.dialog else { return };
        d.helper = None;
        d.helper_id = id;
        d.prompt = None;
        d.answered = false;
        d.broken = false;
        d.checking = false;
        d.pam_error = None;
        let user = &d.begin.users[d.chosen];
        match Helper::start(&user.login, &d.begin.cookie, id, events) {
            Ok(h) => d.helper = Some(h),
            Err(e) => {
                tracing::error!("{e}");
                d.status = Some((format!("Can't check passwords: {e}"), true));
                d.broken = true;
            }
        }
        self.redraw();
    }

    fn helper_event(&mut self, id: u64, event: Event) {
        let Some(d) = &mut self.dialog else { return };
        if id != d.helper_id {
            return;
        }
        match event {
            Event::Prompt { text, echo } => {
                d.prompt = Some((text, echo));
                d.checking = false;
                if std::mem::take(&mut d.submit_when_ready) {
                    self.submit();
                }
            }
            Event::Error(text) => {
                d.pam_error = Some(text.clone());
                d.status = Some((text, true));
            }
            Event::Info(text) => d.status = Some((text, false)),
            Event::Done(true) => {
                tracing::info!("authenticated");
                return self.finish(Ok(()));
            }
            Event::Done(false) => {
                d.helper = None;
                d.checking = false;
                d.prompt = None;
                d.input.clear();
                if d.answered {
                    // Wrong password: say so and start over, so the next try is ready.
                    d.failures += 1;
                    let msg = d.pam_error.take().unwrap_or_else(|| match d.failures {
                        1 => "Wrong password, try again".into(),
                        n => format!("Wrong password ({n} tries)"),
                    });
                    d.status = Some((msg, true));
                    let status = d.status.clone();
                    self.start_helper();
                    if let Some(d) = &mut self.dialog {
                        d.status = status;
                    }
                } else {
                    d.broken = true;
                    let msg = d
                        .pam_error
                        .take()
                        .unwrap_or_else(|| "Couldn't check the password. Press Enter to try again.".into());
                    d.status = Some((msg, true));
                }
            }
        }
        self.redraw();
    }

    fn submit(&mut self) {
        let Some(d) = &mut self.dialog else { return };
        if d.broken {
            return self.start_helper();
        }
        if d.checking || d.input.is_empty() {
            return;
        }
        let Some(helper) = &mut d.helper else { return };
        if d.prompt.is_none() {
            d.submit_when_ready = true;
            return;
        }
        let input = d.input.take();
        helper.respond(input.as_str());
        drop(input);
        d.prompt = None;
        d.answered = true;
        d.checking = true;
        d.status = None;
        self.redraw();
    }

    fn switch_user(&mut self) {
        let Some(d) = &mut self.dialog else { return };
        if d.begin.users.len() < 2 || d.checking {
            return;
        }
        d.chosen = (d.chosen + 1) % d.begin.users.len();
        d.input.clear();
        d.status = None;
        d.failures = 0;
        self.start_helper();
    }

    /// Answer polkitd and close the dialog; show the next one if any.
    fn finish(&mut self, result: Result<(), AgentError>) {
        if let Some(d) = self.dialog.take() {
            let _ = d.begin.reply.try_send(result);
        }
        self.open_next();
    }

    fn cancel(&mut self) {
        tracing::info!("cancelled");
        self.finish(Err(AgentError::Cancelled("cancelled by the user".into())));
    }

    fn redraw(&mut self) {
        if let Err(e) = self.draw() {
            tracing::warn!("drawing the dialog: {e}");
        }
    }

    fn draw(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(d) = &mut self.dialog else { return Ok(()) };
        if d.size.0 == 0 || d.size.1 == 0 {
            return Ok(());
        }
        let scale = d.scale();
        let (lw, lh) = d.size;
        let w = (lw as f64 * scale).round() as i32;
        let h = (lh as f64 * scale).round() as i32;
        let stride = w as usize * 4;
        let (buffer, pixels) = self.pool.create_buffer(w, h, stride as i32, wl_shm::Format::Argb8888)?;

        let bg = draw::scrim(&self.look.theme);
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
        let user = &d.begin.users[d.chosen];
        let ui = draw::Ui {
            message: &d.begin.message,
            action_id: &d.begin.action_id,
            user_label: &user.label,
            can_switch: d.begin.users.len() > 1,
            prompt: d.prompt.as_ref().map(|(p, _)| p.as_str()),
            echo: d.prompt.as_ref().is_some_and(|(_, e)| *e),
            input: &d.input,
            checking: d.checking,
            status: d.status.as_ref().map(|(s, e)| (s.as_str(), *e)),
            caps_lock: self.caps_lock,
            hover: d.hover,
        };
        let card = draw::card(&self.look, &ui, scale, w, h);
        card.canvas.write_argb8888(pixels, w, h, stride, card.at.0, card.at.1);
        d.hits = card.hits;

        let surface = d.surface.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        match &d.viewport {
            Some(v) => v.set_destination(lw as i32, lh as i32),
            None => surface.set_buffer_scale(d.int_scale),
        }
        buffer.attach_to(surface)?;
        surface.commit();
        Ok(())
    }

    fn key(&mut self, event: KeyEvent) {
        let ctrl = self.ctrl;
        let Some(d) = &mut self.dialog else { return };
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => return self.submit(),
            Keysym::Escape => return self.cancel(),
            Keysym::Tab | Keysym::ISO_Left_Tab => return self.switch_user(),
            Keysym::BackSpace if ctrl => d.input.clear(),
            Keysym::BackSpace => d.input.pop(),
            Keysym::u if ctrl => d.input.clear(),
            _ => match &event.utf8 {
                Some(t) if !ctrl && !t.is_empty() && !t.chars().any(char::is_control) && !d.checking => {
                    d.input.push(t);
                    // Typing again after a wrong password clears the message.
                    if d.status.as_ref().is_some_and(|(_, err)| *err) && !d.broken {
                        d.status = None;
                    }
                }
                _ => return,
            },
        }
        self.redraw();
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.dialog.as_ref().is_some_and(|d| d.surface == *layer) {
            self.cancel();
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(d) = self.dialog.as_mut().filter(|d| d.surface == *layer) {
            d.size = configure.new_size;
        }
        self.redraw();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, factor: i32) {
        if let Some(d) = self.dialog.as_mut().filter(|d| d.surface.wl_surface() == surface) {
            d.int_scale = factor.max(1);
            self.redraw();
        }
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

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let repeat = Box::new(|app: &mut App, _: &WlKeyboard, event: KeyEvent| app.key(event));
            match self
                .seats
                .get_keyboard_with_repeat(qh, &seat, None, self.loop_handle.clone(), repeat)
            {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => tracing::error!("no keyboard: {e}"),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seats.get_pointer(qh, &seat) {
                Ok(p) => self.pointer = Some(p),
                Err(e) => tracing::warn!("no pointer: {e}"),
            }
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
        if capability == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for App {
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
        if self.caps_lock != modifiers.caps_lock {
            self.caps_lock = modifiers.caps_lock;
            self.redraw();
        }
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlPointer, events: &[PointerEvent]) {
        let mut clicked = None;
        for e in events {
            let Some(d) = self.dialog.as_mut().filter(|d| d.surface.wl_surface() == &e.surface) else {
                continue;
            };
            d.pointer = e.position;
            match e.kind {
                PointerEventKind::Press { button: BTN_LEFT, .. } => clicked = d.hit_at(e.position),
                PointerEventKind::Leave { .. } => d.pointer = (-1.0, -1.0),
                _ => {}
            }
        }
        let Some(d) = &mut self.dialog else { return };
        let hover = d.hit_at(d.pointer);
        if hover != d.hover {
            d.hover = hover;
            self.redraw();
        }
        match clicked {
            Some(Hit::Ok) => self.submit(),
            Some(Hit::Cancel) => self.cancel(),
            Some(Hit::SwitchUser) => self.switch_user(),
            None => {}
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        app: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(d) = app.dialog.as_mut().filter(|d| d.fractional.as_ref() == Some(proxy))
        {
            d.scale120 = Some(scale);
            app.redraw();
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);
wayland_client::delegate_noop!(App: ignore WpViewporter);
wayland_client::delegate_noop!(App: ignore WpViewport);
wayland_client::delegate_noop!(App: ignore WpFractionalScaleManagerV1);
