//! The config file lives in the shared `kiln-config` crate (the settings app uses it too). This
//! re-exports it and adds the conversions to smithay types that only the compositor needs.

use smithay::{input::keyboard::ModifiersState, utils};
pub use kiln_config::config::*;

/// The modifiers held in a keyboard state.
pub fn mods_from_state(m: &ModifiersState) -> Mods {
    Mods {
        logo: m.logo,
        alt: m.alt,
        ctrl: m.ctrl,
        shift: m.shift,
    }
}

/// A monitor rotation from the config, as smithay names it.
pub fn smithay_transform(t: Transform) -> utils::Transform {
    match t {
        Transform::Normal => utils::Transform::Normal,
        Transform::_90 => utils::Transform::_90,
        Transform::_180 => utils::Transform::_180,
        Transform::_270 => utils::Transform::_270,
        Transform::Flipped => utils::Transform::Flipped,
        Transform::Flipped90 => utils::Transform::Flipped90,
        Transform::Flipped180 => utils::Transform::Flipped180,
        Transform::Flipped270 => utils::Transform::Flipped270,
    }
}
