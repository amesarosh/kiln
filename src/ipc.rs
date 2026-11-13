//! `kiln msg <action>`: run an action in the running compositor from a script.
//!
//! The compositor listens on `$XDG_RUNTIME_DIR/kiln.<WAYLAND_DISPLAY>.sock` (the path
//! is also in `$KILN_SOCKET` for programs it starts). A client sends one line, an action
//! written as in `[binds]`, and gets back one line: `ok` or `error: ...`.
//!
//! A thread accepts connections and reads the line, so a slow client never holds up the
//! desktop; the action itself runs on the main loop.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    time::Duration,
};

use smithay::reexports::calloop::channel;

use crate::state::State;

/// Where the socket for a Wayland display lives.
pub fn socket_path(wayland_display: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(dir).join(format!("kiln.{wayland_display}.sock")))
}

impl State {
    /// Start listening for `msg` commands. Returns the socket path, so it can be removed at exit.
    pub fn start_ipc(&mut self) -> Option<PathBuf> {
        let path = socket_path(&self.socket_name.to_string_lossy())?;
        // A socket left behind by a crashed run would block the new one.
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("msg socket {}: {e}", path.display());
                return None;
            }
        };
        let (tx, rx) = channel::channel::<(String, UnixStream)>();
        let inserted = self.loop_handle.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg((line, mut stream)) = event {
                let reply = state.run_command(line.trim());
                let _ = writeln!(stream, "{reply}");
            }
        });
        if let Err(e) = inserted {
            tracing::warn!("msg socket: {e}");
            return None;
        }
        let spawned = std::thread::Builder::new().name("ipc".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                // One short-lived thread per command, so one stuck client can't block others.
                let _ = std::thread::Builder::new().name("ipc-client".into()).spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    let Ok(reader) = stream.try_clone() else { return };
                    if BufReader::new(std::io::Read::take(reader, 4096)).read_line(&mut line).is_ok() && !line.trim().is_empty() {
                        let _ = tx.send((line, stream));
                    }
                });
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("msg socket thread: {e}");
            return None;
        }
        tracing::info!("listening for `kiln msg` on {}", path.display());
        Some(path)
    }

    /// Run one command line from a client, returning the reply.
    fn run_command(&mut self, line: &str) -> String {
        if let Some(test) = line.strip_prefix("test-") {
            return self.run_test_command(test).unwrap_or_else(|e| format!("error: {e}"));
        }
        match crate::config::parse_action(line) {
            Ok(action) => {
                self.run_action(action);
                "ok".into()
            }
            Err(e) => format!("error: {e}"),
        }
    }
}

impl State {
    /// Fake input, for driving a headless session in tests:
    ///   test-pointer X Y          move the pointer to a global position
    ///   test-click [BUTTON]       press and release (272 left, 273 right, 274 middle)
    ///   test-press [BUTTON] / test-release [BUTTON]
    ///   test-key CODE             press and release an evdev key code (1 is Escape)
    ///   test-bar                  list the focused bar's parts and where they are
    fn run_test_command(&mut self, cmd: &str) -> Result<String, String> {
        use smithay::backend::input::{ButtonState, KeyState};
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let num = |p: Option<&str>, default: f64| -> Result<f64, String> {
            p.map_or(Ok(default), |s| {
                s.parse().map_err(|_| format!("\"{s}\" is not a number"))
            })
        };
        let time = self.start_time.elapsed().as_millis() as u32;
        // Fake input counts as activity, like real input does.
        if matches!(name, "pointer" | "press" | "click" | "key" | "keydown" | "keyup") {
            let seat = self.seat.clone();
            self.idle_notifier.notify_activity(&seat);
            self.note_activity(true);
        }
        match name {
            "pointer" => {
                let (x, y) = (num(parts.next(), 0.0)?, num(parts.next(), 0.0)?);
                self.on_pointer_moved((x, y).into(), time, None);
            }
            "press" | "release" | "click" => {
                let button = num(parts.next(), 272.0)? as u32;
                if name != "release" {
                    self.on_button_raw(button, ButtonState::Pressed, time);
                }
                if name != "press" {
                    self.on_button_raw(button, ButtonState::Released, time);
                }
            }
            "key" => {
                let code = num(parts.next(), 1.0)? as u32 + 8;
                self.on_key_raw(code.into(), KeyState::Pressed, time);
                self.on_key_raw(code.into(), KeyState::Released, time);
            }
            // Hold a key (a modifier, say) down, or let it go, to test combinations.
            "keydown" | "keyup" => {
                let code = num(parts.next(), 1.0)? as u32 + 8;
                let st = if name == "keydown" {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                self.on_key_raw(code.into(), st, time);
            }
            // Which monitors would flip without vsync now (a fullscreen game asked to tear).
            "tearing" => {
                let outputs: Vec<_> = self.monitors.iter().map(|m| m.output.clone()).collect();
                let lines: Vec<String> = outputs
                    .iter()
                    .map(|o| format!("{} {}", o.name(), self.output_wants_tearing(o)))
                    .collect();
                return Ok(lines.join("; "));
            }
            // Save what a headless output last showed, as a monitor would get it.
            "dump" => {
                let (Some(name), Some(path)) = (parts.next(), parts.next()) else {
                    return Err("usage: test-dump <output> <file.ppm>".into());
                };
                crate::backend::headless::dump(self, name, path)?;
            }
            // Where everything on the focused monitor's bar is, one "part x y w h" per line.
            "bar" => return Ok(self.bar_parts_debug().join("; ")),
            _ => return Err(format!("unknown test command \"{name}\"")),
        }
        Ok("ok".into())
    }
}

/// The client side: send `action` to the compositor this shell's `$WAYLAND_DISPLAY` belongs to.
pub fn send(action: &str) -> Result<String, String> {
    let path = std::env::var_os("KILN_SOCKET")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| socket_path(&std::env::var("WAYLAND_DISPLAY").ok()?))
        .ok_or("can't find the compositor: WAYLAND_DISPLAY and KILN_SOCKET are unset")?;
    let mut stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    writeln!(stream, "{action}").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).map_err(|e| e.to_string())?;
    Ok(reply.trim().to_string())
}
