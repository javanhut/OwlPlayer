//! OwlPlayer. `owl-player [FILE…]` opens media; a second invocation hands
//! its files to the running instance instead of starting another process,
//! so opening from the file manager adds to the queue rather than
//! spawning a second window that fights for the sound card.

mod browse;
mod config;
mod queue;
mod stage;
mod theme;
mod window;

use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use window::APP_ID;

/// What OwlPlayer tells the desktop it can open. FFmpeg handles far more
/// than this, but a MIME list is a claim about what the player is *for* —
/// it is what makes it the default for a double-clicked film — so it
/// names the containers people actually have, not every format libavformat
/// can probe.
pub const MIME_TYPES: &[&str] = &[
    "video/mp4",
    "video/x-matroska",
    "video/webm",
    "video/quicktime",
    "video/x-msvideo",
    "video/mpeg",
    "video/x-ms-wmv",
    "video/x-flv",
    "video/3gpp",
    "video/ogg",
    "video/x-ogm+ogg",
    "video/mp2t",
    "video/x-theora+ogg",
    "video/dv",
    "video/x-m4v",
    "application/x-matroska",
    "audio/mpeg",
    "audio/flac",
    "audio/x-vorbis+ogg",
    "audio/ogg",
    "audio/opus",
    "audio/x-wav",
    "audio/mp4",
    "audio/aac",
    "audio/x-aiff",
    "audio/x-ms-wma",
    "audio/x-musepack",
    "audio/x-ape",
    "audio/x-wavpack",
];

const USAGE: &str = "\
Owl Player — see more

Usage:
  owl-player [FILE…]            play files (or an empty window)
  owl-player open FILE…         same as above
  owl-player set-default        make Owl Player the default media player
  owl-player --help | --version

Keys:
  Space, K      play or pause        F, F11   fullscreen
  ← →           back/forward 10s     M        mute
  ↑ ↓           back/forward 60s     O        open files
  N, P          next/previous        Esc      leave fullscreen
";

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let mut args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return glib::ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("owl-player {}", env!("CARGO_PKG_VERSION"));
            return glib::ExitCode::SUCCESS;
        }
        Some("set-default") => return set_default(),
        // `open` is sugar; GApplication takes the files as plain arguments.
        Some("open") => {
            args.remove(1);
        }
        _ => {}
    }

    if let Err(e) = owl_media::init() {
        eprintln!("owl-player: cannot initialise FFmpeg: {e}");
        return glib::ExitCode::FAILURE;
    }

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_startup(|_| theme::apply());
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
        } else {
            let ui = window::build(app);
            window::register(&ui);
            ui.window.present();
        }
    });
    app.connect_open(|app, files, _hint| {
        let paths: Vec<std::path::PathBuf> = files.iter().filter_map(|f| f.path()).collect();
        let ui = match app.active_window() {
            Some(_) => window::current().expect("a window implies a Ui"),
            None => {
                let ui = window::build(app);
                window::register(&ui);
                ui.window.present();
                ui
            }
        };
        ui.window.present();
        ui.open_all(&paths);
    });

    app.run_with_args(&args)
}

/// Register OwlPlayer as the handler for everything in `MIME_TYPES`.
fn set_default() -> glib::ExitCode {
    let desktop = format!("{APP_ID}.desktop");
    let mut failed = 0;
    for mime in MIME_TYPES {
        let status = std::process::Command::new("xdg-mime")
            .args(["default", &desktop, mime])
            .status();
        match status {
            Ok(s) if s.success() => {}
            _ => failed += 1,
        }
    }
    if failed > 0 {
        eprintln!("owl-player: could not set {failed} of {} associations", MIME_TYPES.len());
        return glib::ExitCode::FAILURE;
    }
    println!("Owl Player is now the default for {} media types.", MIME_TYPES.len());
    glib::ExitCode::SUCCESS
}
