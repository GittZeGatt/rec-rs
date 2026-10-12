//! Live preview: the newest composited frame, scaled on the GPU to exactly
//! the preview's size in pixels (Retina: 2x its size in points) and shown
//! 1:1 without copies.
//!
//! 1. Metal Performance Shaders' Lanczos scales the frame to the target
//!    pixel size (`scale::FrameScaler`, the recording's scaler).
//! 2. A YCbCr frame is converted to BGRA by a VideoToolbox pixel transfer
//!    (BT.709, hardware).
//! 3. The BGRA frame's IOSurface becomes a Metal texture that wgpu and egui
//!    sample directly: no CPU readback, swizzle or upload, and no second
//!    scaling (the UI draws it pixel-aligned at its native size). It is a
//!    plain (non-sRGB) format on purpose: egui works on gamma-encoded
//!    values and expects its textures as such; an sRGB format would decode
//!    them to linear on sampling and darken the preview (mid-gray 128 -> 48).
//!
//! Steps 1 and 2 run on a worker thread (they wait for the GPU); the UI
//! thread only wraps the result. Low power: half the resolution at 15 fps.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_core_video::*;
use objc2_metal::*;
use objc2_video_toolbox::*;

use crate::scale::FrameScaler;
use crate::util::*;
use crate::writer::PreviewFn;

/// Newest frame from the session, for the worker.
static FRAME: Mutex<Option<SendBox<CFRetained<CVPixelBuffer>>>> = Mutex::new(None);
static FRAME_CV: Condvar = Condvar::new();
/// Newest scaled BGRA frame, for the UI.
static READY: Mutex<Option<SendBox<CFRetained<CVPixelBuffer>>>> = Mutex::new(None);
static WORKER: OnceLock<()> = OnceLock::new();
static WANTED: AtomicBool = AtomicBool::new(false);
static LOW_POWER: AtomicBool = AtomicBool::new(false);
static SKIP: AtomicBool = AtomicBool::new(false);
/// Preview area in pixels (width << 32 | height); 0 = unknown.
static TARGET: AtomicU64 = AtomicU64::new(0);

/// Whether frames are wanted (main window visible).
pub fn set_wanted(on: bool) {
    WANTED.store(on, Ordering::Relaxed);
    if !on {
        FRAME.lock().unwrap().take();
        READY.lock().unwrap().take();
    }
}

pub fn set_low_power(on: bool) {
    LOW_POWER.store(on, Ordering::Relaxed);
}

/// The preview area's size in pixels (set by the UI each frame).
pub fn set_target(w: u32, h: u32) {
    TARGET.store((w as u64) << 32 | h as u64, Ordering::Relaxed);
}

/// Session callback: hand the newest frame to the worker, which scales it
/// and wakes the UI.
pub fn callback(wake: Arc<dyn Fn() + Send + Sync>) -> PreviewFn {
    WORKER.get_or_init(|| {
        let wake = wake.clone();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                let mut w = Worker::new();
                loop {
                    let frame = {
                        let mut f = FRAME.lock().unwrap();
                        while f.is_none() {
                            f = FRAME_CV.wait(f).unwrap();
                        }
                        f.take().unwrap()
                    };
                    let t = TARGET.load(Ordering::Relaxed);
                    let out = objc2::rc::autoreleasepool(|_| w.scaled_bgra(&frame.0, ((t >> 32) as u32, t as u32), LOW_POWER.load(Ordering::Relaxed)));
                    if let Some(out) = out
                        && WANTED.load(Ordering::Relaxed)
                    {
                        let first = READY.lock().unwrap().replace(SendBox(out)).is_none();
                        if first {
                            wake();
                        }
                    }
                }
            })
            .expect("preview thread");
    });
    Arc::new(move |frame| {
        if !WANTED.load(Ordering::Relaxed) {
            return;
        }
        // Frames come at 30 fps; low power keeps every other one.
        if LOW_POWER.load(Ordering::Relaxed) && SKIP.fetch_xor(true, Ordering::Relaxed) {
            return;
        }
        FRAME.lock().unwrap().replace(SendBox(frame));
        FRAME_CV.notify_one();
    })
}

/// The pixel size to render a `fw`x`fh` frame at for the preview area: its
/// aspect ratio fitted into the area, never larger than the frame.
pub fn target_size(fw: usize, fh: usize, area: (u32, u32), low_power: bool) -> (usize, usize) {
    let (aw, ah) = if area.0 == 0 || area.1 == 0 { (960.0, 540.0) } else { (area.0 as f64, area.1 as f64) };
    let mut s = (aw / fw as f64).min(ah / fh as f64).min(1.0);
    if low_power {
        s *= 0.5;
    }
    (((fw as f64 * s).round() as usize).max(2) & !1, ((fh as f64 * s).round() as usize).max(2) & !1)
}

type Pool = SendBox<CFRetained<CVPixelBufferPool>>;

/// The preview texture's format: plain BGRA, not sRGB (see the module docs:
/// egui expects gamma-encoded textures).
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const MTL_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// A preview frame ready to draw: a wgpu texture over the frame's IOSurface.
pub struct Frame {
    pub texture: wgpu::Texture,
    pub size: [u32; 2],
}

/// The UI side: turns scaled frames into textures.
pub struct Scaler {
    /// Frames whose IOSurfaces back textures that may still be drawn.
    keep: VecDeque<SendBox<CFRetained<CVPixelBuffer>>>,
}

impl Scaler {
    pub fn new() -> Self {
        Scaler { keep: VecDeque::new() }
    }

    /// The newest scaled frame (if a new one arrived) as a texture.
    pub fn take(&mut self, device: &wgpu::Device) -> Option<Frame> {
        let img = READY.lock().unwrap().take()?.0;
        let (w, h) = (CVPixelBufferGetWidth(&img), CVPixelBufferGetHeight(&img));
        let texture = texture_from_buffer(device, &img, w as u32, h as u32)?;
        self.keep.push_back(SendBox(img));
        while self.keep.len() > 3 {
            self.keep.pop_front();
        }
        Some(Frame { texture, size: [w as u32, h as u32] })
    }
}

/// The worker side: GPU scaling and color conversion.
struct Worker {
    scaler: Option<((usize, usize), FrameScaler)>,
    scaled_pool: Option<((usize, usize, u32), Pool)>,
    bgra_pool: Option<((usize, usize), Pool)>,
    to_bgra: CFRetained<VTPixelTransferSession>,
}

fn new_buffer(pool: &CVPixelBufferPool) -> Option<CFRetained<CVPixelBuffer>> {
    let mut b = std::ptr::null_mut();
    unsafe {
        CVPixelBufferPool::create_pixel_buffer(None, pool, std::ptr::NonNull::from(&mut b));
        Some(CFRetained::from_raw(std::ptr::NonNull::new(b)?))
    }
}

impl Worker {
    fn new() -> Self {
        let to_bgra = unsafe {
            let mut s = std::ptr::null_mut();
            VTPixelTransferSession::create(None, std::ptr::NonNull::from(&mut s));
            let s = CFRetained::from_raw(std::ptr::NonNull::new(s).expect("pixel transfer session"));
            let key: &CFString = kVTPixelTransferPropertyKey_DestinationYCbCrMatrix;
            VTSessionSetProperty(s.as_ref(), key, Some(kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref() as &CFType));
            s
        };
        Worker { scaler: None, scaled_pool: None, bgra_pool: None, to_bgra }
    }

    /// Steps 1 and 2: `src` as BGRA at its preview size for `area`.
    fn scaled_bgra(&mut self, src: &CVPixelBuffer, area: (u32, u32), low_power: bool) -> Option<CFRetained<CVPixelBuffer>> {
        let (fw, fh) = (CVPixelBufferGetWidth(src), CVPixelBufferGetHeight(src));
        if fw == 0 || fh == 0 {
            return None;
        }
        let (w, h) = target_size(fw, fh, area, low_power);
        let fmt = CVPixelBufferGetPixelFormatType(src);
        // 1. Lanczos to the target size.
        let mut img: CFRetained<CVPixelBuffer> = unsafe { CFRetained::retain(std::ptr::NonNull::from(src)) };
        if (fw, fh) != (w, h) {
            if self.scaler.as_ref().is_none_or(|(sz, _)| *sz != (w, h)) {
                self.scaler = Some(((w, h), FrameScaler::new(w, h)?));
            }
            if self.scaled_pool.as_ref().is_none_or(|(k, _)| *k != (w, h, fmt)) {
                self.scaled_pool = Some(((w, h, fmt), SendBox(make_pool(w, h, fmt))));
            }
            let out = new_buffer(&self.scaled_pool.as_ref()?.1.0)?;
            if !self.scaler.as_mut()?.1.scale(src, &out) {
                return None;
            }
            img = out;
        }
        // 2. YCbCr to BGRA.
        if fmt != PIX_BGRA {
            if self.bgra_pool.as_ref().is_none_or(|(k, _)| *k != (w, h)) {
                self.bgra_pool = Some(((w, h), SendBox(make_pool(w, h, PIX_BGRA))));
            }
            let out = new_buffer(&self.bgra_pool.as_ref()?.1.0)?;
            if unsafe { self.to_bgra.transfer_image(&img, &out) } != 0 {
                return None;
            }
            img = out;
        }
        Some(img)
    }
}

/// A wgpu texture over a BGRA pixel buffer's IOSurface (no copy). The
/// buffer must outlive the texture's use.
fn texture_from_buffer(device: &wgpu::Device, buf: &CVPixelBuffer, w: u32, h: u32) -> Option<wgpu::Texture> {
    use wgpu::hal::api::Metal;
    let surface = CVPixelBufferGetIOSurface(Some(buf))?;
    unsafe {
        let hal = device.as_hal::<Metal>()?;
        let mtl: &Retained<ProtocolObject<dyn MTLDevice>> = hal.raw_device();
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTL_FORMAT,
            w as usize,
            h as usize,
            false,
        );
        desc.setUsage(MTLTextureUsage::ShaderRead);
        let raw = mtl.newTextureWithDescriptor_iosurface_plane(&desc, &surface, 0)?;
        drop(hal);
        let hal_tex = wgpu::hal::metal::Device::texture_from_raw(
            raw,
            TEXTURE_FORMAT,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent { width: w, height: h, depth: 1 },
            None,
        );
        Some(device.create_texture_from_hal::<Metal>(
            hal_tex,
            &wgpu::TextureDescriptor {
                label: Some("preview"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEXTURE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::TextureUses::RESOURCE,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process's CPU time (user + system), seconds.
    fn cpu() -> f64 {
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
        let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        tv(u.ru_utime) + tv(u.ru_stime)
    }

    fn write_bgra(buf: &CVPixelBuffer, path: &str) {
        unsafe {
            CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly);
            let (w, h, stride) = (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf), CVPixelBufferGetBytesPerRow(buf));
            let base = CVPixelBufferGetBaseAddress(buf) as *const u8;
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * stride), w * 4));
            }
            CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly);
            std::fs::write(path, out).unwrap();
            println!("{path} {w}x{h}");
        }
    }

    /// Preview quality harness (scripts/previewquality.sh offline): reads
    /// one 1920x1080.. frame of REC_PV_SRC (any video; decoded to NV12 by
    /// ffmpeg), writes the new preview pixels for REC_PV_AREA (WxH px) to
    /// REC_PV_OUT.new.bgra and the old path's 960 px image (CoreImage
    /// affine, before egui stretched it) to REC_PV_OUT.old.bgra.
    #[test]
    #[ignore]
    fn preview_pixels() {
        use std::io::Read;
        use objc2_core_foundation::{CGAffineTransform, CGPoint, CGRect, CGSize};
        let src = std::env::var("REC_PV_SRC").unwrap();
        let out = std::env::var("REC_PV_OUT").unwrap();
        let (aw, ah) = std::env::var("REC_PV_AREA").unwrap().split_once('x').map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())).unwrap();
        let (w, h) = (3840usize, 2160usize);
        let mut ff = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-ss", "2", "-i", &src, "-frames:v", "1", "-vf", "scale=in_range=tv:out_range=tv,format=nv12", "-f", "rawvideo", "-"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut raw = vec![0u8; w * h * 3 / 2];
        ff.stdout.take().unwrap().read_exact(&mut raw).unwrap();
        let pool = make_pool(w, h, PIX_420V);
        let frame = new_buffer(&pool).unwrap();
        unsafe {
            CVPixelBufferLockBaseAddress(&frame, CVPixelBufferLockFlags(0));
            for (plane, (rows, off)) in [(h, 0usize), (h / 2, w * h)].into_iter().enumerate() {
                let base = CVPixelBufferGetBaseAddressOfPlane(&frame, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&frame, plane);
                for y in 0..rows {
                    std::ptr::copy_nonoverlapping(raw.as_ptr().add(off + y * w), base.add(y * stride), w);
                }
            }
            CVPixelBufferUnlockBaseAddress(&frame, CVPixelBufferLockFlags(0));
        }
        crate::scale::tag_bt709(&frame);
        let mut sc = Worker::new();
        sc.scaled_bgra(&frame, (aw, ah), false);
        let (t, c) = (std::time::Instant::now(), cpu());
        let mut new = None;
        for _ in 0..30 {
            new = sc.scaled_bgra(&frame, (aw, ah), false);
        }
        println!("new path: {:.2} ms/frame wall, {:.2} ms CPU", t.elapsed().as_secs_f64() * 1000.0 / 30.0, (cpu() - c) * 1000.0 / 30.0);
        write_bgra(&new.unwrap(), &format!("{out}.new.bgra"));
        // The old path: CoreImage affine to 960 px wide, sRGB, then read
        // back and swizzled on the CPU for an egui texture upload.
        unsafe {
            use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
            use objc2_core_image::{CIContext, CIImage};
            let opts = dict(vec![(&*ns("kCIContextCacheIntermediates"), num_b(false))]);
            let ctx = CIContext::contextWithOptions(Some(&opts));
            let cs = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)).unwrap();
            let (ow, oh) = (960usize, 540usize);
            let opool = make_pool(ow, oh, PIX_BGRA);
            let (t, c) = (std::time::Instant::now(), cpu());
            let mut last = None;
            for _ in 0..30 {
                let o = new_buffer(&opool).unwrap();
                let s = ow as f64 / w as f64;
                let img = CIImage::imageWithCVPixelBuffer(&frame).imageByApplyingTransform(CGAffineTransform { a: s, b: 0.0, c: 0.0, d: s, tx: 0.0, ty: 0.0 });
                ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, &o, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(ow as f64, oh as f64)), Some(&cs));
                CVPixelBufferLockBaseAddress(&o, CVPixelBufferLockFlags::ReadOnly);
                let base = CVPixelBufferGetBaseAddress(&o) as *const u8;
                let stride = CVPixelBufferGetBytesPerRow(&o);
                let mut rgba = Vec::with_capacity(ow * oh * 4);
                for y in 0..oh {
                    let row = std::slice::from_raw_parts(base.add(y * stride), ow * 4);
                    for px in row.as_chunks::<4>().0 {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    }
                }
                CVPixelBufferUnlockBaseAddress(&o, CVPixelBufferLockFlags::ReadOnly);
                std::hint::black_box(egui::ColorImage::from_rgba_unmultiplied([ow, oh], &rgba));
                last = Some(o);
            }
            println!(
                "old path: {:.2} ms/frame wall, {:.2} ms CPU (before the texture upload)",
                t.elapsed().as_secs_f64() * 1000.0 / 30.0,
                (cpu() - c) * 1000.0 / 30.0
            );
            write_bgra(&last.unwrap(), &format!("{out}.old.bgra"));
        }
    }

    /// An sRGB texture would be decoded to linear when sampled, and egui
    /// treats samples as gamma-encoded: the preview came out far too dark
    /// (mid-gray 128 shown as 47).
    #[test]
    fn preview_texture_is_not_srgb() {
        assert!(!TEXTURE_FORMAT.is_srgb());
        assert_eq!(MTL_FORMAT, MTLPixelFormat::BGRA8Unorm);
    }

    #[test]
    fn preview_size_follows_the_area_in_pixels() {
        // A 4K frame in a 1390x782 pt area on a 2x display: 2780x1564 px.
        assert_eq!(target_size(3840, 2160, (2780, 1564), false), (2780, 1564));
        // Never upscaled; aspect kept; low power halves it.
        assert_eq!(target_size(1920, 1080, (2780, 1564), false), (1920, 1080));
        assert_eq!(target_size(3840, 2160, (2000, 2000), false), (2000, 1124));
        assert_eq!(target_size(3840, 2160, (2780, 1564), true), (1390, 782));
    }
}
