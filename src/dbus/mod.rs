//! The session bus: desktop notifications, tray icons and media players.
//!
//! Everything D-Bus happens off the main loop. A worker thread owns the connection, serves
//! the notification daemon and the tray watcher, and does every call to other programs (with
//! a timeout, so a hung app can't stall it). Small listener threads wait for signals (an icon
//! changed, a player started) and ask the worker to refresh. Results reach the main loop as
//! [`Event`]s; the main loop asks for things with [`Command`]s.

pub mod mpris;
pub mod notify;
pub mod tray;

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use smithay::reexports::calloop::channel;

use crate::state::State;

/// From the bus to the main loop.
pub enum Event {
    Notify(notify::Notification),
    /// An app withdrew a notification.
    CloseNotification(u32),
    /// The tray icons, whenever they change.
    Tray(Vec<tray::TrayItem>),
    /// A tray icon's menu, asked for with [`Command::TrayMenu`].
    TrayMenu { item: String, menu: Vec<tray::MenuNode> },
    /// The media player shown in the bar, whenever it changes.
    Media(Option<mpris::Media>),
}

/// From the main loop to the bus.
#[derive(Debug)]
pub enum Command {
    NotificationClosed { id: u32, reason: notify::CloseReason },
    ActionInvoked { id: u32, key: String },
    TrayActivate { item: String, x: i32, y: i32 },
    TraySecondary { item: String, x: i32, y: i32 },
    TrayScroll { item: String, delta: i32 },
    TrayMenu { item: String },
    TrayMenuClick { item: String, id: i32 },
    Media(mpris::MediaCommand),
    /// Set a backlight (by name) through logind.
    SetBrightness { device: String, value: u32 },
}

/// What the worker thread is told: a command, or that something should be read again.
pub(crate) enum Msg {
    Cmd(Command),
    /// The list of tray items may have changed.
    TrayList,
    /// One tray item's icon, title or status changed. (bus name, object path)
    TrayItem(String, String),
    /// A bus name went away; drop what it owned.
    NameGone(String),
    Media,
}

/// The main loop's end of the bus.
#[derive(Default)]
pub struct Bus {
    worker: Option<mpsc::Sender<Msg>>,
    pub tray: Vec<tray::TrayItem>,
    pub media: Option<mpris::Media>,
}

impl State {
    /// Connect to the session bus. Without one (nested in some setups, or a bare TTY), the
    /// tray, media and notification features just stay empty.
    pub fn start_dbus(&mut self) {
        if self.bus.worker.is_some() {
            return;
        }
        let (event_tx, event_rx) = channel::channel::<Event>();
        let inserted = self.loop_handle.insert_source(event_rx, |event, _, state| {
            if let channel::Event::Msg(e) = event {
                state.on_bus_event(e);
            }
        });
        if let Err(e) = inserted {
            return tracing::warn!("session bus channel: {e}");
        }
        let (tx, rx) = mpsc::channel();
        let options = Options {
            notifications: self.config.notifications.enabled,
        };
        let worker_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("dbus".into())
            .spawn(move || worker(options, event_tx, rx, worker_tx));
        match spawned {
            Ok(_) => self.bus.worker = Some(tx),
            Err(e) => tracing::warn!("session bus thread: {e}"),
        }
    }

    pub fn bus_command(&mut self, cmd: Command) {
        if let Some(w) = &self.bus.worker
            && w.send(Msg::Cmd(cmd)).is_err()
        {
            self.bus.worker = None;
        }
    }

    fn on_bus_event(&mut self, event: Event) {
        match event {
            Event::Notify(n) => self.show_notification(n),
            Event::CloseNotification(id) => self.withdraw_notification(id),
            Event::Tray(items) => {
                self.bus.tray = items;
                self.needs_redraw = true;
            }
            Event::TrayMenu { item, menu } => self.open_tray_menu(&item, menu),
            Event::Media(m) => {
                if self.bus.media != m {
                    self.bus.media = m;
                    self.needs_redraw = true;
                }
            }
        }
    }
}

struct Options {
    notifications: bool,
}

/// Calls to other programs give up after this, so a hung app can't stall the bus thread.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Media players are read again this often anyway, in case a signal was missed.
const MEDIA_POLL: Duration = Duration::from_secs(5);

fn worker(options: Options, events: channel::Sender<Event>, rx: mpsc::Receiver<Msg>, me: mpsc::Sender<Msg>) {
    let conn = match zbus::blocking::connection::Builder::session().and_then(|b| b.method_timeout(CALL_TIMEOUT).build()) {
        Ok(c) => c,
        Err(e) => return tracing::warn!("no session bus, so no tray, media or notifications: {e}"),
    };
    let notifications = options.notifications && notify::serve(&conn, events.clone());
    let mut tray = tray::Host::start(&conn, me.clone());
    let mut media = mpris::Players::default();
    listen(&conn, me.clone());

    // Read everything once at the start.
    let _ = me.send(Msg::TrayList);
    let _ = me.send(Msg::Media);
    let mut next_media_poll = Instant::now() + MEDIA_POLL;
    loop {
        let timeout = next_media_poll.saturating_duration_since(Instant::now());
        let msg = match rx.recv_timeout(timeout) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                next_media_poll = Instant::now() + MEDIA_POLL;
                Msg::Media
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        // False once the compositor has gone (it's shutting down).
        let send = |e: Event| events.send(e).is_ok();
        let alive = match msg {
            Msg::Cmd(cmd) => {
                match cmd {
                    Command::NotificationClosed { id, reason } if notifications => notify::closed(&conn, id, reason),
                    Command::ActionInvoked { id, key } if notifications => notify::action_invoked(&conn, id, &key),
                    Command::NotificationClosed { .. } | Command::ActionInvoked { .. } => {}
                    Command::TrayActivate { item, x, y } => tray.activate(&conn, &item, x, y, false),
                    Command::TraySecondary { item, x, y } => tray.activate(&conn, &item, x, y, true),
                    Command::TrayScroll { item, delta } => tray.scroll(&conn, &item, delta),
                    Command::TrayMenu { item } => {
                        if let Some(menu) = tray.menu(&conn, &item) {
                            send(Event::TrayMenu { item, menu });
                        }
                    }
                    Command::TrayMenuClick { item, id } => tray.menu_click(&conn, &item, id),
                    Command::Media(c) => {
                        // A key can come before the player was ever read (e.g. Spotify just started).
                        if !media.has_shown()
                            && let Some(m) = media.refresh(&conn)
                        {
                            send(Event::Media(m));
                        }
                        media.command(&conn, c);
                        let _ = me.send(Msg::Media);
                    }
                    Command::SetBrightness { device, value } => {
                        if let Err(e) = conn.call_method(
                            Some("org.freedesktop.login1"),
                            "/org/freedesktop/login1/session/auto",
                            Some("org.freedesktop.login1.Session"),
                            "SetBrightness",
                            &("backlight", device.as_str(), value),
                        ) {
                            tracing::warn!("brightness: {e}");
                        }
                    }
                }
                true
            }
            Msg::TrayList => tray.refresh_list(&conn).is_none_or(|items| send(Event::Tray(items))),
            Msg::TrayItem(bus, path) => tray
                .refresh_item(&conn, &bus, &path)
                .is_none_or(|items| send(Event::Tray(items))),
            Msg::NameGone(name) => {
                if name.starts_with(mpris::PREFIX) || name.starts_with(':') {
                    let _ = me.send(Msg::Media);
                }
                tray.name_gone(&conn, &name).is_none_or(|items| send(Event::Tray(items)))
            }
            Msg::Media => media.refresh(&conn).is_none_or(|m| send(Event::Media(m))),
        };
        if !alive {
            return;
        }
    }
}

/// Wait for the signals that mean something should be read again, each on its own thread.
fn listen(conn: &zbus::blocking::Connection, me: mpsc::Sender<Msg>) {
    type ToMsg = fn(&zbus::Message) -> Option<Msg>;
    let rules: [(&str, ToMsg); 4] = [
        // A program left the bus: its tray icons and players go with it.
        (
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
            |m| {
                let (name, _old, new): (String, String, String) = m.body().deserialize().ok()?;
                if new.is_empty() {
                    Some(Msg::NameGone(name))
                } else if name.starts_with(mpris::PREFIX) {
                    Some(Msg::Media)
                } else {
                    None
                }
            },
        ),
        // A tray icon changed (NewIcon, NewStatus, NewTitle, ...).
        ("type='signal',interface='org.kde.StatusNotifierItem'", |m| {
            let h = m.header();
            Some(Msg::TrayItem(h.sender()?.to_string(), h.path()?.to_string()))
        }),
        // Another program's tray watcher listed or dropped an item.
        ("type='signal',interface='org.kde.StatusNotifierWatcher'", |_| {
            Some(Msg::TrayList)
        }),
        // A player started, paused or changed track.
        (
            "type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path='/org/mpris/MediaPlayer2'",
            |_| Some(Msg::Media),
        ),
    ];
    for (rule, to_msg) in rules {
        let iter = match zbus::MatchRule::try_from(rule).and_then(|r| zbus::blocking::MessageIterator::for_match_rule(r, conn, Some(64)))
        {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("can't watch the bus for {rule}: {e}");
                continue;
            }
        };
        let me = me.clone();
        let _ = std::thread::Builder::new().name("dbus-signals".into()).spawn(move || {
            for msg in iter.flatten() {
                if let Some(m) = to_msg(&msg)
                    && me.send(m).is_err()
                {
                    return;
                }
            }
        });
    }
}
