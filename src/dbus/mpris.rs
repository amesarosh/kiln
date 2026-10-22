//! Media players (Spotify, browsers, mpv with mpris...): what's playing, play/pause, next.
//!
//! Every player on the bus is named `org.mpris.MediaPlayer2.<something>`. The bar shows one:
//! the one playing, else the one most recently seen playing, else any paused one.

use std::collections::HashMap;

use zbus::{blocking::Connection, zvariant::OwnedValue};

pub const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// The player shown in the bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    /// Its bus name.
    pub player: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub can_next: bool,
    pub can_previous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
}

#[derive(Default)]
pub struct Players {
    /// The player last seen playing; it stays the one shown while paused.
    last_playing: Option<String>,
    shown: Option<Media>,
}

impl Players {
    /// Read the players again. Returns the shown one if anything about it changed.
    pub fn refresh(&mut self, conn: &Connection) -> Option<Option<Media>> {
        let names: Vec<String> = zbus::blocking::fdo::DBusProxy::new(conn)
            .and_then(|p| p.list_names().map_err(zbus::Error::from))
            .map(|n| n.into_iter().map(|n| n.to_string()).filter(|n| n.starts_with(PREFIX)).collect())
            .unwrap_or_default();
        let mut found: Vec<Media> = names.iter().filter_map(|n| read(conn, n)).collect();
        // Browsers register one player per tab; ones with nothing loaded have no title.
        found.retain(|m| !m.title.is_empty() || m.playing);
        let playing = found.iter().find(|m| m.playing).cloned();
        if let Some(p) = &playing {
            self.last_playing = Some(p.player.clone());
        }
        let shown = playing
            .or_else(|| {
                let last = self.last_playing.as_ref()?;
                found.iter().find(|m| &m.player == last).cloned()
            })
            .or_else(|| found.first().cloned());
        if shown == self.shown {
            return None;
        }
        self.shown = shown.clone();
        Some(shown)
    }

    pub fn command(&mut self, conn: &Connection, cmd: MediaCommand) {
        let Some(m) = &self.shown else { return };
        let method = match cmd {
            MediaCommand::PlayPause => "PlayPause",
            MediaCommand::Next => "Next",
            MediaCommand::Previous => "Previous",
        };
        if let Err(e) = conn.call_method(Some(m.player.as_str()), PATH, Some(PLAYER_IFACE), method, &()) {
            tracing::debug!("media {}: {method}: {e}", m.player);
        }
    }
}

fn read(conn: &Connection, name: &str) -> Option<Media> {
    let proxy = zbus::blocking::fdo::PropertiesProxy::builder(conn)
        .destination(name.to_string())
        .ok()?
        .path(PATH)
        .ok()?
        .build()
        .ok()?;
    let props: HashMap<String, OwnedValue> = proxy
        .get_all(zbus::names::InterfaceName::from_static_str_unchecked(PLAYER_IFACE))
        .ok()?;
    let status = props
        .get("PlaybackStatus")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default();
    if status == "Stopped" {
        return None;
    }
    let flag = |k: &str| props.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
    let meta: HashMap<String, OwnedValue> = props
        .get("Metadata")
        .and_then(|v| HashMap::<String, OwnedValue>::try_from(v.clone()).ok())
        .unwrap_or_default();
    let title = meta
        .get("xesam:title")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default();
    let artist = meta
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .map(|a| a.join(", "))
        .unwrap_or_default();
    Some(Media {
        player: name.to_string(),
        title,
        artist,
        playing: status == "Playing",
        can_next: flag("CanGoNext"),
        can_previous: flag("CanGoPrevious"),
    })
}
