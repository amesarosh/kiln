//! Icons from the freedesktop icon themes, shared by the compositor and its apps.
//!
//! App icons: app_id → .desktop file → icon name → file in the icon theme → pixels.
//! Symbolic icons (battery, network, ...) are found in any context and tinted one color.
//!
//! Follows the freedesktop icon theme layout loosely: the chosen theme and the themes it
//! inherits from, then `hicolor`, then `/usr/share/pixmaps`. PNG and SVG icons are supported.
//! Everything is cached, so each icon is found and decoded once per size.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
};

/// Square icon pixels, premultiplied RGBA.
#[derive(Debug, PartialEq)]
pub struct Icon {
    pub size: u32,
    pub rgba: Vec<u8>,
}

pub struct Icons {
    /// The theme name asked for ("auto" picks the desktop's).
    wanted: String,
    /// Theme directories to search, in order. Built on first use.
    themes: Option<Vec<ThemeDir>>,
    /// Lowercased app id, desktop file name or WM class → icon name. Built on first use.
    desktop: Option<HashMap<String, String>>,
    cache: HashMap<(String, u32), Option<Rc<Icon>>>,
    /// Every icon directory of the theme chain and Adwaita, any context. Built on first use.
    symbolic_dirs: Option<Vec<PathBuf>>,
    /// (name, size, color) → tinted icon.
    symbolic_cache: HashMap<(String, u32, [u8; 4]), Option<Rc<Icon>>>,
}

/// One theme root (e.g. /usr/share/icons/hicolor) and its subdirectories that hold icons.
struct ThemeDir {
    /// (directory, nominal size or None for scalable)
    dirs: Vec<(PathBuf, Option<u32>)>,
}

impl Icons {
    pub fn new(theme: &str) -> Self {
        Self {
            wanted: theme.to_string(),
            themes: None,
            desktop: None,
            cache: HashMap::new(),
            symbolic_dirs: None,
            symbolic_cache: HashMap::new(),
        }
    }

    /// Any icon by name, in any context (status, devices, actions...), in its own colors.
    /// Falls back to the app icon lookup. Names ending in "-symbolic" are tinted `tint`.
    pub fn any(&mut self, name: &str, px: u32, tint: [u8; 4]) -> Option<Rc<Icon>> {
        if name.ends_with("-symbolic") {
            return self.symbolic(name, px, tint);
        }
        if name.starts_with('/') || name.starts_with("file://") {
            return self.get(name.trim_start_matches("file://"), px);
        }
        let key = (format!("any:{name}"), px);
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let wanted = self.wanted.clone();
        let dirs = self.symbolic_dirs.get_or_insert_with(|| symbolic_dirs(&wanted));
        // Full-color versions before symbolic ones.
        let found = ["png", "svg"]
            .iter()
            .find_map(|ext| {
                dirs.iter()
                    .filter(|d| !d.to_string_lossy().contains("symbolic"))
                    .map(|d| d.join(format!("{name}.{ext}")))
                    .find(|p| p.is_file())
            })
            .and_then(|p| load(&p, px))
            .map(Rc::new);
        let found = found.or_else(|| self.get(name, px));
        self.cache.insert(key, found.clone());
        found
    }

    /// A symbolic icon (like "battery-level-80-symbolic"), `px` square, in one color
    /// (`[r, g, b, a]`, 0-255). Looked up in the icon theme, then in Adwaita.
    pub fn symbolic(&mut self, name: &str, px: u32, color: [u8; 4]) -> Option<Rc<Icon>> {
        if name.is_empty() || px == 0 {
            return None;
        }
        let key = (name.to_string(), px, color);
        if let Some(hit) = self.symbolic_cache.get(&key) {
            return hit.clone();
        }
        let wanted = self.wanted.clone();
        let dirs = self.symbolic_dirs.get_or_insert_with(|| symbolic_dirs(&wanted));
        let found = ["svg", "png"]
            .iter()
            .find_map(|ext| dirs.iter().map(|d| d.join(format!("{name}.{ext}"))).find(|p| p.is_file()))
            .and_then(|path| load(&path, px))
            .map(|icon| Rc::new(tint(icon, color)));
        if found.is_none() {
            tracing::debug!("no symbolic icon \"{name}\"");
        }
        self.symbolic_cache.insert(key, found.clone());
        found
    }

    /// The icon for an app, `px` pixels square.
    pub fn get(&mut self, app_id: &str, px: u32) -> Option<Rc<Icon>> {
        if app_id.is_empty() || px == 0 {
            return None;
        }
        let key = (app_id.to_string(), px);
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let found = self.find(app_id, px).map(Rc::new);
        if found.is_none() {
            tracing::debug!("no icon for \"{app_id}\"");
        }
        self.cache.insert(key, found.clone());
        found
    }

    fn find(&mut self, app_id: &str, px: u32) -> Option<Icon> {
        let lower = app_id.to_lowercase();
        // "org.gnome.Nautilus" is often known as just "nautilus".
        let short = lower.rsplit('.').next().unwrap_or(&lower).to_string();
        let desktop = self.desktop.get_or_insert_with(desktop_index);
        let mut names: Vec<String> = [&lower, &short]
            .iter()
            .filter_map(|k| desktop.get(k.as_str()).cloned())
            .collect();
        names.extend([app_id.to_string(), lower.clone(), short]);
        let mut seen = HashSet::new();
        names.retain(|n| seen.insert(n.clone()));

        for name in names {
            if let Some(path) = self.icon_file(&name, px)
                && let Some(icon) = load(&path, px)
            {
                return Some(icon);
            }
        }
        None
    }

    /// The best file for an icon name: an exact-size PNG, else an SVG, else the closest PNG.
    fn icon_file(&mut self, name: &str, px: u32) -> Option<PathBuf> {
        if name.starts_with('/') {
            return Path::new(name).is_file().then(|| PathBuf::from(name));
        }
        let wanted = self.wanted.clone();
        let themes = self.themes.get_or_insert_with(|| theme_chain(&wanted));
        for theme in themes.iter() {
            let mut best: Option<(PathBuf, i64)> = None;
            for (dir, size) in &theme.dirs {
                for ext in ["png", "svg"] {
                    let path = dir.join(format!("{name}.{ext}"));
                    if !path.is_file() {
                        continue;
                    }
                    // Lower scores win: exact PNGs, then SVGs, then bigger PNGs, then smaller.
                    let score = match (ext, size) {
                        ("svg", _) | (_, None) => 1,
                        (_, Some(s)) if *s == px => 0,
                        (_, Some(s)) if *s > px => 2 + (*s - px) as i64,
                        (_, Some(s)) => 10_000 + (px - *s) as i64,
                    };
                    if best.as_ref().is_none_or(|(_, b)| score < *b) {
                        best = Some((path, score));
                    }
                }
            }
            if let Some((path, _)) = best {
                return Some(path);
            }
        }
        ["png", "svg"].iter().find_map(|ext| {
            let path = PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"));
            path.is_file().then_some(path)
        })
    }
}

/// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, plus Flatpak and Snap exports.
pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => dirs.push(PathBuf::from(d)),
        None => dirs.extend(home.as_ref().map(|h| h.join(".local/share"))),
    }
    dirs.extend(xdg_data_dirs().split(':').map(PathBuf::from));
    let mut seen = HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// Map app ids, desktop file names and WM classes to icon names, from every .desktop file.
fn desktop_index() -> HashMap<String, String> {
    let mut index = HashMap::new();
    let mut weak = HashMap::new();
    for dir in data_dirs() {
        let mut files = Vec::new();
        collect_desktop_files(&dir.join("applications"), 2, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let (mut icon, mut wm_class, mut exec, mut in_entry) = (None, None, None, false);
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_entry = line == "[Desktop Entry]";
                } else if in_entry {
                    if let Some(v) = line.strip_prefix("Icon=") {
                        icon = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("StartupWMClass=") {
                        wm_class = Some(v.trim().to_lowercase());
                    } else if let Some(v) = line.strip_prefix("Exec=") {
                        // The program's name, e.g. "/usr/share/code/code --new-window" → "code".
                        let program = v.split_whitespace().find(|w| *w != "env" && !w.contains('='));
                        exec = program.and_then(|p| p.rsplit('/').next()).map(|p| p.to_lowercase());
                    }
                }
            }
            let Some(icon) = icon.filter(|i| !i.is_empty()) else {
                continue;
            };
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            // Snap names files "app_app.desktop".
            let snap = stem.split('_').next().unwrap_or(&stem).to_string();
            let short = stem.rsplit('.').next().unwrap_or(&stem).to_string();
            // Earlier directories (the user's own) win.
            for key in [Some(stem.clone()), wm_class, Some(snap), Some(short)]
                .into_iter()
                .flatten()
            {
                index.entry(key).or_insert_with(|| icon.clone());
            }
            // The program name is the weakest match: several entries may run the same one.
            if let Some(exec) = exec {
                weak.entry(exec).or_insert_with(|| icon.clone());
            }
        }
    }
    for (k, v) in weak {
        index.entry(k).or_insert(v);
    }
    index
}

pub fn collect_desktop_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "desktop") {
            out.push(path);
        } else if depth > 0 && path.is_dir() {
            collect_desktop_files(&path, depth - 1, out);
        }
    }
}

/// The icon theme the desktop uses, when the config says "auto". Only installed themes count.
fn auto_theme(roots: &[PathBuf]) -> String {
    let installed = |name: &str| roots.iter().any(|r| r.join(name).join("index.theme").is_file());
    let from_gtk = || {
        let home = std::env::var_os("HOME")?;
        let ini = std::fs::read_to_string(PathBuf::from(home).join(".config/gtk-3.0/settings.ini")).ok()?;
        ini.lines()
            .find_map(|l| l.trim().strip_prefix("gtk-icon-theme-name="))
            .map(|v| v.trim().to_string())
    };
    let from_gsettings = || {
        let out = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "icon-theme"])
            .output()
            .ok()?;
        Some(
            String::from_utf8(out.stdout)
                .ok()?
                .trim()
                .trim_matches('\'')
                .to_string(),
        )
    };
    from_gtk()
        .filter(|t| installed(t))
        .or_else(|| from_gsettings().filter(|t| installed(t)))
        .unwrap_or_else(|| "Adwaita".into())
}

/// The theme, the themes it inherits from, and hicolor, each as directories to search.
fn theme_chain(wanted: &str) -> Vec<ThemeDir> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".icons"));
    }
    roots.extend(data_dirs().into_iter().map(|d| d.join("icons")));

    let first = if wanted == "auto" {
        auto_theme(&roots)
    } else {
        wanted.to_string()
    };
    tracing::info!("icon theme: {first}");
    let mut queue = vec![first];
    let mut seen = HashSet::new();
    let mut chain = Vec::new();
    while let Some(name) = queue.first().cloned() {
        queue.remove(0);
        if !seen.insert(name.clone()) {
            continue;
        }
        let mut dirs = Vec::new();
        for root in &roots {
            let theme = root.join(&name);
            if let Ok(index) = std::fs::read_to_string(theme.join("index.theme"))
                && let Some(parents) = index.lines().find_map(|l| l.trim().strip_prefix("Inherits="))
            {
                queue.extend(
                    parents
                        .split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty()),
                );
            }
            collect_icon_dirs(&theme, &mut dirs);
        }
        if !dirs.is_empty() {
            chain.push(ThemeDir { dirs });
        }
        if queue.is_empty() && !seen.contains("hicolor") {
            queue.push("hicolor".into());
        }
    }
    chain
}

/// Every icon directory (any context, symbolic ones first) of the theme chain, then Adwaita
/// and hicolor, which most symbolic icons come from.
fn symbolic_dirs(wanted: &str) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".icons"));
    }
    roots.extend(data_dirs().into_iter().map(|d| d.join("icons")));
    let first = if wanted == "auto" { auto_theme(&roots) } else { wanted.to_string() };
    let mut names = vec![first];
    let mut i = 0;
    while i < names.len() {
        for root in &roots {
            if let Ok(index) = std::fs::read_to_string(root.join(&names[i]).join("index.theme"))
                && let Some(parents) = index.lines().find_map(|l| l.trim().strip_prefix("Inherits="))
            {
                for p in parents.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    if !names.iter().any(|n| n == p) {
                        names.push(p.to_string());
                    }
                }
            }
        }
        i += 1;
    }
    for extra in ["Adwaita", "hicolor"] {
        if !names.iter().any(|n| n == extra) {
            names.push(extra.into());
        }
    }
    let mut out = Vec::new();
    for name in &names {
        let mut theme_dirs: Vec<PathBuf> = Vec::new();
        for root in &roots {
            let Ok(top) = std::fs::read_dir(root.join(name)) else { continue };
            for a in top.flatten().filter(|e| e.path().is_dir()) {
                let Ok(sub) = std::fs::read_dir(a.path()) else { continue };
                theme_dirs.extend(sub.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
            }
        }
        // Symbolic and scalable folders first: they hold the vector versions.
        theme_dirs.sort_by_key(|p| {
            let s = p.to_string_lossy();
            if s.contains("symbolic") {
                0
            } else if s.contains("scalable") {
                1
            } else {
                2
            }
        });
        out.extend(theme_dirs);
    }
    out
}

/// Paint every pixel of an icon one color, keeping its shape (its alpha).
fn tint(mut icon: Icon, color: [u8; 4]) -> Icon {
    for px in icon.rgba.chunks_exact_mut(4) {
        let a = px[3] as u32 * color[3] as u32 / 255;
        let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        px.copy_from_slice(&[m(color[0]), m(color[1]), m(color[2]), a as u8]);
    }
    icon
}

/// Premultiplied RGBA of any size, scaled into a `px` square (aspect kept, centered).
pub fn scaled(rgba: &[u8], width: u32, height: u32, px: u32) -> Icon {
    Icon {
        size: px,
        rgba: resize(rgba, width, height, px),
    }
}

/// Decode an icon file (PNG or SVG) to `px` square.
pub fn load_file(path: &Path, px: u32) -> Option<Icon> {
    load(path, px)
}

/// `$XDG_DATA_DIRS` for launched apps, so they (and we) find Flatpak and Snap apps.
/// Starts from the inherited value (or the spec default) and adds any missing dirs.
pub fn xdg_data_dirs() -> String {
    let inherited = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let mut dirs: Vec<String> = if inherited.is_empty() {
        vec!["/usr/local/share".into(), "/usr/share".into()]
    } else {
        inherited.split(':').filter(|d| !d.is_empty()).map(String::from).collect()
    };
    let mut extra = vec![
        "/var/lib/flatpak/exports/share".to_string(),
        "/var/lib/snapd/desktop".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        extra.insert(0, format!("{home}/.local/share/flatpak/exports/share"));
    }
    for dir in extra {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}

/// App icon directories in a theme, like "48x48/apps", "apps/48" or "scalable/apps".
fn collect_icon_dirs(theme: &Path, out: &mut Vec<(PathBuf, Option<u32>)>) {
    let Ok(top) = std::fs::read_dir(theme) else { return };
    for a in top.flatten().filter(|e| e.path().is_dir()) {
        let Ok(sub) = std::fs::read_dir(a.path()) else { continue };
        for b in sub.flatten().filter(|e| e.path().is_dir()) {
            let (first, second) = (a.file_name(), b.file_name());
            let (first, second) = (first.to_string_lossy(), second.to_string_lossy());
            let size = if second == "apps" {
                first
            } else if first == "apps" {
                second
            } else {
                continue;
            };
            // "48x48", "48x48@2x", "48" or "scalable". Doubled dirs hold bigger images.
            let (base, factor) = match size.split_once('@') {
                Some((b, f)) => (b.to_string(), f.trim_end_matches('x').parse::<u32>().unwrap_or(1)),
                None => (size.to_string(), 1),
            };
            let nominal = if base == "scalable" || base.contains("symbolic") {
                None
            } else {
                match base.split('x').next().and_then(|n| n.parse::<u32>().ok()) {
                    Some(n) => Some(n * factor),
                    None => continue,
                }
            };
            out.push((b.path(), nominal));
        }
    }
}

fn load(path: &Path, px: u32) -> Option<Icon> {
    let data = std::fs::read(path).ok()?;
    let icon = if path.extension().is_some_and(|e| e == "svg") {
        load_svg(&data, px)
    } else {
        load_png(&data, px)
    };
    if icon.is_none() {
        tracing::debug!("could not load icon {}", path.display());
    }
    icon
}

fn load_svg(data: &[u8], px: u32) -> Option<Icon> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(data, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let scale = px as f32 / size.width().max(size.height());
    let (dx, dy) = (
        (px as f32 - size.width() * scale) / 2.0,
        (px as f32 - size.height() * scale) / 2.0,
    );
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(Icon {
        size: px,
        rgba: pixmap.take(),
    })
}

fn load_png(data: &[u8], px: u32) -> Option<Icon> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    // Expand to RGBA whatever came out (gray, gray+alpha, RGB, RGBA).
    let channels = info.color_type.samples();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for p in buf[..info.buffer_size()].chunks_exact(channels) {
        let (r, g, b, a) = match *p {
            [v] => (v, v, v, 255),
            [v, a] => (v, v, v, a),
            [r, g, b] => (r, g, b, 255),
            [r, g, b, a] => (r, g, b, a),
            _ => return None,
        };
        let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
        rgba.extend([m(r), m(g), m(b), a]);
    }
    Some(Icon {
        size: px,
        rgba: resize(&rgba, w, h, px),
    })
}

/// Scale premultiplied RGBA to `px` square, keeping the aspect ratio and centering it.
/// Each output pixel averages the source area it covers, so big icons shrink cleanly.
fn resize(src: &[u8], w: u32, h: u32, px: u32) -> Vec<u8> {
    let mut out = vec![0u8; (px * px * 4) as usize];
    let scale = px as f32 / w.max(h) as f32;
    let (ow, oh) = ((w as f32 * scale).round() as u32, (h as f32 * scale).round() as u32);
    let (ox, oy) = ((px - ow.min(px)) / 2, (px - oh.min(px)) / 2);
    let step = 1.0 / scale;
    for y in 0..oh.min(px) {
        for x in 0..ow.min(px) {
            // Source footprint of this pixel, at least one source pixel wide.
            let (x0, y0) = (x as f32 * step, y as f32 * step);
            let (x1, y1) = (x0 + step.max(1.0), y0 + step.max(1.0));
            let mut acc = [0f32; 4];
            let mut total = 0.0;
            let mut sy = y0.floor() as u32;
            while (sy as f32) < y1 && sy < h {
                let cy = ((sy + 1) as f32).min(y1) - (sy as f32).max(y0);
                let mut sx = x0.floor() as u32;
                while (sx as f32) < x1 && sx < w {
                    let cx = ((sx + 1) as f32).min(x1) - (sx as f32).max(x0);
                    let wgt = cx * cy;
                    let i = ((sy * w + sx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += src[i + c] as f32 * wgt;
                    }
                    total += wgt;
                    sx += 1;
                }
                sy += 1;
            }
            let o = (((y + oy) * px + x + ox) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / total.max(f32::EPSILON)).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_averages_blocks() {
        // 2x2 → 1x1: the average of the four pixels.
        let src = [255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 0, 0, 255];
        assert_eq!(resize(&src, 2, 2, 1), vec![128, 0, 0, 255]);
        // Wide images are centered vertically.
        let wide = [255u8; 4 * 4 * 2];
        let out = resize(&wide, 4, 2, 4);
        assert_eq!(&out[..16], &[0; 16]);
        assert_eq!(&out[16..20], &[255; 4]);
    }

    #[test]
    fn svg_renders() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;
        let icon = load_svg(svg, 8).unwrap();
        assert_eq!(icon.rgba.len(), 8 * 8 * 4);
        assert_eq!(&icon.rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn tint_keeps_shape() {
        let icon = Icon {
            size: 1,
            rgba: vec![10, 20, 30, 128],
        };
        assert_eq!(tint(icon, [255, 0, 0, 255]).rgba, vec![128, 0, 0, 128]);
    }

    /// Needs real icon themes, so it only reports. `cargo test icon_lookup -- --nocapture`
    #[test]
    fn icon_lookup() {
        let mut icons = Icons::new("auto");
        for app in [
            "Alacritty",
            "vivaldi-stable",
            "code",
            "com.microsoft.VSCode",
            "steam",
            "org.gnome.Nautilus",
        ] {
            let found = icons.get(app, 16).is_some();
            eprintln!("{app}: {}", if found { "found" } else { "missing" });
        }
    }
}
