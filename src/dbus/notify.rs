//! The desktop notification daemon (`org.freedesktop.Notifications`), on the bus side.
//!
//! Apps call `Notify`; each notification is handed to the main loop, which shows and keeps it.
//! When one is dismissed, times out or has an action clicked, the main loop says so and the
//! matching signal goes back to the app. If another daemon (mako, dunst) already owns the name,
//! it's left alone and keeps doing the job.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU32, Ordering},
};

use smithay::reexports::calloop::channel;
use zbus::zvariant::{OwnedValue, Value};

use super::Event;

const PATH: &str = "/org/freedesktop/Notifications";
const NAME: &str = "org.freedesktop.Notifications";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// How long a notification stays up on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// The app left it to us.
    Default,
    /// Until it's dismissed.
    Never,
    Millis(u32),
}

/// Why a notification went away, as the spec numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    /// The app asked (CloseNotification).
    Closed = 3,
}

/// A picture sent with a notification: premultiplied RGBA.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    /// It updates one already shown, which keeps its place.
    pub replaces: bool,
    pub app_name: String,
    /// An icon name or file path for the app.
    pub app_icon: String,
    /// A picture of its own (a photo, album art), or a file to load one from.
    pub image: Option<Image>,
    pub image_path: Option<String>,
    pub summary: String,
    pub body: String,
    /// (key, label). The "default" key is what clicking the notification does.
    pub actions: Vec<(String, String)>,
    pub urgency: Urgency,
    pub timeout: Timeout,
    /// Shown but not kept in the list afterwards.
    pub transient: bool,
}

struct Server {
    events: std::sync::Mutex<channel::Sender<Event>>,
    next_id: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            // Never hand out 0: it means "new" in replaces_id.
            loop {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                if id != 0 {
                    break id;
                }
            }
        };
        let hint_str = |k: &str| hints.get(k).and_then(|v| String::try_from(v.clone()).ok());
        let urgency = match hints.get("urgency").and_then(|v| u8::try_from(v.clone()).ok()) {
            Some(0) => Urgency::Low,
            Some(2) => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let image = ["image-data", "image_data", "icon_data"]
            .iter()
            .find_map(|k| hints.get(*k))
            .and_then(image_from_value);
        let image_path = hint_str("image-path").or_else(|| hint_str("image_path"));
        let app_icon = if app_icon.is_empty() {
            hint_str("desktop-entry").unwrap_or_default()
        } else {
            app_icon
        };
        let n = Notification {
            id,
            replaces: replaces_id != 0,
            app_name,
            app_icon,
            image,
            image_path,
            summary: plain(&summary),
            body: plain(&body),
            actions: actions
                .chunks_exact(2)
                .map(|kv| (kv[0].clone(), kv[1].clone()))
                .collect(),
            urgency,
            timeout: match expire_timeout {
                t if t < 0 => Timeout::Default,
                0 => Timeout::Never,
                t => Timeout::Millis(t as u32),
            },
            transient: hints
                .get("transient")
                .and_then(|v| bool::try_from(v.clone()).ok())
                .unwrap_or(false),
        };
        let _ = self.events.lock().unwrap().send(Event::Notify(n));
        id
    }

    fn close_notification(&self, id: u32) {
        let _ = self.events.lock().unwrap().send(Event::CloseNotification(id));
    }

    fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["body", "actions", "icon-static", "persistence"]
    }

    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        ("kiln", "kiln", env!("CARGO_PKG_VERSION"), "1.2")
    }
}

/// Serve the daemon. Returns false if another one is running (or the bus said no).
pub fn serve(conn: &zbus::blocking::Connection, events: channel::Sender<Event>) -> bool {
    let server = Server {
        events: std::sync::Mutex::new(events),
        next_id: AtomicU32::new(1),
    };
    if let Err(e) = conn.object_server().at(PATH, server) {
        tracing::warn!("notifications: {e}");
        return false;
    }
    match conn.request_name_with_flags(NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into()) {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner) => {
            tracing::info!("showing desktop notifications");
            true
        }
        Ok(_) => {
            tracing::info!("another notification daemon is running; leaving notifications to it");
            let _ = conn.object_server().remove::<Server, _>(PATH);
            false
        }
        Err(e) => {
            tracing::warn!("notifications: can't take {NAME}: {e}");
            let _ = conn.object_server().remove::<Server, _>(PATH);
            false
        }
    }
}

pub fn closed(conn: &zbus::blocking::Connection, id: u32, reason: CloseReason) {
    let _ = conn.emit_signal(
        None::<zbus::names::BusName>,
        PATH,
        NAME,
        "NotificationClosed",
        &(id, reason as u32),
    );
}

pub fn action_invoked(conn: &zbus::blocking::Connection, id: u32, key: &str) {
    let _ = conn.emit_signal(None::<zbus::names::BusName>, PATH, NAME, "ActionInvoked", &(id, key));
}

/// The `image-data` hint: (width, height, rowstride, has_alpha, bits_per_sample, channels, data).
fn image_from_value(v: &OwnedValue) -> Option<Image> {
    let Value::Structure(s) = &**v else { return None };
    let f = s.fields();
    if f.len() != 7 {
        return None;
    }
    let int = |v: &Value| i32::try_from(v).ok();
    let (w, h, stride) = (int(&f[0])?, int(&f[1])?, int(&f[2])?);
    let alpha = bool::try_from(&f[3]).ok()?;
    let (bits, channels) = (int(&f[4])?, int(&f[5])?);
    let data: Vec<u8> = match &f[6] {
        Value::Array(a) => a.iter().filter_map(|b| u8::try_from(b).ok()).collect(),
        _ => return None,
    };
    if w <= 0 || h <= 0 || w > 1024 || h > 1024 || bits != 8 || !(3..=4).contains(&channels) {
        return None;
    }
    let (w, h, stride, ch) = (w as usize, h as usize, stride as usize, channels as usize);
    if data.len() < stride * (h - 1) + w * ch {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * ch;
            let a = if alpha && ch == 4 { data[i + 3] } else { 255 };
            let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
            rgba.extend([m(data[i]), m(data[i + 1]), m(data[i + 2]), a]);
        }
    }
    Some(Image {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

/// Notification text as plain text: tags dropped, common entities decoded. Apps may send a
/// little markup even though we don't ask for it.
pub fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut had_tags = false;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        // A tag starts with a letter or '/' and ends with '>'; anything else is a plain '<'.
        let tag_end = after
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic() || *c == '/')
            .and_then(|_| after.find('>'));
        match tag_end {
            Some(j) => {
                had_tags = true;
                rest = &after[j + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    // Only worth decoding if the app sent tags too; otherwise "&lt;" was meant literally.
    if !had_tags {
        return out;
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_stripped() {
        assert_eq!(plain("<b>Hi</b> &amp; bye"), "Hi & bye");
        assert_eq!(plain("a < b &amp; c"), "a < b &amp; c");
        assert_eq!(plain("plain"), "plain");
    }
}
