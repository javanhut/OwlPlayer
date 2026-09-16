//! Headless check that the engine actually decodes: `cargo run -p
//! owl-media --example smoke -- FILE`. Prints what it found, plays for a
//! few seconds, seeks, and reports how the clock and the frame queue
//! behaved. No GTK, no window.

use std::time::{Duration, Instant};

use owl_media::{Player, TrackKind, TrackPreference};

fn main() -> anyhow::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = std::env::args().nth(1).ok_or("usage: smoke FILE")?;
    let path = std::path::PathBuf::from(path);

    let info = owl_media::probe(&path)?;
    println!("── {} ──", info.display_title());
    println!("  container {}  ·  {:.2}s  ·  {} kb/s", info.container, info.duration, info.bit_rate / 1000);
    println!("  chips: {}", info.chips().join(" · "));
    for t in &info.tracks {
        println!("  [{}] {:?}  {}", t.index, t.kind, t.label());
    }
    for c in &info.chapters {
        println!("  chapter {:.1}–{:.1}  {}", c.start, c.end, c.title);
    }

    let mut player = Player::new();
    player.open(&path, TrackPreference::default())?;

    // Subtitles are off unless the file marks a track forced, so turn on
    // the first one there is to exercise the path.
    if let Some(track) = info.tracks_of(TrackKind::Subtitle).next() {
        println!("  enabling subtitle track {} ({})", track.index, track.label());
        player.set_track(TrackKind::Subtitle, Some(track.index))?;
    }
    player.play();

    let mut subtitle_generation = 0u64;
    let mut cues_seen = 0u32;

    let mut frames = 0u32;
    let mut first_frame: Option<Duration> = None;
    let mut last_pts = f64::NAN;
    let started = Instant::now();

    // Pretend to be a 60 Hz display asking for a picture each refresh.
    while started.elapsed() < Duration::from_secs(3) {
        if let Some(frame) = player.frame_for_now() {
            if first_frame.is_none() {
                first_frame = Some(started.elapsed());
                let (dw, dh) = frame.display_size();
                println!(
                    "  first frame {}x{} (display {dw}x{dh}) · {:?} · {}-bit{}",
                    frame.width(),
                    frame.height(),
                    frame.layout,
                    frame.layout.bit_depth(),
                    frame.color.hdr_label().map(|h| format!(" · {h}")).unwrap_or_default(),
                );
            }
            last_pts = frame.pts;
            frames += 1;
        }
        if let Some((generation, cues)) = player.subtitles_if_changed(subtitle_generation) {
            subtitle_generation = generation;
            for cue in &cues {
                cues_seen += 1;
                match &cue.content {
                    owl_media::SubtitleContent::Text { markup, alignment } => println!(
                        "  [sub {:.2}-{:.2}s {alignment:?}] {}",
                        cue.start,
                        cue.end,
                        markup.replace('\n', " / ")
                    ),
                    owl_media::SubtitleContent::Bitmap { rects, reference } => println!(
                        "  [sub {:.2}-{:.2}s] {} bitmap rect(s), reference {}x{}",
                        cue.start,
                        cue.end,
                        rects.len(),
                        reference.0,
                        reference.1
                    ),
                }
            }
        }
        for event in player.poll_events() {
            if !matches!(event, owl_media::Event::Opened(_)) {
                println!("  event: {event:?}");
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("  subtitle cues shown: {cues_seen}");

    println!("\n  decoded {frames} frames in 3s · clock {:.2}s · last pts {last_pts:.2}s", player.position());
    println!("  time to first frame: {:?}", first_frame.unwrap_or_default());

    // Seek, and check the clock and the frames follow it.
    let target = (player.duration() * 0.6).max(1.0);
    println!("\n  seeking to {target:.2}s");
    player.seek(target);
    std::thread::sleep(Duration::from_millis(600));
    let mut after = f64::NAN;
    for _ in 0..40 {
        if let Some(f) = player.frame_for_now() {
            after = f.pts;
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("  after seek: clock {:.2}s · frame pts {after:.2}s", player.position());

    if let Some(audio) = info.tracks_of(TrackKind::Audio).next() {
        println!("\n  switching audio to track {}", audio.index);
        player.set_track(TrackKind::Audio, Some(audio.index))?;
    }

    player.pause();
    let paused_at = player.position();
    std::thread::sleep(Duration::from_millis(400));
    println!("  paused at {paused_at:.2}s, still {:.2}s after 400ms", player.position());

    player.close();
    println!("\n  closed cleanly");
    Ok(())
}
