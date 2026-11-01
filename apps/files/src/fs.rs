//! Everything the picker and the file manager know about files: folder listings, the places in
//! the sidebar, recently used files, file types (from the shared MIME database) and their icons,
//! filters, and how sizes and dates are written.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// One file or folder in a listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// Bytes; 0 for folders.
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hidden: bool,
}

impl Entry {
    pub fn from_path(path: PathBuf) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        // Follow links, so a link to a folder opens like one.
        let meta = std::fs::metadata(&path)
            .or_else(|_| std::fs::symlink_metadata(&path))
            .ok()?;
        Some(Self {
            hidden: name.starts_with('.') || name.ends_with('~'),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            name,
            path,
        })
    }
}

/// Everything in `dir`, unsorted.
pub fn list(dir: &Path) -> std::io::Result<Vec<Entry>> {
    Ok(std::fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| Entry::from_path(e.path()))
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Size,
    Modified,
}

/// Folders first, then by `by`; names compare like people expect ("file2" before "file10").
pub fn sort(entries: &mut [Entry], by: SortBy, descending: bool) {
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let o = match by {
                SortBy::Name => natural(&a.name, &b.name),
                SortBy::Size => a.size.cmp(&b.size).then_with(|| natural(&a.name, &b.name)),
                SortBy::Modified => a.modified.cmp(&b.modified).then_with(|| natural(&a.name, &b.name)),
            };
            if descending { o.reverse() } else { o }
        })
    });
}

/// Case-insensitive comparison with runs of digits compared as numbers.
pub fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// `~` and `~/x` in typed paths.
pub fn expand(s: &str) -> PathBuf {
    PathBuf::from(wt_config::expand_home(s))
}

/// Delete a file, or a folder and everything in it.
pub fn remove(path: &Path) -> std::io::Result<()> {
    if path.symlink_metadata()?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// `dir/name`, or `dir/name (2).ext` and so on if that's taken.
pub fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() && first.symlink_metadata().is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| p.symlink_metadata().is_err())
        .unwrap_or(first)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// The Downloads folder (from `user-dirs.dirs`), or home if there isn't one.
pub fn downloads() -> PathBuf {
    let home = home();
    let dir = user_dirs(&home)
        .remove("DOWNLOAD")
        .unwrap_or_else(|| home.join("Downloads"));
    if dir.is_dir() { dir } else { home }
}

/// A place in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub label: String,
    /// None for Recent, which isn't a folder.
    pub path: Option<PathBuf>,
    /// Symbolic icon name.
    pub icon: &'static str,
    /// One of the GTK bookmarks (which can be removed from the sidebar).
    pub bookmark: bool,
}

/// The sidebar: Recent, Home, the XDG user folders that exist, GTK bookmarks, mounted drives,
/// and the whole computer.
pub fn places() -> Vec<Place> {
    let home = home();
    let mut out = vec![
        Place {
            label: "Recent".into(),
            path: None,
            icon: "document-open-recent-symbolic",
            bookmark: false,
        },
        Place {
            label: "Home".into(),
            path: Some(home.clone()),
            icon: "user-home-symbolic",
            bookmark: false,
        },
    ];
    let dirs = user_dirs(&home);
    for (key, label, icon) in [
        ("DESKTOP", "Desktop", "user-desktop-symbolic"),
        ("DOCUMENTS", "Documents", "folder-documents-symbolic"),
        ("DOWNLOAD", "Downloads", "folder-download-symbolic"),
        ("MUSIC", "Music", "folder-music-symbolic"),
        ("PICTURES", "Pictures", "folder-pictures-symbolic"),
        ("VIDEOS", "Videos", "folder-videos-symbolic"),
    ] {
        let path = dirs.get(key).cloned().unwrap_or_else(|| home.join(label));
        if path.is_dir() && path != home {
            out.push(Place {
                label: label.into(),
                path: Some(path),
                icon,
                bookmark: false,
            });
        }
    }
    // Bookmarks, skipping ones that repeat a folder above.
    let bookmarks = std::fs::read_to_string(bookmarks_file()).unwrap_or_default();
    for line in bookmarks.lines() {
        let (uri, label) = line.split_once(' ').unwrap_or((line, ""));
        let Some(path) = uri_to_path(uri) else { continue };
        if !path.is_dir() || out.iter().any(|p| p.path.as_ref() == Some(&path)) {
            continue;
        }
        let label = if label.is_empty() {
            path.file_name()
                .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
        } else {
            label.to_string()
        };
        out.push(Place {
            label,
            path: Some(path),
            icon: "folder-symbolic",
            bookmark: true,
        });
    }
    // Drives mounted for this user.
    let user = std::env::var("USER").unwrap_or_default();
    for root in [format!("/media/{user}"), format!("/run/media/{user}")] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut drives: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        drives.sort();
        for path in drives {
            out.push(Place {
                label: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                path: Some(path),
                icon: "drive-removable-media-symbolic",
                bookmark: false,
            });
        }
    }
    out.push(Place {
        label: "Computer".into(),
        path: Some(PathBuf::from("/")),
        icon: "drive-harddisk-symbolic",
        bookmark: false,
    });
    out
}

/// `~/.config/user-dirs.dirs`: "DOWNLOAD" → /home/me/Downloads, ...
fn user_dirs(home: &Path) -> HashMap<String, PathBuf> {
    let text = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (key, value) = l.trim().split_once('=')?;
            let key = key.strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            let value = value.trim_matches('"').replace("$HOME", &home.to_string_lossy());
            Some((key.to_string(), PathBuf::from(value)))
        })
        .collect()
}

/// GTK's bookmarks: one `file://` URI per line, optionally followed by a label. Shared with
/// other file managers and file dialogs.
fn bookmarks_file() -> PathBuf {
    home().join(".config/gtk-3.0/bookmarks")
}

/// Change the bookmark lines, written back whole through a temporary file.
fn edit_bookmarks(change: impl FnOnce(&mut Vec<String>)) -> std::io::Result<()> {
    // Through a symlink (dotfiles kept elsewhere) to the real file, so the link stays.
    let file = std::fs::canonicalize(bookmarks_file()).unwrap_or_else(|_| bookmarks_file());
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    change(&mut lines);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("wt-tmp");
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &file)
}

fn bookmark_path(line: &str) -> Option<PathBuf> {
    uri_to_path(line.split_once(' ').map_or(line, |(uri, _)| uri))
}

/// Add `dir` to the bookmarks, at the end (nothing if it's there already).
pub fn add_bookmark(dir: &Path) -> std::io::Result<()> {
    edit_bookmarks(|lines| {
        if !lines.iter().any(|l| bookmark_path(l).as_deref() == Some(dir)) {
            lines.push(path_to_uri(dir));
        }
    })
}

/// Take `dir` out of the bookmarks.
pub fn remove_bookmark(dir: &Path) -> std::io::Result<()> {
    edit_bookmarks(|lines| lines.retain(|l| bookmark_path(l).as_deref() != Some(dir)))
}

/// A `file://` URI as a path, with %-escapes decoded.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// A path as a `file://` URI, escaping what URIs can't hold as is.
pub fn path_to_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Recently used files that still exist, newest first, from `recently-used.xbel`.
pub fn recent(limit: usize) -> Vec<Entry> {
    let path = home().join(".local/share/recently-used.xbel");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for tag in text.split("<bookmark ").skip(1) {
        let attr = |name: &str| {
            let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
            let end = tag[start..].find('"')?;
            Some(xml_unescape(&tag[start..start + end]))
        };
        let (Some(href), Some(modified)) = (attr("href"), attr("modified").or_else(|| attr("visited"))) else {
            continue;
        };
        if let Some(p) = uri_to_path(&href) {
            found.push((modified, p));
        }
    }
    // ISO 8601 times sort as text.
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = Vec::new();
    for (_, p) in found {
        if out.iter().any(|e: &Entry| e.path == p) {
            continue;
        }
        if let Some(e) = Entry::from_path(p) {
            out.push(e);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// File types from the shared MIME database: which type a name has, and which icon shows it.
#[derive(Default)]
pub struct Mime {
    /// Lowercased extension ("tar.gz", "png") → MIME type, highest weight first.
    by_ext: HashMap<String, String>,
    /// MIME type → its globs ("*.png"), for filters given as MIME types.
    globs: HashMap<String, Vec<String>>,
    /// MIME type → generic icon ("image-x-generic").
    generic: HashMap<String, String>,
}

impl Mime {
    pub fn load() -> Self {
        let mut me = Self::default();
        let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        let mut dirs: Vec<PathBuf> = dirs.split(':').map(|d| Path::new(d).join("mime")).collect();
        dirs.insert(0, home().join(".local/share/mime"));
        // Earlier directories win, so read them last.
        for dir in dirs.iter().rev() {
            let mut weighted: Vec<(u32, String, String)> = Vec::new();
            for line in std::fs::read_to_string(dir.join("globs2")).unwrap_or_default().lines() {
                let mut f = line.splitn(4, ':');
                let (Some(w), Some(mime), Some(glob)) = (f.next(), f.next(), f.next()) else {
                    continue;
                };
                let Ok(w) = w.parse::<u32>() else { continue };
                weighted.push((w, mime.to_string(), glob.to_string()));
            }
            weighted.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, mime, glob) in weighted {
                me.globs.entry(mime.clone()).or_default().push(glob.clone());
                if let Some(ext) = glob.strip_prefix("*.")
                    && !ext.contains(['*', '?', '['])
                {
                    // Higher weights come later and replace lower ones.
                    me.by_ext.insert(ext.to_lowercase(), mime);
                }
            }
            for line in std::fs::read_to_string(dir.join("generic-icons"))
                .unwrap_or_default()
                .lines()
            {
                if let Some((mime, icon)) = line.split_once(':') {
                    me.generic.insert(mime.to_string(), icon.to_string());
                }
            }
        }
        me
    }

    /// The MIME type of a file name, by its extension ("a.tar.gz" tries "tar.gz" first).
    pub fn type_of(&self, name: &str) -> Option<&str> {
        let lower = name.to_lowercase();
        let mut rest = lower.as_str();
        while let Some((_, ext)) = rest.split_once('.') {
            if let Some(m) = self.by_ext.get(ext) {
                return Some(m);
            }
            rest = ext;
        }
        None
    }

    /// Icon names to try for a file, best first.
    pub fn icons_for(&self, entry: &Entry) -> Vec<String> {
        if entry.is_dir {
            return vec!["folder".into()];
        }
        let mut names = Vec::new();
        if let Some(mime) = self.type_of(&entry.name) {
            names.push(mime.replace('/', "-"));
            if let Some(g) = self.generic.get(mime) {
                names.push(g.clone());
            }
            if let Some((media, _)) = mime.split_once('/') {
                names.push(format!("{media}-x-generic"));
            }
        }
        names.push("text-x-generic".into());
        names
    }

    /// The globs of a MIME type, or of every type matching "image/*".
    pub fn globs_of(&self, mime: &str) -> Vec<String> {
        match mime.strip_suffix("/*") {
            Some(media) => self
                .globs
                .iter()
                .filter(|(m, _)| m.split('/').next() == Some(media))
                .flat_map(|(_, g)| g.iter().cloned())
                .collect(),
            None => self.globs.get(mime).cloned().unwrap_or_default(),
        }
    }
}

/// A file type filter, from the app: a name and its patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub name: String,
    /// (0 = glob, 1 = MIME type, pattern)
    pub patterns: Vec<(u32, String)>,
}

impl Filter {
    /// Its patterns as globs, MIME types looked up.
    pub fn globs(&self, mime: &Mime) -> Vec<String> {
        self.patterns
            .iter()
            .flat_map(|(kind, p)| if *kind == 1 { mime.globs_of(p) } else { vec![p.clone()] })
            .collect()
    }
}

/// Whether `name` matches a glob with `*`, `?` and `[...]`, ignoring case.
pub fn glob_match(glob: &str, name: &str) -> bool {
    fn go(g: &[char], n: &[char]) -> bool {
        match g.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&g[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&g[1..], &n[1..]),
            Some('[') => {
                let Some(end) = g.iter().skip(1).position(|&c| c == ']').map(|p| p + 1) else {
                    return !n.is_empty() && n[0] == '[' && go(&g[1..], &n[1..]);
                };
                let Some(&c) = n.first() else { return false };
                let class = &g[1..end];
                let (negate, class) = match class.first() {
                    Some('!') | Some('^') => (true, &class[1..]),
                    _ => (false, class),
                };
                let mut hit = false;
                let mut i = 0;
                while i < class.len() {
                    if i + 2 < class.len() && class[i + 1] == '-' {
                        hit |= (class[i]..=class[i + 2]).contains(&c);
                        i += 3;
                    } else {
                        hit |= class[i] == c;
                        i += 1;
                    }
                }
                hit != negate && go(&g[end + 1..], &n[1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&g[1..], &n[1..]),
        }
    }
    let g: Vec<char> = glob.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    go(&g, &n)
}

/// "512 bytes", "1.4 kB", "23 MB".
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// "3:07 PM" today, "Oct 21" this year, "Oct 21, 2024" before.
pub fn date_text(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as libc::time_t);
    let (then, now) = (local(secs), local(unsafe { libc::time(std::ptr::null_mut()) }));
    let fmt = if then.tm_year == now.tm_year && then.tm_yday == now.tm_yday {
        c"%-I:%M %p"
    } else if then.tm_year == now.tm_year {
        c"%b %-d"
    } else {
        c"%b %-d, %Y"
    };
    let mut buf = [0u8; 64];
    // SAFETY: strftime writes at most `buf.len()` bytes and returns how many.
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &then) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn local(secs: libc::time_t) -> libc::tm {
    // SAFETY: localtime_r writes into our own `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "a"];
        v.sort_by(|a, b| natural(a, b));
        assert_eq!(v, ["a", "file1", "File2", "file10"]);
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.png", "Shot.PNG"));
        assert!(glob_match("*.[jJ][pP]g", "a.jpg"));
        assert!(!glob_match("*.png", "a.png.txt"));
        assert!(glob_match("photo-??.*", "photo-01.raw"));
        assert!(glob_match("[!a]*", "b"));
    }

    #[test]
    fn uris_round_trip() {
        let p = Path::new("/tmp/a b/ü#1.txt");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%BC%231.txt");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn sizes() {
        assert_eq!(size_text(999), "999 bytes");
        assert_eq!(size_text(1400), "1.4 kB");
        assert_eq!(size_text(23_000_000), "23 MB");
    }

    #[test]
    fn mime_lookup() {
        let m = Mime::load();
        assert_eq!(m.type_of("x.PNG"), Some("image/png"));
        assert_eq!(m.type_of("a.tar.gz"), Some("application/x-compressed-tar"));
        assert!(m.globs_of("image/*").iter().any(|g| g == "*.png"));
    }
}
