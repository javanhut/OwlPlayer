//! What is inside a file: streams, tracks, chapters, and the labels the
//! UI puts on them.

use std::path::{Path, PathBuf};

use ffmpeg_next as ff;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// One selectable stream. `index` is the FFmpeg stream index, which is
/// what the engine is told to switch to — not a position in a filtered
/// list, because those renumber as soon as a track is hidden.
#[derive(Debug, Clone)]
pub struct Track {
    pub index: usize,
    pub kind: TrackKind,
    /// FFmpeg's name for the codec ("h264", "pcm_s16le"), not the
    /// decoder's ("mp3float", "libdav1d"). `codec_label` makes it readable.
    pub codec: String,
    /// ISO 639 code from the container, e.g. "eng". Absent in plenty of
    /// real files, which is why the UI falls back to "Track N".
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    /// Forced subtitles carry the signs-and-songs for a dubbed track and
    /// should be switched on even when the viewer wants no subtitles.
    pub is_forced: bool,
    /// Audio only.
    pub channels: u16,
    pub sample_rate: u32,
    /// Video only.
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    /// "HDR10" or "HLG" when the transfer function says so. Read from the
    /// codec parameters at probe time, so a queue row can say a file is
    /// HDR without decoding a frame of it.
    pub hdr: Option<&'static str>,
}

impl Track {
    /// The one-line label for a track menu.
    pub fn label(&self) -> String {
        if let Some(t) = &self.title {
            return t.clone();
        }
        let lang = self.language.as_deref().map(language_name).unwrap_or("Unknown");
        match self.kind {
            TrackKind::Audio => {
                let layout = match self.channels {
                    1 => "Mono".to_string(),
                    2 => "Stereo".to_string(),
                    6 => "5.1".to_string(),
                    8 => "7.1".to_string(),
                    n => format!("{n} ch"),
                };
                format!("{lang} · {layout} · {}", codec_label(&self.codec))
            }
            TrackKind::Subtitle => {
                if self.is_forced { format!("{lang} (Forced)") } else { lang.to_string() }
            }
            TrackKind::Video => format!("{}×{} · {}", self.width, self.height, codec_label(&self.codec)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub title: String,
    pub start: f64,
    pub end: f64,
}

/// Everything the UI needs to describe a file without decoding it.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub title: Option<String>,
    pub duration: f64,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub container: String,
    pub bit_rate: i64,
}

impl MediaInfo {
    /// True when there is nothing to show — a music file, or one whose
    /// only video stream is cover art, which `probe` already discards.
    pub fn is_audio_only(&self) -> bool {
        self.tracks_of(TrackKind::Video).next().is_none()
            && self.tracks_of(TrackKind::Audio).next().is_some()
    }

    pub fn tracks_of(&self, kind: TrackKind) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(move |t| t.kind == kind)
    }

    /// The title to show: container metadata if the file carries it,
    /// otherwise a tidied filename, because "A World of Stillness" reads
    /// better than "a.world.of.stillness.2024.2160p.mkv".
    pub fn display_title(&self) -> String {
        if let Some(t) = &self.title {
            if !t.trim().is_empty() {
                return t.clone();
            }
        }
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().replace(['.', '_'], " "))
            .unwrap_or_else(|| "Unknown".into())
    }

    /// The chips beside the title: resolution class, HDR, and the headline
    /// audio format. Derived, never stored, so it always matches the file.
    pub fn chips(&self) -> Vec<String> {
        let mut chips = Vec::new();
        if let Some(v) = self.tracks_of(TrackKind::Video).next() {
            let resolution = match v.height {
                h if h >= 4320 => "8K".to_string(),
                h if h >= 2160 => "4K".to_string(),
                h if h >= 1440 => "1440p".to_string(),
                h if h >= 1080 => "1080p".to_string(),
                h if h >= 720 => "720p".to_string(),
                h => format!("{h}p"),
            };
            // Resolution and dynamic range read as one fact about the
            // picture, so they share a chip: "4K HDR10", not "4K" "HDR10".
            chips.push(match v.hdr {
                Some(hdr) => format!("{resolution} {hdr}"),
                None => resolution,
            });
        }
        if let Some(a) = self.tracks_of(TrackKind::Audio).next() {
            chips.push(codec_label(&a.codec));
        }
        chips
    }
}

/// Read a file's structure without starting playback. Used for the queue,
/// where six files have to show a duration before any of them is opened.
pub fn probe(path: &Path) -> Result<MediaInfo> {
    crate::init()?;
    let input = ff::format::input(path).map_err(|source| Error::Open { path: path.to_path_buf(), source })?;
    Ok(describe(path, &input))
}

pub(crate) fn describe(path: &Path, input: &ff::format::context::Input) -> MediaInfo {
    let meta = input.metadata();
    let duration = if input.duration() > 0 {
        input.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE)
    } else {
        0.0
    };

    let tracks = input.streams().filter_map(|s| track_of(&s)).collect();

    let chapters = input
        .chapters()
        .map(|c| {
            let tb = f64::from(c.time_base());
            Chapter {
                title: c.metadata().get("title").unwrap_or("Chapter").to_string(),
                start: c.start() as f64 * tb,
                end: c.end() as f64 * tb,
            }
        })
        .collect();

    MediaInfo {
        path: path.to_path_buf(),
        title: meta.get("title").map(str::to_string),
        duration,
        tracks,
        chapters,
        container: input.format().name().to_string(),
        bit_rate: input.bit_rate(),
    }
}

fn track_of(s: &ff::format::stream::Stream) -> Option<Track> {
    let params = s.parameters();
    let kind = match params.medium() {
        ff::media::Type::Video => TrackKind::Video,
        ff::media::Type::Audio => TrackKind::Audio,
        ff::media::Type::Subtitle => TrackKind::Subtitle,
        _ => return None,
    };

    // A cover-art JPEG inside an MP3 is a video stream with one frame.
    // Listing it as a video track would make the player try to show it.
    let disposition = s.disposition();
    if kind == TrackKind::Video && disposition.contains(ff::format::stream::Disposition::ATTACHED_PIC) {
        return None;
    }

    let meta = s.metadata();
    let mut track = Track {
        index: s.index(),
        kind,
        codec: codec_name(params.id()),
        language: meta.get("language").map(str::to_string).filter(|l| l != "und"),
        title: meta.get("title").map(str::to_string),
        is_default: disposition.contains(ff::format::stream::Disposition::DEFAULT),
        is_forced: disposition.contains(ff::format::stream::Disposition::FORCED),
        channels: 0,
        sample_rate: 0,
        width: 0,
        height: 0,
        frame_rate: 0.0,
        hdr: None,
    };

    if let Ok(ctx) = ff::codec::context::Context::from_parameters(params) {
        match kind {
            TrackKind::Video => {
                if let Ok(v) = ctx.decoder().video() {
                    track.width = v.width();
                    track.height = v.height();
                    track.hdr = match v.color_transfer_characteristic() {
                        ff::color::TransferCharacteristic::SMPTE2084 => Some("HDR10"),
                        ff::color::TransferCharacteristic::ARIB_STD_B67 => Some("HLG"),
                        _ => None,
                    };
                }
                let r = s.avg_frame_rate();
                if r.denominator() != 0 {
                    track.frame_rate = f64::from(r);
                }
            }
            TrackKind::Audio => {
                if let Ok(a) = ctx.decoder().audio() {
                    track.channels = a.channels();
                    track.sample_rate = a.rate();
                }
            }
            TrackKind::Subtitle => {}
        }
    }
    Some(track)
}

fn codec_name(id: ff::codec::Id) -> String {
    id.name().to_string()
}

/// What a person calls a codec. PCM in all its bit depths and byte orders
/// is just "PCM": to a listener it is uncompressed audio, and "PCM_S24LE"
/// is noise. Anything unlisted falls back to FFmpeg's name, uppercased.
fn codec_label(codec: &str) -> String {
    if codec.starts_with("pcm_") {
        return "PCM".into();
    }
    if codec.starts_with("adpcm_") {
        return "ADPCM".into();
    }
    let name = match codec {
        // Audio.
        "eac3" => "Dolby Digital+",
        "ac3" => "Dolby Digital",
        "truehd" => "Dolby TrueHD",
        "dts" => "DTS",
        "mp3" => "MP3",
        "mp2" => "MP2",
        "flac" => "FLAC",
        "aac" => "AAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "alac" => "ALAC",
        "wavpack" => "WavPack",
        "speex" => "Speex",
        "wmav1" | "wmav2" => "WMA",
        "wmapro" => "WMA Pro",
        "wmalossless" => "WMA Lossless",
        "ape" => "Monkey's Audio",
        "musepack7" | "musepack8" => "Musepack",
        "tta" => "TTA",
        "amr_nb" | "amr_wb" => "AMR",
        n if n.starts_with("dsd_") => "DSD",
        // Video.
        "h264" => "H.264",
        "hevc" => "HEVC",
        "h263" => "H.263",
        "vp8" => "VP8",
        "vp9" => "VP9",
        "av1" => "AV1",
        "mpeg4" => "MPEG-4",
        "mpeg2video" => "MPEG-2",
        "mpeg1video" => "MPEG-1",
        "prores" => "ProRes",
        "theora" => "Theora",
        "wmv1" | "wmv2" | "wmv3" => "WMV",
        "vc1" => "VC-1",
        "flv1" => "Sorenson",
        "rawvideo" => "Raw",
        other => return other.to_uppercase(),
    };
    name.into()
}

/// The handful of languages worth spelling out. Anything else shows its
/// ISO code, which is still more useful than "Track 3".
fn language_name(code: &str) -> &str {
    match code {
        "eng" | "en" => "English",
        "spa" | "es" => "Spanish",
        "fra" | "fre" | "fr" => "French",
        "deu" | "ger" | "de" => "German",
        "ita" | "it" => "Italian",
        "por" | "pt" => "Portuguese",
        "rus" | "ru" => "Russian",
        "jpn" | "ja" => "Japanese",
        "kor" | "ko" => "Korean",
        "zho" | "chi" | "zh" => "Chinese",
        "ara" | "ar" => "Arabic",
        "hin" | "hi" => "Hindi",
        "nld" | "dut" | "nl" => "Dutch",
        "swe" | "sv" => "Swedish",
        "nor" | "no" => "Norwegian",
        "dan" | "da" => "Danish",
        "fin" | "fi" => "Finnish",
        "pol" | "pl" => "Polish",
        "tur" | "tr" => "Turkish",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_labels_read_as_formats() {
        assert_eq!(codec_label("pcm_s16le"), "PCM");
        assert_eq!(codec_label("pcm_f32be"), "PCM");
        assert_eq!(codec_label("adpcm_ima_wav"), "ADPCM");
        assert_eq!(codec_label("wmav2"), "WMA");
        assert_eq!(codec_label("mp3"), "MP3");
        assert_eq!(codec_label("h264"), "H.264");
        assert_eq!(codec_label("eac3"), "Dolby Digital+");
        assert_eq!(codec_label("dsd_lsbf_planar"), "DSD");
        assert_eq!(codec_label("something_new"), "SOMETHING_NEW");
    }
}
