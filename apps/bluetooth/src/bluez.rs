//! BlueZ, the Bluetooth service, over the system bus: what adapter and devices there are,
//! kept up to date from its signals, the things that can be done to them (each on a thread of
//! its own, as connecting or pairing can take many seconds), and the pairing agent that asks
//! the user to confirm a passkey or type a PIN.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use wt_ui::egui;
use zbus::{
    MatchRule,
    blocking::{Connection, MessageIterator, fdo::ObjectManagerProxy},
    message::Type as MessageType,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const BLUEZ: &str = "org.bluez";
const AGENT_PATH: &str = "/org/wayland_time/bluetooth/agent";

#[derive(Debug, Clone, PartialEq)]
pub struct Adapter {
    pub path: String,
    /// The name other devices see.
    pub name: String,
    pub powered: bool,
    pub discovering: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub path: String,
    pub address: String,
    /// What the device calls itself, or the name it was given here.
    pub name: String,
    /// Whether it has a name of its own (nameless ones are mostly not worth listing).
    pub named: bool,
    /// A freedesktop icon name, like "audio-headset" (may be empty).
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    /// Signal strength while searching, in dBm.
    pub rssi: Option<i16>,
    /// Battery level, from devices that report it.
    pub battery: Option<u8>,
}

/// Something the pairing agent needs the user for.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    /// Both sides show this number: does it match?
    Confirm {
        device: String,
        passkey: u32,
    },
    /// Type this on the device (no answer needed).
    ShowPin {
        device: String,
        pin: String,
    },
    ShowPasskey {
        device: String,
        passkey: u32,
    },
    /// The device shows a code to type here.
    EnterPin {
        device: String,
    },
    EnterPasskey {
        device: String,
    },
    /// A device wants to connect or use a service.
    Authorize {
        device: String,
    },
}

impl Prompt {
    /// Prompts that only show something and wait for nothing.
    pub fn is_info(&self) -> bool {
        matches!(self, Prompt::ShowPin { .. } | Prompt::ShowPasskey { .. })
    }
}

pub enum Answer {
    Yes,
    No,
    Text(String),
}

#[derive(Default)]
pub struct Model {
    /// Whether the Bluetooth service is running.
    pub service: bool,
    pub adapter: Option<Adapter>,
    pub devices: Vec<Device>,
    /// Devices being connected, disconnected, paired or forgotten.
    pub busy: HashSet<String>,
    pub prompt: Option<(Prompt, Option<async_channel::Sender<Answer>>)>,
    /// The last thing that went wrong, to show once.
    pub error: Option<String>,
    /// Read at least once (so "not running" isn't shown before we know).
    pub loaded: bool,
}

#[derive(Clone)]
pub struct Bluez {
    conn: Connection,
    pub model: Arc<Mutex<Model>>,
    ctx: egui::Context,
    refresh: mpsc::Sender<()>,
}

impl Bluez {
    pub fn start(ctx: &egui::Context) -> Result<Self, String> {
        let conn = Connection::system().map_err(|e| format!("can't reach the system bus: {e}"))?;
        let (refresh, wanted) = mpsc::channel();
        let me = Self {
            conn,
            model: Arc::new(Mutex::new(Model::default())),
            ctx: ctx.clone(),
            refresh,
        };
        let agent = Agent { bt: me.clone() };
        if let Err(e) = me.conn.object_server().at(AGENT_PATH, agent) {
            tracing::warn!("no pairing agent: {e}");
        }
        // Read again when BlueZ says something changed (a little after, as changes come in
        // bursts), when it starts or stops, and every few seconds in case a signal was missed.
        let reader = me.clone();
        std::thread::spawn(move || {
            let mut agent_registered = false;
            loop {
                reader.read(&mut agent_registered);
                let _ = wanted.recv_timeout(Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(150));
                while wanted.try_recv().is_ok() {}
            }
        });
        me.listen(MatchRule::builder().msg_type(MessageType::Signal).sender(BLUEZ));
        me.listen(
            MatchRule::builder()
                .msg_type(MessageType::Signal)
                .sender("org.freedesktop.DBus")
                .and_then(|r| r.member("NameOwnerChanged"))
                .and_then(|r| r.add_arg(BLUEZ)),
        );
        Ok(me)
    }

    fn listen(&self, rule: zbus::Result<zbus::match_rule::Builder<'static>>) {
        let me = self.clone();
        std::thread::spawn(move || {
            let iter = rule
                .map(|r| r.build())
                .and_then(|r| MessageIterator::for_match_rule(r, &me.conn, None));
            match iter {
                Ok(iter) => {
                    for _ in iter {
                        let _ = me.refresh.send(());
                    }
                }
                Err(e) => tracing::warn!("not watching Bluetooth changes: {e}"),
            }
        });
    }

    /// Read everything BlueZ knows.
    fn read(&self, agent_registered: &mut bool) {
        let objects = ObjectManagerProxy::builder(&self.conn)
            .destination(BLUEZ)
            .and_then(|b| b.path("/"))
            .and_then(|b| b.build())
            .and_then(|p| p.get_managed_objects().map_err(zbus::Error::from));
        let mut adapter = None;
        let mut devices = Vec::new();
        let service = objects.is_ok();
        if let Ok(objects) = objects {
            let mut paths: Vec<_> = objects.keys().collect();
            paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            for path in paths {
                let ifaces = &objects[path];
                let get = |iface: &str| ifaces.iter().find(|(name, _)| name.as_str() == iface).map(|(_, p)| p);
                if let Some(p) = get("org.bluez.Adapter1")
                    && adapter.is_none()
                {
                    adapter = Some(Adapter {
                        path: path.to_string(),
                        name: prop(p, "Alias").or_else(|| prop(p, "Name")).unwrap_or_default(),
                        powered: prop(p, "Powered").unwrap_or(false),
                        discovering: prop(p, "Discovering").unwrap_or(false),
                    });
                }
                if let Some(p) = get("org.bluez.Device1") {
                    let address: String = prop(p, "Address").unwrap_or_default();
                    let name: Option<String> = prop(p, "Name");
                    devices.push(Device {
                        path: path.to_string(),
                        name: prop(p, "Alias").or(name.clone()).unwrap_or_else(|| address.clone()),
                        named: name.is_some(),
                        address,
                        icon: prop(p, "Icon").unwrap_or_default(),
                        paired: prop(p, "Paired").unwrap_or(false) || prop(p, "Bonded").unwrap_or(false),
                        connected: prop(p, "Connected").unwrap_or(false),
                        rssi: prop(p, "RSSI"),
                        battery: get("org.bluez.Battery1").and_then(|b| prop(b, "Percentage")),
                    });
                }
            }
        }
        // Devices only on the adapter we show.
        if let Some(a) = &adapter {
            let prefix = format!("{}/", a.path);
            devices.retain(|d| d.path.starts_with(&prefix));
        }
        if service && !*agent_registered {
            *agent_registered = self.register_agent();
        }
        if !service {
            *agent_registered = false;
        }
        let mut m = self.model.lock().unwrap();
        let changed = !m.loaded || m.service != service || m.adapter != adapter || m.devices != devices;
        m.loaded = true;
        m.service = service;
        m.adapter = adapter;
        m.devices = devices;
        drop(m);
        if changed {
            self.ctx.request_repaint();
        }
    }

    /// Answer BlueZ's pairing questions while the window is open.
    fn register_agent(&self) -> bool {
        let path = zbus::zvariant::ObjectPath::from_static_str_unchecked(AGENT_PATH);
        const MANAGER: &str = "org.bluez.AgentManager1";
        let registered = self.call("/org/bluez", MANAGER, "RegisterAgent", &(&path, "KeyboardDisplay"));
        if let Err(e) = &registered {
            tracing::warn!("no pairing agent: {e}");
        } else if let Err(e) = self.call("/org/bluez", MANAGER, "RequestDefaultAgent", &(&path,)) {
            tracing::info!("another pairing agent is the default: {e}");
        }
        registered.is_ok()
    }

    fn set_error(&self, text: String) {
        self.model.lock().unwrap().error = Some(text);
        self.ctx.request_repaint();
    }

    /// Run `work` on a thread, with `path` marked busy meanwhile; its error is shown.
    fn background(&self, path: Option<String>, work: impl FnOnce(&Self) -> Result<(), String> + Send + 'static) {
        if let Some(p) = &path {
            self.model.lock().unwrap().busy.insert(p.clone());
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let result = work(&me);
            if let Some(p) = &path {
                me.model.lock().unwrap().busy.remove(p);
            }
            if let Err(e) = result {
                me.set_error(e);
            }
            let _ = me.refresh.send(());
            me.ctx.request_repaint();
        });
    }

    fn call(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> zbus::Result<()> {
        self.conn
            .call_method(Some(BLUEZ), path, Some(iface), method, body)
            .map(|_| ())
    }

    fn set_prop(&self, path: &str, iface: &str, name: &str, value: Value<'_>) -> zbus::Result<()> {
        self.call(path, "org.freedesktop.DBus.Properties", "Set", &(iface, name, value))
    }

    fn device_name(&self, path: &str) -> String {
        let m = self.model.lock().unwrap();
        m.devices
            .iter()
            .find(|d| d.path == path)
            .map_or_else(|| "the device".to_string(), |d| d.name.clone())
    }

    pub fn set_powered(&self, on: bool) {
        let Some(adapter) = self.model.lock().unwrap().adapter.clone() else {
            return;
        };
        self.background(None, move |me| {
            me.set_prop(&adapter.path, "org.bluez.Adapter1", "Powered", Value::from(on))
                .map_err(|e| match error_name(&e).as_deref() {
                    Some("Blocked") | Some("Failed") if on => {
                        "Bluetooth is blocked (airplane mode, or a switch on the computer)".to_string()
                    }
                    _ => format!(
                        "Couldn't turn Bluetooth {}: {}",
                        if on { "on" } else { "off" },
                        message(&e)
                    ),
                })
        });
    }

    pub fn search(&self, on: bool) {
        let Some(adapter) = self.model.lock().unwrap().adapter.clone() else {
            return;
        };
        self.background(None, move |me| {
            let method = if on { "StartDiscovery" } else { "StopDiscovery" };
            match me.call(&adapter.path, "org.bluez.Adapter1", method, &()) {
                Ok(()) => Ok(()),
                // Already searching (or already stopped).
                Err(e) if matches!(error_name(&e).as_deref(), Some("InProgress" | "NotAuthorized")) => Ok(()),
                Err(e) => Err(format!("Couldn't search for devices: {}", message(&e))),
            }
        });
    }

    pub fn connect(&self, path: &str) {
        let name = self.device_name(path);
        let path = path.to_string();
        self.background(Some(path.to_string()), move |me| {
            me.call(&path, "org.bluez.Device1", "Connect", &())
                .or_else(already)
                .map_err(|e| connect_error(&name, &e))
        });
    }

    pub fn disconnect(&self, path: &str) {
        let name = self.device_name(path);
        let path = path.to_string();
        self.background(Some(path.clone()), move |me| {
            me.call(&path, "org.bluez.Device1", "Disconnect", &())
                .map_err(|e| format!("Couldn't disconnect {name}: {}", message(&e)))
        });
    }

    /// Pair, then trust (so it can reconnect by itself) and connect.
    pub fn pair(&self, path: &str) {
        let name = self.device_name(path);
        let path = path.to_string();
        self.background(Some(path.clone()), move |me| {
            me.call(&path, "org.bluez.Device1", "Pair", &())
                .or_else(already)
                .map_err(|e| match error_name(&e).as_deref() {
                    Some("AuthenticationCanceled") => format!("Pairing with {name} was cancelled"),
                    Some("AuthenticationFailed" | "AuthenticationRejected") => {
                        format!("Pairing with {name} failed: the codes didn't match or it said no")
                    }
                    Some("AuthenticationTimeout") => format!("{name} didn't answer in time"),
                    _ => connect_error(&name, &e),
                })?;
            let _ = me.set_prop(&path, "org.bluez.Device1", "Trusted", Value::from(true));
            me.call(&path, "org.bluez.Device1", "Connect", &())
                .or_else(already)
                .map_err(|e| connect_error(&name, &e))
        });
    }

    /// Forget a device: unpair it and drop it from the list.
    pub fn forget(&self, path: &str) {
        let Some(adapter) = self.model.lock().unwrap().adapter.clone() else {
            return;
        };
        let name = self.device_name(path);
        let path = path.to_string();
        self.background(Some(path.clone()), move |me| {
            let device = OwnedObjectPath::try_from(path.as_str()).map_err(|e| e.to_string())?;
            me.call(&adapter.path, "org.bluez.Adapter1", "RemoveDevice", &(device,))
                .map_err(|e| format!("Couldn't forget {name}: {}", message(&e)))
        });
    }

    /// The user answered (or dismissed) the prompt.
    pub fn answer(&self, answer: Answer) {
        if let Some((_, Some(tx))) = self.model.lock().unwrap().prompt.take() {
            let _ = tx.try_send(answer);
        }
        self.ctx.request_repaint();
    }

    pub fn clear_error(&self) {
        self.model.lock().unwrap().error = None;
    }

    /// Show `prompt` and wait for the user (None for prompts that only show something).
    async fn ask(&self, prompt: Prompt) -> Option<Answer> {
        let info = prompt.is_info();
        let (tx, rx) = async_channel::bounded(1);
        // A new question replaces an old one, which counts as a no.
        self.model.lock().unwrap().prompt = Some((prompt, (!info).then_some(tx)));
        self.ctx.request_repaint();
        if info {
            return None;
        }
        rx.recv().await.ok()
    }
}

/// "Already connected" or "already paired" is fine.
fn already(e: zbus::Error) -> zbus::Result<()> {
    match error_name(&e).as_deref() {
        Some("AlreadyConnected" | "AlreadyExists") => Ok(()),
        _ => Err(e),
    }
}

fn connect_error(name: &str, e: &zbus::Error) -> String {
    let msg = message(e);
    if msg.contains("page-timeout") || msg.contains("Host is down") || msg.contains("timeout") {
        format!("{name} didn't answer. Is it on, nearby and ready to connect?")
    } else if error_name(e).as_deref() == Some("NotReady") {
        "Bluetooth is off".to_string()
    } else {
        format!("Couldn't connect to {name}: {msg}")
    }
}

/// The last part of a BlueZ error's name, like "AuthenticationFailed".
fn error_name(e: &zbus::Error) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, _, _) => name.rsplit('.').next().map(str::to_string),
        _ => None,
    }
}

fn message(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) if !msg.is_empty() => msg.clone(),
        zbus::Error::MethodError(name, _, _) => name.to_string(),
        other => other.to_string(),
    }
}

fn prop<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    props.get(key)?.try_clone().ok().and_then(|v| T::try_from(v).ok())
}

/// What BlueZ hears back from the agent when the user says no or closes the question.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// The pairing agent: BlueZ calls it when pairing needs the user.
struct Agent {
    bt: Bluez,
}

impl Agent {
    async fn yes(&self, prompt: Prompt) -> Result<(), AgentError> {
        match self.bt.ask(prompt).await {
            Some(Answer::Yes) => Ok(()),
            Some(_) => Err(AgentError::Rejected("No".into())),
            None => Err(AgentError::Canceled("Closed".into())),
        }
    }

    async fn text(&self, prompt: Prompt) -> Result<String, AgentError> {
        match self.bt.ask(prompt).await {
            Some(Answer::Text(t)) => Ok(t),
            Some(_) => Err(AgentError::Rejected("No".into())),
            None => Err(AgentError::Canceled("Closed".into())),
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn release(&self) {}

    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        let device = self.bt.device_name(&device);
        self.text(Prompt::EnterPin { device }).await
    }

    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        let device = self.bt.device_name(&device);
        self.bt.ask(Prompt::ShowPin { device, pin: pincode }).await;
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        let device = self.bt.device_name(&device);
        let text = self.text(Prompt::EnterPasskey { device }).await?;
        text.trim()
            .parse()
            .map_err(|_| AgentError::Rejected("Not a number".into()))
    }

    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        let device = self.bt.device_name(&device);
        self.bt.ask(Prompt::ShowPasskey { device, passkey }).await;
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        let device = self.bt.device_name(&device);
        self.yes(Prompt::Confirm { device, passkey }).await
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        let device = self.bt.device_name(&device);
        self.yes(Prompt::Authorize { device }).await
    }

    async fn authorize_service(&self, device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        let device = self.bt.device_name(&device);
        self.yes(Prompt::Authorize { device }).await
    }

    /// BlueZ gave up on the question (the device went away, or it timed out).
    async fn cancel(&self) {
        self.bt.model.lock().unwrap().prompt = None;
        self.bt.ctx.request_repaint();
    }
}
