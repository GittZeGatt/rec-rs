//! Live preview: the newest composited frame, scaled down on the GPU with
//! CoreImage and copied into an egui texture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_image::{CIContext, CIImage};
use objc2_core_video::*;

use crate::util::*;
use crate::writer::PreviewFn;

/// Preview width in pixels (height follows the frame's aspect ratio).
const WIDTH: usize = 960;

static FRAME: Mutex<Option<SendBox<CFRetained<CVPixelBuffer>>>> = Mutex::new(None);
static WANTED: AtomicBool = AtomicBool::new(false);

/// Whether frames are wanted (main window visible).
pub fn set_wanted(on: bool) {
    WANTED.store(on, Ordering::Relaxed);
    if !on {
        FRAME.lock().unwrap().take();
    }
}

/// Session callback: keep the newest frame and wake the UI.
pub fn callback(wake: Arc<dyn Fn() + Send + Sync>) -> PreviewFn {
    Arc::new(move |frame| {
        if !WANTED.load(Ordering::Relaxed) {
            return;
        }
        let first = FRAME.lock().unwrap().replace(SendBox(frame)).is_none();
        if first {
            wake();
        }
    })
}

type Pool = SendBox<CFRetained<CVPixelBufferPool>>;

pub struct Scaler {
    ctx: Retained<CIContext>,
    cs: CFRetained<CGColorSpace>,
    pool: Option<((usize, usize), Pool)>,
}

impl Scaler {
    pub fn new() -> Self {
        unsafe {
            let opts = dict(vec![(&*ns("kCIContextCacheIntermediates"), num_b(false))]);
            Scaler {
                ctx: CIContext::contextWithOptions(Some(&opts)),
                cs: CGColorSpace::with_name(Some(kCGColorSpaceSRGB)).expect("sRGB"),
                pool: None,
            }
        }
    }

    /// Take the pending frame (if any) as an RGBA image.
    pub fn take(&mut self) -> Option<egui::ColorImage> {
        let frame = FRAME.lock().unwrap().take()?;
        self.scale(&frame.0)
    }

    fn scale(&mut self, src: &CVPixelBuffer) -> Option<egui::ColorImage> {
        let (sw, sh) = (CVPixelBufferGetWidth(src), CVPixelBufferGetHeight(src));
        if sw == 0 || sh == 0 {
            return None;
        }
        let w = WIDTH.min(sw);
        let h = ((sh * w) as f64 / sw as f64).round().max(2.0) as usize;
        if self.pool.as_ref().is_none_or(|(sz, _)| *sz != (w, h)) {
            self.pool = Some(((w, h), SendBox(make_pool(w, h, PIX_BGRA))));
        }
        let pool = &self.pool.as_ref()?.1.0;
        unsafe {
            let mut out = std::ptr::null_mut();
            CVPixelBufferPool::create_pixel_buffer(None, pool, std::ptr::NonNull::from(&mut out));
            let out: CFRetained<CVPixelBuffer> = CFRetained::from_raw(std::ptr::NonNull::new(out)?);
            let s = w as f64 / sw as f64;
            let img = CIImage::imageWithCVPixelBuffer(src)
                .imageByApplyingTransform(CGAffineTransform { a: s, b: 0.0, c: 0.0, d: s, tx: 0.0, ty: 0.0 });
            let bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
            self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, &out, bounds, Some(&self.cs));
            CVPixelBufferLockBaseAddress(&out, CVPixelBufferLockFlags::ReadOnly);
            let base = CVPixelBufferGetBaseAddress(&out) as *const u8;
            let stride = CVPixelBufferGetBytesPerRow(&out);
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                let row = std::slice::from_raw_parts(base.add(y * stride), w * 4);
                for px in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
            }
            CVPixelBufferUnlockBaseAddress(&out, CVPixelBufferLockFlags::ReadOnly);
            Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
        }
    }
}
