//! Drives that can be mounted, from UDisks2 on the system bus: listed every couple of seconds in
//! the background, and mounted, unmounted or ejected on request. Mounting an internal disk may
//! need an admin password, which the polkit agent asks for.

use std::{collections::HashMap, path::PathBuf, sync::mpsc, time::Duration};

use wayland_time_files::fs;
use wt_ui::egui;
use zbus::{
    blocking::{Connection, fdo::ObjectManagerProxy},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const UDISKS: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const LOOP: &str = "org.freedesktop.UDisks2.Loop";

/// A filesystem that can be mounted.
#[derive(Debug, Clone, PartialEq)]
pub struct Drive {
    /// The UDisks block device.
    block: OwnedObjectPath,
    /// The disk it's on, for ejecting.
    disk: Option<OwnedObjectPath>,
    pub label: String,
    /// Symbolic icon name.
    pub icon: String,
    /// Where it's mounted; None if it isn't.
    pub mount: Option<PathBuf>,
    /// The disk can be ejected (removable media, USB sticks).
    pub ejectable: bool,
}

/// What the background threads report.
pub enum Event {
    /// The drives now (only sent when they change).
    List(Vec<Drive>),
    /// A drive was mounted at `path`; `open` it if that was asked for.
    Mounted { label: String, path: PathBuf, open: bool },
    /// A drive was unmounted (and maybe ejected); it's no longer at `path`.
    Unmounted { label: String, path: PathBuf, ejected: bool },
    Failed { what: String, error: String },
}

pub struct Drives {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
    conn: Option<Connection>,
}

impl Drives {
    /// Start listing drives. Without a system bus or UDisks there are none.
    pub fn start(ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let conn = Connection::system()
            .inspect_err(|e| tracing::warn!("no system bus, so no drives to mount: {e}"))
            .ok();
        if let Some(conn) = conn.clone() {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || watch(&conn, &tx, &ctx));
        }
        Self {
            rx,
            tx,
            ctx: ctx.clone(),
            conn,
        }
    }

    /// What happened since last time.
    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// Mount `d` in the background; `open` it once mounted.
    pub fn mount(&self, d: &Drive, open: bool) {
        let what = format!("Couldn't mount {}", d.label);
        let d = d.clone();
        self.run(what, move |conn| {
            let path: String = call(conn, &d.block, FILESYSTEM, "Mount")?.body().deserialize()?;
            Ok(Event::Mounted {
                label: d.label,
                path: PathBuf::from(path),
                open,
            })
        });
    }

    /// Unmount `d`, and with `eject` also eject its disk.
    pub fn unmount(&self, d: &Drive, eject: bool) {
        let what = format!("Couldn't {} {}", if eject { "eject" } else { "unmount" }, d.label);
        let d = d.clone();
        self.run(what, move |conn| {
            call(conn, &d.block, FILESYSTEM, "Unmount")?;
            if let Some(disk) = d.disk.as_ref().filter(|_| eject) {
                call(conn, disk, DRIVE, "Eject")?;
            }
            Ok(Event::Unmounted {
                label: d.label,
                path: d.mount.unwrap_or_default(),
                ejected: eject,
            })
        });
    }

    fn run(&self, what: String, f: impl FnOnce(&Connection) -> zbus::Result<Event> + Send + 'static) {
        let Some(conn) = self.conn.clone() else { return };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let event = f(&conn).unwrap_or_else(|e| Event::Failed { what, error: message(&e) });
            let _ = tx.send(event);
            ctx.request_repaint();
        });
    }
}

/// Call a UDisks method that takes only an options dictionary (left empty).
fn call(conn: &Connection, path: &OwnedObjectPath, iface: &str, method: &str) -> zbus::Result<zbus::Message> {
    let options: HashMap<&str, Value> = HashMap::new();
    conn.call_method(Some(UDISKS), path, Some(iface), method, &(options,))
}

/// The readable part of a D-Bus error: UDisks puts the reason in the error's text.
fn message(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, text, _) => {
            if name.as_str().ends_with("NotAuthorizedDismissed") {
                return "the password prompt was cancelled".into();
            }
            text.clone().unwrap_or_else(|| name.to_string())
        }
        e => e.to_string(),
    }
}

/// List the drives every two seconds, sending the list when it changes.
fn watch(conn: &Connection, tx: &mpsc::Sender<Event>, ctx: &egui::Context) {
    let manager = match ObjectManagerProxy::builder(conn)
        .destination(UDISKS)
        .and_then(|b| b.path("/org/freedesktop/UDisks2"))
        .and_then(|b| b.build())
    {
        Ok(m) => m,
        Err(e) => return tracing::warn!("can't reach UDisks, so no drives to mount: {e}"),
    };
    let mut last = None;
    loop {
        match manager.get_managed_objects() {
            Ok(objects) => {
                let drives = list(&objects);
                if last.as_ref() != Some(&drives) {
                    last = Some(drives.clone());
                    if tx.send(Event::List(drives)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
            Err(e) => tracing::warn!("listing drives: {e}"),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

type Props = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<zbus::names::OwnedInterfaceName, Props>>;

fn get<T: TryFrom<OwnedValue>>(props: &Props, key: &str) -> Option<T> {
    T::try_from(props.get(key)?.try_clone().ok()?).ok()
}

/// The filesystems worth offering: not ones UDisks says to hide (EFI, recovery and swap
/// partitions), not ones the system mounted for itself (`/`, `/boot`, snaps), not other users' disk images.
fn list(objects: &Objects) -> Vec<Drive> {
    let iface = |o: &HashMap<zbus::names::OwnedInterfaceName, Props>, name: &str| -> Option<Props> {
        o.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| {
            v.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.try_clone().ok()?)))
                .collect()
        })
    };
    let mut out = Vec::new();
    for (path, o) in objects {
        let (Some(block), Some(filesystem)) = (iface(o, BLOCK), iface(o, FILESYSTEM)) else {
            continue;
        };
        if get(&block, "HintIgnore") == Some(true) || get::<String>(&block, "IdUsage").as_deref() != Some("filesystem") {
            continue;
        }
        let mounts: Vec<PathBuf> = get::<Vec<Vec<u8>>>(&filesystem, "MountPoints")
            .unwrap_or_default()
            .into_iter()
            .map(|mut p| {
                // Each is a NUL-terminated byte string.
                if p.last() == Some(&0) {
                    p.pop();
                }
                PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(p))
            })
            .collect();
        let mount = if mounts.is_empty() {
            // Disk images: only ones this user attached (snaps' are the system's).
            if let Some(lp) = iface(o, LOOP)
                && get::<u32>(&lp, "SetupByUID") != Some(unsafe { libc::getuid() })
            {
                continue;
            }
            None
        } else {
            // Mounted somewhere for people to use, or else part of the system.
            match mounts.into_iter().find(|m| ["/media/", "/run/media/", "/mnt/"].iter().any(|r| m.starts_with(r))) {
                Some(m) => Some(m),
                None => continue,
            }
        };
        let disk: Option<OwnedObjectPath> = get(&block, "Drive").filter(|d: &OwnedObjectPath| d.as_str() != "/");
        let disk_props = disk.as_ref().and_then(|d| iface(objects.get(d)?, DRIVE));
        let flag = |key| disk_props.as_ref().and_then(|p| get::<bool>(p, key)).unwrap_or(false);
        let removable = flag("Removable") || flag("MediaRemovable");
        let label = [get::<String>(&block, "IdLabel"), get(&block, "HintName")]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{} Volume", fs::size_text(get(&block, "Size").unwrap_or(0))));
        let icon = get::<String>(&block, "HintSymbolicIconName")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if removable {
                    "drive-removable-media-symbolic"
                } else {
                    "drive-harddisk-symbolic"
                }
                .into()
            });
        out.push(Drive {
            block: path.clone(),
            disk,
            label,
            icon,
            mount,
            ejectable: flag("Ejectable"),
        });
    }
    // Internal disks first, then removable ones; by name within each.
    out.sort_by_key(|d| (d.ejectable, d.label.to_lowercase(), d.block.to_string()));
    out
}
