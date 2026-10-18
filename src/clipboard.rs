//! Keeps the clipboard after the app you copied from closes (`keep_clipboard` in `[general]`).
//!
//! On Wayland the app that copied owns the data, so normally closing it empties the clipboard.
//! Here, after each copy, every format the app offers is read into memory in the background.
//! Once that's done the compositor takes the clipboard over and serves its own copy, like
//! `wl-clip-persist` does. Only the regular clipboard is kept; the middle-click selection is
//! meant to be fleeting. Copies too big to hold, or apps too slow to hand them over, are left
//! with the app as before.

use std::{
    io::Write,
    os::fd::{AsFd, OwnedFd},
    sync::Arc,
    time::Duration,
};

use smithay::{
    reexports::calloop::{
        Interest, Mode, PostAction, RegistrationToken,
        generic::Generic,
        timer::{TimeoutAction, Timer},
    },
    wayland::selection::{
        SelectionTarget,
        data_device::{
            current_data_device_selection_userdata, request_data_device_client_selection, set_data_device_selection,
        },
    },
};

use crate::state::State;

/// Largest copy kept, all formats together.
const MAX_BYTES: usize = 64 << 20;
/// How long an app gets to hand over every format.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// X11 bookkeeping targets, not data.
const NOT_DATA: &[&str] = &["TARGETS", "TIMESTAMP", "MULTIPLE", "SAVE_TARGETS", "DELETE"];

/// A kept clipboard: each format and its bytes.
pub type Kept = Arc<Vec<(String, Arc<[u8]>)>>;

/// Attached to clipboards the compositor offers itself.
#[derive(Debug, Clone)]
pub enum SelectionData {
    /// An X11 app owns it; Wayland apps read it through Xwayland.
    X11,
    /// Our own copy, one entry per format.
    Kept(Kept),
}

#[derive(Default)]
pub struct Keeper {
    /// Bumped on every copy, so reads for an older one are dropped.
    generation: u64,
    reading: Option<Reading>,
}

struct Reading {
    generation: u64,
    /// One slot per format, filled as each finishes.
    formats: Vec<(String, Option<Vec<u8>>)>,
    bytes: usize,
    tokens: Vec<RegistrationToken>,
}

impl State {
    /// Something new was copied (by an X11 app when `from_x11`). Start keeping a copy once the
    /// new clipboard is in place, which happens after this call returns.
    pub fn keep_clipboard(&mut self, mime_types: Vec<String>, from_x11: bool) {
        self.stop_keeping();
        if !self.config.general.keep_clipboard {
            return;
        }
        let generation = self.clipboard.generation;
        let formats: Vec<String> = mime_types
            .into_iter()
            .filter(|m| !NOT_DATA.contains(&m.as_str()))
            .collect();
        if formats.is_empty() {
            return;
        }
        self.loop_handle.insert_idle(move |state| {
            if state.clipboard.generation == generation {
                state.start_reading(generation, formats, from_x11);
            }
        });
    }

    /// The clipboard was cleared or replaced: drop any copy in progress.
    pub fn stop_keeping(&mut self) {
        self.clipboard.generation += 1;
        if let Some(r) = self.clipboard.reading.take() {
            for t in r.tokens {
                self.loop_handle.remove(t);
            }
        }
    }

    fn start_reading(&mut self, generation: u64, formats: Vec<String>, from_x11: bool) {
        let mut tokens = Vec::new();
        for (i, mime) in formats.iter().enumerate() {
            let (read, write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("keep clipboard: no pipe: {e}");
                    return self.give_up(tokens);
                }
            };
            // Only our end is non-blocking; the app writes as it normally would.
            if rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK).is_err() {
                return self.give_up(tokens);
            }
            let requested = if from_x11 {
                match self.xwm.as_mut() {
                    Some(xwm) => xwm
                        .send_selection(
                            SelectionTarget::Clipboard,
                            mime.clone(),
                            write,
                            self.loop_handle.clone(),
                        )
                        .map_err(|e| format!("{e:?}")),
                    None => Err("Xwayland is gone".into()),
                }
            } else {
                request_data_device_client_selection(&self.seat, mime.clone(), write).map_err(|e| format!("{e:?}"))
            };
            if let Err(e) = requested {
                tracing::debug!("keep clipboard: can't read {mime}: {e}");
                return self.give_up(tokens);
            }
            let mut buf = Vec::new();
            let source = Generic::new(read, Interest::READ, Mode::Level);
            let token = self.loop_handle.insert_source(source, move |_, fd, state| {
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    match rustix::io::read(fd.as_fd(), &mut chunk) {
                        Ok(0) => {
                            state.format_read(generation, i, std::mem::take(&mut buf));
                            return Ok(PostAction::Remove);
                        }
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if !state.count_bytes(generation, n) {
                                return Ok(PostAction::Remove);
                            }
                        }
                        Err(rustix::io::Errno::AGAIN) => return Ok(PostAction::Continue),
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(e) => {
                            tracing::debug!("keep clipboard: reading failed: {e}");
                            state.stop_keeping();
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            });
            match token {
                Ok(t) => tokens.push(t),
                Err(e) => {
                    tracing::warn!("keep clipboard: {e}");
                    return self.give_up(tokens);
                }
            }
        }
        let timer = self
            .loop_handle
            .insert_source(Timer::from_duration(READ_TIMEOUT), move |_, _, state| {
                if state
                    .clipboard
                    .reading
                    .as_ref()
                    .is_some_and(|r| r.generation == generation)
                {
                    tracing::debug!("keep clipboard: the app took too long; leaving the clipboard with it");
                    state.stop_keeping();
                }
                TimeoutAction::Drop
            });
        if let Ok(t) = timer {
            tokens.push(t);
        }
        // Apps only see the requests once we flush.
        let _ = self.display_handle.flush_clients();
        self.clipboard.reading = Some(Reading {
            generation,
            formats: formats.into_iter().map(|m| (m, None)).collect(),
            bytes: 0,
            tokens,
        });
    }

    fn give_up(&mut self, tokens: Vec<RegistrationToken>) {
        for t in tokens {
            self.loop_handle.remove(t);
        }
    }

    /// Returns false (and gives up) once the copy is too big to keep.
    fn count_bytes(&mut self, generation: u64, n: usize) -> bool {
        let Some(r) = self.clipboard.reading.as_mut().filter(|r| r.generation == generation) else {
            return false;
        };
        r.bytes += n;
        if r.bytes > MAX_BYTES {
            tracing::debug!("keep clipboard: over {} MiB, leaving it with the app", MAX_BYTES >> 20);
            self.stop_keeping();
            return false;
        }
        true
    }

    fn format_read(&mut self, generation: u64, index: usize, data: Vec<u8>) {
        let Some(r) = self.clipboard.reading.as_mut().filter(|r| r.generation == generation) else {
            return;
        };
        r.formats[index].1 = Some(data);
        if r.formats.iter().any(|(_, d)| d.is_none()) {
            return;
        }
        let r = self.clipboard.reading.take().unwrap();
        // The read sources removed themselves; only the timer is left.
        for t in r.tokens {
            self.loop_handle.remove(t);
        }
        let kept: Vec<(String, Arc<[u8]>)> = r
            .formats
            .into_iter()
            .map(|(m, d)| (m, Arc::from(d.unwrap_or_default())))
            .collect();
        let mime_types = kept.iter().map(|(m, _)| m.clone()).collect();
        tracing::debug!("keep clipboard: took over {} bytes in {} formats", r.bytes, kept.len());
        let dh = self.display_handle.clone();
        set_data_device_selection(&dh, &self.seat, mime_types, SelectionData::Kept(Arc::new(kept)));
    }

    /// The clipboard is our own copy right now.
    pub fn kept_clipboard(&self) -> Option<Kept> {
        match current_data_device_selection_userdata(&self.seat).as_deref() {
            Some(SelectionData::Kept(k)) => Some(k.clone()),
            _ => None,
        }
    }
}

/// Write one format of a kept clipboard to an app that's pasting. Done on a thread, since the
/// app may read slowly and the compositor mustn't wait for it.
pub fn serve_kept(kept: &[(String, Arc<[u8]>)], mime_type: &str, fd: OwnedFd) {
    let Some((_, data)) = kept.iter().find(|(m, _)| m == mime_type) else {
        return;
    };
    let data = data.clone();
    // The requesting app may have made its end non-blocking; a thread can simply wait.
    let _ = rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::empty());
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        if let Err(e) = file.write_all(&data) {
            tracing::debug!("paste of kept clipboard cut short: {e}");
        }
    });
}
