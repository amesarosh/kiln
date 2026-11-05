//! The desktop portal backend: `org.freedesktop.impl.portal.FileChooser` on the session bus.
//!
//! xdg-desktop-portal forwards an app's "open a file" request here (when the portal config
//! picks `wayland-time` for FileChooser). Each request starts the picker window as its own
//! process and answers once it closes. The request's handle object takes `Close`, which
//! closes the window, for when the app gives up.

use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicI32, AtomicUsize, Ordering},
    },
};

use zbus::{
    ObjectServer,
    fdo::{RequestNameFlags, RequestNameReply},
    interface,
    zvariant::{OwnedObjectPath, OwnedValue, Structure, StructureBuilder, Value},
};

use wayland_time_files::fs::{self, Filter};

use crate::request::{Answer, Choice, Mode, Request};

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.wayland-time";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// Portal answers: 0 done, 1 cancelled, 2 something went wrong.
const OK: u32 = 0;
const CANCELLED: u32 = 1;
const FAILED: u32 = 2;

type Options = HashMap<String, OwnedValue>;
type Results = HashMap<String, OwnedValue>;

/// Pickers open right now. The portal only retires (see `retire_when_replaced`) when it's 0.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Serve the portal until the bus connection goes away, or until a newer version is installed.
pub fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::blocking::connection::Builder::session()?
        .serve_at(PATH, FileChooser)?
        .build()?;
    // Don't wait in line behind a portal that's already running: a second copy would only
    // pile up.
    match conn.request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => {
            tracing::info!("{BUS_NAME} is already served, exiting");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }
    tracing::info!("serving {BUS_NAME}");
    retire_when_replaced(&conn);
    Ok(())
}

/// D-Bus starts the portal once and it keeps running, so after a reinstall it's still the old
/// version. Once its program file has been replaced, step aside when no picker is open, so the
/// next request starts the new one.
fn retire_when_replaced(conn: &zbus::blocking::Connection) {
    let replaced = || {
        std::fs::read_link("/proc/self/exe").is_ok_and(|p| p.to_string_lossy().ends_with(" (deleted)"))
    };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        if !replaced() || ACTIVE.load(Ordering::SeqCst) > 0 {
            continue;
        }
        tracing::info!("a newer version was installed, handing over");
        let _ = conn.release_name(BUS_NAME);
        // A request may have arrived just before the name went; let it finish.
        std::thread::sleep(std::time::Duration::from_secs(1));
        while ACTIVE.load(Ordering::SeqCst) > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        return;
    }
}

struct FileChooser;

/// The handle object of a request in progress: `Close` ends its picker.
struct RequestHandle {
    pid: Arc<AtomicI32>,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestHandle {
    fn close(&self) {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: signalling our own child process.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    async fn open_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> (u32, Results) {
        let mut r = common(Mode::Open, title, &options);
        r.multiple = get::<bool>(&options, "multiple").unwrap_or(false);
        r.directory = get::<bool>(&options, "directory").unwrap_or(false);
        tracing::info!("{app_id} asks to open a file");
        run(server, handle, r, true).await
    }

    async fn save_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> (u32, Results) {
        let mut r = common(Mode::Save, title, &options);
        r.name = get::<String>(&options, "current_name").unwrap_or_default();
        // An existing file to save over: start in its folder, with its name.
        if let Some(file) = bytes_path(&options, "current_file") {
            if let Some(dir) = file.parent() {
                r.folder = Some(dir.to_path_buf());
            }
            if let Some(n) = file.file_name() {
                r.name = n.to_string_lossy().into_owned();
            }
        }
        tracing::info!("{app_id} asks to save a file");
        run(server, handle, r, false).await
    }

    async fn save_files(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> (u32, Results) {
        let mut r = common(Mode::SaveFiles, title, &options);
        r.directory = true;
        r.files = match options.get("files").map(|v| &**v) {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(bytes)
                .map(|b| String::from_utf8_lossy(b.strip_suffix(&[0]).unwrap_or(&b)).into_owned())
                .collect(),
            _ => Vec::new(),
        };
        tracing::info!("{app_id} asks to save {} files", r.files.len());
        run(server, handle, r, false).await
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        4
    }
}

/// What every kind of request has: title, button label, filters, choices, starting folder.
fn common(mode: Mode, title: String, options: &Options) -> Request {
    let filters: Vec<Filter> = get::<Vec<(String, Vec<(u32, String)>)>>(options, "filters")
        .unwrap_or_default()
        .into_iter()
        .map(|(name, patterns)| Filter { name, patterns })
        .collect();
    let current = get::<(String, Vec<(u32, String)>)>(options, "current_filter")
        .map(|(name, patterns)| Filter { name, patterns });
    let mut filters = filters;
    // The current filter should be one of the list; if it isn't, it becomes the only one.
    let current_filter = current.map(|c| match filters.iter().position(|f| *f == c) {
        Some(i) => i,
        None => {
            filters.push(c);
            filters.len() - 1
        }
    });
    let choices = get::<Vec<(String, String, Vec<(String, String)>, String)>>(options, "choices")
        .unwrap_or_default()
        .into_iter()
        .map(|(id, label, options, selected)| Choice {
            id,
            label,
            options,
            selected,
        })
        .collect();
    Request {
        mode,
        title,
        accept: get::<String>(options, "accept_label")
            .unwrap_or_default()
            .replace('_', ""),
        folder: bytes_path(options, "current_folder"),
        filters,
        current_filter,
        choices,
        ..Default::default()
    }
}

fn get<T>(options: &Options, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    let v = options.get(key)?.try_clone().ok()?;
    T::try_from(v).ok()
}

/// A path sent as a nul-terminated byte array.
fn bytes_path(options: &Options, key: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut b = bytes(options.get(key)?)?;
    while b.last() == Some(&0) {
        b.pop();
    }
    (!b.is_empty()).then(|| std::path::PathBuf::from(std::ffi::OsString::from_vec(b)))
}

/// A byte array value (`ay`), possibly wrapped in a variant.
fn bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Array(a) => a.iter().map(|b| u8::try_from(b).ok()).collect(),
        Value::Value(inner) => bytes(inner),
        _ => None,
    }
}

/// Show the picker for `r` and turn what was picked into the portal's results.
async fn run(server: &ObjectServer, handle: OwnedObjectPath, r: Request, open: bool) -> (u32, Results) {
    ACTIVE.fetch_add(1, Ordering::SeqCst);
    let pid = Arc::new(AtomicI32::new(0));
    let registered = server.at(&handle, RequestHandle { pid: pid.clone() }).await.is_ok();
    let (tx, rx) = async_channel::bounded(1);
    let request = r.clone();
    let pid_for_thread = pid.clone();
    std::thread::spawn(move || {
        let _ = tx.send_blocking(pick(&request, &pid_for_thread));
    });
    let outcome = rx.recv().await.unwrap_or(Err("the picker thread died".into()));
    ACTIVE.fetch_sub(1, Ordering::SeqCst);
    if registered {
        let _ = server.remove::<RequestHandle, _>(&handle).await;
    }
    match outcome {
        Ok(Some(answer)) => (OK, results(&r, &answer, open)),
        Ok(None) => (CANCELLED, Results::new()),
        Err(e) => {
            tracing::error!("file picker: {e}");
            (FAILED, Results::new())
        }
    }
}

/// Run the picker window and wait for it. None if the user cancelled.
fn pick(r: &Request, pid: &AtomicI32) -> Result<Option<Answer>, String> {
    // This very program, even if it has since been replaced by a reinstall (then the path
    // `current_exe` gives no longer exists). It's also the picker that speaks our version of
    // the request format.
    let mut child = Command::new("/proc/self/exe")
        .arg("--pick")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting the picker: {e}"))?;
    pid.store(child.id() as i32, Ordering::SeqCst);
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(r.encode().as_bytes()).map_err(|e| e.to_string())?;
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    pid.store(0, Ordering::SeqCst);
    Ok(status.success().then(|| Answer::decode(&out)))
}

fn results(r: &Request, a: &Answer, open: bool) -> Results {
    let mut out = Results::new();
    let uris: Vec<String> = a.paths.iter().map(|p| fs::path_to_uri(p)).collect();
    let mut put = |k: &str, v: Value| {
        if let Ok(v) = OwnedValue::try_from(v) {
            out.insert(k.to_string(), v);
        }
    };
    put("uris", Value::from(uris));
    if !a.choices.is_empty() {
        put("choices", Value::from(a.choices.clone()));
    }
    if let Some(f) = a.filter.and_then(|i| r.filters.get(i)) {
        put("current_filter", Value::from(filter_value(f)));
    }
    if open {
        put("writable", Value::from(true));
    }
    out
}

fn filter_value(f: &Filter) -> Structure<'static> {
    StructureBuilder::new()
        .add_field(f.name.clone())
        .add_field(f.patterns.clone())
        .build()
        .expect("two fields")
}
