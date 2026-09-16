//! Hardware decoding.
//!
//! This machine is the awkward common case: a discrete NVIDIA part and an
//! AMD integrated one, either of which may be the one the display is
//! actually attached to. So nothing is hard-coded — the engine asks the
//! codec which hardware configurations it supports, tries them in order of
//! how well they work on Linux, and quietly falls back to software if none
//! of them open. A player that refuses to start because VA-API is broken
//! is worse than one that uses the CPU.
//!
//! The current path decodes on the GPU and downloads the frame with
//! `av_hwframe_transfer_data`. That still wins on 4K HEVC and AV1, where
//! decode is the expensive part, but it is not the end state: the frames
//! are already GPU-side, and exporting them as dmabuf for the renderer to
//! import would skip the round trip entirely. `HwAccel` keeps the device
//! reference around so that path can be added without touching callers.

use std::ffi::CString;

use ffmpeg_next as ff;
use ff::ffi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwAccel {
    pub kind: ffi::AVHWDeviceType,
    /// The pixel format frames arrive in while they are still on the GPU.
    pub pixel_format: ffi::AVPixelFormat,
}

impl HwAccel {
    pub fn name(&self) -> &'static str {
        match self.kind {
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI => "VA-API",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA => "NVDEC",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VDPAU => "VDPAU",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN => "Vulkan",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM => "DRM",
            _ => "hardware",
        }
    }
}

/// Preference order. VA-API first because on Linux it is the one that
/// works on both vendors' open drivers and integrates with dmabuf;
/// NVDEC next because it is the only option on the proprietary driver;
/// Vulkan last because it is the newest and the least battle-tested.
const CANDIDATES: &[ffi::AVHWDeviceType] = &[
    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VDPAU,
    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
];

/// Try to attach a hardware device to `ctx` for `codec`. Returns the
/// accelerator that took, or `None` for "decode on the CPU".
///
/// # Safety
/// `ctx` must be an unopened `AVCodecContext` and `codec` the decoder it
/// is about to be opened with.
pub unsafe fn attach(ctx: *mut ffi::AVCodecContext, codec: *const ffi::AVCodec) -> Option<HwAccel> {
    if std::env::var_os("OWL_NO_HWACCEL").is_some() {
        log::info!("hardware decoding disabled by OWL_NO_HWACCEL");
        return None;
    }

    for &kind in CANDIDATES {
        let Some(pixel_format) = (unsafe { hw_pixel_format(codec, kind) }) else { continue };

        let Some(device) = (unsafe { open_device(kind) }) else { continue };
        let mut device = device;

        unsafe {
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(device);
            ffi::av_buffer_unref(&mut device);
            // `get_format` is how the decoder asks which of its supported
            // output formats we want. Stash the answer in `opaque`, which
            // is ours to use, and read it back in the callback.
            (*ctx).opaque = pixel_format as i32 as isize as *mut std::ffi::c_void;
            (*ctx).get_format = Some(pick_format);
        }

        let accel = HwAccel { kind, pixel_format };
        log::info!("hardware decoding via {}", accel.name());
        return Some(accel);
    }

    log::info!("no hardware decoder available, using the CPU");
    None
}

/// Open a hardware device, trying each DRM render node in turn for the
/// types that address the GPU through one.
///
/// A hybrid machine has two render nodes and the default is whichever
/// libva enumerates first — routinely the discrete card, which on the
/// open NVIDIA driver cannot decode video at all. Asking for each node by
/// name is the difference between "no hardware decoding on this laptop"
/// and using the integrated GPU that can actually do it.
unsafe fn open_device(kind: ffi::AVHWDeviceType) -> Option<*mut ffi::AVBufferRef> {
    let uses_drm_node = matches!(
        kind,
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI | ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM
    );

    let mut candidates: Vec<Option<CString>> = vec![None];
    if uses_drm_node {
        let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("renderD")))
            .collect();
        nodes.sort();
        candidates.extend(nodes.into_iter().filter_map(|p| CString::new(p.to_string_lossy().as_bytes()).ok()).map(Some));
    }

    for candidate in candidates {
        let mut device = std::ptr::null_mut();
        let name = candidate.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let err = unsafe { ffi::av_hwdevice_ctx_create(&mut device, kind, name, std::ptr::null_mut(), 0) };
        if err >= 0 {
            if let Some(c) = &candidate {
                log::debug!("{kind:?} opened on {}", c.to_string_lossy());
            }
            return Some(device);
        }
    }
    None
}

/// Ask the codec whether it supports `kind`, and in which pixel format.
unsafe fn hw_pixel_format(codec: *const ffi::AVCodec, kind: ffi::AVHWDeviceType) -> Option<ffi::AVPixelFormat> {
    let mut i = 0;
    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec, i) };
        if config.is_null() {
            return None;
        }
        let config = unsafe { &*config };
        let usable = config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0;
        if usable && config.device_type == kind {
            return Some(config.pix_fmt);
        }
        i += 1;
    }
}

/// Chosen by the decoder from the formats it can emit. Returning the
/// hardware format keeps frames on the GPU; returning anything else makes
/// it decode in software. If our format is not on offer — which happens
/// when the file turns out to use a profile the hardware cannot do, such
/// as 12-bit HEVC on older silicon — take the decoder's own first choice
/// and let it fall back rather than failing the open.
unsafe extern "C" fn pick_format(
    ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let wanted = unsafe { (*ctx).opaque } as isize as i32;
    unsafe {
        let mut p = formats;
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as i32 == wanted {
                return *p;
            }
            p = p.add(1);
        }

        // Our format is not on offer. This happens when the device turned
        // out not to support this codec or profile at all — Vulkan without
        // VK_KHR_video_decode_queue, or 12-bit HEVC on older silicon.
        //
        // Returning the list head here would hand back *another* hardware
        // format, which then fails against a device context of the wrong
        // type, and FFmpeg walks the whole list failing once per entry.
        // Pick the first software format instead and decode on the CPU.
        let mut p = formats;
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            let desc = ffi::av_pix_fmt_desc_get(*p);
            let is_hw = !desc.is_null() && (*desc).flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0;
            if !is_hw {
                log::info!("this stream cannot be decoded in hardware; using the CPU");
                return *p;
            }
            p = p.add(1);
        }
        *formats
    }
}

/// Pull a frame off the GPU into system memory.
///
/// # Safety
/// `src` must be a frame whose format is a hardware format.
pub unsafe fn transfer(src: &ff::frame::Video) -> Result<ff::frame::Video, ff::Error> {
    let mut dst = ff::frame::Video::empty();
    let err = unsafe { ffi::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0) };
    if err < 0 {
        return Err(ff::Error::from(err));
    }
    // The transfer copies pixels but not timing, so carry it across.
    unsafe {
        (*dst.as_mut_ptr()).pts = (*src.as_ptr()).pts;
        (*dst.as_mut_ptr()).best_effort_timestamp = (*src.as_ptr()).best_effort_timestamp;
    }
    Ok(dst)
}

/// Whether a pixel format lives on the GPU and needs `transfer`.
pub fn is_hw_format(format: ff::format::Pixel) -> bool {
    let desc = unsafe { ffi::av_pix_fmt_desc_get(format.into()) };
    if desc.is_null() {
        return false;
    }
    unsafe { (*desc).flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0 }
}
