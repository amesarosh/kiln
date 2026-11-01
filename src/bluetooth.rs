//! Bluetooth for the bar: whether there's an adapter, whether it's on, and how many devices
//! are connected, read from BlueZ on the system bus. A thread waits for BlueZ's signals and
//! reads again, so the icon follows changes made anywhere (the Bluetooth app, bluetoothctl, a
//! device connecting by itself). Turning it on or off is a call on a thread too, so a slow
//! bluetoothd never stalls the desktop.

use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use smithay::reexports::calloop::channel;
use zbus::{
    MatchRule,
    blocking::{Connection, MessageIterator, fdo::ObjectManagerProxy},
    message::Type,
    zvariant::{OwnedValue, Value},
};

use crate::state::State;

const BLUEZ: &str = "org.bluez";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bluetooth {
    /// There's an adapter (the widget only shows then).
    pub present: bool,
    pub powered: bool,
    /// Devices connected now.
    pub connected: usize,
}

pub struct Watcher {
    conn: Connection,
    /// The adapter's object path, to turn it on and off.
    adapter: Arc<Mutex<Option<String>>>,
}

impl State {
    /// Start following BlueZ (once).
    pub fn start_bluetooth(&mut self) {
        if std::mem::replace(&mut self.bluetooth_started, true) {
            return;
        }
        let conn = match zbus::blocking::connection::Builder::system()
            .and_then(|b| b.method_timeout(Duration::from_secs(5)).build())
        {
            Ok(c) => c,
            Err(e) => return tracing::warn!("no system bus, so no Bluetooth widget: {e}"),
        };
        let (tx, rx) = channel::channel::<Bluetooth>();
        let inserted = self.loop_handle.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg(bt) = event
                && bt != state.bluetooth
            {
                state.bluetooth = bt;
                state.needs_redraw = true;
            }
        });
        if let Err(e) = inserted {
            return tracing::warn!("bluetooth channel: {e}");
        }
        let adapter = Arc::new(Mutex::new(None));
        let (wake, wanted) = mpsc::channel();
        for rule in [
            MatchRule::builder().msg_type(Type::Signal).sender(BLUEZ),
            MatchRule::builder()
                .msg_type(Type::Signal)
                .sender("org.freedesktop.DBus")
                .and_then(|r| r.member("NameOwnerChanged"))
                .and_then(|r| r.add_arg(BLUEZ)),
        ] {
            let (conn, wake) = (conn.clone(), wake.clone());
            std::thread::spawn(move || {
                match rule
                    .map(|r| r.build())
                    .and_then(|r| MessageIterator::for_match_rule(r, &conn, None))
                {
                    Ok(iter) => iter.for_each(|_| {
                        let _ = wake.send(());
                    }),
                    Err(e) => tracing::warn!("not watching Bluetooth: {e}"),
                }
            });
        }
        let (reader_conn, reader_adapter) = (conn.clone(), adapter.clone());
        let spawned = std::thread::Builder::new().name("bluetooth".into()).spawn(move || {
            let mut last = None;
            loop {
                let bt = read(&reader_conn, &reader_adapter);
                if last.as_ref() != Some(&bt) {
                    if tx.send(bt.clone()).is_err() {
                        return;
                    }
                    last = Some(bt);
                }
                // Changes come in bursts (and many while something searches): read once
                // they settle, and now and then anyway in case a signal was missed.
                let _ = wanted.recv_timeout(Duration::from_secs(30));
                std::thread::sleep(Duration::from_millis(200));
                while wanted.try_recv().is_ok() {}
            }
        });
        if let Err(e) = spawned {
            return tracing::warn!("bluetooth thread: {e}");
        }
        self.bluetooth_watch = Some(Watcher { conn, adapter });
    }

    /// Right-click on the widget: turn Bluetooth on or off.
    pub fn toggle_bluetooth(&mut self) {
        let Some(w) = &self.bluetooth_watch else { return };
        let Some(path) = w.adapter.lock().unwrap().clone() else {
            return;
        };
        let (conn, on) = (w.conn.clone(), !self.bluetooth.powered);
        std::thread::spawn(move || {
            let args = ("org.bluez.Adapter1", "Powered", Value::from(on));
            let set = conn.call_method(
                Some(BLUEZ),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &args,
            );
            if let Err(e) = set {
                tracing::warn!("couldn't turn Bluetooth {}: {e}", if on { "on" } else { "off" });
            }
        });
    }
}

fn read(conn: &Connection, adapter_path: &Mutex<Option<String>>) -> Bluetooth {
    let objects = ObjectManagerProxy::builder(conn)
        .destination(BLUEZ)
        .and_then(|b| b.path("/"))
        .and_then(|b| b.build())
        .and_then(|p| p.get_managed_objects().map_err(zbus::Error::from));
    let Ok(objects) = objects else {
        *adapter_path.lock().unwrap() = None;
        return Bluetooth::default();
    };
    let flag = |props: &std::collections::HashMap<String, OwnedValue>, key: &str| {
        props
            .get(key)
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    };
    let mut paths: Vec<_> = objects.keys().collect();
    paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    // The first adapter, like the Bluetooth app.
    let adapter = paths.iter().find_map(|path| {
        let props = objects[*path]
            .iter()
            .find(|(i, _)| i.as_str() == "org.bluez.Adapter1")?
            .1;
        Some((path.to_string(), flag(props, "Powered")))
    });
    let Some((path, powered)) = adapter else {
        *adapter_path.lock().unwrap() = None;
        return Bluetooth::default();
    };
    let prefix = format!("{path}/");
    let connected = paths
        .iter()
        .filter(|p| p.as_str().starts_with(&prefix))
        .filter_map(|p| objects[*p].iter().find(|(i, _)| i.as_str() == "org.bluez.Device1"))
        .filter(|(_, props)| flag(props, "Connected"))
        .count();
    *adapter_path.lock().unwrap() = Some(path);
    Bluetooth {
        present: true,
        powered,
        connected,
    }
}
