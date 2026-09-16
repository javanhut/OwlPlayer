//! OwlPlayer's playback engine.
//!
//! The rule this crate exists to enforce: **nothing here knows about GTK,
//! and nothing here blocks the UI thread.** The app talks to it by sending
//! [`Command`]s and reading [`Event`]s, and pulls pictures out of it with
//! [`Player::frame_for_now`] on the render tick. Everything expensive —
//! demuxing, decoding, resampling — happens on threads this crate owns.
//!
//! The thread layout, and why:
//!
//! ```text
//!   demux ──packets──▶ video decode ──frames──▶ [queue] ──▶ GL thread
//!     │                                                      (presents)
//!     └────packets──▶ audio decode ──samples──▶ [ring] ──▶ sound card
//!                                                            (master clock)
//! ```
//!
//! Demuxing is one thread because an `AVFormatContext` is not thread safe
//! and seeking needs exclusive use of it. Video and audio decode are
//! separate threads so a slow video frame cannot starve the sound card,
//! which is the one consumer in the system that must never be late.

pub mod clock;
pub mod error;
pub mod frame;
pub mod media;
pub mod ring;
pub mod subtitle;

mod audio;
mod engine;
mod hwaccel;
mod player;

pub use clock::Clock;
pub use error::{Error, Result};
pub use frame::{ColorInfo, PixelLayout, PlaneRef, VideoFrame};
pub use hwaccel::HwAccel;
pub use media::{Chapter, MediaInfo, Track, TrackKind, probe};
pub use subtitle::{Alignment, BitmapRect, SubtitleContent, SubtitleCue};
pub use engine::Selection;
pub use player::{Event, Player, State, TrackPreference};

/// Initialise FFmpeg once per process. Safe to call repeatedly.
pub fn init() -> Result<()> {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    static mut RESULT: Option<ffmpeg_next::Error> = None;
    ONCE.call_once(|| {
        // Quiet FFmpeg's own logging; anything worth surfacing comes back
        // as an `Event::Error` with context the user can act on.
        ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
        if let Err(e) = ffmpeg_next::init() {
            unsafe { RESULT = Some(e) }
        }
    });
    match unsafe { (*std::ptr::addr_of!(RESULT)).clone() } {
        Some(e) => Err(Error::Ffmpeg(e)),
        None => Ok(()),
    }
}
