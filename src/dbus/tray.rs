//! Tray icons: the StatusNotifierItem protocol (Steam, Discord, nm-applet, Telegram...).
//!
//! Apps register their icon with a "watcher". We are the watcher unless another program
//! already is (then we read its list). Each item is an object on its app's bus connection with
//! an icon (a name from the icon theme, or pixels), a title, a status, and maybe a menu,
//! exported through the `com.canonical.dbusmenu` interface.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
};

use zbus::{
    blocking::Connection,
    message::Header,
    object_server::SignalEmitter,
    zvariant::{OwnedValue, Value},
};

use super::Msg;

const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";

/// One tray icon, as the bar needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayItem {
    /// "bus-name/object/path": how the app registered it.
    pub id: String,
    pub title: String,
    /// Wants attention (blinking in other trays).
    pub attention: bool,
    pub icon_name: String,
    /// Extra folder the app's icons are in.
    pub icon_theme_path: String,
    /// The icon's pixels when it has no name: (width, height, ARGB32 big-endian).
    pub pixmap: Option<(i32, i32, Vec<u8>)>,
    pub has_menu: bool,
    /// Left click should open the menu rather than "activate".
    pub item_is_menu: bool,
}

/// One entry of a tray icon's menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuNode {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    /// Some(checked) for check and radio items.
    pub toggle: Option<bool>,
    pub children: Vec<MenuNode>,
}

/// The tray side of the bus worker.
pub struct Host {
    /// Registered items when we're the watcher; otherwise None and the other watcher is asked.
    own_items: Option<Arc<Mutex<Vec<String>>>>,
    items: Vec<TrayItem>,
    /// Unique bus name of each item's app, for matching signals (they come from that name).
    owners: HashMap<String, String>,
}

struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
    worker: Mutex<mpsc::Sender<Msg>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        // Apps pass either their bus name (the item is at the standard path) or a path (the
        // item is on the connection that called).
        let id = if service.starts_with('/') {
            format!("{sender}{service}")
        } else {
            format!("{service}/StatusNotifierItem")
        };
        let new = {
            let mut items = self.items.lock().unwrap();
            let new = !items.contains(&id);
            if new {
                items.push(id.clone());
            }
            new
        };
        if new {
            let _ = Self::status_notifier_item_registered(&emitter, &id).await;
            let _ = self.worker.lock().unwrap().send(Msg::TrayList);
        }
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// "bus.name/object/path" → (bus name, path).
fn split_id(id: &str) -> Option<(&str, &str)> {
    let i = id.find('/')?;
    Some((&id[..i], &id[i..]))
}

impl Host {
    pub fn start(conn: &Connection, worker: mpsc::Sender<Msg>) -> Self {
        let items = Arc::new(Mutex::new(Vec::new()));
        let watcher = Watcher {
            items: items.clone(),
            worker: Mutex::new(worker),
        };
        let own = conn.object_server().at(WATCHER_PATH, watcher).is_ok()
            && matches!(
                conn.request_name_with_flags(WATCHER_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into()),
                Ok(zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner)
            );
        if own {
            tracing::info!("tray: watching for tray icons");
            if let Ok(iface) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH) {
                let _ = zbus::block_on(Watcher::status_notifier_host_registered(iface.signal_emitter()));
            }
        } else {
            let _ = conn.object_server().remove::<Watcher, _>(WATCHER_PATH);
            tracing::info!("tray: another program is the tray watcher; showing its icons");
            // Tell it we show icons, so apps know there's a tray.
            let name = conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
            let _ = conn.call_method(
                Some(WATCHER_NAME),
                WATCHER_PATH,
                Some(WATCHER_NAME),
                "RegisterStatusNotifierHost",
                &(name,),
            );
        }
        Self {
            own_items: own.then_some(items),
            items: Vec::new(),
            owners: HashMap::new(),
        }
    }

    /// The registered items' ids.
    fn ids(&self, conn: &Connection) -> Vec<String> {
        match &self.own_items {
            Some(items) => items.lock().unwrap().clone(),
            None => zbus::blocking::fdo::PropertiesProxy::builder(conn)
                .destination(WATCHER_NAME)
                .and_then(|b| b.path(WATCHER_PATH))
                .and_then(|b| b.build())
                .and_then(|p| {
                    p.get(
                        zbus::names::InterfaceName::from_static_str_unchecked(WATCHER_NAME),
                        "RegisteredStatusNotifierItems",
                    )
                    .map_err(zbus::Error::from)
                })
                .ok()
                .and_then(|v| Vec::<String>::try_from(v).ok())
                .unwrap_or_default(),
        }
    }

    /// Read the list again: add new items, drop gone ones. Returns the items if they changed.
    pub fn refresh_list(&mut self, conn: &Connection) -> Option<Vec<TrayItem>> {
        let ids = self.ids(conn);
        let before = self.items.clone();
        self.items.retain(|i| ids.contains(&i.id));
        for id in ids {
            if self.items.iter().any(|i| i.id == id) {
                continue;
            }
            if let Some(item) = fetch(conn, &id) {
                if let Some((bus, _)) = split_id(&id)
                    && let Some(owner) = owner_of(conn, bus)
                {
                    self.owners.insert(id.clone(), owner);
                }
                self.items.push(item);
            }
        }
        (self.items != before).then(|| self.items.clone())
    }

    /// An item's app sent a change signal from `sender` for the object at `path`.
    pub fn refresh_item(&mut self, conn: &Connection, sender: &str, path: &str) -> Option<Vec<TrayItem>> {
        let id = self
            .items
            .iter()
            .map(|i| i.id.clone())
            .find(|id| split_id(id).is_some_and(|(bus, p)| p == path && (bus == sender || self.owners.get(id).is_some_and(|o| o == sender))))?;
        let item = fetch(conn, &id)?;
        let slot = self.items.iter_mut().find(|i| i.id == id)?;
        if *slot == item {
            return None;
        }
        *slot = item;
        Some(self.items.clone())
    }

    /// A bus name went away: drop its items.
    pub fn name_gone(&mut self, conn: &Connection, name: &str) -> Option<Vec<TrayItem>> {
        let gone: Vec<String> = self
            .items
            .iter()
            .map(|i| i.id.clone())
            .chain(self.own_items.iter().flat_map(|o| o.lock().unwrap().clone()))
            .filter(|id| split_id(id).is_some_and(|(bus, _)| bus == name) || self.owners.get(id).is_some_and(|o| o == name))
            .collect();
        if gone.is_empty() {
            return None;
        }
        if let Some(own) = &self.own_items {
            own.lock().unwrap().retain(|id| !gone.contains(id));
            if let Ok(iface) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH) {
                for id in &gone {
                    let _ = zbus::block_on(Watcher::status_notifier_item_unregistered(iface.signal_emitter(), id));
                }
            }
        }
        for id in &gone {
            self.owners.remove(id);
        }
        self.items.retain(|i| !gone.contains(&i.id));
        Some(self.items.clone())
    }

    /// A click on an item: `Activate` (left) or `SecondaryActivate` (middle).
    pub fn activate(&self, conn: &Connection, id: &str, x: i32, y: i32, secondary: bool) {
        let Some((bus, path)) = split_id(id) else { return };
        let method = if secondary { "SecondaryActivate" } else { "Activate" };
        if let Err(e) = conn.call_method(Some(bus), path, Some(ITEM_IFACE), method, &(x, y)) {
            tracing::debug!("tray {id}: {method}: {e}");
        }
    }

    pub fn scroll(&self, conn: &Connection, id: &str, delta: i32) {
        let Some((bus, path)) = split_id(id) else { return };
        let _ = conn.call_method(Some(bus), path, Some(ITEM_IFACE), "Scroll", &(delta, "vertical"));
    }

    /// An item's menu, read fresh.
    pub fn menu(&self, conn: &Connection, id: &str) -> Option<Vec<MenuNode>> {
        let (bus, _) = split_id(id)?;
        let menu_path = menu_path(conn, id)?;
        // Some apps build the menu only when told it's about to show.
        let _ = conn.call_method(Some(bus), menu_path.as_str(), Some(MENU_IFACE), "AboutToShow", &(0i32,));
        let reply = conn
            .call_method(
                Some(bus),
                menu_path.as_str(),
                Some(MENU_IFACE),
                "GetLayout",
                &(0i32, -1i32, Vec::<&str>::new()),
            )
            .map_err(|e| tracing::debug!("tray {id}: GetLayout: {e}"))
            .ok()?;
        let body = reply.body();
        let (_rev, root): (u32, Layout) = body
            .deserialize()
            .map_err(|e| tracing::debug!("tray {id}: menu layout: {e}"))
            .ok()?;
        Some(root.2.iter().filter_map(|c| parse_node(c)).collect())
    }

    pub fn menu_click(&self, conn: &Connection, id: &str, item: i32) {
        let Some((bus, _)) = split_id(id) else { return };
        let Some(path) = menu_path(conn, id) else { return };
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        if let Err(e) = conn.call_method(
            Some(bus),
            path.as_str(),
            Some(MENU_IFACE),
            "Event",
            &(item, "clicked", Value::from(""), time),
        ) {
            tracing::debug!("tray {id}: menu click: {e}");
        }
    }
}

fn owner_of(conn: &Connection, bus: &str) -> Option<String> {
    if bus.starts_with(':') {
        return Some(bus.to_string());
    }
    let proxy = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
    let name = zbus::names::BusName::try_from(bus).ok()?;
    proxy.get_name_owner(name).ok().map(|o| o.to_string())
}

fn props(conn: &Connection, id: &str) -> Option<HashMap<String, OwnedValue>> {
    let (bus, path) = split_id(id)?;
    let proxy = zbus::blocking::fdo::PropertiesProxy::builder(conn)
        .destination(bus.to_string())
        .ok()?
        .path(path.to_string())
        .ok()?
        .build()
        .ok()?;
    proxy
        .get_all(zbus::names::InterfaceName::from_static_str_unchecked(ITEM_IFACE))
        .map_err(|e| tracing::debug!("tray {id}: {e}"))
        .ok()
}

fn menu_path(conn: &Connection, id: &str) -> Option<String> {
    let p = props(conn, id)?;
    let path = p.get("Menu")?;
    let path = zbus::zvariant::OwnedObjectPath::try_from(path.clone()).ok()?;
    (path.as_str() != "/").then(|| path.to_string())
}

/// Read an item's properties. Passive items (nothing to show right now) are skipped.
fn fetch(conn: &Connection, id: &str) -> Option<TrayItem> {
    let p = props(conn, id)?;
    let s = |k: &str| p.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    let status = s("Status");
    if status == "Passive" {
        return None;
    }
    let attention = status == "NeedsAttention";
    let pixmap_key = if attention { "AttentionIconPixmap" } else { "IconPixmap" };
    let pixmap = p
        .get(pixmap_key)
        .or_else(|| p.get("IconPixmap"))
        .and_then(|v| Vec::<(i32, i32, Vec<u8>)>::try_from(v.clone()).ok())
        .and_then(|mut list| {
            // The one closest to 32 pixels, bigger preferred.
            list.retain(|(w, h, d)| (1..=1024).contains(w) && (1..=1024).contains(h) && d.len() >= (*w * *h * 4) as usize);
            list.sort_by_key(|(w, _, _)| if *w >= 32 { *w - 32 } else { 1000 + 32 - *w });
            list.into_iter().next()
        });
    let icon_name = match (attention, s("AttentionIconName")) {
        (true, n) if !n.is_empty() => n,
        _ => s("IconName"),
    };
    let title = match s("Title") {
        t if t.is_empty() => s("Id"),
        t => t,
    };
    let menu = p
        .get("Menu")
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v.clone()).ok())
        .is_some_and(|m| m.as_str() != "/");
    Some(TrayItem {
        id: id.to_string(),
        title,
        attention,
        icon_name,
        icon_theme_path: s("IconThemePath"),
        pixmap,
        has_menu: menu,
        item_is_menu: p.get("ItemIsMenu").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false),
    })
}

/// The top of a menu layout: (id, properties, children). Each child is a variant holding
/// the same structure.
/// The id and properties of the top aren't used, but have to be there to match the reply.
#[derive(serde::Deserialize, zbus::zvariant::Type)]
#[allow(dead_code)]
struct Layout(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// One `(ia{sv}av)` layout node, children included.
fn parse_node(v: &Value<'_>) -> Option<MenuNode> {
    let v = match v {
        Value::Value(inner) => inner.as_ref(),
        other => other,
    };
    let Value::Structure(s) = v else { return None };
    let f = s.fields();
    let id = i32::try_from(f.first()?).ok()?;
    // Some apps wrap the property table and the children in variants; look through them.
    fn unwrap<'a>(v: Option<&'a Value<'a>>) -> Option<&'a Value<'a>> {
        match v {
            Some(Value::Value(inner)) => Some(inner.as_ref()),
            other => other,
        }
    }
    let mut props: HashMap<String, Value<'_>> = HashMap::new();
    if let Some(Value::Dict(d)) = unwrap(f.get(1)) {
        for (k, val) in d.iter() {
            if let Ok(k) = <&str>::try_from(k) {
                let val = match val {
                    Value::Value(inner) => inner.as_ref().try_clone().ok(),
                    other => other.try_clone().ok(),
                };
                if let Some(val) = val {
                    props.insert(k.to_string(), val);
                }
            }
        }
    }
    let str_prop = |k: &str| props.get(k).and_then(|v| <&str>::try_from(v).ok()).map(String::from);
    let bool_prop = |k: &str| props.get(k).and_then(|v| bool::try_from(v).ok());
    let int_prop = |k: &str| props.get(k).and_then(|v| i32::try_from(v).ok());
    // Hidden entries are left out.
    if bool_prop("visible") == Some(false) {
        return None;
    }
    let children = match unwrap(f.get(2)) {
        Some(Value::Array(a)) => a.iter().filter_map(parse_node).collect(),
        _ => Vec::new(),
    };
    let toggle = match str_prop("toggle-type").as_deref() {
        Some("checkmark" | "radio") => Some(int_prop("toggle-state") == Some(1)),
        _ => None,
    };
    Some(MenuNode {
        id,
        label: strip_mnemonic(&str_prop("label").unwrap_or_default()),
        enabled: bool_prop("enabled").unwrap_or(true),
        separator: str_prop("type").as_deref() == Some("separator"),
        toggle,
        children,
    })
}

/// "_Open" → "Open", "Save __as" → "Save _as".
fn strip_mnemonic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_split() {
        assert_eq!(split_id(":1.42/StatusNotifierItem"), Some((":1.42", "/StatusNotifierItem")));
        assert_eq!(
            split_id("org.kde.StatusNotifierItem-1-1/org/ayatana/NotificationItem/x"),
            Some(("org.kde.StatusNotifierItem-1-1", "/org/ayatana/NotificationItem/x"))
        );
    }

    #[test]
    fn mnemonics() {
        assert_eq!(strip_mnemonic("_Open"), "Open");
        assert_eq!(strip_mnemonic("Save __as"), "Save _as");
    }
}
