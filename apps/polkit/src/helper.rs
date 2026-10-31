//! Talking to polkit's password checker, `polkit-agent-helper-1`.
//!
//! The helper runs as root, checks the password with PAM and tells polkitd the outcome itself,
//! so this program never decides who is allowed anything. Newer polkit runs it through a
//! systemd socket (`/run/polkit/agent-helper.socket`); older systems have it setuid root and
//! we start it ourselves. Either way the conversation is the same, one line at a time:
//!
//! - we send the cookie polkitd gave us (on the socket, the user name first);
//! - it sends `PAM_PROMPT_ECHO_OFF <prompt>` (or `_ON`), and we answer with one line;
//! - it may send `PAM_ERROR_MSG <text>` or `PAM_TEXT_INFO <text>` to show;
//! - it ends with `SUCCESS` or `FAILURE`.
//!
//! The helper's lines are escaped like GLib's `g_strescape`.

use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
};

use smithay_client_toolkit::reexports::calloop::channel::Sender;

const SOCKET: &str = "/run/polkit/agent-helper.socket";
const PROGRAMS: &[&str] = &[
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-agent-helper-1",
    "/usr/local/lib/polkit-1/polkit-agent-helper-1",
    "/usr/lib/policykit-1/polkit-agent-helper-1",
];

#[derive(Debug, PartialEq)]
pub enum Event {
    /// Wants an answer; `echo` when it isn't secret (like a user name).
    Prompt {
        text: String,
        echo: bool,
    },
    Error(String),
    Info(String),
    /// The check is over: the password was right, or not (or the helper failed).
    Done(bool),
}

/// One conversation with the helper. Dropping it ends the conversation.
pub struct Helper {
    /// Only None while being dropped.
    conn: Option<Conn>,
}

enum Conn {
    Socket(UnixStream),
    Child { child: Child, stdin: ChildStdin },
}

impl Helper {
    /// Start checking a password for `user` against `cookie`. Events arrive on `events`,
    /// tagged with `id`.
    pub fn start(user: &str, cookie: &str, id: u64, events: Sender<(u64, Event)>) -> Result<Self, String> {
        let (mut conn, reader): (Conn, Box<dyn Read + Send>) = if Path::new(SOCKET).exists() {
            match UnixStream::connect(SOCKET) {
                Ok(mut s) => {
                    let reader = s.try_clone().map_err(|e| e.to_string())?;
                    s.write_all(format!("{user}\n").as_bytes()).map_err(|e| e.to_string())?;
                    (Conn::Socket(s), Box::new(reader))
                }
                Err(e) => {
                    tracing::warn!("can't reach {SOCKET} ({e}); trying the setuid helper");
                    spawn(user)?
                }
            }
        } else {
            spawn(user)?
        };
        conn.writer()
            .write_all(format!("{cookie}\n").as_bytes())
            .map_err(|e| format!("talking to the password checker: {e}"))?;

        std::thread::Builder::new()
            .name("polkit-helper".into())
            .spawn(move || read_events(reader, id, events))
            .map_err(|e| e.to_string())?;
        Ok(Self { conn: Some(conn) })
    }

    /// Answer the last prompt.
    pub fn respond(&mut self, answer: &str) {
        // One buffer for the line, wiped after, so the password isn't left in freed memory.
        let mut line = Vec::with_capacity(answer.len() + 1);
        line.extend_from_slice(answer.as_bytes());
        line.push(b'\n');
        let sent = match &mut self.conn {
            Some(c) => c.writer().write_all(&line),
            None => Ok(()),
        };
        if let Err(e) = sent {
            tracing::warn!("sending the answer to the password checker: {e}");
        }
        for b in line.iter_mut() {
            // SAFETY: a valid &mut u8; volatile so the wipe isn't optimized away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl Conn {
    fn writer(&mut self) -> &mut dyn Write {
        match self {
            Conn::Socket(s) => s,
            Conn::Child { stdin, .. } => stdin,
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        match self.conn.take() {
            Some(Conn::Socket(s)) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            Some(Conn::Child { mut child, stdin }) => {
                // It runs as root, so it can't be killed; closing its input ends it instead.
                drop(stdin);
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            None => {}
        }
    }
}

fn spawn(user: &str) -> Result<(Conn, Box<dyn Read + Send>), String> {
    let program = PROGRAMS
        .iter()
        .find(|p| Path::new(p).is_file())
        .ok_or("polkit-agent-helper-1 isn't installed")?;
    let mut child = Command::new(program)
        .arg(user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting {program}: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    Ok((Conn::Child { child, stdin }, Box::new(stdout)))
}

fn read_events(reader: Box<dyn Read + Send>, id: u64, events: Sender<(u64, Event)>) {
    let mut done = false;
    for line in BufReader::new(reader).split(b'\n') {
        let Ok(line) = line else { break };
        let Some(event) = parse(&unescape(&line)) else { continue };
        done = matches!(event, Event::Done(_));
        if events.send((id, event)).is_err() || done {
            return;
        }
    }
    if !done {
        let _ = events.send((id, Event::Done(false)));
    }
}

/// One line from the helper; None for its own log messages, which come down the socket too.
fn parse(line: &str) -> Option<Event> {
    if line.starts_with("polkit-agent-helper-1:") {
        tracing::info!("{line}");
        return None;
    }
    let after = |prefix: &str| line.strip_prefix(prefix).map(|s| s.trim().to_string());
    Some(if let Some(text) = after("PAM_PROMPT_ECHO_OFF ") {
        Event::Prompt { text, echo: false }
    } else if let Some(text) = after("PAM_PROMPT_ECHO_ON ") {
        Event::Prompt { text, echo: true }
    } else if let Some(text) = after("PAM_ERROR_MSG ") {
        Event::Error(text)
    } else if let Some(text) = after("PAM_TEXT_INFO ") {
        Event::Info(text)
    } else if line.starts_with("SUCCESS") {
        Event::Done(true)
    } else {
        if !line.starts_with("FAILURE") {
            tracing::warn!("unexpected line from the password checker: {line:?}");
        }
        Event::Done(false)
    })
}

/// Undo `g_strescape`: `\n`, `\t`, `\\`, `\"`, ... and octal `\123`.
fn unescape(line: &[u8]) -> String {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let c = line[i];
        i += 1;
        if c != b'\\' || i >= line.len() {
            out.push(c);
            continue;
        }
        let e = line[i];
        i += 1;
        match e {
            b'0'..=b'7' => {
                let mut v = (e - b'0') as u32;
                for _ in 0..2 {
                    match line.get(i) {
                        Some(d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(
            parse("PAM_PROMPT_ECHO_OFF Password: "),
            Some(Event::Prompt {
                text: "Password:".into(),
                echo: false
            })
        );
        assert_eq!(
            parse("PAM_TEXT_INFO Swipe your finger"),
            Some(Event::Info("Swipe your finger".into()))
        );
        assert_eq!(parse("SUCCESS"), Some(Event::Done(true)));
        assert_eq!(parse("FAILURE"), Some(Event::Done(false)));
        assert_eq!(parse("garbage"), Some(Event::Done(false)));
        assert_eq!(
            parse("polkit-agent-helper-1: pam_authenticate failed: Authentication failure"),
            None
        );
    }

    #[test]
    fn unescaping() {
        assert_eq!(unescape(br"a\nb\\c\tq"), "a\nb\\c\tq");
        assert_eq!(unescape(br"caf\303\251"), "café");
        assert_eq!(unescape(br#"say \"hi\""#), "say \"hi\"");
    }
}
