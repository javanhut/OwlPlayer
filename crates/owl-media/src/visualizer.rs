//! What to draw when there is no picture.
//!
//! A music file has nothing to show, and a black rectangle is a poor
//! answer. This taps the samples on their way to the sound card and turns
//! them into a spectrum the renderer can draw.
//!
//! The tap is written from the realtime audio callback, so it allocates
//! nothing and never locks — same constraint as `ring.rs`, for the same
//! reason. The analysis happens on the GL thread instead, once per frame,
//! where being a few hundred microseconds late costs nothing.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Samples per analysis window. At 48 kHz this is about 21 ms — long
/// enough to resolve the bass, short enough that the display still moves
/// with the music rather than lagging behind it.
pub const WINDOW: usize = 1024;

/// Bars on screen.
pub const BANDS: usize = 56;

/// How far below full scale the display reaches. 60 dB is the usual choice
/// for a music meter: quiet enough to show detail in the mix, loud enough
/// that room noise does not fill the screen.
const DISPLAY_RANGE_DB: f32 = 60.0;

/// A lock-free window of recent mono samples.
pub struct Tap {
    buf: UnsafeCell<[f32; WINDOW]>,
    write: AtomicUsize,
    /// False until audio actually starts flowing, so the renderer can tell
    /// "silent" from "no audio at all".
    live: AtomicBool,
}

// Safety: the producer is the single audio callback and the consumer is
// the single GL thread. A torn read costs one frame of a moving display
// and nothing else, which is why this does not need a seqlock.
unsafe impl Send for Tap {}
unsafe impl Sync for Tap {}

impl Default for Tap {
    fn default() -> Self {
        Self {
            buf: UnsafeCell::new([0.0; WINDOW]),
            write: AtomicUsize::new(0),
            live: AtomicBool::new(false),
        }
    }
}

impl Tap {
    /// Called from the audio callback with interleaved output samples.
    /// Channels are averaged: a spectrum is about content, not staging,
    /// and summing without averaging would clip the display on loud
    /// stereo material.
    pub fn push(&self, interleaved: &[f32], channels: u16) {
        let channels = channels.max(1) as usize;
        let buf = unsafe { &mut *self.buf.get() };
        let mut write = self.write.load(Ordering::Relaxed);
        for frame in interleaved.chunks_exact(channels) {
            let sum: f32 = frame.iter().sum();
            buf[write % WINDOW] = sum / channels as f32;
            write += 1;
        }
        self.write.store(write, Ordering::Release);
        self.live.store(true, Ordering::Relaxed);
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.live.store(false, Ordering::Relaxed);
    }

    /// Copy the most recent window, oldest sample first.
    fn snapshot(&self, out: &mut [f32; WINDOW]) {
        let write = self.write.load(Ordering::Acquire);
        let buf = unsafe { &*self.buf.get() };
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = buf[(write + i) % WINDOW];
        }
    }
}

/// Turns the tap into bar heights. Holds the smoothing state, so it lives
/// on the drawing side and is stepped once per frame.
pub struct Analyzer {
    levels: [f32; BANDS],
    window: [f32; WINDOW],
    /// Hann coefficients, computed once. Without a window function every
    /// bar leaks into its neighbours and the display turns to mush.
    hann: [f32; WINDOW],
    edges: [usize; BANDS + 1],
}

impl Default for Analyzer {
    fn default() -> Self {
        let mut hann = [0.0; WINDOW];
        for (i, h) in hann.iter_mut().enumerate() {
            *h = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / WINDOW as f32).cos();
        }

        // Logarithmic band edges: pitch is logarithmic, so linear bins
        // would spend most of the display on the top two octaves, where
        // music has almost nothing, and cram the bass into one bar.
        let bins = WINDOW / 2;
        let lowest = 2.0f32; // skip DC and the first bin, which is rumble
        let highest = bins as f32;
        let mut edges = [0usize; BANDS + 1];
        for (i, edge) in edges.iter_mut().enumerate() {
            let t = i as f32 / BANDS as f32;
            *edge = (lowest * (highest / lowest).powf(t)).round() as usize;
        }
        // Guarantee every band owns at least one bin, even at the bottom
        // where the logarithm packs them together.
        for i in 1..=BANDS {
            if edges[i] <= edges[i - 1] {
                edges[i] = edges[i - 1] + 1;
            }
        }

        Self { levels: [0.0; BANDS], window: [0.0; WINDOW], hann, edges }
    }
}

impl Analyzer {
    /// Step the display one frame and return the current bar heights,
    /// each in 0..=1.
    pub fn update(&mut self, tap: &Tap) -> &[f32; BANDS] {
        tap.snapshot(&mut self.window);

        let mut re = [0.0f32; WINDOW];
        let mut im = [0.0f32; WINDOW];
        for i in 0..WINDOW {
            re[i] = self.window[i] * self.hann[i];
        }
        fft(&mut re, &mut im);

        for band in 0..BANDS {
            let (from, to) = (self.edges[band], self.edges[band + 1].min(WINDOW / 2));
            let mut peak = 0.0f32;
            for bin in from..to.max(from + 1) {
                let power = re[bin] * re[bin] + im[bin] * im[bin];
                peak = peak.max(power);
            }
            // Normalise before taking decibels. An un-normalised FFT scales
            // with the window length, so a full-scale sine reads about
            // +48 dB here rather than 0, and every bar pins to the ceiling
            // while still moving convincingly enough to look correct.
            //
            // The 0.5 is the Hann window's coherent gain: the window halves
            // the amplitude of whatever passes through it.
            let amplitude = peak.sqrt() * (2.0 / (WINDOW as f32 * 0.5));
            let db = 20.0 * (amplitude + 1e-9).log10();
            let level = ((db + DISPLAY_RANGE_DB) / DISPLAY_RANGE_DB).clamp(0.0, 1.0);

            // Fast attack, slow decay: the display should hit a transient
            // immediately and fall back gently, the way a VU meter does.
            let previous = self.levels[band];
            self.levels[band] = if level > previous {
                previous + (level - previous) * 0.55
            } else {
                previous + (level - previous) * 0.12
            };
        }
        &self.levels
    }

    pub fn levels(&self) -> &[f32; BANDS] {
        &self.levels
    }
}

/// In-place iterative radix-2 Cooley-Tukey. `WINDOW` is a power of two, so
/// there is no need for the general case, and this avoids a dependency for
/// sixty lines of well-understood arithmetic.
fn fft(re: &mut [f32; WINDOW], im: &mut [f32; WINDOW]) {
    let n = WINDOW;

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let angle = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (angle.cos(), angle.sin());
        let mut start = 0;
        while start < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next;
            }
            start += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-scale tone should read near the top of the display without
    /// pinning, and a tone 40 dB quieter should read near the bottom. This
    /// is what catches an un-normalised transform: the bars still move, so
    /// only the numbers give it away.
    #[test]
    fn levels_are_calibrated_against_full_scale() {
        let level_for = |amplitude: f32| {
            let tap = Tap::default();
            let samples: Vec<f32> = (0..WINDOW * 2)
                .map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * amplitude)
                .collect();
            tap.push(&samples, 1);
            let mut analyzer = Analyzer::default();
            for _ in 0..60 {
                analyzer.update(&tap);
            }
            analyzer.levels().iter().cloned().fold(0.0f32, f32::max)
        };

        let full = level_for(1.0);
        assert!((0.85..=1.0).contains(&full), "full scale should be near the top, got {full}");

        // 40 dB down is two thirds of the way down a 60 dB display.
        let quiet = level_for(0.01);
        assert!((0.15..=0.45).contains(&quiet), "-40 dB should sit low, got {quiet}");
        assert!(full > quiet + 0.4, "the display has to separate loud from quiet");
    }

    /// A pure tone must land in the bin its frequency belongs to. If the
    /// transform is wrong this is the test that says so, and every bar on
    /// screen is wrong in a way that still looks plausible.
    #[test]
    fn a_pure_tone_lands_in_its_own_bin() {
        let target = 64usize;
        let mut re = [0.0f32; WINDOW];
        let mut im = [0.0f32; WINDOW];
        for i in 0..WINDOW {
            re[i] = (std::f32::consts::TAU * target as f32 * i as f32 / WINDOW as f32).sin();
        }
        fft(&mut re, &mut im);

        let power = |k: usize| re[k] * re[k] + im[k] * im[k];
        let loudest = (1..WINDOW / 2).max_by(|a, b| power(*a).total_cmp(&power(*b))).unwrap();
        assert_eq!(loudest, target, "peak bin");
        assert!(power(target) > power(target + 4) * 100.0, "energy should not be smeared");
    }

    #[test]
    fn silence_produces_no_bars() {
        let tap = Tap::default();
        let mut analyzer = Analyzer::default();
        for _ in 0..40 {
            analyzer.update(&tap);
        }
        assert!(analyzer.levels().iter().all(|&l| l < 0.01), "silence must read as silence");
    }

    /// Every band has to own at least one bin, or the bass end of the
    /// display is a row of permanently dead bars.
    #[test]
    fn every_band_owns_a_bin() {
        let analyzer = Analyzer::default();
        for band in 0..BANDS {
            assert!(
                analyzer.edges[band + 1] > analyzer.edges[band],
                "band {band} is empty: {}..{}",
                analyzer.edges[band],
                analyzer.edges[band + 1]
            );
        }
    }

    #[test]
    fn a_loud_tone_raises_a_bar() {
        let tap = Tap::default();
        let rate = 48_000.0f32;
        // 1 kHz, which should land somewhere in the middle of the display.
        let samples: Vec<f32> = (0..WINDOW * 2)
            .map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / rate).sin() * 0.8)
            .collect();
        tap.push(&samples, 1);

        let mut analyzer = Analyzer::default();
        for _ in 0..40 {
            analyzer.update(&tap);
        }
        let peak = analyzer.levels().iter().cloned().fold(0.0f32, f32::max);
        assert!(peak > 0.5, "a loud tone should raise a bar, got {peak}");
    }
}
