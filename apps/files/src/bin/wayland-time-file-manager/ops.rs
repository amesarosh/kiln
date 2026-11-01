//! Changing files: copying and moving (in the background, with progress), trashing,
//! deleting, renaming and making folders. Nothing is ever overwritten: a name that's taken
//! gets " (2)" added.

use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
};

use wayland_time_files::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Copy,
    Move,
    Trash,
    Delete,
}

impl Kind {
    pub fn verb(self) -> &'static str {
        match self {
            Kind::Copy => "Copying",
            Kind::Move => "Moving",
            Kind::Trash => "Moving to the trash",
            Kind::Delete => "Deleting",
        }
    }
}

/// What a finished job did.
pub struct Outcome {
    /// Where the files ended up (copies and moves), to select them.
    pub made: Vec<PathBuf>,
    /// One line per file that failed.
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// A copy, move, trash or delete running on its own thread.
pub struct Job {
    pub kind: Kind,
    pub count: usize,
    /// Bytes done and in all (copies and cross-drive moves; 0 total when not counted).
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<Outcome>>,
}

impl Job {
    /// Start `kind` on `paths`; copies and moves go into `dest`.
    pub fn start(kind: Kind, paths: Vec<PathBuf>, dest: Option<PathBuf>, wake: impl Fn() + Send + 'static) -> Self {
        let done = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let count = paths.len();
        let ctx = Ctx {
            done: done.clone(),
            total: total.clone(),
            cancel: cancel.clone(),
            wake: Box::new(wake),
        };
        let thread = std::thread::spawn(move || {
            let out = run(kind, &paths, dest.as_deref(), &ctx);
            (ctx.wake)();
            out
        });
        Self {
            kind,
            count,
            done,
            total,
            cancel,
            thread: Some(thread),
        }
    }

    /// From 0 to 1, if it's known.
    pub fn progress(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        (total > 0).then(|| self.done.load(Ordering::Relaxed) as f32 / total as f32)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The outcome, once the job has finished.
    pub fn finished(&mut self) -> Option<Outcome> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        let outcome = self.thread.take()?.join().unwrap_or_else(|_| Outcome {
            made: Vec::new(),
            errors: vec!["the job crashed".into()],
            cancelled: false,
        });
        Some(outcome)
    }
}

struct Ctx {
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    /// Ask the window to redraw, so progress shows.
    wake: Box<dyn Fn() + Send>,
}

impl Ctx {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

fn run(kind: Kind, paths: &[PathBuf], dest: Option<&Path>, ctx: &Ctx) -> Outcome {
    let mut out = Outcome {
        made: Vec::new(),
        errors: Vec::new(),
        cancelled: false,
    };
    if kind == Kind::Copy {
        ctx.total
            .store(paths.iter().map(|p| size_of(p)).sum::<u64>().max(1), Ordering::Relaxed);
    }
    for path in paths {
        if ctx.cancelled() {
            out.cancelled = true;
            break;
        }
        let name = path
            .file_name()
            .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let result = match (kind, dest) {
            (Kind::Copy, Some(dest)) => copy_into(path, dest, ctx).map(|p| out.made.push(p)),
            (Kind::Move, Some(dest)) => move_into(path, dest, ctx).map(|p| out.made.extend(p)),
            (Kind::Trash, _) => wayland_time_files::trash::trash(path).map(|_| ()),
            (Kind::Delete, _) => remove(path).map_err(|e| e.to_string()),
            _ => Ok(()),
        };
        match result {
            Err(e) if e == CANCELLED => {
                out.cancelled = true;
                break;
            }
            Err(e) => out.errors.push(format!("{name}: {e}")),
            Ok(()) => {}
        }
        (ctx.wake)();
    }
    out
}

const CANCELLED: &str = "cancelled";

/// Bytes in a file, or in everything below a folder.
fn size_of(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| size_of(&e.path())).sum())
        .unwrap_or(0)
}

/// Copy `src` into the folder `dest`. Returns the copy's path.
fn copy_into(src: &Path, dest: &Path, ctx: &Ctx) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or("no name")?.to_string_lossy().into_owned();
    if dest.starts_with(src) {
        return Err("can't copy a folder into itself".into());
    }
    let target = fs::free_name(dest, &name);
    copy_tree(src, &target, ctx).inspect_err(|_| {
        // Don't leave half a copy behind.
        let _ = remove(&target);
    })?;
    Ok(target)
}

fn copy_tree(src: &Path, target: &Path, ctx: &Ctx) -> Result<(), String> {
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    let meta = src.symlink_metadata().map_err(|e| e.to_string())?;
    if meta.is_symlink() {
        let link = std::fs::read_link(src).map_err(|e| e.to_string())?;
        return std::os::unix::fs::symlink(link, target).map_err(|e| e.to_string());
    }
    if meta.is_dir() {
        std::fs::create_dir(target).map_err(|e| e.to_string())?;
        for e in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
            copy_tree(&e.path(), &target.join(e.file_name()), ctx)?;
        }
        let _ = std::fs::set_permissions(target, meta.permissions());
        return Ok(());
    }
    copy_file(src, target, ctx)?;
    let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(meta.permissions().mode()));
    Ok(())
}

/// Copy one file in chunks, counting progress and stopping when cancelled.
fn copy_file(src: &Path, target: &Path, ctx: &Ctx) -> Result<(), String> {
    let mut from = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if ctx.cancelled() {
            return Err(CANCELLED.into());
        }
        let n = from.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        ctx.done.fetch_add(n as u64, Ordering::Relaxed);
        (ctx.wake)();
    }
    if let Ok(t) = from.metadata().and_then(|m| m.modified()) {
        let _ = to.set_modified(t);
    }
    Ok(())
}

/// Move `src` into the folder `dest`: a rename on the same drive, a copy and delete across
/// drives. Returns the new path (None if it was already there).
fn move_into(src: &Path, dest: &Path, ctx: &Ctx) -> Result<Option<PathBuf>, String> {
    if src.parent() == Some(dest) {
        return Ok(None);
    }
    if dest.starts_with(src) {
        return Err("can't move a folder into itself".into());
    }
    let name = src.file_name().ok_or("no name")?.to_string_lossy().into_owned();
    let target = fs::free_name(dest, &name);
    match std::fs::rename(src, &target) {
        Ok(()) => Ok(Some(target)),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            ctx.total.fetch_add(size_of(src), Ordering::Relaxed);
            copy_tree(src, &target, ctx).inspect_err(|_| {
                let _ = remove(&target);
            })?;
            remove(src).map_err(|e| format!("copied, but couldn't remove the original: {e}"))?;
            Ok(Some(target))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Delete a file or a whole folder, for good.
pub fn remove(path: &Path) -> std::io::Result<()> {
    fs::remove(path)
}

/// Make a new empty folder in `dir`, named "New Folder" (or "New Folder (2)"...).
pub fn new_folder(dir: &Path) -> Result<PathBuf, String> {
    let path = fs::free_name(dir, "New Folder");
    std::fs::create_dir(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Rename `path` to `name`, in the same folder. Returns the new path.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the name can't be empty".into());
    }
    if name.contains('/') {
        return Err("names can't have a slash in them".into());
    }
    if name == "." || name == ".." {
        return Err("that name is taken by the system".into());
    }
    let target = path.with_file_name(name);
    if target == path {
        return Ok(target);
    }
    // A different case of the same name is fine; anything else that exists isn't.
    let same_file = target
        .symlink_metadata()
        .ok()
        .zip(path.symlink_metadata().ok())
        .is_some_and(|(a, b)| {
            use std::os::unix::fs::MetadataExt;
            a.ino() == b.ino() && a.dev() == b.dev()
        });
    if target.symlink_metadata().is_ok() && !same_file {
        return Err(format!("there's already something called \"{name}\" here"));
    }
    std::fs::rename(path, &target).map_err(|e| e.to_string())?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(mut job: Job) -> Outcome {
        loop {
            if let Some(o) = job.finished() {
                return o;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn copy_move_rename() {
        let dir = std::env::temp_dir().join(format!("wt-ops-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("sub/f.txt"), "hello").unwrap();

        // Copying a folder next to itself gets a new name.
        let out = wait(Job::start(Kind::Copy, vec![a.clone()], Some(dir.clone()), || {}));
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.made, vec![dir.join("a (2)")]);
        assert_eq!(std::fs::read_to_string(dir.join("a (2)/sub/f.txt")).unwrap(), "hello");

        // Into itself: refused.
        let out = wait(Job::start(Kind::Copy, vec![a.clone()], Some(a.join("sub")), || {}));
        assert_eq!(out.errors.len(), 1);

        let out = wait(Job::start(Kind::Move, vec![dir.join("a (2)")], Some(b.clone()), || {}));
        assert_eq!(out.made, vec![b.join("a (2)")]);
        assert!(!dir.join("a (2)").exists());

        let renamed = rename(&b.join("a (2)"), "c").unwrap();
        assert_eq!(renamed, b.join("c"));
        assert!(rename(&renamed, "x/y").is_err());
        std::fs::create_dir(b.join("taken")).unwrap();
        assert!(rename(&renamed, "taken").is_err());

        assert_eq!(new_folder(&b).unwrap(), b.join("New Folder"));
        assert_eq!(new_folder(&b).unwrap(), b.join("New Folder (2)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
