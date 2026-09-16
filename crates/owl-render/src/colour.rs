//! YUV to RGB, derived rather than tabulated.
//!
//! Every broadcast colour space is the same pair of equations with
//! different luma weights, so the matrices are generated from Kr and Kb
//! instead of being copied out of a spec as nine magic numbers each. That
//! also means the range and bit-depth corrections are applied in one
//! place, which is where players usually get this wrong: an 8-bit rule
//! reused for 10-bit content produces a picture that is subtly too dark
//! and slightly green, and it is easy to look at without noticing.

use ffmpeg_next::color::Space;

/// Luma coefficients (Kr, Kb) for the spaces worth distinguishing.
fn luma_weights(space: Space) -> (f32, f32) {
    match space {
        // BT.601, 525- and 625-line. SD content, and anything unlabelled
        // below 720p.
        Space::BT470BG | Space::SMPTE170M | Space::SMPTE240M => (0.299, 0.114),
        // BT.2020, both constant and non-constant luminance. The matrix is
        // the same; only the transfer function differs, and that is handled
        // in the shader.
        Space::BT2020NCL | Space::BT2020CL => (0.2627, 0.0593),
        // BT.709 and anything unrecognised. HD is overwhelmingly the
        // common case, and 709 is the least wrong guess for the rest.
        _ => (0.2126, 0.0722),
    }
}

/// The 3x3 matrix (column-major, ready for `glUniformMatrix3fv`) and the
/// offset to subtract from each sample before applying it.
pub fn yuv_to_rgb(space: Space, full_range: bool, bit_depth: u8) -> ([f32; 9], [f32; 3]) {
    let (kr, kb) = luma_weights(space);
    let kg = 1.0 - kr - kb;

    let bits = bit_depth.max(8) as u32;
    let max = ((1u32 << bits) - 1) as f32;
    let shift = (1u32 << (bits - 8)) as f32;

    // Studio-swing video uses 16..235 for luma and 16..240 for chroma, at
    // 8 bits, scaled up by the depth. Full-range (JPEG, and most screen
    // recordings) uses the whole interval and needs no correction.
    let (y_scale, c_scale, y_offset) = if full_range {
        (1.0, 1.0, 0.0)
    } else {
        (max / (219.0 * shift), max / (224.0 * shift), 16.0 * shift / max)
    };

    // Neutral chroma is the midpoint of the container, not 0.5: at 8 bits
    // that is 128/255, at 10 bits 512/1023.
    let c_offset = (1u32 << (bits - 1)) as f32 / max;

    let vr = 2.0 * (1.0 - kr) * c_scale;
    let ub = 2.0 * (1.0 - kb) * c_scale;
    let ug = 2.0 * (1.0 - kb) * kb / kg * c_scale;
    let vg = 2.0 * (1.0 - kr) * kr / kg * c_scale;

    // Column-major: columns are the Y, U and V contributions to RGB.
    let matrix = [y_scale, y_scale, y_scale, 0.0, -ug, ub, vr, -vg, 0.0];
    (matrix, [y_offset, c_offset, c_offset])
}

/// BT.2020 primaries to BT.709, applied in linear light while tone
/// mapping. Identity when the source is already SDR, so the shader can
/// use the same uniform either way.
pub fn primaries_matrix(tone_mapping: bool) -> [f32; 9] {
    if !tone_mapping {
        return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    }
    // Column-major form of the standard 2020->709 conversion.
    [1.6605, -0.1246, -0.0182, -0.5876, 1.1329, -0.1006, -0.0728, -0.0083, 1.1187]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// White must survive the round trip. If the range handling is wrong
    /// this is the first thing that breaks, and it breaks invisibly:
    /// the picture just looks a little flat.
    #[test]
    fn limited_range_white_maps_to_one() {
        for bits in [8u8, 10, 12] {
            let (m, off) = yuv_to_rgb(Space::BT709, false, bits);
            let max = ((1u32 << bits) - 1) as f32;
            let white_y = (235.0 * (1u32 << (bits - 8)) as f32) / max;
            let neutral = (1u32 << (bits - 1)) as f32 / max;
            let yuv = [white_y - off[0], neutral - off[1], neutral - off[2]];
            // Row 0 of a column-major matrix is elements 0, 3, 6.
            let r = m[0] * yuv[0] + m[3] * yuv[1] + m[6] * yuv[2];
            assert!((r - 1.0).abs() < 0.002, "{bits}-bit white came out at {r}");
        }
    }

    #[test]
    fn limited_range_black_maps_to_zero() {
        let (m, off) = yuv_to_rgb(Space::BT709, false, 8);
        let yuv = [16.0 / 255.0 - off[0], 128.0 / 255.0 - off[1], 128.0 / 255.0 - off[2]];
        let r = m[0] * yuv[0] + m[3] * yuv[1] + m[6] * yuv[2];
        assert!(r.abs() < 0.002, "black came out at {r}");
    }

    /// Full-range content must not be stretched a second time.
    #[test]
    fn full_range_is_untouched() {
        let (m, off) = yuv_to_rgb(Space::BT709, true, 8);
        assert_eq!(off[0], 0.0);
        assert!((m[0] - 1.0).abs() < f32::EPSILON);
    }

    /// A pure-red BT.709 sample should come back red, not orange: this
    /// catches the U and V columns being swapped, which is otherwise a
    /// surprisingly subtle bug to see.
    #[test]
    fn red_stays_red() {
        let (m, off) = yuv_to_rgb(Space::BT709, false, 8);
        let (y, u, v) = (63.0 / 255.0, 102.0 / 255.0, 240.0 / 255.0);
        let yuv = [y - off[0], u - off[1], v - off[2]];
        let r = m[0] * yuv[0] + m[3] * yuv[1] + m[6] * yuv[2];
        let g = m[1] * yuv[0] + m[4] * yuv[1] + m[7] * yuv[2];
        let b = m[2] * yuv[0] + m[5] * yuv[1] + m[8] * yuv[2];
        assert!(r > 0.9, "red channel {r}");
        assert!(g < 0.2, "green channel {g}");
        assert!(b < 0.2, "blue channel {b}");
    }
}
