//! One error type for the whole engine. FFmpeg's `AVERROR` codes are kept
//! as-is inside `Ffmpeg` so callers can still tell EOF and EAGAIN apart —
//! both are normal control flow in a decode loop, not failures.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ffmpeg: {0}")]
    Ffmpeg(#[from] ffmpeg_next::Error),

    #[error("cannot open {path}: {source}")]
    Open { path: PathBuf, source: ffmpeg_next::Error },

    #[error("{0} has no stream this player can decode")]
    NoPlayableStream(PathBuf),

    #[error("no audio output device")]
    NoAudioDevice,

    #[error("audio device rejected the stream: {0}")]
    AudioStream(String),

    #[error("the playback engine stopped")]
    EngineGone,
}

pub type Result<T> = std::result::Result<T, Error>;

/// FFmpeg says "again" when a decoder needs more input before it can hand
/// back a frame, and "eof" when it is drained. Neither is an error.
pub fn is_again(e: &ffmpeg_next::Error) -> bool {
    matches!(e, ffmpeg_next::Error::Other { errno } if *errno == ffmpeg_next::util::error::EAGAIN)
}

pub fn is_eof(e: &ffmpeg_next::Error) -> bool {
    matches!(e, ffmpeg_next::Error::Eof)
}
