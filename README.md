# Owl Player

A media player for Raven Linux. Built straight on the FFmpeg libraries —
`libavformat`, `libavcodec`, `libswscale`, `libswresample` — with its own
GPU renderer, wearing Raven Glass.

## Layout

    crates/owl-media    The engine. Demux, decode, resample, A/V clock.
                        No GTK, no GL.
    crates/owl-render   The video plane. GL textures, colour conversion,
                        HDR tone mapping. No GTK.
    crates/owl-player   The app. GTK4 + libadwaita.

The split exists to enforce one rule: **the decode threads never touch
GTK, and the GTK thread never blocks on a decoder.**

## How it plays

    demux ──packets──▶ video decode ──frames──▶ [queue] ──▶ GL thread
      │                                                     (presents)
      └───packets──▶ audio decode ──samples──▶ [ring] ──▶ sound card
                                                           (master clock)

Demuxing is one thread because an `AVFormatContext` is not thread safe and
seeking needs exclusive use of it. Video and audio decode are separate so a
slow frame cannot starve the sound card — the one consumer that must never
be late.

**Audio is the master clock.** A sound card consumes samples at a rate the
program cannot change, so "how much audio has actually been played" *is*
the definition of now, and video is fitted to it by dropping or repeating
frames. With no audio track the clock falls back to the monotonic one.

**Seeking** uses a generation counter. The demuxer bumps it, every packet
carries the generation it was read in, and a decoder seeing a newer one
flushes its codec and drops what it held. No thread waits for another to
acknowledge a seek. Because a seek lands on the nearest keyframe *at or
before* the target — which with a long GOP can be many seconds early — the
decoders then run forward and discard frames until they reach the target.

**Frames are never copied** between the decoder and the texture upload.
`VideoFrame` holds a reference to the decoded `AVFrame`; moving one between
threads costs a pointer, not the 12 MB a 4:2:0 4K frame would.

**Colour** is done on the GPU. The shader handles 8/10/12-bit planar and
bi-planar YUV and packed RGB, derives its conversion matrix from the luma
weights rather than tabulating one per space, and tone maps PQ and HLG to
SDR by luminance so bright skies stay blue instead of going white.
`libswscale` is reached only for pixel formats exotic enough that the
shader does not know them, and converts them to NV12 or P010.

**Subtitles** come in two unrelated kinds under one name. Text formats —
SRT, WebVTT, MOV_TEXT, ASS — are normalised by their decoders into ASS
dialogue lines, so there is one text path and it is the ASS one; the
override tags that appear in real dialogue (italic, bold, underline, line
breaks, `\an` alignment) become Pango markup and the rest are dropped,
because a viewer would rather miss a karaoke effect than read `{\k42}`
across the screen. Text cues are drawn by GTK, which brings shaping, RTL
and font fallback with it. Bitmap formats — PGS, DVB, VOBSUB — are
paletted images composited by the renderer, positioned against the
*picture* rather than the widget so they never land in the letterbox bars.

Cue endings differ by format and are handled with one rule: SRT and ASS
state an end time, Matroska puts it on the packet, and PGS states nothing
at all and simply replaces the cue later. An open-ended cue gets a long
provisional end and the next one truncates it.

A track marked **forced** is switched on automatically, because that
disposition means signs and foreign-language lines for a soundtrack you
otherwise understand. A merely "default" track is not: that flag is set on
full subtitle tracks all the time.

**Hardware decoding** is tried in order — VA-API, NVDEC, VDPAU, Vulkan —
and falls back to the CPU rather than failing to open. On a hybrid machine
each DRM render node is tried by name, because the default is routinely the
discrete card, which on the open NVIDIA driver cannot decode at all.

**Music gets a visualiser.** A file with no picture — or whose only video
stream is cover art — draws a spectrum instead of a black rectangle. The
samples are tapped in the audio callback, so the bars move with what is
actually audible rather than with what has merely been decoded, and the
analysis happens on the GL thread where being late costs nothing.

Bands are spaced logarithmically, because pitch is: linear bins would
spend most of the display on the top two octaves, where music has almost
nothing, and crush the bass into one bar. Levels are in decibels over a
60 dB range for the same reason — a linear bar makes everything except
the kick drum invisible. Attack is fast and decay slow, the way a meter
behaves. The transform is normalised against full scale, without which
every bar pins to the ceiling while still moving convincingly enough to
look correct.

**Finding something to play** is the browser, not just a file dialog. The
sidebar navigates: Library and Local Files open your home folder, Movies
and TV Shows open Videos, Music Videos opens Music. Rows that are not
wired to anything yet — Playlists, Watch Later, Favorites, Network — are
insensitive rather than absent, because a row that looks live and does
nothing is worse than one that looks pending.

Activating a file queues everything playable beside it, in the order
shown, starting where you clicked — so one click on episode three plays
the series from there. Sorting is natural, so "Episode 2" comes before
"Episode 10".

**Audio** is not opened through `default_output_device()`. On a PipeWire
desktop that resolves to ALSA's `default` PCM, which routes through dmix,
which cannot open the card because PipeWire is already holding it: the
obvious call is the one that always fails. Devices are ranked instead,
PipeWire and Pulse first, and tried until one accepts a stream. ALSA's
`null` PCM is excluded by name — it opens, accepts any format, and
discards every sample, so a "first device that works" fallback lands on it
and plays silence with nothing in the log to explain why.

**The chrome gets out of the way.** While a film is playing every piece of
it belongs to an edge and appears when the pointer goes near that edge:
left for the sidebar, bottom for the transport, top for the title and the
window controls, right for the queue. Nothing is on a timer, which makes
it predictable in a way a timeout is not — the one exception is the
pointer itself, which has no edge to belong to and so is hidden once it
stops moving.

When nothing is playing, everything is simply shown: immersion is for
when there is something to be immersed in. The sidebar is in a Revealer
rather than merely faded, so the picture takes its width rather than
leaving a dead column. In fullscreen the sidebar and queue stay away
whatever the pointer does; only the transport answers.

## Building

Needs `libclang` for bindgen, which reads the FFmpeg headers. Raven ships
`llvm-libs` but not `clang`:

    rvn --user -i clang

`.cargo/config.toml` points `LIBCLANG_PATH` at that per-user prefix and
tells libclang where its own builtin headers live. A later system-wide
`rvn -i clang` takes precedence; a stale path is harmless.

    make            # build
    make run FILE=some.mkv
    make smoke FILE=some.mkv   # headless: engine only, no window, no GL
    make test
    sudo make install

`lazy.toml` mirrors the Makefile, as in every other Raven app, so imlazy
works just as well — keep the two in step:

    imlazy build
    imlazy run -- some.mkv
    imlazy smoke -- some.mkv
    imlazy audio               # which output devices cpal can actually open
    imlazy test

## Keys

    Space, K   play or pause      F, F11  fullscreen
    ← →        back/forward 10s   M       mute
    ↑ ↓        back/forward 60s   O       open files
    N, P       next/previous      Esc     leave fullscreen

## Configuration

`~/.config/raven/desktop.toml` is owned by Raven Settings and read only for
the look — accent, light/dark, transparency — so the player matches the
rest of the desktop. `~/.config/raven/player.toml` is the player's own:
volume, and where each file was left.
