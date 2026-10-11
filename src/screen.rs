//! ScreenCaptureKit: display video, system audio, and microphone.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, kCGColorSpaceSRGB, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::*;
use dispatch2::DispatchQueue;

use crate::record::Shared;
use crate::util::*;
use crate::writer::AudioKind;

pub struct DisplayInfo {
    pub id: u32,
    pub points: (i64, i64),
    pub pixels: (usize, usize),
    pub display: Retained<SCDisplay>,
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let r = if let Some(c) = unsafe { Retained::retain(content) } {
            Ok(SendBox(c))
        } else {
            let msg = unsafe { err.as_ref() }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "unknown error".into());
            Err(msg)
        };
        let _ = tx.send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => bail!(
            "ScreenCaptureKit refused: {e}\n  -> grant Screen & System Audio Recording to Rec (see `rec permissions`)"
        ),
        Err(_) => bail!("timed out waiting for ScreenCaptureKit shareable content"),
    }
}

pub fn displays() -> Result<Vec<DisplayInfo>> {
    let content = shareable_content()?;
    let mut out = vec![];
    for d in unsafe { content.displays() }.iter() {
        let id = unsafe { d.displayID() };
        let mode = CGDisplayCopyDisplayMode(id);
        let pixels = (
            CGDisplayMode::pixel_width(mode.as_deref()),
            CGDisplayMode::pixel_height(mode.as_deref()),
        );
        out.push(DisplayInfo { id, points: unsafe { (d.width() as i64, d.height() as i64) }, pixels, display: d });
    }
    Ok(out)
}

pub struct WindowInfo {
    pub id: u32,
    pub app: String,
    pub title: String,
    /// Size in points.
    pub size: (f64, f64),
    pub window: Retained<SCWindow>,
}

/// On-screen, normal-layer windows that have a title or an owning app.
pub fn windows() -> Result<Vec<WindowInfo>> {
    let content = shareable_content()?;
    let mut out = vec![];
    for w in unsafe { content.windows() }.iter() {
        unsafe {
            if !w.isOnScreen() || w.windowLayer() != 0 {
                continue;
            }
            let app = w.owningApplication().map(|a| a.applicationName().to_string()).unwrap_or_default();
            let title = w.title().map(|t| t.to_string()).unwrap_or_default();
            let f = w.frame();
            if (app.is_empty() && title.is_empty()) || f.size.width < 40.0 || f.size.height < 40.0 {
                continue;
            }
            out.push(WindowInfo { id: w.windowID(), app, title, size: (f.size.width, f.size.height), window: w });
        }
    }
    Ok(out)
}

/// Find a window by numeric ID, or case-insensitive substring of
/// "<app> <title>". Prefers the largest match.
pub fn find_window(sel: &str) -> Result<WindowInfo> {
    let ws = windows()?;
    if let Ok(id) = sel.parse::<u32>() {
        if let Some(w) = ws.into_iter().find(|w| w.id == id) {
            return Ok(w);
        }
        bail!("no on-screen window with id {id} (see `rec-rs devices`)");
    }
    let lower = sel.to_lowercase();
    ws.into_iter()
        .filter(|w| format!("{} {}", w.app, w.title).to_lowercase().contains(&lower))
        .max_by(|a, b| (a.size.0 * a.size.1).total_cmp(&(b.size.0 * b.size.1)))
        .ok_or_else(|| anyhow!("no on-screen window matches {sel:?} (see `rec-rs devices`)"))
}

pub enum Target {
    /// A display, optionally cropped to a region (points, top-left origin).
    Display(DisplayInfo, Option<objc2_core_foundation::CGRect>),
    Window(WindowInfo),
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::Display(d, None) => format!("display {}", d.id),
            Target::Display(d, Some(r)) => format!(
                "display {} region {}x{}+{}+{}",
                d.id, r.size.width, r.size.height, r.origin.x, r.origin.y
            ),
            Target::Window(w) => format!("window \"{} {}\" ({})", w.app, w.title, w.id),
        }
    }
}

pub struct ScreenConfig {
    pub target: Target,
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub pixel_format: u32,
    pub cursor: bool,
    pub clicks: bool,
    pub system_audio: bool,
    /// None = no mic; Some(None) = default mic; Some(Some(id)) = specific device.
    pub mic: Option<Option<String>>,
}

pub struct Ivars {
    shared: Arc<Shared>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RecScreenOutput"]
    #[ivars = Ivars]
    pub struct ScreenOutput;

    unsafe impl NSObjectProtocol for ScreenOutput {}

    unsafe impl SCStreamOutput for ScreenOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(&self, _stream: &SCStream, sb: &CMSampleBuffer, kind: SCStreamOutputType) {
            self.handle(sb, kind);
        }
    }

    unsafe impl SCStreamDelegate for ScreenOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!("screen capture stopped: {} [{} {}]", error.localizedDescription(), error.domain(), error.code());
            self.ivars().shared.fail(msg);
        }
    }
);

impl ScreenOutput {
    fn create(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { shared });
        unsafe { msg_send![super(this), init] }
    }

    fn handle(&self, sb: &CMSampleBuffer, kind: SCStreamOutputType) {
        unsafe {
            if !sb.is_valid() {
                return;
            }
        }
        let shared = &self.ivars().shared;
        if kind == SCStreamOutputType::Screen {
            if !frame_complete(sb) {
                return;
            }
            let Some(pb) = (unsafe { sb.image_buffer() }) else { return };
            let pts: CMTime = unsafe { sb.presentation_time_stamp() };
            let pb: objc2_core_foundation::CFRetained<CVPixelBuffer> = pb;
            shared.with(|s| s.push_screen(pts, pb));
        } else if kind == SCStreamOutputType::Audio {
            shared.with(|s| s.push_audio(AudioKind::System, sb));
        } else if kind == SCStreamOutputType::Microphone {
            shared.with(|s| s.push_audio(AudioKind::Mic, sb));
        }
    }
}

/// Only `Complete` frames carry new pixels; idle/blank/suspended do not.
fn frame_complete(sb: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(arr) = sb.sample_attachments_array(false) else { return false };
        let arr: &NSArray<NSDictionary<NSString, AnyObject>> =
            &*(&*arr as *const objc2_core_foundation::CFArray as *const NSArray<NSDictionary<NSString, AnyObject>>);
        let Some(info) = arr.firstObject() else { return false };
        let Some(status) = info.objectForKey(SCStreamFrameInfoStatus) else { return false };
        let Ok(n) = status.downcast::<NSNumber>() else { return false };
        n.integerValue() == SCFrameStatus::Complete.0
    }
}

pub struct ScreenCapture {
    stream: Retained<SCStream>,
    _output: Retained<ScreenOutput>,
    _queues: Vec<dispatch2::DispatchRetained<DispatchQueue>>,
}

unsafe impl Send for ScreenCapture {}

impl ScreenCapture {
    pub fn start(cfg: &ScreenConfig, shared: Arc<Shared>) -> Result<Self> {
        unsafe {
            let conf = SCStreamConfiguration::new();
            let filter = match &cfg.target {
                Target::Display(d, region) => {
                    if let Some(r) = region {
                        conf.setSourceRect(*r);
                    }
                    // Exclude rec's own windows (the GUI preview), which
                    // would otherwise be recorded as a hall of mirrors.
                    let me = std::process::id() as i32;
                    let own = shareable_content()
                        .ok()
                        .and_then(|c| c.applications().iter().find(|a| a.processID() == me));
                    if std::env::var_os("REC_DEBUG").is_some() {
                        eprintln!("rec: debug: excluding own app from capture: {}", own.is_some());
                    }
                    match own {
                        Some(app) => SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                            SCContentFilter::alloc(),
                            &d.display,
                            &NSArray::from_retained_slice(&[app]),
                            &NSArray::new(),
                        ),
                        None => SCContentFilter::initWithDisplay_excludingWindows(
                            SCContentFilter::alloc(),
                            &d.display,
                            &NSArray::new(),
                        ),
                    }
                }
                Target::Window(w) => {
                    SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w.window)
                }
            };
            conf.setWidth(cfg.width);
            conf.setHeight(cfg.height);
            // Slightly under the frame period, so capture jitter never makes
            // ScreenCaptureKit skip a frame the pacer needs (as OBS does).
            conf.setMinimumFrameInterval(CMTime::new(9, cfg.fps as i32 * 10));
            conf.setPixelFormat(cfg.pixel_format);
            conf.setQueueDepth(8);
            conf.setShowsCursor(cfg.cursor);
            conf.setShowMouseClicks(cfg.clicks);
            conf.setScalesToFit(true);
            conf.setPreservesAspectRatio(true);
            conf.setCaptureResolution(SCCaptureResolutionType::Best);
            conf.setColorSpaceName(kCGColorSpaceSRGB);
            if cfg.pixel_format == PIX_420V {
                conf.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
            }
            conf.setCapturesAudio(cfg.system_audio);
            conf.setSampleRate(48000);
            conf.setChannelCount(2);
            conf.setExcludesCurrentProcessAudio(true);
            if let Some(dev) = &cfg.mic {
                conf.setCaptureMicrophone(true);
                if let Some(id) = dev {
                    conf.setMicrophoneCaptureDeviceID(Some(&NSString::from_str(id)));
                }
            }

            let output = ScreenOutput::create(shared);
            let delegate = ProtocolObject::from_ref(&*output);
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &conf, Some(delegate));
            let out_proto: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*output);
            let vq = DispatchQueue::new("rec.screen.video", None);
            let aq = DispatchQueue::new("rec.screen.audio", None);
            let mq = DispatchQueue::new("rec.screen.mic", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Screen, Some(&vq))
                .map_err(|e| anyhow!("add screen output: {}", e.localizedDescription()))?;
            if cfg.system_audio {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Audio, Some(&aq))
                    .map_err(|e| anyhow!("add audio output: {}", e.localizedDescription()))?;
            }
            if cfg.mic.is_some() {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Microphone, Some(&mq))
                    .map_err(|e| anyhow!("add microphone output: {}", e.localizedDescription()))?;
            }
            wait_err(|b| stream.startCaptureWithCompletionHandler(Some(b)))
                .context("starting screen capture")?;
            Ok(ScreenCapture { stream, _output: output, _queues: vec![vq, aq, mq] })
        }
    }

    pub fn stop(&self) -> Result<()> {
        unsafe { wait_err(|b| self.stream.stopCaptureWithCompletionHandler(Some(b))) }
    }
}

fn wait_err(f: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |err: *mut NSError| {
        let r = match unsafe { err.as_ref() } {
            None => Ok(()),
            Some(e) => Err(format!("{} [{} {}]", e.localizedDescription(), e.domain(), e.code())),
        };
        let _ = tx.send(r);
    });
    f(&block);
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => bail!("timed out"),
    }
}
