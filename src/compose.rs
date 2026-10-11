//! GPU (CoreImage/Metal) compositing of the webcam over the screen.

use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_image::{CIColor, CIContext, CIFilter, CIImage, CIVector};
use objc2_core_video::CVPixelBuffer;

use crate::util::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    /// Rounded rectangle, camera aspect ratio.
    Rounded,
    /// Circle (center square crop).
    Circle,
    /// Plain rectangle.
    Rect,
}

/// What the output frame shows. Switchable live while the webcam runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scene {
    /// The screen only.
    Screen,
    /// The screen with the webcam picture-in-picture.
    #[default]
    ScreenCam,
    /// The webcam full frame.
    Cam,
}

impl Scene {
    pub const ALL: [Scene; 3] = [Scene::Screen, Scene::ScreenCam, Scene::Cam];

    pub fn label(self) -> &'static str {
        match self {
            Scene::Screen => "Screen",
            Scene::ScreenCam => "Screen + Camera",
            Scene::Cam => "Camera",
        }
    }

    pub fn uses_cam(self) -> bool {
        self != Scene::Screen
    }
}

/// Uniform scale and offset that fits (`cover` false) or fills (`cover`
/// true) a `sw`x`sh` source into a `dw`x`dh` canvas, centred.
pub fn fit(sw: f64, sh: f64, dw: f64, dh: f64, cover: bool) -> (f64, f64, f64) {
    let (a, b) = (dw / sw, dh / sh);
    let s = if cover { a.max(b) } else { a.min(b) };
    (s, (dw - sw * s) / 2.0, (dh - sh * s) / 2.0)
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

    pub fn render(&mut self, scene: Scene, screen: Option<&CVPixelBuffer>, cam: Option<&CVPixelBuffer>, out: &CVPixelBuffer) {
        unsafe {
            let canvas = rect(0.0, 0.0, self.w, self.h);
            let black = CIImage::imageWithColor(&CIColor::colorWithRed_green_blue(0.0, 0.0, 0.0)).imageByCroppingToRect(canvas);
            if scene == Scene::Cam {
                let mut img = black;
                if let Some(cam) = cam {
                    let c = CIImage::imageWithCVPixelBuffer(cam);
                    let e = c.extent();
                    let (s, tx, ty) = fit(e.size.width, e.size.height, self.w, self.h, true);
                    let (a, tx) = if self.pip.mirror { (-s, self.w - tx) } else { (s, tx) };
                    let t = CGAffineTransform { a, b: 0.0, c: 0.0, d: s, tx, ty };
                    img = c.imageByApplyingTransform(t).imageByCroppingToRect(canvas).imageByCompositingOverImage(&img);
                }
                self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, out, canvas, Some(&self.cs));
                return;
            }
            let mut img = black;
            if let Some(screen) = screen {
                let sc = CIImage::imageWithCVPixelBuffer(screen);
                let se = sc.extent();
                // Fit the screen into the canvas (a no-op normally: SCK
                // already delivers the output size).
                let (s, tx, ty) = fit(se.size.width, se.size.height, self.w, self.h, false);
                img = sc.imageByApplyingTransform(CGAffineTransform { a: s, b: 0.0, c: 0.0, d: s, tx, ty }).imageByCompositingOverImage(&img);
            }
            if scene == Scene::ScreenCam && let Some(cam) = cam {
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

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_letterboxes_and_cover_crops() {
        // 4:3 camera into a 16:9 canvas.
        let (s, tx, ty) = fit(1280.0, 960.0, 1920.0, 1080.0, false);
        assert_eq!((s, tx, ty), (1.125, 240.0, 0.0));
        let (s, tx, ty) = fit(1280.0, 960.0, 1920.0, 1080.0, true);
        assert_eq!((s, tx, ty), (1.5, 0.0, -180.0));
    }
}
