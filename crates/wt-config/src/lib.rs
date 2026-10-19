//! wayland-time's config file and shared look, used by the compositor and by its apps.
//!
//! - [`config`]: the TOML file: defaults, parsing, validation, keybinds and actions
//! - [`theme`]: the named color palette and how color expressions resolve
//! - [`fonts`]: finding font files by fontconfig name

pub mod config;
pub mod fonts;
pub mod theme;

pub use config::*;
