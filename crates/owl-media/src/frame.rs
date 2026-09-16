//! What comes out of the video decoder and goes into the renderer.
//!
//! A `VideoFrame` owns a reference to the decoded `AVFrame` rather than a
//! copy of its pixels. FFmpeg frames are reference counted, so moving one
//! between the decode thread and the GL thread costs a pointer, not the
//! 12 MB a 4:2:0 4K frame would. The copy happens exactly once, when the
//! renderer uploads it as a texture.

use ffmpeg_next as ff;

/// How the planes are laid out in memory. The renderer switches on this
/// to pick a sampler and a chroma-fetch path.
///
/// Anything not listed here is converted by libswscale on the decode
/// thread into `BiPlanar8` (8-bit sources) or `BiPlanar16` (deeper ones)
/// before it ever reaches the renderer, which is what lets the player
/// claim to open anything FFmpeg can open without the shader growing a
/// branch per exotic format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    /// Y, U, V in three planes. `sub_x`/`sub_y` are chroma subsampling
    /// shifts: (1,1) is 4:2:0, (1,0) is 4:2:2, (0,0) is 4:4:4.
    Planar8 { sub_x: u8, sub_y: u8 },
    /// The same, 16-bit little-endian containers holding `bits` of data.
    Planar16 { sub_x: u8, sub_y: u8, bits: u8 },
    /// Y plane plus one interleaved chroma plane. NV12, or NV21 swapped.
    BiPlanar8 { swapped: bool },
    /// P010 / P012 / P016: 10-, 12- or 16-bit NV12.
    BiPlanar16 { bits: u8 },
    Rgba,
    Bgra,
    Rgb24,
}

impl PixelLayout {
    /// How many planes the renderer should bind.
    pub fn planes(self) -> usize {
        match self {
            PixelLayout::Planar8 { .. } | PixelLayout::Planar16 { .. } => 3,
            PixelLayout::BiPlanar8 { .. } | PixelLayout::BiPlanar16 { .. } => 2,
            PixelLayout::Rgba | PixelLayout::Bgra | PixelLayout::Rgb24 => 1,
        }
    }

    pub fn is_rgb(self) -> bool {
        matches!(self, PixelLayout::Rgba | PixelLayout::Bgra | PixelLayout::Rgb24)
    }

    /// Bits of real precision. A P010 frame is 16-bit words holding 10
    /// bits in the high end, so the shader has to scale by 65535/1023
    /// after sampling or everything comes out dark.
    pub fn bit_depth(self) -> u8 {
        match self {
            PixelLayout::Planar16 { bits, .. } | PixelLayout::BiPlanar16 { bits } => bits,
            _ => 8,
        }
    }

    /// Map an FFmpeg pixel format onto a layout the renderer knows, or
    /// `None` to mean "hand this to swscale first".
    pub fn from_ffmpeg(p: ff::format::Pixel) -> Option<PixelLayout> {
        use ff::format::Pixel as P;
        Some(match p {
            P::YUV420P | P::YUVJ420P => PixelLayout::Planar8 { sub_x: 1, sub_y: 1 },
            P::YUV422P | P::YUVJ422P => PixelLayout::Planar8 { sub_x: 1, sub_y: 0 },
            P::YUV444P | P::YUVJ444P => PixelLayout::Planar8 { sub_x: 0, sub_y: 0 },
            P::YUV420P10LE => PixelLayout::Planar16 { sub_x: 1, sub_y: 1, bits: 10 },
            P::YUV422P10LE => PixelLayout::Planar16 { sub_x: 1, sub_y: 0, bits: 10 },
            P::YUV444P10LE => PixelLayout::Planar16 { sub_x: 0, sub_y: 0, bits: 10 },
            P::YUV420P12LE => PixelLayout::Planar16 { sub_x: 1, sub_y: 1, bits: 12 },
            P::YUV422P12LE => PixelLayout::Planar16 { sub_x: 1, sub_y: 0, bits: 12 },
            P::YUV444P12LE => PixelLayout::Planar16 { sub_x: 0, sub_y: 0, bits: 12 },
            P::NV12 => PixelLayout::BiPlanar8 { swapped: false },
            P::NV21 => PixelLayout::BiPlanar8 { swapped: true },
            P::P010LE => PixelLayout::BiPlanar16 { bits: 10 },
            P::P012LE => PixelLayout::BiPlanar16 { bits: 12 },
            P::P016LE => PixelLayout::BiPlanar16 { bits: 16 },
            P::RGBA => PixelLayout::Rgba,
            P::BGRA => PixelLayout::Bgra,
            P::RGB24 => PixelLayout::Rgb24,
            _ => return None,
        })
    }
}

/// Colour metadata, carried per frame because it can legitimately change
/// mid-stream (a file can splice SDR and HDR segments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInfo {
    pub space: ff::color::Space,
    pub range: ff::color::Range,
    pub primaries: ff::color::Primaries,
    pub transfer: ff::color::TransferCharacteristic,
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self {
            space: ff::color::Space::BT709,
            range: ff::color::Range::MPEG,
            primaries: ff::color::Primaries::BT709,
            transfer: ff::color::TransferCharacteristic::BT709,
        }
    }
}

impl ColorInfo {
    /// True when the transfer function is one of the HDR curves, so the
    /// renderer knows to tone-map instead of treating the values as
    /// display-referred.
    pub fn is_hdr(&self) -> bool {
        matches!(
            self.transfer,
            ff::color::TransferCharacteristic::SMPTE2084 | ff::color::TransferCharacteristic::ARIB_STD_B67
        )
    }

    /// The label the UI puts in the chip row next to the title.
    pub fn hdr_label(&self) -> Option<&'static str> {
        match self.transfer {
            ff::color::TransferCharacteristic::SMPTE2084 => Some("HDR10"),
            ff::color::TransferCharacteristic::ARIB_STD_B67 => Some("HLG"),
            _ => None,
        }
    }

    /// An unspecified colour space is not an error — most files leave it
    /// blank — so fall back the way every player does: BT.709 for HD and
    /// up, BT.601 below it, because that is what the content actually is.
    pub fn resolved_space(&self, height: u32) -> ff::color::Space {
        match self.space {
            ff::color::Space::Unspecified => {
                if height >= 720 { ff::color::Space::BT709 } else { ff::color::Space::BT470BG }
            }
            other => other,
        }
    }
}

/// One decoded picture, ready to upload.
pub struct VideoFrame {
    frame: ff::frame::Video,
    pub layout: PixelLayout,
    pub color: ColorInfo,
    /// Presentation timestamp in seconds from the start of the file.
    pub pts: f64,
    /// Pixel aspect ratio numerator/denominator. Anamorphic DVD and some
    /// broadcast content is stored at the wrong shape on purpose and is
    /// only correct once this is applied.
    pub sar: (u32, u32),
}

impl VideoFrame {
    pub fn new(frame: ff::frame::Video, layout: PixelLayout, color: ColorInfo, pts: f64, sar: (u32, u32)) -> Self {
        Self { frame, layout, color, pts, sar }
    }

    pub fn width(&self) -> u32 {
        self.frame.width()
    }

    pub fn height(&self) -> u32 {
        self.frame.height()
    }

    /// Width and height after the pixel aspect ratio is applied — what the
    /// picture should actually be displayed as.
    pub fn display_size(&self) -> (u32, u32) {
        let (num, den) = self.sar;
        if num == 0 || den == 0 || num == den {
            return (self.width(), self.height());
        }
        ((self.width() as u64 * num as u64 / den as u64) as u32, self.height())
    }

    pub fn plane(&self, i: usize) -> PlaneRef<'_> {
        PlaneRef {
            data: self.frame.data(i),
            stride: self.frame.stride(i),
            width: self.frame.plane_width(i),
            height: self.frame.plane_height(i),
        }
    }
}

/// A borrowed view of one plane, handed to the renderer for upload.
pub struct PlaneRef<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
}
