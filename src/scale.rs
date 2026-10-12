//! High-quality scaling of captured screen frames to the output size.
//!
//! ScreenCaptureKit can scale itself (`scalesToFit`), but its downscale is
//! soft, and VideoToolbox's pixel transfer only averages well near 1.33x.
//! Measured on a static text screen from a 5K display against a Lanczos
//! reference (scripts/quality.sh; ProRes, so the encoder doesn't matter),
//! SSIM-Y / PSNR-Y / VMAF:
//!
//! | Scaler | 4K | 1440p | 1080p |
//! |---|---|---|---|
//! | ScreenCaptureKit | 0.9948 / 35.0 / 90.2 | 0.9943 / 35.0 / 91.9 | 0.9948 / 35.3 / 88.8 |
//! | VideoToolbox, area average | 0.9967 / 36.6 / 96.4 | 0.9869 / 30.0 / 95.2 | 0.9737 / 27.9 / 79.2 |
//!
//! So the screen is captured at its native pixel size and scaled here with
//! Metal Performance Shaders' Lanczos filter, plane by plane (Y and CbCr,
//! like video scalers: no RGB round trip, gamma-encoded values), bound to
//! the frames' IOSurfaces without copies. Bars (when the aspect ratio
//! differs) are video black. VideoToolbox is the fallback.

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_core_video::*;
use objc2_metal::*;
use objc2_metal_performance_shaders::{MPSImageLanczosScale, MPSScaleTransform, MPSUnaryImageKernel};
use objc2_video_toolbox::*;

use crate::util::*;

struct Metal {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    cache: CFRetained<CVMetalTextureCache>,
    lanczos: Retained<MPSImageLanczosScale>,
}

pub struct FrameScaler {
    metal: Option<Metal>,
    vt: Option<CFRetained<VTPixelTransferSession>>,
    /// Output (width, height).
    size: (usize, usize),
}

// The Metal objects and the session are only used by the sink, behind its
// lock.
unsafe impl Send for FrameScaler {}

/// Tag a buffer BT.709 (what the encoders declare).
#[cfg(test)]
pub fn tag_bt709(buf: &CVPixelBuffer) {
    unsafe {
        let mode = CVAttachmentMode::ShouldPropagate;
        buf.set_attachment(kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(), mode);
        buf.set_attachment(kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2.as_ref(), mode);
        buf.set_attachment(kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2.as_ref(), mode);
    }
}

fn set(session: &VTPixelTransferSession, key: &CFString, value: &CFString) {
    unsafe {
        VTSessionSetProperty(session.as_ref(), key, Some(value.as_ref() as &CFType));
    }
}

fn vt_session() -> Option<CFRetained<VTPixelTransferSession>> {
    unsafe {
        let mut s = std::ptr::null_mut();
        if VTPixelTransferSession::create(None, std::ptr::NonNull::from(&mut s)) != 0 {
            return None;
        }
        let vt = CFRetained::from_raw(std::ptr::NonNull::new(s)?);
        set(&vt, kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_Letterbox);
        set(&vt, kVTPixelTransferPropertyKey_DownsamplingMode, kVTDownsamplingMode_Average);
        // Same curve and primaries in and out (no color conversion, as in
        // the unscaled path); the matrix matches what encoders tag.
        set(&vt, kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
        Some(vt)
    }
}

fn metal() -> Option<Metal> {
    unsafe {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        let mut cache = std::ptr::null_mut();
        if CVMetalTextureCache::create(None, None, &device, None, std::ptr::NonNull::from(&mut cache)) != 0 {
            return None;
        }
        let cache = CFRetained::from_raw(std::ptr::NonNull::new(cache)?);
        let lanczos = MPSImageLanczosScale::initWithDevice(MPSImageLanczosScale::alloc(), &device);
        Some(Metal { queue, cache, lanczos })
    }
}

/// One plane of a pixel buffer as a Metal texture (kept alive with it).
struct PlaneTexture {
    _cv: CFRetained<CVMetalTexture>,
    tex: Retained<ProtocolObject<dyn MTLTexture>>,
}

fn plane(cache: &CVMetalTextureCache, buf: &CVPixelBuffer, i: usize, format: MTLPixelFormat) -> Option<PlaneTexture> {
    let planar = CVPixelBufferIsPlanar(buf);
    let (w, h) = if planar {
        (CVPixelBufferGetWidthOfPlane(buf, i), CVPixelBufferGetHeightOfPlane(buf, i))
    } else {
        (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf))
    };
    unsafe {
        let mut t = std::ptr::null_mut();
        if CVMetalTextureCache::create_texture_from_image(None, cache, buf, None, format, w, h, i, std::ptr::NonNull::from(&mut t)) != 0 {
            return None;
        }
        let cv = CFRetained::from_raw(std::ptr::NonNull::new(t)?);
        let tex = CVMetalTextureGetTexture(&cv)?;
        Some(PlaneTexture { _cv: cv, tex })
    }
}

/// Planes of a frame: (Metal format, video-black clear color).
fn planes(buf: &CVPixelBuffer) -> Option<Vec<(MTLPixelFormat, [f64; 4])>> {
    let f = CVPixelBufferGetPixelFormatType(buf);
    if f == PIX_420V {
        Some(vec![(MTLPixelFormat::R8Unorm, [16.0 / 255.0, 0.0, 0.0, 1.0]), (MTLPixelFormat::RG8Unorm, [0.5, 0.5, 0.0, 1.0])])
    } else if f == PIX_BGRA {
        Some(vec![(MTLPixelFormat::BGRA8Unorm, [0.0, 0.0, 0.0, 1.0])])
    } else {
        None
    }
}

impl FrameScaler {
    pub fn new(width: usize, height: usize) -> Option<FrameScaler> {
        let metal = if std::env::var("REC_SCALER").as_deref() == Ok("vt") { None } else { metal() };
        let vt = if metal.is_none() { Some(vt_session()?) } else { None };
        Some(FrameScaler { metal, vt, size: (width, height) })
    }

    /// Whether `src` needs scaling to the output size.
    pub fn needed(&self, src: &CVPixelBuffer) -> bool {
        (CVPixelBufferGetWidth(src), CVPixelBufferGetHeight(src)) != self.size
    }

    /// Scale `src` into `out` (output size, same pixel format), letterboxed.
    pub fn scale(&mut self, src: &CVPixelBuffer, out: &CVPixelBuffer) -> bool {
        if self.metal.is_some() && self.scale_metal(src, out) {
            return true;
        }
        let vt = match &self.vt {
            Some(v) => v,
            None => match vt_session() {
                Some(v) => self.vt.insert(v),
                None => return false,
            },
        };
        unsafe { vt.transfer_image(src, out) == 0 }
    }

    fn scale_metal(&mut self, src: &CVPixelBuffer, out: &CVPixelBuffer) -> bool {
        let Some(m) = &self.metal else { return false };
        let Some(fmts) = planes(src) else { return false };
        if CVPixelBufferGetPixelFormatType(out) != CVPixelBufferGetPixelFormatType(src) {
            return false;
        }
        let (sw, sh) = (CVPixelBufferGetWidth(src) as f64, CVPixelBufferGetHeight(src) as f64);
        let (w, h) = (self.size.0 as f64, self.size.1 as f64);
        // Fit (letterbox) in output pixels, on even coordinates so the
        // chroma planes line up.
        let (s, tx, ty) = crate::compose::fit(sw, sh, w, h, false);
        let (tx, ty) = ((tx / 2.0).round() * 2.0, (ty / 2.0).round() * 2.0);
        let bars = tx > 0.0 || ty > 0.0;
        unsafe {
            let Some(cmd) = m.queue.commandBuffer() else { return false };
            let mut keep = vec![];
            for (i, (format, black)) in fmts.into_iter().enumerate() {
                let (Some(st), Some(dt)) = (plane(&m.cache, src, i, format), plane(&m.cache, out, i, format)) else {
                    return false;
                };
                // Plane scale: chroma planes are subsampled in both images.
                let k = dt.tex.width() as f64 / w;
                if bars {
                    let pass = MTLRenderPassDescriptor::new();
                    let a = pass.colorAttachments().objectAtIndexedSubscript(0);
                    a.setTexture(Some(&dt.tex));
                    a.setLoadAction(MTLLoadAction::Clear);
                    a.setStoreAction(MTLStoreAction::Store);
                    a.setClearColor(MTLClearColor { red: black[0], green: black[1], blue: black[2], alpha: black[3] });
                    let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) else { return false };
                    enc.endEncoding();
                }
                let t = MPSScaleTransform { scaleX: s, scaleY: s, translateX: tx * k, translateY: ty * k };
                m.lanczos.setScaleTransform(&t);
                let kernel: &MPSUnaryImageKernel = &m.lanczos;
                let (cw, ch) = ((sw * s * k).round() as usize, (sh * s * k).round() as usize);
                kernel.setClipRect(MTLRegion {
                    origin: MTLOrigin { x: (tx * k) as usize, y: (ty * k) as usize, z: 0 },
                    size: MTLSize { width: cw.min(dt.tex.width()), height: ch.min(dt.tex.height()), depth: 1 },
                });
                kernel.encodeToCommandBuffer_sourceTexture_destinationTexture(&cmd, &st.tex, &dt.tex);
                keep.push((st, dt));
            }
            cmd.commit();
            cmd.waitUntilCompleted();
            drop(keep);
            m.cache.flush(0);
            cmd.status() == MTLCommandBufferStatus::Completed
        }
    }
}
