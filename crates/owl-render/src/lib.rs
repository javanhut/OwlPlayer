//! The video plane.
//!
//! GTK draws the chrome; this draws the picture, inside the GL context a
//! `GtkGLArea` hands over. Everything the decoder can produce is uploaded
//! as one, two or three single-channel textures and turned into RGB by
//! one shader, so there is no CPU-side colour conversion anywhere in the
//! playback path — `libswscale` is only ever reached for pixel formats
//! exotic enough that the shader does not know them.

use std::ffi::CString;

use owl_media::{BitmapRect, PixelLayout, VideoFrame};

mod colour;
pub use colour::{primaries_matrix, yuv_to_rgb};

const VERTEX_SRC: &str = include_str!("shaders/video.vert");
const FRAGMENT_SRC: &str = include_str!("shaders/video.frag");
const SPECTRUM_VERTEX_SRC: &str = include_str!("shaders/spectrum.vert");
const SPECTRUM_FRAGMENT_SRC: &str = include_str!("shaders/spectrum.frag");
const OVERLAY_VERTEX_SRC: &str = include_str!("shaders/overlay.vert");
const OVERLAY_FRAGMENT_SRC: &str = include_str!("shaders/overlay.frag");

/// Resolve GL entry points for the context GTK hands us.
///
/// Not through libepoxy, which is the obvious first guess because GTK
/// links it: epoxy's `glFoo` names are preprocessor macros over
/// `epoxy_glFoo` *data* symbols holding function pointers, so there is no
/// `glFoo` in its dynamic symbol table for `dlsym` to find.
///
/// `eglGetProcAddress` is the right source instead. Mesa implements
/// `EGL_KHR_get_all_proc_addresses`, so it resolves core entry points and
/// not just extensions, and GTK4 uses EGL on both Wayland and X11.
/// `libGL.so.1` is kept as a fallback for the core functions on a stack
/// where EGL declines to hand them over.
///
/// Idempotent: calling it on every `GLArea` realize is fine.
pub fn load_gl() {
    use std::ffi::c_void;
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        type GetProc = unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void;

        // Leaked on purpose: the resolved pointers live inside these
        // libraries and must outlive every GL call, which is the life of
        // the process.
        let open = |name: &str| -> Option<&'static libloading::os::unix::Library> {
            unsafe { libloading::os::unix::Library::new(name) }.ok().map(|l| &*Box::leak(Box::new(l)))
        };

        let egl_get_proc: Option<GetProc> = open("libEGL.so.1")
            .and_then(|lib| unsafe { lib.get::<GetProc>(b"eglGetProcAddress\0") }.ok().map(|s| *s));
        let gl_lib = open("libGL.so.1");

        if egl_get_proc.is_none() && gl_lib.is_none() {
            log::error!("no GL loader found: neither libEGL.so.1 nor libGL.so.1 could be opened");
        }

        gl::load_with(|name| {
            let Ok(symbol) = CString::new(name) else { return std::ptr::null() };
            if let Some(get_proc) = egl_get_proc {
                let address = unsafe { get_proc(symbol.as_ptr()) };
                if !address.is_null() {
                    return address;
                }
            }
            if let Some(lib) = gl_lib {
                // A `Symbol<*const c_void>` dereferences to the symbol's
                // own address, which for a function is what we want.
                if let Ok(sym) = unsafe { lib.get::<*const c_void>(symbol.as_bytes_with_nul()) } {
                    return *sym;
                }
            }
            log::warn!("GL entry point {name} could not be resolved");
            std::ptr::null()
        });
    });
}

/// How bright the HDR source is assumed to be when tone mapping to an SDR
/// display, in nits. Most HDR10 grades are mastered at 1000.
const DEFAULT_PEAK_NITS: f32 = 1000.0;

/// One uploaded subtitle image, with where it goes in the subtitle's own
/// coordinate space.
struct SubtitleImage {
    texture: u32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

pub struct Renderer {
    program: u32,
    spectrum_program: u32,
    spectrum_texture: u32,
    spectrum_accent: i32,
    spectrum_bands: i32,
    spectrum_sampler: i32,
    spectrum_len: usize,
    overlay_program: u32,
    overlay_rect: i32,
    overlay_image: i32,
    subtitles: Vec<SubtitleImage>,
    /// The space `subtitles` coordinates are expressed in.
    subtitle_reference: (f32, f32),
    vao: u32,
    textures: [u32; 3],
    /// What the textures currently hold, so they are only reallocated when
    /// the stream's geometry actually changes.
    allocated: Option<(u32, u32, PixelLayout)>,
    uniforms: Uniforms,
    /// Display size of the frame last uploaded, for letterboxing.
    frame_aspect: f32,
    have_frame: bool,
    /// What to clear to when there is no picture. Black is right for the
    /// letterbox bars beside a film, and wrong for an empty player: that
    /// leaves a black rectangle sitting in the middle of a Raven window.
    backdrop: [f32; 3],
}

struct Uniforms {
    viewport: i32,
    planes: i32,
    swap_uv: i32,
    is_rgb: i32,
    bgra: i32,
    depth_scale: i32,
    yuv_to_rgb: i32,
    yuv_offset: i32,
    transfer: i32,
    primaries: i32,
    peak_nits: i32,
    plane: [i32; 3],
}

impl Renderer {
    /// Must be called with the target GL context current.
    pub fn new() -> Result<Renderer, String> {
        load_gl();
        unsafe {
            let program = build_program(VERTEX_SRC, FRAGMENT_SRC)?;
            let overlay_program = build_program(OVERLAY_VERTEX_SRC, OVERLAY_FRAGMENT_SRC)?;
            let spectrum_program = build_program(SPECTRUM_VERTEX_SRC, SPECTRUM_FRAGMENT_SRC)?;
            let mut spectrum_texture = 0;
            gl::GenTextures(1, &mut spectrum_texture);
            gl::BindTexture(gl::TEXTURE_2D, spectrum_texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            let mut vao = 0;
            // Core profile forbids drawing with no VAO bound, even when the
            // vertices come from gl_VertexID and nothing is attached to it.
            gl::GenVertexArrays(1, &mut vao);

            let mut textures = [0u32; 3];
            gl::GenTextures(3, textures.as_mut_ptr());
            for &t in &textures {
                gl::BindTexture(gl::TEXTURE_2D, t);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                // Clamp, or bilinear sampling wraps the edge pixels and
                // paints a thin strip of the opposite side along each border.
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            }

            let u = |name: &str| {
                let c = CString::new(name).unwrap();
                gl::GetUniformLocation(program, c.as_ptr())
            };
            let uniforms = Uniforms {
                viewport: u("uViewport"),
                planes: u("uPlanes"),
                swap_uv: u("uSwapUV"),
                is_rgb: u("uIsRgb"),
                bgra: u("uBgra"),
                depth_scale: u("uDepthScale"),
                yuv_to_rgb: u("uYuvToRgb"),
                yuv_offset: u("uYuvOffset"),
                transfer: u("uTransfer"),
                primaries: u("uPrimaries"),
                peak_nits: u("uPeakNits"),
                plane: [u("uPlane0"), u("uPlane1"), u("uPlane2")],
            };

            let overlay_rect = {
                let c = CString::new("uRect").unwrap();
                gl::GetUniformLocation(overlay_program, c.as_ptr())
            };
            let overlay_image = {
                let c = CString::new("uImage").unwrap();
                gl::GetUniformLocation(overlay_program, c.as_ptr())
            };

            let su = |name: &str| {
                let c = CString::new(name).unwrap();
                gl::GetUniformLocation(spectrum_program, c.as_ptr())
            };

            Ok(Renderer {
                program,
                spectrum_program,
                spectrum_texture,
                spectrum_accent: su("uAccent"),
                spectrum_bands: su("uBands"),
                spectrum_sampler: su("uSpectrum"),
                spectrum_len: 0,
                overlay_program,
                overlay_rect,
                overlay_image,
                subtitles: Vec::new(),
                subtitle_reference: (1920.0, 1080.0),
                vao,
                textures,
                allocated: None,
                uniforms,
                frame_aspect: 16.0 / 9.0,
                have_frame: false,
                backdrop: [0.0, 0.0, 0.0],
            })
        }
    }

    pub fn has_frame(&self) -> bool {
        self.have_frame
    }

    /// The colour behind an empty stage, so a player with nothing loaded
    /// is the window's own colour rather than a black hole in it. Takes
    /// straight sRGB components, as the stylesheet states them.
    pub fn set_backdrop(&mut self, rgb: [f32; 3]) {
        self.backdrop = rgb;
    }

    /// Push a decoded frame into the textures. The only copy in the whole
    /// pipeline happens here.
    pub fn upload(&mut self, frame: &VideoFrame) {
        let layout = frame.layout;
        let (w, h) = (frame.width(), frame.height());
        if w == 0 || h == 0 {
            return;
        }

        let (dw, dh) = frame.display_size();
        self.frame_aspect = dw as f32 / dh.max(1) as f32;

        let reallocate = self.allocated != Some((w, h, layout));
        unsafe {
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            for i in 0..layout.planes() {
                let plane = frame.plane(i);
                let spec = TextureSpec::for_plane(layout, i);
                // Strides are padded for SIMD; telling GL the real row
                // length means the plane uploads straight from the decoder's
                // buffer with no repacking.
                gl::PixelStorei(gl::UNPACK_ROW_LENGTH, (plane.stride / spec.bytes_per_texel) as i32);
                gl::ActiveTexture(gl::TEXTURE0 + i as u32);
                gl::BindTexture(gl::TEXTURE_2D, self.textures[i]);

                let rows_available = plane.data.len() / plane.stride.max(1);
                let height = plane.height.min(rows_available as u32);
                if height == 0 {
                    continue;
                }

                if reallocate {
                    gl::TexImage2D(
                        gl::TEXTURE_2D,
                        0,
                        spec.internal as i32,
                        plane.width as i32,
                        height as i32,
                        0,
                        spec.format,
                        spec.kind,
                        plane.data.as_ptr() as *const _,
                    );
                } else {
                    gl::TexSubImage2D(
                        gl::TEXTURE_2D,
                        0,
                        0,
                        0,
                        plane.width as i32,
                        height as i32,
                        spec.format,
                        spec.kind,
                        plane.data.as_ptr() as *const _,
                    );
                }
            }
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
        }

        self.allocated = Some((w, h, layout));
        self.have_frame = true;
        self.configure(frame);
    }

    /// Set the uniforms that depend on the frame's colour metadata.
    fn configure(&mut self, frame: &VideoFrame) {
        let layout = frame.layout;
        let depth = layout.bit_depth();
        let colour = frame.color;
        let space = colour.resolved_space(frame.height());
        let full_range = matches!(colour.range, ffmpeg_next::color::Range::JPEG);

        let (matrix, offset) = yuv_to_rgb(space, full_range, depth);
        let depth_scale = depth_scale(layout);
        let transfer = match colour.transfer {
            ffmpeg_next::color::TransferCharacteristic::SMPTE2084 => 1,
            ffmpeg_next::color::TransferCharacteristic::ARIB_STD_B67 => 2,
            _ => 0,
        };

        unsafe {
            gl::UseProgram(self.program);
            gl::Uniform1i(self.uniforms.planes, layout.planes() as i32);
            gl::Uniform1i(self.uniforms.swap_uv, matches!(layout, PixelLayout::BiPlanar8 { swapped: true }) as i32);
            gl::Uniform1i(self.uniforms.is_rgb, layout.is_rgb() as i32);
            gl::Uniform1i(self.uniforms.bgra, matches!(layout, PixelLayout::Bgra) as i32);
            gl::Uniform1f(self.uniforms.depth_scale, depth_scale);
            gl::UniformMatrix3fv(self.uniforms.yuv_to_rgb, 1, gl::FALSE, matrix.as_ptr());
            gl::Uniform3f(self.uniforms.yuv_offset, offset[0], offset[1], offset[2]);
            gl::Uniform1i(self.uniforms.transfer, transfer);
            let primaries = primaries_matrix(transfer != 0);
            gl::UniformMatrix3fv(self.uniforms.primaries, 1, gl::FALSE, primaries.as_ptr());
            gl::Uniform1f(self.uniforms.peak_nits, DEFAULT_PEAK_NITS);
            for (i, &loc) in self.uniforms.plane.iter().enumerate() {
                gl::Uniform1i(loc, i as i32);
            }
        }
    }

    /// Draw the music visualiser: bar heights in 0..=1, left to right.
    ///
    /// Used when the file has no picture. A black rectangle is a poor
    /// answer to a music file, and this is the one thing the renderer can
    /// show that is actually about the sound.
    pub fn draw_spectrum(&mut self, width: i32, height: i32, levels: &[f32], accent: [f32; 3]) {
        if width <= 0 || height <= 0 || levels.is_empty() {
            return;
        }
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.spectrum_texture);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            if self.spectrum_len != levels.len() {
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::R32F as i32,
                    levels.len() as i32,
                    1,
                    0,
                    gl::RED,
                    gl::FLOAT,
                    levels.as_ptr() as *const _,
                );
                self.spectrum_len = levels.len();
            } else {
                gl::TexSubImage2D(
                    gl::TEXTURE_2D,
                    0,
                    0,
                    0,
                    levels.len() as i32,
                    1,
                    gl::RED,
                    gl::FLOAT,
                    levels.as_ptr() as *const _,
                );
            }

            let [r, g, b] = self.backdrop;
            gl::Viewport(0, 0, width, height);
            gl::ClearColor(r, g, b, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            gl::UseProgram(self.spectrum_program);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.spectrum_texture);
            gl::Uniform1i(self.spectrum_sampler, 0);
            gl::Uniform1f(self.spectrum_bands, levels.len() as f32);
            gl::Uniform3f(self.spectrum_accent, accent[0], accent[1], accent[2]);
            gl::BindVertexArray(self.vao);
            gl::DrawArrays(gl::TRIANGLES, 0, 3);
            gl::BindVertexArray(0);
            gl::Disable(gl::BLEND);
        }
    }

    /// Forget the current picture, so switching from a film to a music
    /// file does not leave the last frame behind the bars.
    pub fn clear_frame(&mut self) {
        self.have_frame = false;
    }

    /// Replace the bitmap subtitles on screen.
    ///
    /// Called only when the visible set actually changes — the engine's
    /// cue generation exists so this is not run per frame with the same
    /// images.
    pub fn set_subtitles(&mut self, rects: &[BitmapRect], reference: (u32, u32)) {
        self.clear_subtitles();
        self.subtitle_reference = (reference.0.max(1) as f32, reference.1.max(1) as f32);
        unsafe {
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            for rect in rects {
                if rect.width == 0 || rect.height == 0 {
                    continue;
                }
                let mut texture = 0;
                gl::GenTextures(1, &mut texture);
                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RGBA8 as i32,
                    rect.width as i32,
                    rect.height as i32,
                    0,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    rect.rgba.as_ptr() as *const _,
                );
                self.subtitles.push(SubtitleImage {
                    texture,
                    x: rect.x as f32,
                    y: rect.y as f32,
                    width: rect.width as f32,
                    height: rect.height as f32,
                });
            }
        }
    }

    pub fn clear_subtitles(&mut self) {
        unsafe {
            for image in &self.subtitles {
                gl::DeleteTextures(1, &image.texture);
            }
        }
        self.subtitles.clear();
    }

    /// Where the picture actually sits inside a `width` x `height`
    /// viewport, in pixels, after letterboxing. GTK needs this to put text
    /// subtitles against the bottom of the *image* rather than the bottom
    /// of the widget, which are not the same edge for a 2.39:1 film in a
    /// 16:9 window.
    pub fn video_rect(&self, width: i32, height: i32) -> (f32, f32, f32, f32) {
        if width <= 0 || height <= 0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let (sx, sy) = self.fit(width, height);
        let w = width as f32 * sx;
        let h = height as f32 * sy;
        ((width as f32 - w) * 0.5, (height as f32 - h) * 0.5, w, h)
    }

    /// Scale factors that fit the frame's aspect inside the viewport.
    fn fit(&self, width: i32, height: i32) -> (f32, f32) {
        let widget_aspect = width as f32 / height as f32;
        if self.frame_aspect > widget_aspect {
            (1.0, widget_aspect / self.frame_aspect)
        } else {
            (self.frame_aspect / widget_aspect, 1.0)
        }
    }

    /// Draw the current frame into a `width` x `height` viewport, centred
    /// and letterboxed to preserve the picture's shape.
    pub fn draw(&self, width: i32, height: i32) {
        unsafe {
            gl::Disable(gl::BLEND);
            gl::Disable(gl::DEPTH_TEST);
            // Black once there is a picture — the bars beside a 2.39:1 film
            // are part of the presentation and must not be tinted — and the
            // window's own colour before then.
            let [r, g, b] = if self.have_frame { [0.0, 0.0, 0.0] } else { self.backdrop };
            gl::ClearColor(r, g, b, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            if !self.have_frame || width <= 0 || height <= 0 {
                return;
            }

            let (sx, sy) = self.fit(width, height);

            gl::Viewport(0, 0, width, height);
            gl::UseProgram(self.program);
            gl::Uniform4f(self.uniforms.viewport, sx, sy, 0.0, 0.0);
            for i in 0..3 {
                gl::ActiveTexture(gl::TEXTURE0 + i);
                gl::BindTexture(gl::TEXTURE_2D, self.textures[i as usize]);
            }
            // The full-screen triangle is three times the size of what it
            // covers; scaled down for letterboxing, it still reaches into
            // the bars, where its texture coordinates run past the edge of
            // the frame. Clamped, those smear the border pixels across the
            // bars, and the triangle's long edge leaves a black wedge
            // beyond it. Scissor it to the picture.
            let (x, y, w, h) = self.video_rect(width, height);
            gl::Enable(gl::SCISSOR_TEST);
            gl::Scissor(
                x.round() as i32,
                y.round() as i32,
                w.round() as i32,
                h.round() as i32,
            );
            gl::BindVertexArray(self.vao);
            gl::DrawArrays(gl::TRIANGLES, 0, 3);
            gl::Disable(gl::SCISSOR_TEST);

            self.draw_subtitles(sx, sy);
            gl::BindVertexArray(0);
        }
    }

    /// Composite bitmap subtitles over the picture.
    ///
    /// Their coordinates are relative to the subtitle stream's own
    /// reference resolution, which is mapped onto the letterboxed video
    /// rectangle — so a caption keeps its position in the frame when the
    /// window is resized, and never strays into the black bars.
    unsafe fn draw_subtitles(&self, sx: f32, sy: f32) {
        if self.subtitles.is_empty() {
            return;
        }
        unsafe {
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            gl::UseProgram(self.overlay_program);
            gl::Uniform1i(self.overlay_image, 0);
            gl::ActiveTexture(gl::TEXTURE0);

            let (ref_w, ref_h) = self.subtitle_reference;
            for image in &self.subtitles {
                let u0 = image.x / ref_w;
                let u1 = (image.x + image.width) / ref_w;
                let v0 = image.y / ref_h;
                let v1 = (image.y + image.height) / ref_h;
                // Texture space is y-down, clip space is y-up.
                let x0 = sx * (2.0 * u0 - 1.0);
                let x1 = sx * (2.0 * u1 - 1.0);
                let y0 = sy * (1.0 - 2.0 * v0);
                let y1 = sy * (1.0 - 2.0 * v1);

                gl::BindTexture(gl::TEXTURE_2D, image.texture);
                gl::Uniform4f(self.overlay_rect, x0, y0, x1, y1);
                gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);
            }
            gl::Disable(gl::BLEND);
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.clear_subtitles();
        unsafe {
            gl::DeleteTextures(3, self.textures.as_ptr());
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteProgram(self.program);
            gl::DeleteProgram(self.overlay_program);
            gl::DeleteProgram(self.spectrum_program);
            gl::DeleteTextures(1, &self.spectrum_texture);
        }
    }
}

struct TextureSpec {
    internal: u32,
    format: u32,
    kind: u32,
    bytes_per_texel: usize,
}

impl TextureSpec {
    fn for_plane(layout: PixelLayout, index: usize) -> TextureSpec {
        match layout {
            PixelLayout::Planar8 { .. } => {
                TextureSpec { internal: gl::R8, format: gl::RED, kind: gl::UNSIGNED_BYTE, bytes_per_texel: 1 }
            }
            PixelLayout::Planar16 { .. } => {
                TextureSpec { internal: gl::R16, format: gl::RED, kind: gl::UNSIGNED_SHORT, bytes_per_texel: 2 }
            }
            PixelLayout::BiPlanar8 { .. } if index == 0 => {
                TextureSpec { internal: gl::R8, format: gl::RED, kind: gl::UNSIGNED_BYTE, bytes_per_texel: 1 }
            }
            PixelLayout::BiPlanar8 { .. } => {
                TextureSpec { internal: gl::RG8, format: gl::RG, kind: gl::UNSIGNED_BYTE, bytes_per_texel: 2 }
            }
            PixelLayout::BiPlanar16 { .. } if index == 0 => {
                TextureSpec { internal: gl::R16, format: gl::RED, kind: gl::UNSIGNED_SHORT, bytes_per_texel: 2 }
            }
            PixelLayout::BiPlanar16 { .. } => {
                TextureSpec { internal: gl::RG16, format: gl::RG, kind: gl::UNSIGNED_SHORT, bytes_per_texel: 4 }
            }
            PixelLayout::Rgba | PixelLayout::Bgra => {
                TextureSpec { internal: gl::RGBA8, format: gl::RGBA, kind: gl::UNSIGNED_BYTE, bytes_per_texel: 4 }
            }
            PixelLayout::Rgb24 => {
                TextureSpec { internal: gl::RGB8, format: gl::RGB, kind: gl::UNSIGNED_BYTE, bytes_per_texel: 3 }
            }
        }
    }
}

/// A 10-bit sample sitting in a 16-bit texture reads back as roughly 1/64
/// of its real value, because GL normalises against 65535. This is the
/// factor that puts it back.
///
/// The two 16-bit families need different answers: planar formats
/// (`yuv420p10le`) put the bits at the bottom of the word, while the NV12
/// family (`p010`) puts them at the top and pads with zeros.
fn depth_scale(layout: PixelLayout) -> f32 {
    match layout {
        PixelLayout::Planar16 { bits, .. } => 65535.0 / ((1u32 << bits) - 1) as f32,
        PixelLayout::BiPlanar16 { bits } => {
            let max = (((1u32 << bits) - 1) << (16 - bits)) as f32;
            65535.0 / max
        }
        _ => 1.0,
    }
}

unsafe fn build_program(vertex: &str, fragment: &str) -> Result<u32, String> {
    unsafe {
        let vs = compile(gl::VERTEX_SHADER, vertex)?;
        let fs = compile(gl::FRAGMENT_SHADER, fragment)?;
        let program = gl::CreateProgram();
        gl::AttachShader(program, vs);
        gl::AttachShader(program, fs);
        gl::LinkProgram(program);

        let mut ok = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut len = 0;
            gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len.max(1) as usize];
            gl::GetProgramInfoLog(program, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            return Err(format!("linking the video shader failed: {}", String::from_utf8_lossy(&buf)));
        }
        gl::DeleteShader(vs);
        gl::DeleteShader(fs);
        Ok(program)
    }
}

unsafe fn compile(kind: u32, source: &str) -> Result<u32, String> {
    unsafe {
        let shader = gl::CreateShader(kind);
        let c = CString::new(source).map_err(|e| e.to_string())?;
        gl::ShaderSource(shader, 1, &c.as_ptr(), std::ptr::null());
        gl::CompileShader(shader);
        let mut ok = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut len = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len.max(1) as usize];
            gl::GetShaderInfoLog(shader, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            let stage = if kind == gl::VERTEX_SHADER { "vertex" } else { "fragment" };
            return Err(format!("compiling the {stage} shader failed: {}", String::from_utf8_lossy(&buf)));
        }
        Ok(shader)
    }
}
