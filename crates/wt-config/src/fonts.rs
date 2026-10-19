//! Finding font files for the theme font.

use std::path::{Path, PathBuf};

/// Fonts tried when fontconfig can't resolve the configured name.
pub const FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
];

/// Ask fontconfig for the file behind a font name. A name with a `/` is taken as a path.
pub fn resolve(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", name])
        .output()
        .ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    Path::new(&path).is_file().then_some(path)
}

/// Font files to try for `name`, best first: fontconfig's match, then common fallbacks.
pub fn candidates(name: &str) -> impl Iterator<Item = PathBuf> {
    resolve(name)
        .into_iter()
        .chain(FALLBACK_FONTS.iter().map(PathBuf::from))
}

/// Installed font family names, sorted, for font pickers. Empty if fontconfig isn't there.
pub fn families() -> Vec<String> {
    let Ok(out) = std::process::Command::new("fc-list").args([":", "family"]).output() else {
        return Vec::new();
    };
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        // A font with several names lists them comma-separated; the first is the main one.
        .filter_map(|l| l.split(',').next())
        .map(|s| s.trim().replace("\\-", "-"))
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_by_key(|s| s.to_lowercase());
    names.dedup();
    names
}

/// How a monitor's pixels split into colored stripes, for subpixel text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subpixel {
    Rgb,
    Bgr,
    /// Plain grayscale anti-aliasing.
    None,
}

/// The subpixel order fontconfig sets for `name`, so compositor text matches other apps.
/// Vertical stripes and an unset order fall back to grayscale.
pub fn subpixel(name: &str) -> Subpixel {
    let query = if name.contains('/') { "sans" } else { name };
    let out = std::process::Command::new("fc-match").args(["-f", "%{rgba}", query]).output();
    match out.ok().and_then(|o| String::from_utf8(o.stdout).ok()).as_deref().map(str::trim) {
        Some("1") => Subpixel::Rgb,
        Some("2") => Subpixel::Bgr,
        _ => Subpixel::None,
    }
}
