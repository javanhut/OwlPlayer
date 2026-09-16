//! The threads. One demuxer, one video decoder, one audio decoder.
//!
//! Seeking is the part worth reading. A seek has to invalidate work that
//! is already in flight — packets sitting in a channel, frames sitting in
//! a queue, samples sitting in the ring — without stopping the threads
//! that produced them. It does that with a generation counter: the
//! demuxer bumps it, every packet carries the generation it was read in,
//! and a decoder that sees a newer generation flushes its codec and drops
//! whatever it was holding. No thread ever waits for another to
//! acknowledge a seek, so seeking stays responsive while 4K frames are
//! still being decoded from the old position.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use ffmpeg_next as ff;
use parking_lot::{Condvar, Mutex};

use crate::audio::Sink;
use crate::clock::Clock;
use crate::error::{Error, Result, is_again, is_eof};
use crate::frame::{ColorInfo, PixelLayout, VideoFrame};
use crate::hwaccel;
use crate::media::{MediaInfo, TrackKind};
use crate::subtitle::{Alignment, BitmapRect, SubtitleContent, SubtitleCue, ass_dialogue_text, ass_to_markup};
use crate::player::Event;

/// Frames decoded ahead of the clock. Four is enough to ride out a slow
/// frame without the memory cost mattering even at 4K, and short enough
/// that a seek does not have much to throw away.
const VIDEO_QUEUE: usize = 4;
/// Packets buffered between the demuxer and a decoder.
const PACKET_QUEUE: usize = 64;

/// A packet plus the seek generation it belongs to.
struct Tagged {
    packet: ff::Packet,
    generation: u64,
}

/// Everything the UI thread and the engine threads share.
pub struct Shared {
    pub clock: Arc<Clock>,
    pub stop: AtomicBool,
    /// Read by the audio callback every buffer, so pausing is immediate
    /// rather than "after the ring drains". Shared as its own `Arc` because
    /// the realtime thread must not hold a reference to the whole engine.
    pub paused: Arc<AtomicBool>,
    /// Samples on their way to the sound card, for the visualiser. Shared
    /// as its own `Arc` for the same reason as `paused`: the realtime
    /// thread must not hold a reference to the whole engine.
    pub visualizer: Arc<crate::visualizer::Tap>,
    pub generation: AtomicU64,
    /// Where the last seek was aiming, as f64 bits.
    ///
    /// A seek lands on the nearest keyframe *at or before* the target, and
    /// with a long GOP that can be many seconds early — a file encoded at
    /// x264's default keyframe interval may have only one keyframe in it,
    /// so every seek decodes from zero. The decoders run forward from
    /// there but throw the frames away until they reach this, which is
    /// what makes a seek land where the viewer asked rather than at the
    /// previous keyframe.
    pub seek_target: AtomicU64,
    /// Linear gain, applied to samples on the decode thread.
    pub volume: AtomicU64,
    video: Mutex<std::collections::VecDeque<VideoFrame>>,
    video_space: Condvar,
    subtitles: Mutex<SubtitleState>,
}

/// Cues waiting to be shown, and the ones on screen now.
///
/// The visible set is kept here rather than recomputed by the caller
/// because bitmap cues are megabytes each: handing them out every frame
/// would copy a PGS image sixty times a second to show the same picture.
/// Instead the set carries a generation, and the renderer only asks for a
/// copy when that number changes.
#[derive(Default)]
struct SubtitleState {
    pending: std::collections::VecDeque<SubtitleCue>,
    active: Vec<SubtitleCue>,
    generation: u64,
}

impl Shared {
    pub fn new(clock: Arc<Clock>) -> Arc<Shared> {
        Arc::new(Shared {
            clock,
            stop: AtomicBool::new(false),
            paused: Arc::new(AtomicBool::new(true)),
            visualizer: Arc::new(crate::visualizer::Tap::default()),
            generation: AtomicU64::new(0),
            seek_target: AtomicU64::new(0.0f64.to_bits()),
            volume: AtomicU64::new(1.0f64.to_bits()),
            video: Mutex::new(std::collections::VecDeque::with_capacity(VIDEO_QUEUE)),
            video_space: Condvar::new(),
            subtitles: Mutex::new(SubtitleState::default()),
        })
    }

    pub fn volume(&self) -> f64 {
        f64::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, v: f64) {
        self.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn seek_target(&self) -> f64 {
        f64::from_bits(self.seek_target.load(Ordering::Relaxed))
    }

    /// Called on the GL thread once per display refresh.
    ///
    /// Returns the newest frame that is due, discarding any that are
    /// already late — that is the drop half of A/V sync. Returning `None`
    /// means nothing new is due yet and the renderer should keep showing
    /// what it has, which is the duplicate half.
    pub fn frame_for_now(&self) -> Option<VideoFrame> {
        let now = self.clock.now();
        let mut queue = self.video.lock();
        let mut chosen = None;
        while let Some(front) = queue.front() {
            if front.pts <= now {
                chosen = queue.pop_front();
            } else {
                break;
            }
        }
        // Nothing is due, but if the queue head is wildly in the future the
        // clock has been moved behind the decoded frames — a backwards
        // seek that has not landed yet. Show the head rather than freeze.
        if chosen.is_none() {
            if let Some(front) = queue.front() {
                if front.pts > now + 1.0 {
                    chosen = queue.pop_front();
                }
            }
        }
        if chosen.is_some() {
            self.video_space.notify_one();
        }
        chosen
    }

    /// File a decoded cue.
    ///
    /// Formats differ on how a cue ends. SRT and ASS state an end time.
    /// PGS usually does not: a cue runs until the next one replaces it,
    /// and a cue with no rectangles is how the stream says "nothing now".
    /// So an open-ended cue is given a long provisional end, and filing
    /// the next one truncates it — which handles both conventions with
    /// one rule and no per-codec branching.
    pub fn push_subtitle(&self, cue: SubtitleCue) {
        let mut state = self.subtitles.lock();
        let start = cue.start;
        for previous in state.pending.iter_mut().rev() {
            if previous.end > start {
                previous.end = start;
            } else {
                break;
            }
        }
        let mut visible_changed = false;
        for previous in state.active.iter_mut() {
            if previous.end > start {
                previous.end = start;
                visible_changed = true;
            }
        }
        // An empty cue exists only to end the one before it.
        let empty = match &cue.content {
            SubtitleContent::Text { markup, .. } => markup.is_empty(),
            SubtitleContent::Bitmap { rects, .. } => rects.is_empty(),
        };
        if !empty {
            state.pending.push_back(cue);
        }
        if visible_changed {
            state.generation += 1;
        }
    }

    /// The cues visible at `t`, but only when they differ from what the
    /// caller last saw. `None` means "carry on showing what you have".
    pub fn subtitles_if_changed(&self, t: f64, last_seen: u64) -> Option<(u64, Vec<SubtitleCue>)> {
        let mut state = self.subtitles.lock();
        let before = state.active.len();
        state.active.retain(|c| c.end > t);
        let mut changed = state.active.len() != before;

        while let Some(front) = state.pending.front() {
            if front.start > t {
                break;
            }
            let cue = state.pending.pop_front().expect("checked");
            // Already over by the time we got here — a seek landed past it.
            if cue.end > t {
                state.active.push(cue);
                changed = true;
            }
        }
        if changed {
            state.generation += 1;
        }
        if state.generation == last_seen {
            return None;
        }
        Some((state.generation, state.active.clone()))
    }

    pub fn clear_subtitles(&self) {
        let mut state = self.subtitles.lock();
        state.pending.clear();
        state.active.clear();
        state.generation += 1;
    }

    pub fn clear_video(&self) {
        self.video.lock().clear();
        self.video_space.notify_all();
    }

    /// Start a seek: bump the generation, record the target and drop the
    /// decoded backlog, all under the queue lock.
    ///
    /// These have to happen together. A decoder finishing a frame reads
    /// the generation while holding the same lock, so there is no instant
    /// at which it can decide a pre-seek frame is still current and push
    /// it after the queue has been cleared. Doing this without the lock
    /// leaves exactly that window, and the symptom is one stale frame
    /// flashing up after every seek.
    pub fn begin_seek(&self, target: f64) -> u64 {
        let generation = {
            let mut queue = self.video.lock();
            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.seek_target.store(target.to_bits(), Ordering::SeqCst);
            queue.clear();
            self.video_space.notify_all();
            generation
        };
        // Separate lock, taken after the video one is released, so the two
        // are never held at once and can never deadlock against each other.
        self.clear_subtitles();
        generation
    }

    fn push_video(&self, frame: VideoFrame, generation: u64) {
        let mut queue = self.video.lock();
        while queue.len() >= VIDEO_QUEUE && !self.stopping() {
            self.video_space.wait_for(&mut queue, std::time::Duration::from_millis(50));
        }
        // Re-checked under the lock: a seek may have happened while this
        // frame was being decoded, in which case it belongs to a position
        // the viewer has already left.
        if !self.stopping() && self.generation.load(Ordering::SeqCst) == generation {
            queue.push_back(frame);
        }
    }
}

/// What the demux thread accepts while it is running.
pub enum DemuxCommand {
    Seek(f64),
    Stop,
}

pub struct Handles {
    pub demux: std::thread::JoinHandle<()>,
    pub video: Option<std::thread::JoinHandle<()>>,
    pub audio: Option<std::thread::JoinHandle<()>>,
    pub subtitle: Option<std::thread::JoinHandle<()>>,
}

/// Selected stream indices for this playback session.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub video: Option<usize>,
    pub audio: Option<usize>,
    pub subtitle: Option<usize>,
}

/// Open `path`, work out what to play, and start the threads.
pub fn start(
    path: PathBuf,
    wanted: Selection,
    shared: Arc<Shared>,
    events: Sender<Event>,
    commands: Receiver<DemuxCommand>,
) -> Result<(MediaInfo, Selection, Handles)> {
    crate::init()?;
    let input =
        ff::format::input(&path).map_err(|source| Error::Open { path: path.clone(), source })?;
    let info = crate::media::describe(&path, &input);

    // Fall back to FFmpeg's own idea of the best stream, which weighs
    // disposition and codec quality, when the caller has no preference.
    let pick = |kind: ff::media::Type, want: Option<usize>| -> Option<usize> {
        want.filter(|i| input.stream(*i).is_some())
            .or_else(|| input.streams().best(kind).map(|s| s.index()))
    };
    let selection = Selection {
        video: pick(ff::media::Type::Video, wanted.video)
            .filter(|i| info.tracks.iter().any(|t| t.index == *i && t.kind == TrackKind::Video)),
        audio: pick(ff::media::Type::Audio, wanted.audio),
        subtitle: wanted.subtitle,
    };

    if selection.video.is_none() && selection.audio.is_none() {
        return Err(Error::NoPlayableStream(path));
    }

    let (video_tx, video_rx) = bounded::<Tagged>(PACKET_QUEUE);
    let (audio_tx, audio_rx) = bounded::<Tagged>(PACKET_QUEUE);
    let (subtitle_tx, subtitle_rx) = bounded::<Tagged>(PACKET_QUEUE);

    let video_handle = selection.video.map(|index| {
        let params = input.stream(index).expect("checked").parameters();
        let time_base = f64::from(input.stream(index).expect("checked").time_base());
        let sar = input.stream(index).expect("checked").parameters();
        let _ = sar;
        let shared = Arc::clone(&shared);
        let events = events.clone();
        std::thread::Builder::new()
            .name("owl-video".into())
            .spawn(move || video_loop(params, time_base, video_rx, shared, events))
            .expect("spawn video thread")
    });

    let audio_handle = selection.audio.map(|index| {
        let params = input.stream(index).expect("checked").parameters();
        let time_base = f64::from(input.stream(index).expect("checked").time_base());
        let shared = Arc::clone(&shared);
        let events = events.clone();
        std::thread::Builder::new()
            .name("owl-audio".into())
            .spawn(move || audio_loop(params, time_base, audio_rx, shared, events))
            .expect("spawn audio thread")
    });

    // The picture's size is the fallback coordinate space for bitmap
    // subtitles, for the many PGS streams that do not declare their own.
    let video_size = info
        .tracks
        .iter()
        .find(|t| Some(t.index) == selection.video)
        .map(|t| (t.width, t.height))
        .unwrap_or((1920, 1080));

    let subtitle_handle = selection.subtitle.and_then(|index| {
        let stream = input.stream(index)?;
        let params = stream.parameters();
        let time_base = f64::from(stream.time_base());
        let shared = Arc::clone(&shared);
        let events = events.clone();
        std::thread::Builder::new()
            .name("owl-subtitle".into())
            .spawn(move || subtitle_loop(params, time_base, video_size, subtitle_rx, shared, events))
            .ok()
    });

    let demux_shared = Arc::clone(&shared);
    let demux_events = events.clone();
    let duration = info.duration;
    let demux = std::thread::Builder::new()
        .name("owl-demux".into())
        .spawn(move || {
            demux_loop(
                input,
                selection,
                duration,
                video_tx,
                audio_tx,
                subtitle_tx,
                demux_shared,
                demux_events,
                commands,
            )
        })
        .expect("spawn demux thread");

    Ok((info, selection, Handles { demux, video: video_handle, audio: audio_handle, subtitle: subtitle_handle }))
}

#[allow(clippy::too_many_arguments)]
fn demux_loop(
    mut input: ff::format::context::Input,
    selection: Selection,
    duration: f64,
    video_tx: Sender<Tagged>,
    audio_tx: Sender<Tagged>,
    subtitle_tx: Sender<Tagged>,
    shared: Arc<Shared>,
    events: Sender<Event>,
    commands: Receiver<DemuxCommand>,
) {
    let mut eof = false;
    // Taken out of the `Option` when a decoder disconnects, so a dead
    // consumer stops being routed to instead of stopping the demuxer.
    let mut video_out = Some(video_tx);
    let mut audio_out = Some(audio_tx);
    let mut subtitle_out = Some(subtitle_tx);

    loop {
        if shared.stopping() {
            break;
        }

        // Commands first: a seek must not wait behind a full packet queue.
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                DemuxCommand::Stop => return,
                DemuxCommand::Seek(mut target) => {
                    target = target.clamp(0.0, if duration > 0.0 { duration } else { target });
                    // Everything read before this instant is now stale.
                    shared.begin_seek(target);
                    shared.clock.reset_to(target);

                    let ts = (target * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
                    // Seek backwards to the nearest keyframe at or before
                    // the target; decoding forward from there is what makes
                    // the landing frame-accurate rather than keyframe-accurate.
                    if let Err(e) = input.seek(ts, ..ts) {
                        log::warn!("seek to {target:.3}s failed: {e}");
                    }
                    eof = false;
                    let _ = events.send(Event::Seeked(target));
                }
            }
        }

        if eof {
            // Stay alive after the last packet: the decoders may still be
            // draining, and a seek can bring the file back to life.
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }

        let generation = shared.generation.load(Ordering::SeqCst);
        let Some((stream, packet)) = input.packets().next() else {
            eof = true;
            let _ = events.send(Event::EndOfFile);
            continue;
        };

        let index = stream.index();
        let slot = if Some(index) == selection.video {
            Some(&mut video_out)
        } else if Some(index) == selection.audio {
            Some(&mut audio_out)
        } else if Some(index) == selection.subtitle {
            Some(&mut subtitle_out)
        } else {
            None
        };

        if let Some(slot) = slot {
            let Some(tx) = slot.as_ref() else { continue };
            let mut tagged = Tagged { packet, generation };
            // Never block forever on a full queue: a seek arriving while
            // we are parked here would not be seen until the decoder drained.
            loop {
                match tx.try_send(tagged) {
                    Ok(()) => break,
                    Err(TrySendError::Full(back)) => {
                        tagged = back;
                        if shared.stopping() || !commands.is_empty() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        // That decoder has gone — most often because the
                        // sound card would not open, which is not a reason
                        // to stop showing the picture. Drop the stream and
                        // keep demuxing for whoever is left. Returning here
                        // instead would kill demuxing outright, stalling
                        // video and silently swallowing every later seek.
                        *slot = None;
                        break;
                    }
                }
            }
            if video_out.is_none() && audio_out.is_none() && subtitle_out.is_none() {
                return;
            }
        }
    }
}

fn video_loop(
    params: ff::codec::Parameters,
    time_base: f64,
    packets: Receiver<Tagged>,
    shared: Arc<Shared>,
    events: Sender<Event>,
) {
    let mut decoder = match open_video(params) {
        Ok(d) => d,
        Err(e) => {
            let _ = events.send(Event::Error(format!("video decoder: {e}")));
            return;
        }
    };

    let mut generation = shared.generation.load(Ordering::SeqCst);
    let mut scaler: Option<ff::software::scaling::Context> = None;

    while let Ok(tagged) = packets.recv() {
        if shared.stopping() {
            return;
        }
        let current = shared.generation.load(Ordering::SeqCst);
        if tagged.generation != current {
            // Stale: belongs to a position we have seeked away from.
            if generation != current {
                decoder.decoder.flush();
                generation = current;
            }
            continue;
        }
        if generation != current {
            decoder.decoder.flush();
            generation = current;
        }

        if decoder.decoder.send_packet(&tagged.packet).is_err() {
            continue;
        }

        let mut frame = ff::frame::Video::empty();
        loop {
            match decoder.decoder.receive_frame(&mut frame) {
                Ok(()) => {}
                Err(e) if is_again(&e) || is_eof(&e) => break,
                Err(e) => {
                    log::debug!("video decode: {e}");
                    break;
                }
            }
            if shared.generation.load(Ordering::SeqCst) != generation {
                break;
            }
            let decoded = std::mem::replace(&mut frame, ff::frame::Video::empty());
            match prepare(decoded, &decoder, time_base, &mut scaler) {
                Ok(ready) => {
                    // Half a frame of slack at 60fps, so the frame that
                    // straddles the target is kept rather than skipped.
                    if ready.pts >= shared.seek_target() - 0.008 {
                        shared.push_video(ready, generation);
                    }
                }
                Err(e) => log::warn!("frame conversion: {e}"),
            }
        }
    }
}

struct VideoDecoder {
    decoder: ff::codec::decoder::Video,
    hw: Option<hwaccel::HwAccel>,
    color: ColorInfo,
}

fn open_video(params: ff::codec::Parameters) -> Result<VideoDecoder> {
    let mut ctx = ff::codec::context::Context::from_parameters(params)?;

    // Let FFmpeg use every core it can for software decode; it is the
    // difference between 4K H.264 playing and not.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    unsafe {
        let raw = ctx.as_mut_ptr();
        (*raw).thread_count = threads.min(16) as i32;
    }

    let codec_id = ctx.id();
    let hw = ff::codec::decoder::find(codec_id).and_then(|codec| unsafe {
        hwaccel::attach(ctx.as_mut_ptr(), codec.as_ptr())
    });

    let decoder = ctx.decoder().video()?;
    let color = ColorInfo {
        space: decoder.color_space(),
        range: decoder.color_range(),
        primaries: decoder.color_primaries(),
        transfer: decoder.color_transfer_characteristic(),
    };
    Ok(VideoDecoder { decoder, hw, color })
}

/// Get a decoded frame into a shape the renderer can sample: off the GPU
/// if it is still on one, and through swscale if its pixel format is not
/// one the shader handles.
fn prepare(
    frame: ff::frame::Video,
    decoder: &VideoDecoder,
    time_base: f64,
    scaler: &mut Option<ff::software::scaling::Context>,
) -> Result<VideoFrame> {
    let pts_ticks = unsafe {
        let raw = frame.as_ptr();
        if (*raw).best_effort_timestamp != ff::ffi::AV_NOPTS_VALUE {
            (*raw).best_effort_timestamp
        } else {
            (*raw).pts
        }
    };
    let pts = if pts_ticks == ff::ffi::AV_NOPTS_VALUE { 0.0 } else { pts_ticks as f64 * time_base };

    // Off the GPU if it is still on one. Otherwise the decoded frame is
    // already what we want and is moved through untouched.
    let frame = if decoder.hw.is_some() && hwaccel::is_hw_format(frame.format()) {
        unsafe { hwaccel::transfer(&frame)? }
    } else {
        frame
    };
    let src = &frame;

    let sar = unsafe {
        let r = (*src.as_ptr()).sample_aspect_ratio;
        if r.num > 0 && r.den > 0 { (r.num as u32, r.den as u32) } else { (1, 1) }
    };

    // The common case: the decoder already emitted something the shader
    // samples directly, so the frame is handed on as-is. No copy happens
    // anywhere between the decoder and the texture upload.
    if let Some(layout) = PixelLayout::from_ffmpeg(src.format()) {
        return Ok(VideoFrame::new(frame, layout, decoder.color, pts, sar));
    }

    // An exotic format — anything from 12-bit 4:2:2 to paletted GIF.
    // Convert once to NV12 (or P010 when the source has depth worth
    // keeping) and let the shader stay simple.
    // `comp[0].depth` is the real bit depth of the luma component. The
    // safe wrapper only exposes bits-per-pixel, which counts subsampled
    // chroma too and so cannot tell 8-bit 4:4:4 from 10-bit 4:2:0.
    let deep = src
        .format()
        .descriptor()
        .map(|d| unsafe { (*d.as_ptr()).comp[0].depth > 8 })
        .unwrap_or(false);
    let target = if deep { ff::format::Pixel::P010LE } else { ff::format::Pixel::NV12 };

    use ff::software::scaling::{Context as Scaler, Flags};
    if scaler.is_none() {
        *scaler = Some(Scaler::get(
            src.format(),
            src.width(),
            src.height(),
            target,
            src.width(),
            src.height(),
            Flags::BICUBIC,
        )?);
    }
    let ctx = scaler.as_mut().expect("just built");
    // `cached` is sws_getCachedContext: a no-op when the geometry has not
    // changed, and a rebuild when it has. Streams really do change
    // resolution mid-file — adaptive sources and some broadcast captures —
    // so this is checked per frame rather than assumed once.
    ctx.cached(src.format(), src.width(), src.height(), target, src.width(), src.height(), Flags::BICUBIC);
    let mut out = ff::frame::Video::empty();
    ctx.run(src, &mut out)?;

    let layout = PixelLayout::from_ffmpeg(target).expect("NV12 and P010 are always known");
    Ok(VideoFrame::new(out, layout, decoder.color, pts, sar))
}

fn audio_loop(
    params: ff::codec::Parameters,
    time_base: f64,
    packets: Receiver<Tagged>,
    shared: Arc<Shared>,
    events: Sender<Event>,
) {
    let sink = match Sink::open(
        Arc::clone(&shared.clock),
        Arc::clone(&shared.paused),
        Arc::clone(&shared.visualizer),
    ) {
        Ok(s) => s,
        Err(e) => {
            // No sound card is not fatal: the file still plays, on the
            // monotonic clock, silently.
            log::warn!("audio unavailable: {e}");
            let _ = events.send(Event::Error(format!("audio: {e}")));
            return;
        }
    };

    let mut decoder = match ff::codec::context::Context::from_parameters(params).and_then(|c| c.decoder().audio()) {
        Ok(d) => d,
        Err(e) => {
            let _ = events.send(Event::Error(format!("audio decoder: {e}")));
            return;
        }
    };

    let out_rate = sink.state.rate;
    let out_channels = sink.state.channels;
    let out_layout = ff::ChannelLayout::default(out_channels as i32);

    let mut resampler = match ff::software::resampling::Context::get(
        decoder.format(),
        decoder.channel_layout(),
        decoder.rate(),
        ff::format::Sample::F32(ff::format::sample::Type::Packed),
        out_layout,
        out_rate,
    ) {
        Ok(r) => r,
        Err(e) => {
            let _ = events.send(Event::Error(format!("resampler: {e}")));
            return;
        }
    };

    let mut generation = shared.generation.load(Ordering::SeqCst);
    let mut interleaved: Vec<f32> = Vec::new();
    let stopping = || shared.stopping();

    while let Ok(tagged) = packets.recv() {
        if shared.stopping() {
            return;
        }
        let current = shared.generation.load(Ordering::SeqCst);
        if tagged.generation != current {
            continue;
        }
        if generation != current {
            decoder.flush();
            generation = current;
            // The ring still holds sound from before the seek; drop it and
            // re-anchor the clock to where the new packets start.
            let pts = tagged.packet.pts().map(|t| t as f64 * time_base).unwrap_or_else(|| shared.clock.now());
            sink.state.flush_to(pts);
        }

        if decoder.send_packet(&tagged.packet).is_err() {
            continue;
        }

        let mut frame = ff::frame::Audio::empty();
        loop {
            match decoder.receive_frame(&mut frame) {
                Ok(()) => {}
                Err(e) if is_again(&e) || is_eof(&e) => break,
                Err(e) => {
                    log::debug!("audio decode: {e}");
                    break;
                }
            }
            if shared.generation.load(Ordering::SeqCst) != generation {
                break;
            }

            // The first frame after a flush defines where the clock is.
            if sink.state.ring.is_empty() && sink.state.ring.consumed() == 0 {
                let pts_ticks = unsafe { (*frame.as_ptr()).pts };
                if pts_ticks != ff::ffi::AV_NOPTS_VALUE {
                    sink.state.set_anchor(pts_ticks as f64 * time_base);
                }
            }

            let frame_pts = unsafe { (*frame.as_ptr()).pts };
            if frame_pts != ff::ffi::AV_NOPTS_VALUE {
                let pts = frame_pts as f64 * time_base;
                let target = shared.seek_target();
                if pts + frame.samples() as f64 / decoder.rate().max(1) as f64 <= target {
                    continue;
                }
            }

            let mut out = ff::frame::Audio::empty();
            if resampler.run(&frame, &mut out).is_err() {
                continue;
            }
            let samples = out.samples() * out_channels as usize;
            if samples == 0 {
                continue;
            }
            // `data(0)` on a packed frame is every channel interleaved.
            let raw = &out.data(0)[..samples * std::mem::size_of::<f32>()];
            interleaved.clear();
            interleaved.extend(
                raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );

            let gain = shared.volume() as f32;
            if (gain - 1.0).abs() > f32::EPSILON {
                for s in &mut interleaved {
                    *s *= gain;
                }
            }
            sink.push_blocking(&interleaved, &stopping);
        }
    }
}

/// Decode the selected subtitle stream.
///
/// Subtitles are a rounding error of bandwidth next to video, so this
/// thread spends nearly all its time blocked. It exists anyway, rather
/// than folding the work into the demuxer, because a PGS keyframe is a
/// full-screen image and decoding one on the demux thread would stall
/// packet delivery to video and audio while it happened.
fn subtitle_loop(
    params: ff::codec::Parameters,
    time_base: f64,
    video_size: (u32, u32),
    packets: Receiver<Tagged>,
    shared: Arc<Shared>,
    events: Sender<Event>,
) {
    let mut decoder = match ff::codec::context::Context::from_parameters(params).and_then(|c| c.decoder().subtitle()) {
        Ok(d) => d,
        Err(e) => {
            let _ = events.send(Event::Error(format!("subtitle decoder: {e}")));
            return;
        }
    };

    // A bitmap stream states the resolution its coordinates refer to; a
    // text one leaves it at zero. Falling back to the picture's own size
    // is right for the PGS streams that omit it.
    let reference = unsafe {
        let ctx = decoder.as_ptr();
        let (w, h) = ((*ctx).width as u32, (*ctx).height as u32);
        if w > 0 && h > 0 { (w, h) } else { video_size }
    };

    let mut generation = shared.generation.load(Ordering::SeqCst);

    while let Ok(tagged) = packets.recv() {
        if shared.stopping() {
            return;
        }
        let current = shared.generation.load(Ordering::SeqCst);
        if tagged.generation != current {
            continue;
        }
        if generation != current {
            generation = current;
        }

        let mut subtitle = ff::Subtitle::new();
        match decoder.decode(&tagged.packet, &mut subtitle) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                log::debug!("subtitle decode: {e}");
                continue;
            }
        }

        // Timing is relative to the cue, not the file: `pts` places it and
        // the display times are millisecond offsets from there.
        let base = match subtitle.pts() {
            Some(pts) => pts as f64 / f64::from(ff::ffi::AV_TIME_BASE),
            None => tagged.packet.pts().map(|t| t as f64 * time_base).unwrap_or(0.0),
        };
        let start = base + subtitle.start() as f64 / 1000.0;
        let raw_end = subtitle.end();
        let end = if raw_end > 0 && raw_end != u32::MAX {
            base + raw_end as f64 / 1000.0
        } else if let Some(duration) = Some(tagged.packet.duration()).filter(|d| *d > 0) {
            // Text formats in Matroska leave `end_display_time` at zero and
            // put the length on the packet instead. Without this a line
            // stays up until the next one replaces it, which with sparse
            // dialogue means it sits on screen for many seconds.
            start + duration as f64 * time_base
        } else {
            // Genuinely open-ended, as PGS is: the next cue truncates it.
            // See `push_subtitle`.
            start + 30.0
        };

        for cue in cues_from(&subtitle, start, end, reference) {
            shared.push_subtitle(cue);
        }
    }
}

/// Turn one decoded `AVSubtitle` into cues. A single subtitle can carry
/// several rectangles — two lines of dialogue, or a sign and a caption —
/// so the text ones are joined into one cue and the bitmaps are kept
/// together in another.
fn cues_from(subtitle: &ff::Subtitle, start: f64, end: f64, reference: (u32, u32)) -> Vec<SubtitleCue> {
    let mut lines: Vec<String> = Vec::new();
    let mut alignment = Alignment::default();
    let mut rects: Vec<BitmapRect> = Vec::new();

    for rect in subtitle.rects() {
        match rect {
            ff::codec::subtitle::Rect::Ass(ass) => {
                let (markup, align) = ass_to_markup(ass_dialogue_text(ass.get()));
                if !markup.is_empty() {
                    lines.push(markup);
                    alignment = align;
                }
            }
            ff::codec::subtitle::Rect::Text(text) => {
                let (markup, align) = ass_to_markup(text.get());
                if !markup.is_empty() {
                    lines.push(markup);
                    alignment = align;
                }
            }
            ff::codec::subtitle::Rect::Bitmap(bitmap) => {
                if let Some(converted) = unsafe { bitmap_to_rgba(&bitmap) } {
                    rects.push(converted);
                }
            }
            ff::codec::subtitle::Rect::None(_) => {}
        }
    }

    let mut cues = Vec::new();
    if !rects.is_empty() {
        cues.push(SubtitleCue { start, end, content: SubtitleContent::Bitmap { rects, reference } });
    } else if subtitle.rects().count() == 0 {
        // A subtitle carrying no rectangles at all is how PGS says "clear
        // the screen now". It is filed as an empty cue so `push_subtitle`
        // truncates its predecessor, then dropped.
        cues.push(SubtitleCue {
            start,
            end,
            content: SubtitleContent::Bitmap { rects: Vec::new(), reference },
        });
    }
    if !lines.is_empty() {
        cues.push(SubtitleCue {
            start,
            end,
            content: SubtitleContent::Text { markup: lines.join("\n"), alignment },
        });
    }
    cues
}

/// Expand a paletted subtitle image to straight RGBA.
///
/// Bitmap subtitles are PAL8: one byte of index per pixel and a 256-entry
/// palette of ARGB words. Almost every pixel indexes a fully transparent
/// entry — the glyphs are a small part of a full-width rectangle — so this
/// is cheap despite looking like a per-pixel loop.
///
/// # Safety
/// `bitmap` must come from a live `AVSubtitle`.
unsafe fn bitmap_to_rgba(bitmap: &ff::codec::subtitle::Bitmap) -> Option<BitmapRect> {
    unsafe {
        let rect = bitmap.as_ptr();
        let (w, h) = ((*rect).w as usize, (*rect).h as usize);
        if w == 0 || h == 0 {
            return None;
        }
        let indices = (*rect).data[0];
        let palette = (*rect).data[1] as *const u32;
        if indices.is_null() || palette.is_null() {
            return None;
        }
        let stride = (*rect).linesize[0] as usize;
        let colours = ((*rect).nb_colors as usize).min(256);
        if stride == 0 {
            return None;
        }

        // Everything past here is safe and unit-tested; this function's
        // only job is turning FFmpeg's raw pointers into slices.
        let indices = std::slice::from_raw_parts(indices, stride * h);
        let palette = std::slice::from_raw_parts(palette, colours);
        let rgba = crate::subtitle::expand_palette(indices, stride, palette, w, h);

        Some(BitmapRect {
            x: (*rect).x.max(0) as u32,
            y: (*rect).y.max(0) as u32,
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }
}
