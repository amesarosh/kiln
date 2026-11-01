//! Sound for the bar: the default output's volume and whether it's muted, followed live from
//! PipeWire (see `wt_pipewire::watch`), so the widget matches changes made anywhere (volume
//! keys, the Sound app, another mixer).

use smithay::reexports::calloop::channel;

use crate::state::State;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sound {
    /// There's an output to show (the widget only shows then).
    pub present: bool,
    /// 1.0 is 100%.
    pub volume: f32,
    pub muted: bool,
}

impl State {
    /// Start following PipeWire (once).
    pub fn start_sound(&mut self) {
        if std::mem::replace(&mut self.sound_started, true) {
            return;
        }
        let (tx, rx) = channel::channel::<Sound>();
        let inserted = self.loop_handle.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg(sound) = event
                && sound != state.sound
            {
                state.sound = sound;
                state.needs_redraw = true;
            }
        });
        if let Err(e) = inserted {
            return tracing::warn!("sound channel: {e}");
        }
        let mut last = None;
        wt_pipewire::watch(move |model| {
            let sound = model
                .as_ref()
                .and_then(|m| m.default_output())
                .map_or(Sound::default(), |d| Sound {
                    present: true,
                    volume: d.volume,
                    muted: d.muted,
                });
            if last == Some(sound) {
                return true;
            }
            last = Some(sound);
            tx.send(sound).is_ok()
        });
    }
}
