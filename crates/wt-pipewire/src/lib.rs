//! Sound from PipeWire: output and input devices, the apps playing or recording, their
//! volumes, and the sound cards' profiles.
//!
//! Reading goes through `pw-dump --monitor`, which prints the whole graph as JSON and then
//! every object again whenever it changes, so [`watch`] always has the current state without
//! polling. Changes go through `wpctl` and `pw-metadata` on a worker thread ([`Control`]).
//! Both tools come with PipeWire and WirePlumber, so nothing else needs to be installed.

use std::{
    collections::HashMap,
    io::BufReader,
    process::{Command as Process, Stdio},
    sync::mpsc,
    time::Duration,
};

use serde_json::Value;

/// Everything the sound app and the bar show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// Speakers, headphones, HDMI...: where sound goes.
    pub outputs: Vec<Device>,
    /// Microphones and line inputs.
    pub inputs: Vec<Device>,
    /// Apps playing sound.
    pub playing: Vec<Stream>,
    /// Apps recording.
    pub recording: Vec<Stream>,
    /// Sound cards, for choosing a profile.
    pub cards: Vec<Card>,
    /// `node.name` of the default output and input.
    pub default_output: Option<String>,
    pub default_input: Option<String>,
}

impl Model {
    pub fn default_output(&self) -> Option<&Device> {
        let name = self.default_output.as_deref()?;
        self.outputs.iter().find(|d| d.name == name)
    }

    pub fn default_input(&self) -> Option<&Device> {
        let name = self.default_input.as_deref()?;
        self.inputs.iter().find(|d| d.name == name)
    }
}

/// An output or input device (a PipeWire sink or source node).
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: u32,
    /// `node.name`: stable across restarts, used for the default and for moving apps.
    pub name: String,
    /// What to call it.
    pub label: String,
    pub kind: Kind,
    /// `object.serial`, which an app's target can name instead of `name`.
    pub serial: Option<String>,
    /// 1.0 is 100%, on the same scale as `wpctl` (cubic, like a volume knob feels).
    pub volume: f32,
    pub muted: bool,
}

/// What sort of device, for picking an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Speakers,
    Headphones,
    Headset,
    /// Sound through a monitor or TV (HDMI, DisplayPort).
    Display,
    Microphone,
}

/// An app's playback or recording stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub id: u32,
    /// The app's name.
    pub app: String,
    /// What it's playing ("media.name"), when the app says.
    pub title: String,
    /// Names to look up an icon by, best first.
    pub icons: Vec<String>,
    pub volume: f32,
    pub muted: bool,
    /// Making sound now (not paused).
    pub active: bool,
    /// The device the user (or the app) picked for it (`node.name` or serial), or None to
    /// follow the default.
    pub target: Option<String>,
    /// Id of the device it's connected to now, if it is.
    pub device: Option<u32>,
}

/// A sound card and its profiles (which outputs and inputs it offers).
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: u32,
    pub label: String,
    pub profiles: Vec<Profile>,
    /// Index of the profile in use.
    pub current: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub index: u32,
    pub label: String,
    /// False when nothing is plugged into what it needs.
    pub available: bool,
}

/// Follow PipeWire, calling `changed` with the new state after each change, or with `None`
/// when PipeWire isn't running (it's tried again every few seconds). Stops once `changed`
/// returns false. Runs on its own thread.
pub fn watch(mut changed: impl FnMut(Option<Model>) -> bool + Send + 'static) {
    let spawned = std::thread::Builder::new().name("pipewire".into()).spawn(move || {
        loop {
            let child = Process::new("pw-dump")
                .args(["--monitor", "--no-colors"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        tracing::warn!("sound needs pw-dump, which comes with PipeWire");
                    } else {
                        tracing::warn!("running pw-dump: {e}");
                    }
                    changed(None);
                    return;
                }
            };
            let mut graph = Graph::default();
            let mut last = None;
            let out = BufReader::new(child.stdout.take().unwrap());
            for update in serde_json::Deserializer::from_reader(out).into_iter::<Value>() {
                let Ok(Value::Array(objects)) = update else { break };
                graph.apply(objects);
                let model = graph.model();
                if last.as_ref() != Some(&model) {
                    if !changed(Some(model.clone())) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    last = Some(model);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            if !changed(None) {
                return;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("pipewire thread: {e}");
    }
}

/// The objects pw-dump has told us about, by id.
#[derive(Default)]
struct Graph {
    objects: HashMap<u64, Value>,
}

impl Graph {
    /// Each update holds whole objects; one with `"info": null` was removed.
    fn apply(&mut self, objects: Vec<Value>) {
        for o in objects {
            let Some(id) = o["id"].as_u64() else { continue };
            let removed = o.get("info").is_some_and(Value::is_null) && o.get("metadata").is_none();
            if removed || o.get("metadata").is_some_and(Value::is_null) {
                self.objects.remove(&id);
            } else if let Some(items) = o["metadata"].as_array()
                && let Some(old) = self.objects.get_mut(&id)
                && let Some(list) = old["metadata"].as_array_mut()
            {
                // Metadata updates hold only the entries that changed; a null value deletes one.
                for item in items {
                    let same = |e: &Value| e["subject"] == item["subject"] && e["key"] == item["key"];
                    list.retain(|e| !same(e));
                    if !item["value"].is_null() {
                        list.push(item.clone());
                    }
                }
            } else {
                self.objects.insert(id, o);
            }
        }
    }

    fn model(&self) -> Model {
        let mut m = Model::default();
        let mut targets = HashMap::new();
        // Metadata: the default devices, and where the user sent each app.
        for o in self.of_type("Metadata") {
            if o["props"]["metadata.name"].as_str() != Some("default") {
                continue;
            }
            for item in o["metadata"].as_array().into_iter().flatten() {
                let (subject, key) = (
                    item["subject"].as_u64().unwrap_or(0),
                    item["key"].as_str().unwrap_or(""),
                );
                match (subject, key) {
                    (0, "default.audio.sink") => m.default_output = name_value(&item["value"]),
                    (0, "default.audio.source") => m.default_input = name_value(&item["value"]),
                    (id, "target.object") if id != 0 => {
                        if let Some(t) = plain_value(&item["value"]) {
                            targets.insert(id, t);
                        }
                    }
                    _ => {}
                }
            }
        }
        // What each stream is connected to: playback streams link into a sink, recording
        // streams are linked from a source.
        let mut linked: HashMap<u64, u64> = HashMap::new();
        for o in self.of_type("Link") {
            let info = &o["info"];
            if let (Some(from), Some(to)) = (info["output-node-id"].as_u64(), info["input-node-id"].as_u64()) {
                linked.entry(from).or_insert(to);
                linked.entry(to).or_insert(from);
            }
        }
        let cards: HashMap<u64, &Value> = self
            .of_type("Device")
            .filter(|o| o["info"]["props"]["media.class"].as_str() == Some("Audio/Device"))
            .filter_map(|o| Some((o["id"].as_u64()?, o)))
            .collect();

        for o in self.of_type("Node") {
            let Some(id) = o["id"].as_u64() else { continue };
            let props = &o["info"]["props"];
            let s = |k: &str| props[k].as_str().unwrap_or("").to_string();
            let class = s("media.class");
            let (volume, muted) = volume(&o["info"]["params"]["Props"]);
            match class.as_str() {
                "Audio/Sink" | "Audio/Source" | "Audio/Source/Virtual" => {
                    let output = class == "Audio/Sink";
                    let card = props["device.id"].as_u64().and_then(|d| cards.get(&d));
                    let card_prop = |k: &str| card.and_then(|c| c["info"]["props"][k].as_str()).unwrap_or("");
                    let name = s("node.name");
                    let form = props["device.form-factor"]
                        .as_str()
                        .unwrap_or_else(|| card_prop("device.form-factor"));
                    let kind = if !output {
                        Kind::Microphone
                    } else if form == "headset" || form == "hands-free" {
                        Kind::Headset
                    } else if form == "headphone" || card_prop("device.bus") == "bluetooth" {
                        Kind::Headphones
                    } else if name.contains("hdmi") || name.contains("displayport") {
                        Kind::Display
                    } else {
                        Kind::Speakers
                    };
                    let label = [s("node.description"), s("node.nick"), name.clone()]
                        .into_iter()
                        .find(|l| !l.is_empty())
                        .unwrap_or_default();
                    let device = Device {
                        id: id as u32,
                        name,
                        label,
                        kind,
                        serial: props["object.serial"].as_u64().map(|n| n.to_string()),
                        volume,
                        muted,
                    };
                    if output {
                        m.outputs.push(device);
                    } else {
                        m.inputs.push(device);
                    }
                }
                "Stream/Output/Audio" | "Stream/Input/Audio" => {
                    // Level meters (like pavucontrol's) aren't apps making sound.
                    if props["stream.monitor"].as_bool() == Some(true) || s("stream.monitor") == "true" {
                        continue;
                    }
                    let app = [s("application.name"), s("node.description"), s("node.name")]
                        .into_iter()
                        .find(|l| !l.is_empty())
                        .unwrap_or_default();
                    let mut title = s("media.name");
                    if title == app
                        || title.eq_ignore_ascii_case("playback")
                        || title.eq_ignore_ascii_case("audio stream")
                    {
                        title.clear();
                    }
                    let mut icons: Vec<String> = [
                        "application.icon-name",
                        "pipewire.access.portal.app_id",
                        "application.id",
                        "application.process.binary",
                    ]
                    .iter()
                    .map(|k| s(k))
                    .filter(|v| !v.is_empty())
                    .collect();
                    icons.dedup();
                    let stream = Stream {
                        id: id as u32,
                        app,
                        title,
                        icons,
                        volume,
                        muted,
                        active: o["info"]["state"].as_str() == Some("running"),
                        target: targets
                            .get(&id)
                            .cloned()
                            .or_else(|| plain_value(&props["target.object"])),
                        device: linked.get(&id).map(|d| *d as u32),
                    };
                    if class == "Stream/Output/Audio" {
                        m.playing.push(stream);
                    } else {
                        m.recording.push(stream);
                    }
                }
                _ => {}
            }
        }
        for (id, o) in cards {
            let params = &o["info"]["params"];
            let profiles: Vec<Profile> = params["EnumProfile"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| {
                    Some(Profile {
                        index: p["index"].as_u64()? as u32,
                        label: p["description"].as_str().or(p["name"].as_str())?.to_string(),
                        available: p["available"].as_str() != Some("no"),
                    })
                })
                .collect();
            if profiles.is_empty() {
                continue;
            }
            let props = &o["info"]["props"];
            let label = ["device.description", "device.nick", "device.name"]
                .iter()
                .find_map(|k| props[k].as_str().filter(|v| !v.is_empty()))
                .unwrap_or("Sound card")
                .to_string();
            m.cards.push(Card {
                id: id as u32,
                label,
                profiles,
                current: params["Profile"][0]["index"].as_u64().map(|i| i as u32),
            });
        }
        let by_label = |a: &Device, b: &Device| a.label.to_lowercase().cmp(&b.label.to_lowercase());
        m.outputs.sort_by(by_label);
        m.inputs.sort_by(by_label);
        m.playing.sort_by_key(|s| s.id);
        m.recording.sort_by_key(|s| s.id);
        m.cards
            .sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
        m
    }

    fn of_type<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Value> {
        self.objects.values().filter(move |o| {
            o["type"]
                .as_str()
                .is_some_and(|t| t.strip_prefix("PipeWire:Interface:") == Some(kind))
        })
    }
}

/// Volume and mute from a node's Props, on wpctl's scale.
fn volume(props: &Value) -> (f32, bool) {
    let Some(p) = props
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p.get("channelVolumes").is_some())
    else {
        return (0.0, false);
    };
    let channels: Vec<f64> = p["channelVolumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .collect();
    let linear = if channels.is_empty() {
        p["volume"].as_f64().unwrap_or(0.0)
    } else {
        channels.iter().sum::<f64>() / channels.len() as f64
    };
    (linear.max(0.0).cbrt() as f32, p["mute"].as_bool().unwrap_or(false))
}

/// A default device's `{"name": "..."}` (sometimes still as a JSON string).
fn name_value(v: &Value) -> Option<String> {
    let parsed;
    let v = match v.as_str() {
        Some(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            &parsed
        }
        None => v,
    };
    v["name"].as_str().map(str::to_string)
}

/// A metadata value as a plain string (a name, or a number like a serial).
fn plain_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() && s != "-1" => Some(s.clone()),
        Value::Number(n) if n.as_i64() != Some(-1) => Some(n.to_string()),
        _ => None,
    }
}

impl Stream {
    /// Whether it's set to play on (or record from) `device`.
    pub fn targets(&self, device: &Device) -> bool {
        self.target
            .as_deref()
            .is_some_and(|t| t == device.name || device.serial.as_deref() == Some(t))
    }
}

/// A change to make.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A device's or app's volume, 1.0 = 100%.
    Volume(u32, f32),
    Mute(u32, bool),
    /// Make this output or input the default.
    Default(u32),
    /// Switch a card to a profile.
    Profile(u32, u32),
    /// Send an app to a device (by `node.name`), or back to following the default.
    Target(u32, Option<String>),
}

/// Makes changes on a worker thread, so a slow PipeWire never stalls the caller. A burst of
/// volume changes (dragging a slider) is applied as just the last one.
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<Command>,
}

impl Control {
    /// `failed` is called (on the worker thread) with a message when a change didn't work.
    pub fn new(failed: impl Fn(String) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let spawned = std::thread::Builder::new().name("sound-control".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut batch: Vec<Command> = std::iter::once(first).chain(rx.try_iter()).collect();
                // Only the last volume for each node counts.
                let mut seen = Vec::new();
                for i in (0..batch.len()).rev() {
                    if let Command::Volume(id, _) = batch[i] {
                        if seen.contains(&id) {
                            batch.remove(i);
                        } else {
                            seen.push(id);
                        }
                    }
                }
                for c in batch {
                    if let Err(e) = run(&c) {
                        failed(e);
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("sound control thread: {e}");
        }
        Self { tx }
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}

fn run(c: &Command) -> Result<(), String> {
    let args: Vec<String> = match c {
        Command::Volume(id, v) => vec![
            "wpctl".into(),
            "set-volume".into(),
            id.to_string(),
            format!("{:.4}", v.clamp(0.0, 1.5)),
        ],
        Command::Mute(id, on) => vec![
            "wpctl".into(),
            "set-mute".into(),
            id.to_string(),
            (*on as u8).to_string(),
        ],
        Command::Default(id) => vec!["wpctl".into(), "set-default".into(), id.to_string()],
        Command::Profile(card, index) => vec![
            "wpctl".into(),
            "set-profile".into(),
            card.to_string(),
            index.to_string(),
        ],
        Command::Target(id, Some(name)) => vec![
            "pw-metadata".into(),
            id.to_string(),
            "target.object".into(),
            name.clone(),
        ],
        Command::Target(id, None) => vec![
            "pw-metadata".into(),
            "--delete".into(),
            id.to_string(),
            "target.object".into(),
        ],
    };
    let out = Process::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run {}: {e}", args[0]))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        tracing::warn!("{}: {err}", args.join(" "));
        Err(if err.is_empty() {
            format!("{} failed", args[0])
        } else {
            err
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, class: &str, name: &str, vol: f64, extra: Value) -> Value {
        let mut props = serde_json::json!({ "media.class": class, "node.name": name, "node.description": name });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        serde_json::json!({
            "id": id, "type": "PipeWire:Interface:Node",
            "info": { "state": "running", "props": props,
                "params": { "Props": [ { "channelVolumes": [vol, vol], "mute": false } ] } }
        })
    }

    #[test]
    fn builds_and_updates() {
        let mut g = Graph::default();
        g.apply(vec![
            node(10, "Audio/Sink", "spk", 0.125, Value::Null),
            node(11, "Audio/Source", "mic", 1.0, Value::Null),
            node(12, "Audio/Sink/Internal", "hidden", 1.0, Value::Null),
            node(13, "Stream/Output/Audio", "ff", 1.0, serde_json::json!({"application.name": "Firefox", "media.name": "Song"})),
            node(14, "Stream/Input/Audio", "peak", 1.0, serde_json::json!({"stream.monitor": "true"})),
            serde_json::json!({ "id": 5, "type": "PipeWire:Interface:Metadata", "props": {"metadata.name": "default"},
                "metadata": [
                    {"subject": 0, "key": "default.audio.sink", "type": "Spa:String:JSON", "value": {"name": "spk"}},
                    {"subject": 0, "key": "default.audio.source", "type": "Spa:String:JSON", "value": "{\"name\":\"mic\"}"},
                    {"subject": 13, "key": "target.object", "type": "", "value": "spk"}
                ] }),
        ]);
        let m = g.model();
        assert_eq!(m.outputs.len(), 1);
        assert!((m.outputs[0].volume - 0.5).abs() < 1e-4, "cubic scale");
        assert_eq!(m.default_output().unwrap().id, 10);
        assert_eq!(m.default_input().unwrap().id, 11);
        assert_eq!(m.playing.len(), 1);
        assert_eq!(m.playing[0].app, "Firefox");
        assert_eq!(m.playing[0].target.as_deref(), Some("spk"));
        assert!(m.recording.is_empty());
        // A metadata update holds only what changed.
        g.apply(vec![
            serde_json::json!({ "id": 5, "type": "PipeWire:Interface:Metadata",
            "props": {"metadata.name": "default"},
            "metadata": [{"subject": 13, "key": "target.object", "type": null, "value": null}] }),
        ]);
        let m = g.model();
        assert_eq!(m.default_output().unwrap().id, 10, "defaults kept");
        assert_eq!(m.playing[0].target, None);
        g.apply(vec![serde_json::json!({"id": 13, "info": null})]);
        assert!(g.model().playing.is_empty());
    }
}
