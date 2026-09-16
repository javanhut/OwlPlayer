//! The sound card, and the clock that hangs off it.
//!
//! The device is opened once at its own preferred rate and channel count,
//! and every stream is resampled to fit. Re-opening the device per file
//! would be a click between tracks and a compatibility minefield; swresample
//! costs a fraction of a percent of a core and never surprises anyone.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::ring::AudioRing;

/// Shared between the decode thread, the realtime callback and the clock.
pub struct AudioState {
    pub ring: AudioRing,
    /// Media PTS corresponding to sample zero of the current continuous
    /// run. Reset on every seek; between seeks the audio stream is gapless,
    /// so the played position is exactly this plus samples/rate.
    anchor: AtomicU64,
    pub rate: u32,
    pub channels: u16,
}

impl AudioState {
    pub fn set_anchor(&self, pts: f64) {
        self.anchor.store(pts.to_bits(), Ordering::Relaxed);
    }

    pub fn anchor(&self) -> f64 {
        f64::from_bits(self.anchor.load(Ordering::Relaxed))
    }

    /// Throw away buffered audio belonging to the old position.
    pub fn flush_to(&self, pts: f64) {
        self.ring.reset();
        self.set_anchor(pts);
    }
}

pub struct Sink {
    /// Never read, and never removed: dropping a cpal stream closes the
    /// device. It stays alive exactly as long as the decode thread does.
    #[allow(dead_code)]
    stream: cpal::Stream,
    pub state: Arc<AudioState>,
}

impl Sink {
    /// Open an output.
    ///
    /// Deliberately not `default_output_device()`. On a PipeWire desktop
    /// that is ALSA's `default` PCM, which routes through dmix, which
    /// cannot open the card because PipeWire is already holding it — so
    /// the obvious call is the one that always fails. The device that
    /// works is the one PipeWire itself publishes, and it has to be found
    /// by asking.
    pub fn open(clock: Arc<Clock>, paused: Arc<std::sync::atomic::AtomicBool>) -> Result<Sink> {
        let host = cpal::default_host();
        let mut candidates: Vec<(u32, String, cpal::Device)> = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let name =
                    device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| String::new());
                if let Some(rank) = rank_device(&name) {
                    candidates.push((rank, name, device));
                }
            }
        }
        // Stable sort, so enumeration order still breaks ties between
        // equally-ranked devices.
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        if candidates.is_empty() {
            return Err(Error::NoAudioDevice);
        }

        let mut last = None;
        for (_, name, device) in candidates {
            match Sink::try_device(&device, &clock, &paused) {
                Ok(sink) => {
                    log::info!("audio out: {name}");
                    return Ok(sink);
                }
                Err(e) => {
                    log::debug!("audio device {name:?} unusable: {e}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or(Error::NoAudioDevice))
    }

    /// The ring holds about a second, which is enough that a scheduling
    /// hiccup on the decode thread is inaudible and short enough that a
    /// seek does not have a second of stale sound to discard.
    fn try_device(
        device: &cpal::Device,
        clock: &Arc<Clock>,
        paused: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Sink> {
        let clock = Arc::clone(clock);
        let paused = Arc::clone(paused);
        let supported = device.default_output_config().map_err(|e| Error::AudioStream(e.to_string()))?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels;

        let state = Arc::new(AudioState {
            ring: AudioRing::new(rate as usize * channels as usize),
            anchor: AtomicU64::new(0),
            rate,
            channels,
        });

        log::debug!("trying {rate} Hz · {channels} ch · {sample_format:?}");

        let err_fn = |e| log::error!("audio stream: {e}");
        let s = Arc::clone(&state);
        let c = Arc::clone(&clock);

        // The callback runs on a realtime thread owned by the audio
        // server. It must not allocate, lock or block — it drains the ring
        // and advances the clock, and nothing else.
        macro_rules! build {
            ($t:ty, $convert:expr) => {{
                let mut scratch: Vec<f32> = Vec::new();
                let paused = Arc::clone(&paused);
                device.build_output_stream(
                    config.clone(),
                    move |out: &mut [$t], _: &cpal::OutputCallbackInfo| {
                        // Pausing is done here rather than by stopping the
                        // decode threads: the ring holds about a second, and
                        // letting that drain would keep playing for a second
                        // after the user pressed pause. Silence now, and the
                        // clock stops because `consumed` stops moving.
                        if paused.load(Ordering::Relaxed) {
                            out.fill($convert(0.0f32));
                            return;
                        }
                        if scratch.len() < out.len() {
                            // Grows once, in the first few callbacks, then
                            // never again: buffer sizes do not change.
                            scratch.resize(out.len(), 0.0);
                        }
                        let buf = &mut scratch[..out.len()];
                        s.ring.pop_into(buf);
                        for (o, &f) in out.iter_mut().zip(buf.iter()) {
                            *o = $convert(f);
                        }
                        advance_clock(&s, &c, out.len());
                    },
                    err_fn,
                    None,
                )
            }};
        }

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build!(f32, |f: f32| f),
            cpal::SampleFormat::I16 => build!(i16, |f: f32| (f.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
            cpal::SampleFormat::U16 => {
                build!(u16, |f: f32| (((f.clamp(-1.0, 1.0) + 1.0) * 0.5) * u16::MAX as f32) as u16)
            }
            cpal::SampleFormat::I32 => build!(i32, |f: f32| (f.clamp(-1.0, 1.0) * i32::MAX as f32) as i32),
            cpal::SampleFormat::F64 => build!(f64, |f: f32| f as f64),
            other => return Err(Error::AudioStream(format!("unsupported sample format {other:?}"))),
        }
        .map_err(|e| Error::AudioStream(e.to_string()))?;

        stream.play().map_err(|e| Error::AudioStream(e.to_string()))?;
        Ok(Sink { stream, state })
    }

    /// Hand samples to the device, waiting when the ring is full. That
    /// wait is the engine's pacing: the sound card sets the speed of
    /// playback and the decode threads follow it.
    pub fn push_blocking(&self, mut samples: &[f32], should_stop: &dyn Fn() -> bool) {
        while !samples.is_empty() {
            if should_stop() {
                return;
            }
            let n = self.state.ring.push(samples);
            samples = &samples[n..];
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
        }
    }

}

/// Publish where playback actually is. `consumed` counts samples handed to
/// the device, including the buffer just filled, which has not been heard
/// yet — so subtract it, or video runs one device buffer ahead of sound.
fn advance_clock(state: &AudioState, clock: &Clock, just_queued: usize) {
    let per_second = (state.rate as u64 * state.channels as u64).max(1);
    let consumed = state.ring.consumed().saturating_sub(just_queued as u64);
    clock.set_audio_pts(state.anchor() + consumed as f64 / per_second as f64);
}

/// How likely a device is to be the one the desktop actually plays through.
/// `None` rejects it outright.
fn rank_device(name: &str) -> Option<u32> {
    let name = name.to_ascii_lowercase();

    // ALSA's `null` PCM. It opens, it accepts any format, and it discards
    // every sample — so a "first device that works" fallback lands on it
    // and playback is silent with nothing in the log to explain why.
    if name.contains("discard all samples") || name == "null" {
        return None;
    }

    Some(if name.contains("pipewire") {
        100
    } else if name.contains("pulse") {
        90
    } else if name.contains("default") {
        // ALSA's own default. On a PipeWire system this is usually dmix
        // and usually broken, but on a bare-ALSA one it is exactly right.
        50
    } else if name.contains("hdmi") {
        // Real, but rarely where the user is listening.
        10
    } else {
        20
    })
}
