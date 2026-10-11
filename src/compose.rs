//! GPU (CoreImage/Metal) compositing of the webcam over the screen.

use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_image::{CIColor, CIContext, CIFilter, CIImage, CIVector};
use objc2_core_video::CVPixelBuffer;

use crate::util::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Shape {
    /// Rounded rectangle, camera aspect ratio.
    Rounded,
    /// Circle (center square crop).
    Circle,
    /// Plain rectangle.
    Rect,
}

#[derive(Clone, Debug)]
pub struct PipSpec {
    /// Overlay width as a fraction of the output width.
    pub size: f64,
    pub corner: Corner,
    pub shape: Shape,
    pub mirror: bool,
}

pub struct Compositor {
    ctx: Retained<CIContext>,
    cs: CFRetained<CGColorSpace>,
    w: f64,
    h: f64,
    pip: PipSpec,
    mask: Option<(CGRect, Retained<CIImage>)>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

impl Compositor {
    pub fn new(width: usize, height: usize, pip: PipSpec) -> Self {
        unsafe {
            let opts = dict(vec![(&*ns("kCIContextCacheIntermediates"), num_b(false))]);
            let ctx = CIContext::contextWithOptions(Some(&opts));
            let cs = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)).expect("sRGB colorspace");
            Compositor { ctx, cs, w: width as f64, h: height as f64, pip, mask: None }
        }
    }

    fn mask_for(&mut self, r: CGRect) -> Retained<CIImage> {
        if let Some((mr, m)) = &self.mask
            && *mr == r {
                return m.clone();
            }
        let radius = match self.pip.shape {
            Shape::Circle => r.size.width / 2.0,
            Shape::Rounded => r.size.width * 0.06,
            Shape::Rect => 0.0,
        };
        let m = unsafe {
            let params = dict(vec![
                (&*ns("inputExtent"), obj(&*CIVector::vectorWithCGRect(r))),
                (&*ns("inputRadius"), num_f(radius)),
                (&*ns("inputColor"), obj(&*CIColor::colorWithRed_green_blue(1.0, 1.0, 1.0))),
            ]);
            let f = CIFilter::filterWithName_withInputParameters(&ns("CIRoundedRectangleGenerator"), Some(&params))
                .expect("CIRoundedRectangleGenerator");
            f.outputImage().expect("mask image").imageByCroppingToRect(r)
        };
        self.mask = Some((r, m.clone()));
        m
    }

    pub fn render(&mut self, screen: &CVPixelBuffer, cam: Option<&CVPixelBuffer>, out: &CVPixelBuffer) {
        unsafe {
            let mut img = CIImage::imageWithCVPixelBuffer(screen);
            let se = img.extent();
            if se.size.width != self.w || se.size.height != self.h {
                // Fit screen into the output canvas (should be a no-op: SCK
                // already delivers the output size).
                let s = (self.w / se.size.width).min(self.h / se.size.height);
                let tx = (self.w - se.size.width * s) / 2.0;
                let ty = (self.h - se.size.height * s) / 2.0;
                img = img.imageByApplyingTransform(CGAffineTransform { a: s, b: 0.0, c: 0.0, d: s, tx, ty });
            }
            if let Some(cam) = cam {
                let c = CIImage::imageWithCVPixelBuffer(cam);
                let e = c.extent();
                let crop = if self.pip.shape == Shape::Circle {
                    let side = e.size.width.min(e.size.height);
                    rect(
                        e.origin.x + (e.size.width - side) / 2.0,
                        e.origin.y + (e.size.height - side) / 2.0,
                        side,
                        side,
                    )
                } else {
                    e
                };
                let tw = (self.w * self.pip.size).round();
                let s = tw / crop.size.width;
                let th = (crop.size.height * s).round();
                let margin = (self.w * 0.02).round();
                let x = match self.pip.corner {
                    Corner::BottomRight | Corner::TopRight => self.w - margin - tw,
                    _ => margin,
                };
                // CoreImage origin is bottom-left.
                let y = match self.pip.corner {
                    Corner::BottomRight | Corner::BottomLeft => margin,
                    _ => self.h - margin - th,
                };
                let (a, tx) = if self.pip.mirror {
                    (-s, x + s * (crop.origin.x + crop.size.width))
                } else {
                    (s, x - s * crop.origin.x)
                };
                let t = CGAffineTransform { a, b: 0.0, c: 0.0, d: s, tx, ty: y - s * crop.origin.y };
                let target = rect(x, y, tw, th);
                let placed = c.imageByCroppingToRect(crop).imageByApplyingTransform(t).imageByCroppingToRect(target);
                let overlay = if self.pip.shape == Shape::Rect {
                    placed
                } else {
                    let mask = self.mask_for(target);
                    let params = dict(vec![(&*ns("inputBackgroundImage"), obj(&*mask))]);
                    placed.imageByApplyingFilter_withInputParameters(&ns("CISourceInCompositing"), &params)
                };
                img = overlay.imageByCompositingOverImage(&img);
            }
            self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, out, rect(0.0, 0.0, self.w, self.h), Some(&self.cs));
        }
    }
}
