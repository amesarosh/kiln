//! The shared look: one font and one palette of named colors that every part of the UI draws from.
//!
//! The palette in `[theme.colors]` maps names to color expressions:
//!   "#rgb", "#rrggbb", "#rrggbbaa"   a literal color
//!   "accent"                          another color in the palette
//!   "mix(a, b, 0.25)"                 25% of the way from `a` to `b` (each side is an expression)
//!
//! Components (the bar, borders, ...) read fixed role names, listed in `Palette`. Any other names
//! are free for the user, e.g. `pink = "#f5c2e7"` then `accent = "pink"`. A component can also
//! have its own `colors` table that overrides names just for that component; derived colors such
//! as `hover = "mix(surface, text, 0.1)"` are then worked out again with the overrides applied.

use std::collections::BTreeMap;

use crate::config::Color;

macro_rules! palette {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        /// Every color role the UI uses, fully resolved.
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct Palette {
            $($(#[$doc])* pub $name: Color,)*
        }

        impl Palette {
            pub const ROLES: &[&str] = &[$(stringify!($name)),*];

            fn from_map(map: &BTreeMap<String, Color>) -> Self {
                Self { $($name: map[stringify!($name)],)* }
            }
        }
    };
}

palette! {
    /// Behind all windows.
    base,
    /// Bars, menus and panels.
    surface,
    /// Regular text and icons.
    text,
    /// Secondary text, empty workspaces.
    subtext,
    /// Text that is less important than `text` but more than `subtext`.
    muted,
    /// The highlight color: focused things, the active workspace.
    accent,
    /// `accent` when its monitor doesn't have focus.
    accent_inactive,
    /// Text and marks drawn on top of `accent`.
    on_accent,
    /// Things that need attention.
    urgent,
    /// Items sitting on a `surface`, like window tabs.
    raised,
    /// An item under the pointer.
    hover,
    /// The chosen item, like the focused window's tab.
    selected,
    /// Thin lines between areas.
    separator,
    /// Border of the focused window.
    border_focused,
    /// Border of other windows.
    border,
    /// Screen shown while locked, before the locker draws.
    locked,
    /// Screen shown while locked if the locker crashed.
    lock_failed,
}

/// The unresolved palette: names to expressions.
pub type ColorDefs = BTreeMap<String, String>;

/// Resolve `defs` with `overrides` layered on top. `scope` prefixes error messages.
pub fn resolve(defs: &ColorDefs, overrides: &ColorDefs, scope: &str) -> Result<Palette, Vec<String>> {
    let mut merged = defs.clone();
    merged.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));

    let mut problems = Vec::new();
    for role in Palette::ROLES {
        if !merged.contains_key(*role) {
            problems.push(format!("{scope}: missing color \"{role}\""));
        }
    }
    let mut done = BTreeMap::new();
    for name in merged.keys() {
        let mut stack = Vec::new();
        if let Err(e) = lookup(name, &merged, &mut done, &mut stack) {
            let msg = format!("{scope}: {}", e.trim_start_matches('@'));
            if !problems.contains(&msg) {
                problems.push(msg);
            }
        }
    }
    if problems.is_empty() {
        Ok(Palette::from_map(&done))
    } else {
        Err(problems)
    }
}

fn lookup(
    name: &str,
    defs: &ColorDefs,
    done: &mut BTreeMap<String, Color>,
    stack: &mut Vec<String>,
) -> Result<Color, String> {
    if let Some(c) = done.get(name) {
        return Ok(*c);
    }
    if stack.iter().any(|n| n == name) {
        stack.push(name.to_string());
        return Err(format!("@colors refer to each other in a loop: {}", stack.join(" -> ")));
    }
    let expr = defs.get(name).ok_or_else(|| format!("unknown color \"{name}\""))?;
    stack.push(name.to_string());
    // Errors from a color this one refers to are already labelled; pass them on unchanged so
    // one mistake is reported once, at its source.
    let c = eval(expr, defs, done, stack).map_err(|e| {
        if e.starts_with('@') {
            e
        } else {
            format!("@{name} = \"{expr}\": {e}")
        }
    })?;
    stack.pop();
    done.insert(name.to_string(), c);
    Ok(c)
}

fn eval(
    expr: &str,
    defs: &ColorDefs,
    done: &mut BTreeMap<String, Color>,
    stack: &mut Vec<String>,
) -> Result<Color, String> {
    let expr = expr.trim();
    if expr.starts_with('#') {
        return parse_hex(expr);
    }
    if let Some(args) = expr.strip_prefix("mix(").and_then(|r| r.strip_suffix(')')) {
        let parts = split_args(args);
        let [a, b, t] = parts.as_slice() else {
            return Err("mix needs three parts: mix(color, color, amount)".into());
        };
        let t: f32 = t
            .trim()
            .parse()
            .ok()
            .filter(|t| (0.0..=1.0).contains(t))
            .ok_or_else(|| format!("mix amount \"{}\" should be a number from 0 to 1", t.trim()))?;
        let a = eval(a, defs, done, stack)?;
        let b = eval(b, defs, done, stack)?;
        return Ok(mix(a, b, t));
    }
    if expr.is_empty() || !expr.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "\"{expr}\" is not a #hex color, a color name, or mix(a, b, amount)"
        ));
    }
    lookup(expr, defs, done, stack)
}

/// Split on commas that aren't inside parentheses.
fn split_args(s: &str) -> Vec<&str> {
    let (mut depth, mut start, mut out) = (0, 0, Vec::new());
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// "#rgb", "#rrggbb", or "#rrggbbaa"
pub fn parse_hex(s: &str) -> Result<Color, String> {
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| format!("\"{s}\" should start with #"))?;
    let expanded: String = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).chain("ff".chars()).collect(),
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => return Err(format!("\"{s}\" is not #rgb, #rrggbb or #rrggbbaa")),
    };
    let v = u32::from_str_radix(&expanded, 16).map_err(|_| format!("\"{s}\" is not valid hex"))?;
    let c = |shift: u32| ((v >> shift) & 0xff) as f32 / 255.0;
    Ok([c(24), c(16), c(8), c(0)])
}

/// Linear mix between two colors; `t = 0` is `a`, `t = 1` is `b`.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// The shared font and palette.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// fontconfig name, or a path to a font file.
    pub font: String,
    pub font_size: f32,
    pub palette: Palette,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(pairs: &[(&str, &str)]) -> ColorDefs {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn full() -> ColorDefs {
        let mut d: ColorDefs = Palette::ROLES
            .iter()
            .map(|r| (r.to_string(), "#000".to_string()))
            .collect();
        d.insert("text".into(), "#fff".into());
        d.insert("hover".into(), "mix(surface, text, 0.5)".into());
        d
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex("#ff0000").unwrap(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(parse_hex("#fff").unwrap(), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(parse_hex("#00000080").unwrap()[3], 128.0 / 255.0);
        assert!(parse_hex("ff0000").is_err());
    }

    #[test]
    fn names_and_mix() {
        let p = resolve(&full(), &defs(&[("accent", "text")]), "theme").unwrap();
        assert_eq!(p.accent, [1.0; 4]);
        assert_eq!(p.hover, [0.5, 0.5, 0.5, 1.0]);
    }

    #[test]
    fn overrides_flow_into_derived_colors() {
        let p = resolve(&full(), &defs(&[("surface", "#fff")]), "bar").unwrap();
        assert_eq!(p.hover, [1.0; 4]);
    }

    #[test]
    fn nested_mix() {
        let p = resolve(
            &full(),
            &defs(&[("accent", "mix(mix(surface, text, 1), #000, 0.5)")]),
            "t",
        )
        .unwrap();
        assert_eq!(p.accent, [0.5, 0.5, 0.5, 1.0]);
    }

    #[test]
    fn errors() {
        let e = resolve(&full(), &defs(&[("accent", "nope")]), "theme").unwrap_err();
        assert!(e[0].contains("unknown color \"nope\""), "{e:?}");
        let e = resolve(&full(), &defs(&[("accent", "border"), ("border", "accent")]), "theme").unwrap_err();
        assert!(e.iter().any(|p| p.contains("loop")), "{e:?}");
        let e = resolve(&full(), &defs(&[("hover", "mix(surface, text)")]), "theme").unwrap_err();
        assert!(e[0].contains("three parts"), "{e:?}");
        let e = resolve(&defs(&[("text", "#fff")]), &ColorDefs::new(), "theme").unwrap_err();
        assert!(e.iter().any(|p| p.contains("missing color \"accent\"")), "{e:?}");
    }
}
