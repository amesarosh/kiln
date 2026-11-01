//! The trash, as the freedesktop spec lays it out, so files trashed here show up in other
//! file managers' trash and the other way round.
//!
//! Files on the home drive go to `~/.local/share/Trash`; files on other drives go to
//! `.Trash-<uid>` at the top of that drive (moving them home would mean copying). Each trashed
//! file sits in `files/` with a `info/<name>.trashinfo` saying where it came from and when.

use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::fs::{self, Entry};

/// The home trash folder.
pub fn home_trash() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| fs::home().join(".local/share"), PathBuf::from)
        .join("Trash")
}

/// Move `path` to the trash. Returns where it went (for [`restore_from`]).
pub fn trash(path: &Path) -> Result<PathBuf, String> {
    let meta = path.symlink_metadata().map_err(|e| e.to_string())?;
    let home = home_trash();
    // The drive the home trash is on (or would be, before it's made).
    let home_dev = home
        .ancestors()
        .find_map(|d| std::fs::metadata(d).ok())
        .map(|m| m.dev());
    let root = if home_dev == Some(meta.dev()) {
        home
    } else {
        top_dir(path, meta.dev()).join(format!(".Trash-{}", unsafe { libc::getuid() }))
    };
    for sub in ["files", "info"] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join(sub))
            .map_err(|e| format!("can't make the trash folder: {e}"))?;
    }
    let name = path
        .file_name()
        .ok_or("can't trash this")?
        .to_string_lossy()
        .into_owned();
    // Claim a name by creating its info file; the file moves in under the same name.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    let info_text = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        escape(&absolute),
        now_text()
    );
    for n in 1.. {
        let candidate = if n == 1 {
            name.clone()
        } else {
            format!("{stem}.{n}{ext}")
        };
        let info = root.join("info").join(format!("{candidate}.trashinfo"));
        let target = root.join("files").join(&candidate);
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("can't write to the trash: {e}")),
        };
        if target.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&info);
            continue;
        }
        if let Err(e) = file.write_all(info_text.as_bytes()) {
            let _ = std::fs::remove_file(&info);
            return Err(format!("can't write to the trash: {e}"));
        }
        if let Err(e) = std::fs::rename(path, &target) {
            let _ = std::fs::remove_file(&info);
            return Err(e.to_string());
        }
        return Ok(target);
    }
    unreachable!()
}

/// The folder at the top of the drive `path` is on: the last parent still on device `dev`.
fn top_dir(path: &Path, dev: u64) -> PathBuf {
    let mut top = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    while let Some(parent) = top.parent() {
        if std::fs::metadata(parent).map(|m| m.dev()).ok() != Some(dev) {
            break;
        }
        top = parent.to_path_buf();
    }
    top
}

/// A file in the trash.
#[derive(Debug, Clone)]
pub struct Item {
    /// The file inside `Trash/files`, with its metadata.
    pub entry: Entry,
    /// Where it was before.
    pub original: PathBuf,
    pub deleted: Option<SystemTime>,
}

impl Item {
    fn info_file(&self) -> PathBuf {
        let root = self
            .entry
            .path
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("/"));
        // By its name inside the trash, which may differ from the name shown.
        let name = self.entry.path.file_name().unwrap_or_default().to_string_lossy();
        root.join("info").join(format!("{name}.trashinfo"))
    }
}

/// Everything in the home trash.
pub fn list() -> Vec<Item> {
    let root = home_trash();
    let Ok(entries) = std::fs::read_dir(root.join("info")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().strip_suffix(".trashinfo")?.to_string();
            let text = std::fs::read_to_string(e.path()).ok()?;
            let field = |k: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                    .map(str::trim)
            };
            let original = fs::uri_to_path(&format!("file://{}", field("Path")?))?;
            let original = if original.is_relative() {
                root.join(original)
            } else {
                original
            };
            let mut entry = Entry::from_path(root.join("files").join(&file))?;
            // Show the name it had, not the numbered one it got in the trash.
            if let Some(n) = original.file_name() {
                entry.name = n.to_string_lossy().into_owned();
                entry.hidden = false;
            }
            Some(Item {
                deleted: field("DeletionDate").and_then(parse_date),
                entry,
                original,
            })
        })
        .collect()
}

/// Put an item back where it was (beside it, if something has taken its name).
pub fn restore(item: &Item) -> Result<PathBuf, String> {
    let dir = item.original.parent().ok_or("no folder to restore to")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    let name = item
        .original
        .file_name()
        .ok_or("no name")?
        .to_string_lossy()
        .into_owned();
    let target = fs::free_name(dir, &name);
    std::fs::rename(&item.entry.path, &target).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(item.info_file());
    Ok(target)
}

/// Put back a file just trashed, from where [`trash`] put it (in any drive's trash).
pub fn restore_from(trashed: &Path, original: &Path) -> Result<PathBuf, String> {
    let entry = Entry::from_path(trashed.to_path_buf()).ok_or("it's no longer in the trash")?;
    restore(&Item {
        entry,
        original: original.to_path_buf(),
        deleted: None,
    })
}

/// Delete an item for good.
pub fn delete(item: &Item) -> Result<(), String> {
    fs::remove(&item.entry.path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(item.info_file());
    Ok(())
}

/// The way the spec writes paths: like a file URI without the `file://`.
fn escape(path: &Path) -> String {
    fs::path_to_uri(path).trim_start_matches("file://").to_string()
}

/// Local time as "2026-10-31T09:41:07".
fn now_text() -> String {
    // SAFETY: localtime_r and strftime only write into our buffers.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let mut buf = [0u8; 32];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%Y-%m-%dT%H:%M:%S".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// "2026-10-31T09:41:07" (local time) as a time.
fn parse_date(s: &str) -> Option<SystemTime> {
    let (date, time) = s.split_once('T')?;
    let d: Vec<i32> = date.split('-').map(|v| v.parse().ok()).collect::<Option<_>>()?;
    let t: Vec<i32> = time
        .split(':')
        .map(|v| v.split('.').next()?.parse().ok())
        .collect::<Option<_>>()?;
    let (&[y, mo, da], Some(&[h, mi, se])) = (d.as_slice(), t.get(..3)) else {
        return None;
    };
    // SAFETY: mktime reads and normalizes our own `tm`.
    let secs = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = y - 1900;
        tm.tm_mon = mo - 1;
        tm.tm_mday = da;
        tm.tm_hour = h;
        tm.tm_min = mi;
        tm.tm_sec = se;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        let t = parse_date(&now_text()).unwrap();
        let diff = SystemTime::now().duration_since(t).unwrap_or_default();
        assert!(diff.as_secs() < 5);
        assert!(parse_date("2026-10-31").is_none());
    }

    #[test]
    fn trash_and_restore() {
        let dir = std::env::temp_dir().join(format!("wt-trash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A private trash, so the test doesn't touch the real one.
        unsafe { std::env::set_var("XDG_DATA_HOME", dir.join("data")) };
        let file = dir.join("a b.txt");
        std::fs::write(&file, "x").unwrap();
        trash(&file).unwrap();
        assert!(!file.exists());
        std::fs::write(&file, "y").unwrap();
        trash(&file).unwrap();
        let items = list();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.original == file && i.entry.name == "a b.txt"));
        restore(&items[0]).unwrap();
        assert!(file.exists());
        // The second one finds the name taken and goes beside it.
        let back = restore(&list()[0]).unwrap();
        assert_eq!(back, dir.join("a b (2).txt"));
        assert!(list().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
