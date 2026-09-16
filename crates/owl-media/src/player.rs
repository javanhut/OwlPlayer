//! The handle the app holds.
//!
//! Every method here returns immediately. Opening a file, seeking and
//! switching tracks all hand work to the engine threads and come back at
//! once, because all three of them are called from GTK's main loop and a
//! main loop that blocks is a window that stops repainting.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::clock::Clock;
use crate::engine::{self, DemuxCommand, Handles, Selection, Shared};
use crate::error::Result;
use crate::frame::VideoFrame;
use crate::media::{MediaInfo, TrackKind};
use crate::subtitle::SubtitleCue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing open.
    Idle,
    Playing,
    Paused,
    /// Reached the end and stayed there, so the UI can show a replay
    /// affordance rather than snapping back to Idle.
    Ended,
}

/// Things the engine tells the app about. Position is deliberately absent:
/// the app reads it from the clock on the render tick, which is both
/// cheaper and more accurate than an event stream could be.
#[derive(Debug, Clone)]
pub enum Event {
    Opened(Box<MediaInfo>),
    Seeked(f64),
    EndOfFile,
    Error(String),
}

/// Retained across an `open` so a newly opened file keeps the last
/// choice the viewer made, which is what they expect from episode to
/// episode of the same show.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackPreference {
    pub audio: Option<usize>,
    pub subtitle: Option<usize>,
}

struct Session {
    commands: Sender<DemuxCommand>,
    handles: Option<Handles>,
    info: MediaInfo,
    selection: Selection,
}

pub struct Player {
    clock: Arc<Clock>,
    shared: Arc<Shared>,
    session: Option<Session>,
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,
    state: State,
    volume: f64,
    muted: bool,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    pub fn new() -> Player {
        let clock = Clock::new();
        let shared = Shared::new(Arc::clone(&clock));
        let (events_tx, events_rx) = unbounded();
        Player { clock, shared, session: None, events_tx, events_rx, state: State::Idle, volume: 1.0, muted: false }
    }

    /// Drain what the engine has said since the last call. The app does
    /// this once per frame; nothing here blocks.
    pub fn poll_events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.events_rx.try_recv() {
            if matches!(e, Event::EndOfFile) {
                // Stop the clock as well as the state. Without this it
                // keeps counting past the end of the file and the transport
                // shows a position longer than the duration.
                self.state = State::Ended;
                self.shared.paused.store(true, Ordering::SeqCst);
                self.clock.set_paused(true);
            }
            out.push(e);
        }
        out
    }

    pub fn open(&mut self, path: &Path, prefer: TrackPreference) -> Result<MediaInfo> {
        self.close();

        // A fresh clock and a fresh shared state per file: carrying either
        // across would leave the new stream chasing the old one's PTS.
        self.clock = Clock::new();
        self.shared = Shared::new(Arc::clone(&self.clock));
        self.shared.set_volume(if self.muted { 0.0 } else { self.volume });

        let (tx, rx) = unbounded();
        let wanted = Selection { video: None, audio: prefer.audio, subtitle: prefer.subtitle };
        let explicit_subtitle = prefer.subtitle.is_some();
        let (info, selection, handles) = engine::start(
            path.to_path_buf(),
            wanted,
            Arc::clone(&self.shared),
            self.events_tx.clone(),
            rx,
        )?;

        self.session = Some(Session { commands: tx, handles: Some(handles), info: info.clone(), selection });
        self.state = State::Paused;
        self.clock.reset_to(0.0);
        let _ = self.events_tx.send(Event::Opened(Box::new(info.clone())));

        // Forced subtitles carry the signs and the foreign-language lines
        // of an otherwise-understood soundtrack, so they are turned on
        // without being asked for. A merely "default" track is not: that
        // disposition is set on full subtitle tracks all the time, and
        // switching them on unbidden is the wrong kind of surprise.
        //
        // The index is taken before the call because `set_track` reopens
        // the file, and it cannot recurse: it passes the track explicitly,
        // which sets `explicit_subtitle` on the way back through here.
        if !explicit_subtitle && selection.subtitle.is_none() {
            let forced = info.tracks_of(TrackKind::Subtitle).find(|t| t.is_forced).map(|t| t.index);
            if let Some(index) = forced {
                log::info!("enabling forced subtitle track {index}");
                let _ = self.set_track(TrackKind::Subtitle, Some(index));
            }
        }

        Ok(info)
    }

    /// Stop playback and wind the threads down. Safe to call when nothing
    /// is open.
    pub fn close(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.paused.store(true, Ordering::SeqCst);
        self.shared.clear_video();
        if let Some(mut session) = self.session.take() {
            let _ = session.commands.send(DemuxCommand::Stop);
            drop(session.commands);
            if let Some(handles) = session.handles.take() {
                // The threads all watch `stop` and none of them park
                // indefinitely, so these joins are bounded.
                let _ = handles.demux.join();
                if let Some(h) = handles.video {
                    let _ = h.join();
                }
                if let Some(h) = handles.audio {
                    let _ = h.join();
                }
                if let Some(h) = handles.subtitle {
                    let _ = h.join();
                }
            }
        }
        self.state = State::Idle;
    }

    pub fn play(&mut self) {
        if self.session.is_none() {
            return;
        }
        if self.state == State::Ended {
            self.seek(0.0);
        }
        self.shared.paused.store(false, Ordering::SeqCst);
        self.clock.set_paused(false);
        self.state = State::Playing;
    }

    pub fn pause(&mut self) {
        if self.session.is_none() {
            return;
        }
        self.shared.paused.store(true, Ordering::SeqCst);
        self.clock.set_paused(true);
        self.state = State::Paused;
    }

    pub fn toggle(&mut self) {
        match self.state {
            State::Playing => self.pause(),
            State::Paused | State::Ended => self.play(),
            State::Idle => {}
        }
    }

    pub fn seek(&mut self, seconds: f64) {
        let Some(session) = &self.session else { return };
        let target = seconds.clamp(0.0, session.info.duration.max(0.0));
        let _ = session.commands.send(DemuxCommand::Seek(target));
        if self.state == State::Ended {
            self.state = State::Paused;
        }
    }

    /// Relative seek, used by the ±10 second buttons and the arrow keys.
    pub fn seek_by(&mut self, delta: f64) {
        let now = self.position();
        self.seek(now + delta);
    }

    pub fn position(&self) -> f64 {
        let Some(session) = &self.session else { return 0.0 };
        let now = self.clock.now().max(0.0);
        // The audio clock can read slightly past the last sample, and a
        // file whose container duration is a rounding under the real one
        // would otherwise show 1:02 of a 1:01 film.
        if session.info.duration > 0.0 { now.min(session.info.duration) } else { now }
    }

    pub fn duration(&self) -> f64 {
        self.session.as_ref().map(|s| s.info.duration).unwrap_or(0.0)
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn info(&self) -> Option<&MediaInfo> {
        self.session.as_ref().map(|s| &s.info)
    }

    pub fn selection(&self) -> Option<Selection> {
        self.session.as_ref().map(|s| s.selection)
    }

    pub fn volume(&self) -> f64 {
        self.volume
    }

    pub fn set_volume(&mut self, v: f64) {
        self.volume = v.clamp(0.0, 1.0);
        self.muted = false;
        self.shared.set_volume(self.volume);
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.shared.set_volume(if muted { 0.0 } else { self.volume });
    }

    /// Switch audio or subtitle track.
    ///
    /// This reopens the file at the current position rather than swapping
    /// a decoder in place. Rebuilding one decoder while the demuxer keeps
    /// running is a worthwhile optimisation, but it is also where players
    /// grow their subtlest desync bugs, and a track change is a deliberate
    /// action that nobody expects to be instant.
    pub fn set_track(&mut self, kind: TrackKind, index: Option<usize>) -> Result<()> {
        let Some(session) = &self.session else { return Ok(()) };
        let path = session.info.path.clone();
        let mut prefer = TrackPreference {
            audio: session.selection.audio,
            subtitle: session.selection.subtitle,
        };
        match kind {
            TrackKind::Audio => prefer.audio = index,
            TrackKind::Subtitle => prefer.subtitle = index,
            TrackKind::Video => return Ok(()),
        }

        let resume = self.position();
        let was_playing = self.state == State::Playing;
        self.open(&path, prefer)?;
        if resume > 0.5 {
            self.seek(resume);
        }
        if was_playing {
            self.play();
        }
        Ok(())
    }

    /// The subtitles visible now, but only when they differ from what the
    /// caller last saw — `None` means "keep showing what you have".
    ///
    /// Pass back the generation from the previous call. Bitmap cues are
    /// whole images, so this exists to avoid copying one every frame to
    /// display the same thing.
    pub fn subtitles_if_changed(&self, last_seen: u64) -> Option<(u64, Vec<SubtitleCue>)> {
        self.shared.subtitles_if_changed(self.clock.now(), last_seen)
    }

    /// Which subtitle stream is playing, if any.
    pub fn subtitle_track(&self) -> Option<usize> {
        self.session.as_ref().and_then(|s| s.selection.subtitle)
    }

    /// The picture to draw right now, or `None` to keep the last one.
    /// Called from the GL thread on every frame.
    pub fn frame_for_now(&self) -> Option<VideoFrame> {
        self.shared.frame_for_now()
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.session.as_ref().map(|s| s.info.path.clone())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.close();
    }
}
