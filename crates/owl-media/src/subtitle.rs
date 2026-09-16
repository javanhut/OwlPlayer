//! Subtitles: what comes out of a subtitle decoder, and how it becomes
//! something drawable.
//!
//! FFmpeg hands back two completely different things under one name. Text
//! formats — SRT, WebVTT, MOV_TEXT, ASS — are normalised by their decoders
//! into ASS dialogue lines, so there is exactly one text path to write and
//! it is the ASS one. Bitmap formats — PGS on Blu-ray, DVB, VOBSUB on DVD —
//! are paletted images with their own coordinate space that have to be
//! composited over the picture.
//!
//! Both arrive with timing relative to their packet, not to the file, which
//! is the detail that makes subtitles drift if it is got wrong.

/// Where on the picture a cue belongs. ASS inherits the numeric-keypad
/// convention: 1-3 along the bottom, 4-6 through the middle, 7-9 across
/// the top. Only the vertical part matters for placement; horizontal
/// centring is what essentially all dialogue uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    #[default]
    Bottom,
    Middle,
    Top,
}

impl Alignment {
    fn from_ass(code: u32) -> Alignment {
        match code {
            7..=9 => Alignment::Top,
            4..=6 => Alignment::Middle,
            _ => Alignment::Bottom,
        }
    }
}

/// One paletted image, already expanded to RGBA.
#[derive(Debug, Clone)]
pub struct BitmapRect {
    /// Position within the subtitle's own coordinate space, not the
    /// widget's and not necessarily the video's.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Straight (non-premultiplied) RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum SubtitleContent {
    /// Pango markup, ready to hand to a label.
    Text { markup: String, alignment: Alignment },
    /// One or more images to composite, in the coordinate space given by
    /// `reference`.
    Bitmap { rects: Vec<BitmapRect>, reference: (u32, u32) },
}

#[derive(Debug, Clone)]
pub struct SubtitleCue {
    pub start: f64,
    pub end: f64,
    pub content: SubtitleContent,
}

impl SubtitleCue {
    pub fn covers(&self, t: f64) -> bool {
        t >= self.start && t < self.end
    }
}

// ── Paletted bitmaps ────────────────────────────────────────────────────

/// Expand a paletted subtitle image into straight RGBA.
///
/// Bitmap subtitles are PAL8: one byte of palette index per pixel, and a
/// palette of ARGB words in native byte order. `stride` is the distance
/// between rows in `indices` and is usually wider than `width`.
///
/// Fully transparent pixels are left as zeroes rather than written, which
/// is most of the image: a subtitle is a few glyphs inside a rectangle
/// that often spans the whole frame.
pub fn expand_palette(
    indices: &[u8],
    stride: usize,
    palette: &[u32],
    width: usize,
    height: usize,
) -> Vec<u8> {
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        let row_start = y * stride;
        // A truncated final row is possible in a malformed stream, and is
        // not a reason to drop the whole cue.
        let Some(row) = indices.get(row_start..row_start + width) else { break };
        for (x, &index) in row.iter().enumerate() {
            let Some(&entry) = palette.get(index as usize) else { continue };
            let alpha = (entry >> 24) as u8;
            if alpha == 0 {
                continue;
            }
            let out = (y * width + x) * 4;
            rgba[out] = (entry >> 16) as u8;
            rgba[out + 1] = (entry >> 8) as u8;
            rgba[out + 2] = entry as u8;
            rgba[out + 3] = alpha;
        }
    }
    rgba
}

// ── ASS to Pango ────────────────────────────────────────────────────────

/// Pull the Text field out of an ASS dialogue line.
///
/// There are two shapes in the wild and FFmpeg emits the second one:
///
/// * A full line from a file: `Dialogue: Layer,Start,End,Style,Name,
///   MarginL,MarginR,MarginV,Effect,Text` — nine commas before the text.
/// * What a decoder puts in `AVSubtitleRect.ass`, which since FFmpeg 4.0
///   drops the prefix and the timestamps and leads with a read order:
///   `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text` —
///   eight commas.
///
/// The text may itself contain commas and must not be split on them,
/// which is why this counts separators instead of splitting.
pub fn ass_dialogue_text(line: &str) -> &str {
    let (rest, separators) = match line.strip_prefix("Dialogue:") {
        Some(rest) => (rest, 9),
        None => (line, 8),
    };
    let mut remaining = rest;
    for _ in 0..separators {
        match remaining.find(',') {
            Some(i) => remaining = &remaining[i + 1..],
            // Fewer fields than expected: a dialect, or plain text that
            // was never a dialogue line. Showing it beats showing nothing.
            None => return rest.trim(),
        }
    }
    remaining
}

/// Convert an ASS text field into Pango markup.
///
/// ASS has a large override-tag language and almost none of it appears in
/// ordinary dialogue. The tags that do — italic, bold, underline, line
/// breaks and alignment — are translated; everything else is dropped
/// rather than shown, because a viewer would rather miss a karaoke effect
/// than read `{\k42}` across the bottom of the screen.
pub fn ass_to_markup(text: &str) -> (String, Alignment) {
    let mut out = String::with_capacity(text.len());
    let mut alignment = Alignment::default();
    // Tags can open without closing before the cue ends, so the closers
    // are tracked and flushed at the end to keep the markup balanced.
    let mut open: Vec<&'static str> = Vec::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let mut tag = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    tag.push(c);
                }
                apply_override(&tag, &mut out, &mut open, &mut alignment);
            }
            '\\' => match chars.peek() {
                // Hard line break, and its soft variant which players are
                // free to treat the same way.
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                // Non-breaking space.
                Some('h') => {
                    chars.next();
                    out.push('\u{00a0}');
                }
                _ => out.push('\\'),
            },
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(c),
        }
    }

    while let Some(tag) = open.pop() {
        out.push_str(tag);
    }
    (out.trim().to_string(), alignment)
}

fn apply_override(tag: &str, out: &mut String, open: &mut Vec<&'static str>, alignment: &mut Alignment) {
    // One brace group can hold several backslash-separated overrides.
    for part in tag.split('\\').skip(1) {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("an") {
            if let Ok(code) = rest.parse::<u32>() {
                *alignment = Alignment::from_ass(code);
            }
            continue;
        }
        // Legacy SSA alignment, which numbers its positions differently.
        if let Some(rest) = part.strip_prefix('a') {
            if let Ok(code) = rest.parse::<u32>() {
                *alignment = match code {
                    5..=7 => Alignment::Top,
                    9..=11 => Alignment::Middle,
                    _ => Alignment::Bottom,
                };
            }
            continue;
        }
        let toggle = |on: bool, tag_open: &'static str, tag_close: &'static str, out: &mut String, open: &mut Vec<&'static str>| {
            if on {
                out.push_str(tag_open);
                open.push(tag_close);
            } else if let Some(i) = open.iter().rposition(|t| *t == tag_close) {
                out.push_str(tag_close);
                open.remove(i);
            }
        };
        match part {
            "i1" => toggle(true, "<i>", "</i>", out, open),
            "i0" => toggle(false, "<i>", "</i>", out, open),
            "b1" => toggle(true, "<b>", "</b>", out, open),
            "b0" => toggle(false, "<b>", "</b>", out, open),
            "u1" => toggle(true, "<u>", "</u>", out, open),
            "u0" => toggle(false, "<u>", "</u>", out, open),
            "s1" => toggle(true, "<s>", "</s>", out, open),
            "s0" => toggle(false, "<s>", "</s>", out, open),
            // Bold with a weight, as in {\b700}. Anything above 400 is bold.
            other if other.starts_with('b') => {
                if let Ok(weight) = other[1..].parse::<u32>() {
                    toggle(weight > 400, "<b>", "</b>", out, open);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogue_text_survives_commas() {
        let line = "Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Well, yes, and no.";
        assert_eq!(ass_dialogue_text(line), "Well, yes, and no.");
    }

    #[test]
    fn bare_text_is_passed_through() {
        assert_eq!(ass_dialogue_text("just text"), "just text");
    }

    /// Exactly what FFmpeg 9 hands back for an SRT track muxed into
    /// Matroska: no `Dialogue:` prefix, a leading read order, and commas
    /// inside the text itself.
    #[test]
    fn ffmpeg_rect_ass_form() {
        let line = "0,0,Default,,0,0,0,,Well, yes, and no.";
        assert_eq!(ass_dialogue_text(line), "Well, yes, and no.");
    }

    #[test]
    fn ffmpeg_rect_ass_form_with_tags() {
        let line = r"1,0,Default,,0,0,0,,{\i1}Italic line{\i0}\NSecond line";
        let (markup, _) = ass_to_markup(ass_dialogue_text(line));
        assert_eq!(markup, "<i>Italic line</i>\nSecond line");
    }

    #[test]
    fn italics_become_markup() {
        let (markup, _) = ass_to_markup(r"{\i1}Hello{\i0} there");
        assert_eq!(markup, "<i>Hello</i> there");
    }

    /// A cue that opens italics and never closes them still has to produce
    /// balanced markup, or Pango refuses the whole string and the line
    /// vanishes instead of merely losing its styling.
    #[test]
    fn unclosed_tags_are_balanced() {
        let (markup, _) = ass_to_markup(r"{\i1}Hello");
        assert_eq!(markup, "<i>Hello</i>");
    }

    #[test]
    fn line_breaks_and_unknown_tags() {
        let (markup, _) = ass_to_markup(r"{\k42}First\NSecond{\pos(10,20)}");
        assert_eq!(markup, "First\nSecond");
    }

    #[test]
    fn markup_special_characters_are_escaped() {
        let (markup, _) = ass_to_markup("5 < 6 & 7 > 2");
        assert_eq!(markup, "5 &lt; 6 &amp; 7 &gt; 2");
    }

    #[test]
    fn alignment_is_read() {
        assert_eq!(ass_to_markup(r"{\an8}Sign text").1, Alignment::Top);
        assert_eq!(ass_to_markup(r"{\an5}Middle").1, Alignment::Middle);
        assert_eq!(ass_to_markup("Plain").1, Alignment::Bottom);
    }

    #[test]
    fn several_overrides_in_one_group() {
        let (markup, align) = ass_to_markup(r"{\an8\i1}Tilted sign");
        assert_eq!(markup, "<i>Tilted sign</i>");
        assert_eq!(align, Alignment::Top);
    }

    #[test]
    fn palette_expands_to_rgba() {
        // 0xAARRGGBB: opaque red, half-alpha green, fully transparent.
        let palette = [0xFF_FF_00_00u32, 0x80_00_FF_00, 0x00_00_00_FF];
        // Stride is wider than the image: the trailing byte must be ignored.
        let indices = [0u8, 1, 9, /* padding */ 7];
        let rgba = expand_palette(&indices, 4, &palette, 3, 1);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "opaque red");
        assert_eq!(&rgba[4..8], &[0, 255, 0, 128], "half-alpha green");
        // Index 9 is past the end of a 3-entry palette: skipped, not
        // wrapped, and definitely not a panic.
        assert_eq!(&rgba[8..12], &[0, 0, 0, 0], "out-of-range index");
    }

    #[test]
    fn transparent_palette_entries_stay_clear() {
        let palette = [0x00_FF_FF_FFu32];
        let rgba = expand_palette(&[0, 0], 2, &palette, 2, 1);
        assert!(rgba.iter().all(|&b| b == 0), "a transparent entry must not write colour");
    }

    #[test]
    fn stride_padding_is_respected() {
        let palette = [0x00_00_00_00u32, 0xFF_11_22_33];
        // Two rows of width 2 in a stride-3 buffer.
        let indices = [1u8, 0, 0, 0, 1, 0];
        let rgba = expand_palette(&indices, 3, &palette, 2, 2);
        assert_eq!(&rgba[0..4], &[0x11, 0x22, 0x33, 0xFF], "row 0 pixel 0");
        assert_eq!(&rgba[12..16], &[0x11, 0x22, 0x33, 0xFF], "row 1 pixel 1");
    }

    #[test]
    fn truncated_rows_do_not_panic() {
        let palette = [0xFF_FF_FF_FFu32];
        // Claims two rows but only supplies one.
        let rgba = expand_palette(&[0, 0], 2, &palette, 2, 2);
        assert_eq!(rgba.len(), 2 * 2 * 4);
    }

    #[test]
    fn numeric_bold_weight() {
        let (markup, _) = ass_to_markup(r"{\b700}Heavy{\b0}");
        assert_eq!(markup, "<b>Heavy</b>");
    }
}
