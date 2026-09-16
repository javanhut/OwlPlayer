//! The playback clock.
//!
//! Audio is the master. A sound card consumes samples at a rate the
//! program cannot change, so the only way to stay in sync with it is to
//! treat "how much audio has actually been played" as the definition of
//! now, and fit video to that. Dropping or repeating a video frame costs
//! one frame of judder; resampling audio to fit a video clock is audible
//! immediately, which is why it is never done that way round.
//!
//! With no audio track (a silent file, or audio disabled) the clock falls
//! back to the monotonic system clock, started when playback started.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// Seconds, as a bit pattern, so the clock is lock-free: the audio
/// callback writes it from a realtime thread and the GTK thread reads it
/// every frame. A mutex between those two is exactly the thing that
/// causes an audio dropout.
#[derive(Debug)]
pub struct Clock {
    /// Last PTS handed to the audio device, in seconds.
    audio_pts: AtomicU64,
    /// Set once audio has actually started flowing; until then the
    /// monotonic fallback drives playback.
    has_audio: AtomicBool,
    paused: AtomicBool,
    /// Monotonic fallback: the instant playback started, and the media
    /// position it started from.
    start: parking_lot::Mutex<(Instant, f64)>,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            audio_pts: AtomicU64::new(0),
            has_audio: AtomicBool::new(false),
            paused: AtomicBool::new(true),
            start: parking_lot::Mutex::new((Instant::now(), 0.0)),
        }
    }
}

impl Clock {
    pub fn new() -> Arc<Clock> {
        Arc::new(Clock::default())
    }

    /// Called from the audio callback with the PTS of the sample about to
    /// leave for the device, minus whatever is still queued in it.
    pub fn set_audio_pts(&self, seconds: f64) {
        self.audio_pts.store(seconds.to_bits(), Ordering::Relaxed);
        self.has_audio.store(true, Ordering::Relaxed);
    }

    /// Where playback is now, in seconds.
    pub fn now(&self) -> f64 {
        if self.has_audio.load(Ordering::Relaxed) {
            return f64::from_bits(self.audio_pts.load(Ordering::Relaxed));
        }
        let (start, base) = *self.start.lock();
        if self.paused.load(Ordering::Relaxed) { base } else { base + start.elapsed().as_secs_f64() }
    }

    pub fn set_paused(&self, paused: bool) {
        if paused == self.paused.load(Ordering::Relaxed) {
            return;
        }
        // Freeze the fallback clock at the current position, so unpausing
        // resumes from here rather than jumping by the pause duration.
        let now = self.now();
        *self.start.lock() = (Instant::now(), now);
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// After a seek both clocks restart at the new position, and the audio
    /// clock is disarmed until the device reports a sample from after the
    /// seek — otherwise video would chase a stale PTS for one buffer.
    pub fn reset_to(&self, seconds: f64) {
        self.audio_pts.store(seconds.to_bits(), Ordering::Relaxed);
        self.has_audio.store(false, Ordering::Relaxed);
        *self.start.lock() = (Instant::now(), seconds);
    }
}
