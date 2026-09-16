//! The video surface: a `GtkGLArea` with OwlPlayer's own renderer inside
//! it. GTK owns the window and the chrome; everything between these four
//! edges is drawn by `owl-render`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use owl_media::{Alignment, Player, SubtitleContent};
use owl_render::Renderer;

/// A text subtitle waiting to be shown, as Pango markup.
pub type TextCue = Option<(String, Alignment)>;

pub struct Stage {
    pub widget: gtk::GLArea,
    /// The current text subtitle. Bitmap cues never appear here — those
    /// are composited by the renderer, because they have to land inside
    /// the picture rather than inside the widget.
    pub text: Rc<RefCell<TextCue>>,
    /// Bumped whenever `text` changes, so the window can rebuild the label
    /// only when there is something new rather than once a frame.
    pub text_revision: Rc<Cell<u64>>,
    /// Where the picture sits inside the widget, in widget pixels:
    /// x, y, width, height. Text subtitles are positioned against this,
    /// not against the widget, so they stay out of the letterbox bars.
    pub video_rect: Rc<Cell<(f32, f32, f32, f32)>>,
}

impl Stage {
    pub fn new(player: Rc<RefCell<Player>>) -> Stage {
        let area = gtk::GLArea::new();
        area.add_css_class("owl-stage");
        area.set_hexpand(true);
        area.set_vexpand(true);
        // Desktop GL, not GLES: the 10- and 12-bit formats need R16/RG16
        // textures, which core GLES 3 does not have without an extension.
        area.set_allowed_apis(gtk::gdk::GLAPI::GL);
        area.set_has_depth_buffer(false);
        area.set_has_stencil_buffer(false);

        let renderer: Rc<RefCell<Option<Renderer>>> = Rc::new(RefCell::new(None));
        let text: Rc<RefCell<TextCue>> = Rc::new(RefCell::new(None));
        let text_revision = Rc::new(Cell::new(0u64));
        let video_rect = Rc::new(Cell::new((0.0, 0.0, 0.0, 0.0)));

        area.connect_realize({
            let renderer = Rc::clone(&renderer);
            move |area| {
                area.make_current();
                if let Some(err) = area.error() {
                    log::error!("GL context: {err}");
                    return;
                }
                match Renderer::new() {
                    Ok(mut r) => {
                        r.set_backdrop(crate::theme::backdrop());
                        *renderer.borrow_mut() = Some(r);
                    }
                    Err(e) => {
                        log::error!("{e}");
                        area.set_error(Some(&glib::Error::new(gtk::gio::IOErrorEnum::Failed, &e)));
                    }
                }
            }
        });

        area.connect_unrealize({
            let renderer = Rc::clone(&renderer);
            move |area| {
                area.make_current();
                // Textures and programs belong to the context that is going
                // away, so they have to be dropped while it is still current.
                renderer.borrow_mut().take();
            }
        });

        area.connect_render({
            let renderer = Rc::clone(&renderer);
            let player = Rc::clone(&player);
            let text = Rc::clone(&text);
            let text_revision = Rc::clone(&text_revision);
            let video_rect = Rc::clone(&video_rect);
            // The generation of the subtitle set already on screen.
            let seen = Cell::new(u64::MAX);
            move |area, _ctx| {
                let mut renderer = renderer.borrow_mut();
                let Some(renderer) = renderer.as_mut() else {
                    return glib::Propagation::Proceed;
                };

                // Ask for the picture that is due right now. `None` means
                // nothing new is due and the last frame still stands, which
                // is the ordinary case on a display refreshing faster than
                // the video's frame rate.
                if let Some(frame) = player.borrow().frame_for_now() {
                    renderer.upload(&frame);
                }

                // Subtitles are only rebuilt when the visible set changes;
                // a bitmap cue is a whole image and re-uploading one every
                // frame to show the same caption would be pure waste.
                if let Some((generation, cues)) = player.borrow().subtitles_if_changed(seen.get()) {
                    seen.set(generation);
                    let mut next_text: TextCue = None;
                    let mut bitmaps = Vec::new();
                    let mut reference = (1920, 1080);
                    for cue in &cues {
                        match &cue.content {
                            SubtitleContent::Text { markup, alignment } => {
                                next_text = Some((markup.clone(), *alignment));
                            }
                            SubtitleContent::Bitmap { rects, reference: r } => {
                                bitmaps.extend_from_slice(rects);
                                reference = *r;
                            }
                        }
                    }
                    renderer.set_subtitles(&bitmaps, reference);
                    if *text.borrow() != next_text {
                        *text.borrow_mut() = next_text;
                        text_revision.set(text_revision.get().wrapping_add(1));
                    }
                }

                let scale = area.scale_factor();
                let (w, h) = (area.width() * scale, area.height() * scale);
                renderer.draw(w, h);
                // Reported in widget pixels, not device pixels, because
                // that is the space GTK positions the label in.
                let (rx, ry, rw, rh) = renderer.video_rect(w, h);
                let scale = scale as f32;
                video_rect.set((rx / scale, ry / scale, rw / scale, rh / scale));
                glib::Propagation::Stop
            }
        });

        // Repaint on the compositor's clock. Doing this rather than on a
        // timer is what keeps playback smooth: frames are chosen against
        // the audio clock at the moment the display is actually about to
        // show them, so the sync decision is never stale.
        area.add_tick_callback({
            let player = Rc::clone(&player);
            move |area, _clock| {
                if player.borrow().state() == owl_media::State::Playing {
                    area.queue_render();
                }
                glib::ControlFlow::Continue
            }
        });

        Stage { widget: area, text, text_revision, video_rect }
    }

    /// Force a repaint outside playback — after a seek while paused, say,
    /// where one new frame is due but the tick callback is idle.
    pub fn refresh(&self) {
        self.widget.queue_render();
    }
}
