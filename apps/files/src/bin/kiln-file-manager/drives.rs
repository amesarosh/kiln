//! Drives that can be mounted: disks and USB sticks from UDisks2 on the system bus, and phones,
//! cameras and music players (MTP, PTP, iPhones) from GVfs on the session bus. Listed every couple
//! of seconds in the background, and mounted, unmounted or ejected on request. Mounting an
//! internal disk may need an admin password, which the polkit agent asks for. A GVfs device's
//! files are reached through GVfs's FUSE folder (`/run/user/<uid>/gvfs/...`).

use std::{collections::HashMap, path::PathBuf, sync::mpsc, time::Duration};

use kiln_files::fs;
use kiln_ui::egui;
use zbus::{
    blocking::{Connection, fdo::ObjectManagerProxy},
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value},
};

const UDISKS: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const LOOP: &str = "org.freedesktop.UDisks2.Loop";

/// GVfs's volume monitors for devices that aren't block devices: Android phones and media
/// players (MTP), cameras (PTP) and iPhones (AFC).
const GVFS_MONITORS: [&str; 3] = [
    "org.gtk.vfs.MTPVolumeMonitor",
    "org.gtk.vfs.GPhoto2VolumeMonitor",
    "org.gtk.vfs.AfcVolumeMonitor",
];
const MONITOR_PATH: &str = "/org/gtk/Private/RemoteVolumeMonitor";
const MONITOR: &str = "org.gtk.Private.RemoteVolumeMonitor";
/// GVfs keeps its mounts here, monitors don't report them.
const GVFS_DAEMON: &str = "org.gtk.vfs.Daemon";
const TRACKER_PATH: &str = "/org/gtk/vfs/mounttracker";
const TRACKER: &str = "org.gtk.vfs.MountTracker";

/// A filesystem that can be mounted.
#[derive(Debug, Clone, PartialEq)]
pub struct Drive {
    source: Source,
    pub label: String,
    /// Symbolic icon name.
    pub icon: String,
    /// Where it's mounted; None if it isn't.
    pub mount: Option<PathBuf>,
    /// The disk can be ejected (removable media, USB sticks).
    pub ejectable: bool,
}

/// Where a drive comes from, and so how it's mounted.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    UDisks {
        /// The UDisks block device.
        block: OwnedObjectPath,
        /// The disk it's on, for ejecting.
        disk: Option<OwnedObjectPath>,
    },
    Gvfs {
        /// The volume monitor's bus name, and the volume's id there.
        monitor: &'static str,
        volume: String,
        /// Its address, like `mtp://Vendor_Model_Serial/`.
        uri: String,
        /// The mount's daemon and object, while mounted.
        mounted: Option<(String, OwnedObjectPath)>,
    },
}

/// What the background threads report.
pub enum Event {
    /// The drives now (only sent when they change).
    List(Vec<Drive>),
    /// A drive was mounted at `path`; `open` it if that was asked for.
    Mounted {
        label: String,
        path: PathBuf,
        open: bool,
    },
    /// A drive was unmounted (and maybe ejected); it's no longer at `path`.
    Unmounted {
        label: String,
        path: PathBuf,
        ejected: bool,
    },
    Failed {
        what: String,
        error: String,
    },
}

pub struct Drives {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
    /// UDisks.
    system: Option<Connection>,
    /// GVfs.
    session: Option<Connection>,
}

impl Drives {
    /// Start listing drives. Without a system bus or UDisks there are none.
    pub fn start(ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let system = Connection::system()
            .inspect_err(|e| tracing::warn!("no system bus, so no disks to mount: {e}"))
            .ok();
        let session = Connection::session()
            .inspect_err(|e| tracing::warn!("no session bus, so no phones or cameras to mount: {e}"))
            .ok();
        if system.is_some() || session.is_some() {
            let (system, session, tx, ctx) = (system.clone(), session.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || watch(system.as_ref(), session.as_ref(), &tx, &ctx));
        }
        Self {
            rx,
            tx,
            ctx: ctx.clone(),
            system,
            session,
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
        self.run(&d.source.clone(), what, move |conn| {
            let path = match &d.source {
                Source::UDisks { block, .. } => {
                    PathBuf::from(call(conn, block, FILESYSTEM, "Mount")?.body().deserialize::<String>()?)
                }
                Source::Gvfs {
                    monitor, volume, uri, ..
                } => {
                    // Without a mount operation: these devices don't ask for passwords.
                    let args = (volume.as_str(), "", 0u32, "");
                    conn.call_method(Some(*monitor), MONITOR_PATH, Some(MONITOR), "VolumeMount", &args)?;
                    gvfs_mounts(conn)?
                        .into_iter()
                        .find(|m| m.is(uri))
                        .map(|m| m.folder())
                        .filter(|p| !p.as_os_str().is_empty())
                        .ok_or_else(|| {
                            zbus::Error::Failure("GVfs gave it no folder (is gvfs-fuse installed?)".into())
                        })?
                }
            };
            Ok(Event::Mounted {
                label: d.label,
                path,
                open,
            })
        });
    }

    /// Unmount `d`, and with `eject` also eject its disk.
    pub fn unmount(&self, d: &Drive, eject: bool) {
        let what = format!("Couldn't {} {}", if eject { "eject" } else { "unmount" }, d.label);
        let d = d.clone();
        self.run(&d.source.clone(), what, move |conn| {
            match &d.source {
                Source::UDisks { block, disk } => {
                    call(conn, block, FILESYSTEM, "Unmount")?;
                    if let Some(disk) = disk.as_ref().filter(|_| eject) {
                        call(conn, disk, DRIVE, "Eject")?;
                    }
                }
                Source::Gvfs {
                    mounted: Some((daemon, object)),
                    ..
                } => {
                    // No mount operation to report back to.
                    let args = ("", ObjectPath::from_static_str_unchecked("/"), 0u32);
                    conn.call_method(
                        Some(daemon.as_str()),
                        object,
                        Some("org.gtk.vfs.Mount"),
                        "Unmount",
                        &args,
                    )?;
                }
                Source::Gvfs { mounted: None, .. } => {}
            }
            Ok(Event::Unmounted {
                label: d.label,
                path: d.mount.unwrap_or_default(),
                ejected: eject,
            })
        });
    }

    /// Run `f` in the background on the bus that `source` is on.
    fn run(&self, source: &Source, what: String, f: impl FnOnce(&Connection) -> zbus::Result<Event> + Send + 'static) {
        let conn = match source {
            Source::UDisks { .. } => &self.system,
            Source::Gvfs { .. } => &self.session,
        };
        let Some(conn) = conn.clone() else { return };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let event = f(&conn).unwrap_or_else(|e| Event::Failed {
                what,
                error: message(&e),
            });
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
fn watch(system: Option<&Connection>, session: Option<&Connection>, tx: &mpsc::Sender<Event>, ctx: &egui::Context) {
    let manager = system.and_then(|conn| {
        ObjectManagerProxy::builder(conn)
            .destination(UDISKS)
            .and_then(|b| b.path("/org/freedesktop/UDisks2"))
            .and_then(|b| b.build())
            .inspect_err(|e| tracing::warn!("can't reach UDisks, so no disks to mount: {e}"))
            .ok()
    });
    // Monitors that aren't installed are dropped after the first try.
    let mut monitors = GVFS_MONITORS.to_vec();
    let mut last = None;
    loop {
        let disks = match &manager {
            Some(m) => m.get_managed_objects().map(|o| list(&o)),
            None => Ok(Vec::new()),
        };
        match disks {
            Ok(mut drives) => {
                if let Some(conn) = session {
                    drives.extend(gvfs_list(conn, &mut monitors));
                }
                // Internal disks first, then removable ones and devices; by name within each.
                drives.sort_by_key(|d| {
                    let external = d.ejectable || matches!(d.source, Source::Gvfs { .. });
                    (external, d.label.to_lowercase(), format!("{:?}", d.source))
                });
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

/// A volume, as a GVfs volume monitor lists it.
#[derive(serde::Deserialize, Type)]
#[allow(dead_code)]
struct GvfsVolume {
    id: String,
    name: String,
    icon: String,
    symbolic_icon: String,
    uuid: String,
    activation_uri: String,
    can_mount: bool,
    should_automount: bool,
    drive_id: String,
    mount_id: String,
    identifiers: HashMap<String, String>,
    sort_key: String,
    expansion: HashMap<String, OwnedValue>,
}

/// A drive, as a GVfs volume monitor lists it (only read to get past it).
#[derive(serde::Deserialize, Type)]
#[allow(dead_code)]
struct GvfsDrive {
    id: String,
    name: String,
    icon: String,
    symbolic_icon: String,
    can_eject: bool,
    can_poll_for_media: bool,
    has_media: bool,
    is_media_removable: bool,
    is_media_check_automatic: bool,
    can_start: bool,
    can_start_degraded: bool,
    can_stop: bool,
    start_stop_type: u32,
    volume_ids: Vec<String>,
    identifiers: HashMap<String, String>,
    sort_key: String,
    expansion: HashMap<String, OwnedValue>,
}

/// A mount, as a GVfs volume monitor lists it (only read to get past it).
#[derive(serde::Deserialize, Type)]
#[allow(dead_code)]
struct GvfsMonitorMount {
    id: String,
    name: String,
    icon: String,
    symbolic_icon: String,
    uuid: String,
    root_uri: String,
    can_unmount: bool,
    volume_id: String,
    x_content_types: Vec<String>,
    sort_key: String,
    expansion: HashMap<String, OwnedValue>,
}

/// A mount, as GVfs's mount tracker lists it.
#[derive(serde::Deserialize, Type)]
#[allow(dead_code)]
struct GvfsMount {
    dbus_id: String,
    object: OwnedObjectPath,
    display_name: String,
    stable_name: String,
    x_content_types: String,
    icon: String,
    symbolic_icon: String,
    filename_encoding: String,
    user_visible: bool,
    fuse_mountpoint: Vec<u8>,
    spec: (Vec<u8>, HashMap<String, OwnedValue>),
    default_location: Vec<u8>,
}

impl GvfsMount {
    /// Where its files are: its FUSE folder. A NUL-terminated byte string, empty without gvfs-fuse.
    fn folder(&self) -> PathBuf {
        let mut p = self.fuse_mountpoint.clone();
        if p.last() == Some(&0) {
            p.pop();
        }
        PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(p))
    }

    /// Whether this is the mount of the volume at `uri` (`scheme://host/`): its spec has that
    /// type and (unescaped) host.
    fn is(&self, uri: &str) -> bool {
        let Some((scheme, rest)) = uri.split_once("://") else {
            return false;
        };
        let host = unescape(rest.split('/').next().unwrap_or_default());
        let item = |key: &str| -> Option<Vec<u8>> {
            let mut v = Vec::<u8>::try_from(self.spec.1.get(key)?.try_clone().ok()?).ok()?;
            if v.last() == Some(&0) {
                v.pop();
            }
            Some(v)
        };
        item("type").as_deref() == Some(scheme.as_bytes()) && item("host") == Some(host)
    }
}

/// Undo a URI's %XX escapes.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len())
            .then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(c) => {
                out.push(c);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

fn gvfs_mounts(conn: &Connection) -> zbus::Result<Vec<GvfsMount>> {
    conn.call_method(Some(GVFS_DAEMON), TRACKER_PATH, Some(TRACKER), "ListMounts2", &(true,))?
        .body()
        .deserialize()
}

/// The phones, cameras and players GVfs offers, mounted or not.
fn gvfs_list(conn: &Connection, monitors: &mut Vec<&'static str>) -> Vec<Drive> {
    let mut volumes = Vec::new();
    monitors.retain(|&monitor| {
        let reply = conn
            .call_method(Some(monitor), MONITOR_PATH, Some(MONITOR), "List", &())
            .and_then(|m| {
                m.body()
                    .deserialize::<(Vec<GvfsDrive>, Vec<GvfsVolume>, Vec<GvfsMonitorMount>)>()
            });
        match reply {
            Ok((_, vols, _)) => {
                volumes.extend(vols.into_iter().map(|v| (monitor, v)));
                true
            }
            Err(zbus::Error::MethodError(name, ..)) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" => {
                tracing::debug!("{monitor} isn't installed");
                false
            }
            Err(e) => {
                tracing::warn!("listing {monitor}'s devices: {e}");
                true
            }
        }
    });
    if volumes.is_empty() {
        return Vec::new();
    }
    let mounts = gvfs_mounts(conn)
        .inspect_err(|e| tracing::warn!("listing GVfs mounts: {e}"))
        .unwrap_or_default();
    volumes
        .into_iter()
        .filter(|(_, v)| v.can_mount && v.activation_uri.contains("://"))
        .map(|(monitor, v)| {
            let mount = mounts.iter().find(|m| m.is(&v.activation_uri));
            // A serialized GIcon: ". GThemedIcon phone-symbolic phone".
            let icon = v
                .symbolic_icon
                .split_whitespace()
                .find(|n| n.ends_with("-symbolic"))
                .unwrap_or("phone-symbolic")
                .to_string();
            Drive {
                source: Source::Gvfs {
                    monitor,
                    volume: v.id,
                    uri: v.activation_uri,
                    mounted: mount.map(|m| (m.dbus_id.clone(), m.object.clone())),
                },
                label: v.name,
                icon,
                mount: mount.map(GvfsMount::folder).filter(|p| !p.as_os_str().is_empty()),
                ejectable: false,
            }
        })
        .collect()
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
        if get(&block, "HintIgnore") == Some(true) || get::<String>(&block, "IdUsage").as_deref() != Some("filesystem")
        {
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
            // SAFETY: getuid can't fail.
            let me = unsafe { libc::getuid() };
            if let Some(lp) = iface(o, LOOP)
                && get::<u32>(&lp, "SetupByUID") != Some(me)
            {
                continue;
            }
            None
        } else {
            // Mounted somewhere for people to use, or else part of the system.
            match mounts
                .into_iter()
                .find(|m| ["/media/", "/run/media/", "/mnt/"].iter().any(|r| m.starts_with(r)))
            {
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
            source: Source::UDisks {
                block: path.clone(),
                disk,
            },
            label,
            icon,
            mount,
            ejectable: flag("Ejectable"),
        });
    }
    out
}
