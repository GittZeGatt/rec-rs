//! Webcam capture (AVCaptureSession) and device enumeration.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::*;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::kCVPixelBufferPixelFormatTypeKey;
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol};
use dispatch2::DispatchQueue;

use crate::record::Shared;
use crate::util::*;

pub struct Device {
    pub id: String,
    pub name: String,
}

fn discover(types: &[&AVCaptureDeviceType], media: &AVMediaType) -> Vec<Retained<AVCaptureDevice>> {
    unsafe {
        let arr = NSArray::from_slice(types);
        let s = AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &arr,
            Some(media),
            AVCaptureDevicePosition::Unspecified,
        );
        s.devices().iter().collect()
    }
}

fn cameras_raw() -> Vec<Retained<AVCaptureDevice>> {
    unsafe {
        discover(
            &[
                AVCaptureDeviceTypeBuiltInWideAngleCamera,
                AVCaptureDeviceTypeExternal,
                AVCaptureDeviceTypeContinuityCamera,
            ],
            AVMediaTypeVideo.unwrap(),
        )
    }
}

fn describe(d: &AVCaptureDevice) -> Device {
    unsafe { Device { id: d.uniqueID().to_string(), name: d.localizedName().to_string() } }
}

pub fn cameras() -> Vec<Device> {
    cameras_raw().iter().map(|d| describe(d)).collect()
}

pub fn microphones() -> Vec<Device> {
    unsafe {
        discover(&[AVCaptureDeviceTypeMicrophone], AVMediaTypeAudio.unwrap())
            .iter()
            .map(|d| describe(d))
            .collect()
    }
}

/// A saved or requested mic resolved against the connected devices.
pub struct MicPick {
    /// Unique ID to open; None = the system default mic.
    pub id: Option<String>,
    /// Why the requested mic wasn't used (it is not connected).
    pub warning: Option<String>,
}

/// Resolve a mic selector without failing: by unique ID, then by name, then
/// the system default with a warning. Audio never blocks a session.
pub fn pick_mic(sel: &str, name: Option<&str>) -> MicPick {
    let mics = microphones();
    match match_mic(&mics, sel, name) {
        Some(id) => MicPick { id: Some(id), warning: None },
        None => {
            let label = name.filter(|n| !n.is_empty()).unwrap_or(sel);
            MicPick { id: None, warning: Some(format!("Microphone \u{201c}{label}\u{201d} not connected; using the default mic")) }
        }
    }
}

fn match_mic(mics: &[Device], sel: &str, name: Option<&str>) -> Option<String> {
    // Exact unique ID.
    if let Some(m) = mics.iter().find(|m| m.id == sel) {
        return Some(m.id.clone());
    }
    // USB unique IDs end in a port/instance index (`…:SERIAL:4`) that can
    // change between plugs; match on the rest.
    let stem = |id: &str| id.rsplit_once(':').filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit())).map(|(s, _)| s.to_string());
    if let Some(st) = stem(sel)
        && let Some(m) = mics.iter().find(|m| stem(&m.id).as_deref() == Some(st.as_str()))
    {
        return Some(m.id.clone());
    }
    // Saved display name (exact, then substring), then the selector as a name.
    for n in name.into_iter().chain([sel]).filter(|n| !n.is_empty()) {
        let lower = n.to_lowercase();
        if let Some(m) = mics.iter().find(|m| m.name.to_lowercase() == lower) {
            return Some(m.id.clone());
        }
        if let Some(m) = mics.iter().find(|m| m.name.to_lowercase().contains(&lower)) {
            return Some(m.id.clone());
        }
    }
    None
}

/// Call `f` on the main queue whenever a capture device is connected or
/// disconnected. The observers live for the process.
pub fn on_device_change(f: impl Fn() + Clone + 'static) {
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};
    unsafe {
        let center = NSNotificationCenter::defaultCenter();
        let queue = NSOperationQueue::mainQueue();
        for name in [AVCaptureDeviceWasConnectedNotification, AVCaptureDeviceWasDisconnectedNotification] {
            let f = f.clone();
            let block = block2::RcBlock::new(move |_n: std::ptr::NonNull<NSNotification>| f());
            let token = center.addObserverForName_object_queue_usingBlock(Some(name), None, Some(&queue), &block);
            std::mem::forget(token);
        }
    }
}

pub struct CamIvars {
    /// Where frames go; switched when the camera moves to another session.
    shared: std::sync::Mutex<Arc<Shared>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RecCameraOutput"]
    #[ivars = CamIvars]
    pub struct CameraOutput;

    unsafe impl NSObjectProtocol for CameraOutput {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn did_output(&self, _o: &AVCaptureOutput, sb: &CMSampleBuffer, _c: &AVCaptureConnection) {
            unsafe {
                if let Some(pb) = sb.image_buffer() {
                    // REC_DEBUG: the camera's color tags (once).
                    static LOGGED: std::sync::Once = std::sync::Once::new();
                    if std::env::var_os("REC_DEBUG").is_some() {
                        LOGGED.call_once(|| {
                            let a = pb.attachments(objc2_core_video::CVAttachmentMode::ShouldPropagate);
                            eprintln!("rec: debug: camera frame attachments: {a:?}");
                        });
                    }
                    let pts = sb.presentation_time_stamp();
                    let shared = self.ivars().shared.lock().unwrap().clone();
                    shared.with(|s| s.push_cam(pts, pb));
                }
            }
        }
    }
);

pub struct Camera {
    session: Retained<AVCaptureSession>,
    out: Retained<CameraOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    pub name: String,
    /// The device's unique ID.
    pub id: String,
    /// When it started: auto-exposure and white balance settle over the
    /// first second or two, so a fresh camera looks dark at first.
    pub started: std::time::Instant,
}

unsafe impl Send for Camera {}

impl Camera {
    /// The device `sel` names: None = the system's preferred camera, else a
    /// unique ID or name substring.
    fn device(sel: Option<&str>) -> Result<Retained<AVCaptureDevice>> {
        let cams = cameras_raw();
        match sel {
            // System-preferred camera (what FaceTime would use), else the
            // built-in camera, else anything.
            None => unsafe { AVCaptureDevice::systemPreferredCamera() }
                .or_else(|| {
                    cams.iter()
                        .find(|d| unsafe { d.deviceType().to_string() == AVCaptureDeviceTypeBuiltInWideAngleCamera.to_string() })
                        .cloned()
                })
                .or_else(|| cams.into_iter().next()),
            Some(s) => {
                let lower = s.to_lowercase();
                cams.into_iter().find(|d| unsafe {
                    d.uniqueID().to_string() == s || d.localizedName().to_string().to_lowercase().contains(&lower)
                })
            }
        }
        .ok_or_else(|| anyhow!("no camera found (see `rec-rs devices`)"))
    }

    /// The unique ID of the camera `sel` names (see `device`).
    pub fn resolve(sel: Option<&str>) -> Result<String> {
        Ok(unsafe { Self::device(sel)?.uniqueID().to_string() })
    }

    /// Send frames to another session's sink (the camera keeps running,
    /// already exposed, instead of restarting).
    pub fn retarget(&self, shared: Arc<Shared>) {
        *self.out.ivars().shared.lock().unwrap() = shared;
    }

    /// `sel`: None = first camera, else unique ID or name substring.
    pub fn start(sel: Option<&str>, shared: Arc<Shared>) -> Result<Self> {
        let dev = Self::device(sel)?;
        unsafe {
            let name = dev.localizedName().to_string();
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            if session.canSetSessionPreset(AVCaptureSessionPreset1280x720) {
                session.setSessionPreset(AVCaptureSessionPreset1280x720);
            } else {
                session.setSessionPreset(AVCaptureSessionPresetHigh);
            }
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&dev)
                .map_err(|e| anyhow!("camera {name}: {}", e.localizedDescription()))?;
            if !session.canAddInput(&input) {
                bail!("cannot use camera {name}");
            }
            session.addInput(&input);
            let output = AVCaptureVideoDataOutput::new();
            output.setVideoSettings(Some(&dict(vec![(
                cf_ns(kCVPixelBufferPixelFormatTypeKey),
                num_i(PIX_BGRA as i64),
            )])));
            output.setAlwaysDiscardsLateVideoFrames(true);
            let delegate = Self::make_delegate(shared);
            let queue = DispatchQueue::new("rec.camera", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            if !session.canAddOutput(&output) {
                bail!("cannot add camera output");
            }
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            if !session.isRunning() {
                bail!("camera {name} failed to start");
            }
            let id = dev.uniqueID().to_string();
            Ok(Camera { session, out: delegate, _queue: queue, name, id, started: std::time::Instant::now() })
        }
    }

    fn make_delegate(shared: Arc<Shared>) -> Retained<CameraOutput> {
        let this = CameraOutput::alloc().set_ivars(CamIvars { shared: std::sync::Mutex::new(shared) });
        unsafe { msg_send![super(this), init] }
    }

    pub fn stop(&self) {
        unsafe { self.session.stopRunning() };
    }
}

pub struct MicIvars {
    shared: Arc<Shared>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RecMicOutput"]
    #[ivars = MicIvars]
    pub struct MicOutput;

    unsafe impl NSObjectProtocol for MicOutput {}

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for MicOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn did_output(&self, _o: &AVCaptureOutput, sb: &CMSampleBuffer, _c: &AVCaptureConnection) {
            self.ivars().shared.with(|s| s.push_audio(crate::writer::AudioKind::Mic, sb));
        }
    }
);

/// Microphone via AVCaptureSession (fallback / test path; the default mic path
/// is ScreenCaptureKit's built-in microphone capture).
pub struct Mic {
    session: Retained<AVCaptureSession>,
    _out: Retained<MicOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

unsafe impl Send for Mic {}

impl Mic {
    pub fn start(id: Option<&str>, shared: Arc<Shared>) -> Result<Self> {
        unsafe {
            let media = AVMediaTypeAudio.unwrap();
            let dev = match id {
                Some(id) => AVCaptureDevice::deviceWithUniqueID(&objc2_foundation::NSString::from_str(id)),
                None => AVCaptureDevice::defaultDeviceWithMediaType(media),
            }
            .ok_or_else(|| anyhow!("microphone not found"))?;
            let session = AVCaptureSession::new();
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&dev)
                .map_err(|e| anyhow!("microphone: {}", e.localizedDescription()))?;
            if !session.canAddInput(&input) {
                bail!("cannot use microphone");
            }
            session.addInput(&input);
            let output = AVCaptureAudioDataOutput::new();
            let this = MicOutput::alloc().set_ivars(MicIvars { shared });
            let delegate: Retained<MicOutput> = msg_send![super(this), init];
            let queue = DispatchQueue::new("rec.mic", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            if !session.canAddOutput(&output) {
                bail!("cannot add microphone output");
            }
            session.addOutput(&output);
            session.startRunning();
            Ok(Mic { session, _out: delegate, _queue: queue })
        }
    }

    pub fn stop(&self) {
        unsafe { self.session.stopRunning() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> Device {
        Device { id: id.into(), name: name.into() }
    }

    #[test]
    fn mic_matching() {
        let mics = [dev("BuiltInMicrophoneDevice", "MacBook Pro Microphone"), dev("AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:2", "OsmoPocket3")];
        let dji = Some("AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:2".to_string());
        // Same device on another port.
        assert_eq!(match_mic(&mics, "AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:4", None), dji);
        // By saved name.
        assert_eq!(match_mic(&mics, "gone-id", Some("OsmoPocket3")), dji);
        // By name substring (CLI).
        assert_eq!(match_mic(&mics, "macbook", None).as_deref(), Some("BuiltInMicrophoneDevice"));
        assert_eq!(match_mic(&mics[..1], "AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:4", Some("OsmoPocket3")), None);
    }
}
