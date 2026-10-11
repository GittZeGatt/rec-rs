//! Hardware H.264 encoding with a VideoToolbox compression session, for the
//! replay buffer and streaming (the recording keeps its own AVAssetWriter
//! encode at its own quality).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use anyhow::{Result, bail};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::*;
use objc2_core_video::CVPixelBuffer;
use objc2_video_toolbox::*;

use crate::util::*;

/// Encoder settings.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoConfig {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    /// Bits per second.
    pub bitrate: u64,
    /// Seconds between keyframes (streams need 2 s or less).
    pub keyframe_secs: f64,
    /// Constant bitrate (what streaming services expect).
    pub cbr: bool,
}

/// One encoded H.264 access unit: AVCC (4-byte length-prefixed NAL units).
pub struct VideoPacket {
    /// Host-clock seconds.
    pub pts: f64,
    pub key: bool,
    pub data: Arc<Vec<u8>>,
    /// The encoder's sample buffer (for writing MP4 files).
    pub sample: SendBox<CFRetained<CMSampleBuffer>>,
    pub format: SendBox<CFRetained<CMFormatDescription>>,
}

impl Clone for VideoPacket {
    fn clone(&self) -> Self {
        VideoPacket {
            pts: self.pts,
            key: self.key,
            data: self.data.clone(),
            sample: SendBox(self.sample.0.clone()),
            format: SendBox(self.format.0.clone()),
        }
    }
}

pub type OnPacket = Box<dyn Fn(VideoPacket) + Send + Sync>;

pub struct VideoEncoder {
    session: CFRetained<VTCompressionSession>,
    pub cfg: VideoConfig,
    /// Owned by the session's output callback (refcon).
    sink: *mut OnPacket,
    force_key: bool,
}

unsafe impl Send for VideoEncoder {}

unsafe extern "C-unwind" fn on_output(refcon: *mut c_void, frame: *mut c_void, status: i32, _flags: VTEncodeInfoFlags, sb: *mut CMSampleBuffer) {
    // Release the source frame we kept alive for the encoder (see encode).
    if !frame.is_null() {
        drop(unsafe { Box::from_raw(frame as *mut SendBox<CFRetained<CVPixelBuffer>>) });
    }
    if status != 0 || sb.is_null() {
        return;
    }
    unsafe {
        let sb: CFRetained<CMSampleBuffer> = CFRetained::retain(NonNull::new_unchecked(sb));
        let Some(fd) = sb.format_description() else { return };
        let Some(block) = sb.data_buffer() else { return };
        let len = block.data_length();
        let mut data = vec![0u8; len];
        if CMBlockBuffer::copy_data_bytes(&block, 0, len, NonNull::new_unchecked(data.as_mut_ptr() as *mut c_void)) != 0 {
            return;
        }
        let pts = secs(sb.presentation_time_stamp());
        let sink = &*(refcon as *const OnPacket);
        sink(VideoPacket { pts, key: is_keyframe(&sb), data: Arc::new(data), sample: SendBox(sb), format: SendBox(fd) });
    }
}

/// No "NotSync" attachment means a sync sample (keyframe).
fn is_keyframe(sb: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(arr) = sb.sample_attachments_array(false) else { return true };
        if arr.count() == 0 {
            return true;
        }
        let dict = arr.value_at_index(0) as *const CFDictionary;
        if dict.is_null() {
            return true;
        }
        let key: &CFString = kCMSampleAttachmentKey_NotSync;
        !(*dict).contains_ptr_key(key as *const CFString as *const c_void)
    }
}

fn set(session: &VTCompressionSession, key: &CFString, value: &objc2::runtime::AnyObject) -> i32 {
    unsafe { VTSessionSetProperty(session, key, Some(&*(value as *const objc2::runtime::AnyObject as *const CFType))) }
}

impl VideoEncoder {
    pub fn new(cfg: VideoConfig, on_packet: OnPacket) -> Result<VideoEncoder> {
        let sink = Box::into_raw(Box::new(on_packet));
        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        let spec = dict(vec![(
            cf_ns(unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder }),
            num_b(true),
        )]);
        let st = unsafe {
            VTCompressionSession::create(
                None,
                cfg.width as i32,
                cfg.height as i32,
                kCMVideoCodecType_H264,
                Some(&*(Retained::as_ptr(&spec) as *const CFDictionary)),
                None,
                None,
                Some(on_output),
                sink as *mut c_void,
                NonNull::from(&mut out),
            )
        };
        let Some(session) = NonNull::new(out) else {
            unsafe { drop(Box::from_raw(sink)) };
            bail!("VTCompressionSessionCreate failed ({st})");
        };
        let session = unsafe { CFRetained::from_raw(session) };
        unsafe {
            set(&session, kVTCompressionPropertyKey_RealTime, &(num_b(true)));
            set(&session, kVTCompressionPropertyKey_AllowFrameReordering, &(num_b(false)));
            set(&session, kVTCompressionPropertyKey_ProfileLevel, &(obj(cf_ns(kVTProfileLevel_H264_High_AutoLevel))));
            set(&session, kVTCompressionPropertyKey_ExpectedFrameRate, &(num_i(cfg.fps as i64)));
            set(&session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &(num_f(cfg.keyframe_secs)));
            set(&session, kVTCompressionPropertyKey_MaxKeyFrameInterval, &(num_i((cfg.keyframe_secs * cfg.fps as f64).round() as i64)));
            let cbr_ok = cfg.cbr && set(&session, kVTCompressionPropertyKey_ConstantBitRate, &(num_i(cfg.bitrate as i64))) == 0;
            if !cbr_ok {
                set(&session, kVTCompressionPropertyKey_AverageBitRate, &(num_i(cfg.bitrate as i64)));
                // Cap one-second bursts at 1.5x the average.
                let limits = objc2_foundation::NSArray::from_retained_slice(&[
                    objc2_foundation::NSNumber::new_i64((cfg.bitrate as f64 * 1.5 / 8.0) as i64),
                    objc2_foundation::NSNumber::new_f64(1.0),
                ]);
                set(&session, kVTCompressionPropertyKey_DataRateLimits, &*(objc2::rc::Retained::as_ptr(&limits) as *const objc2::runtime::AnyObject));
            }
            VTCompressionSession::prepare_to_encode_frames(&session);
        }
        Ok(VideoEncoder { session, cfg, sink, force_key: true })
    }

    /// Ask for a keyframe on the next frame (new stream viewer, reconnect).
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Encode one frame captured at host time `t`. Frames of another size
    /// are scaled by VideoToolbox.
    pub fn encode(&mut self, frame: &CVPixelBuffer, t: f64) {
        let pts = unsafe { CMTime::new((t * 60000.0).round() as i64, 60000) };
        let dur = unsafe { CMTime::new(60000 / self.cfg.fps.max(1) as i64, 60000) };
        let props = self.force_key.then(|| dict(vec![(cf_ns(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }), num_b(true))]));
        self.force_key = false;
        // Capture pools recycle a buffer as soon as nobody holds it, and
        // the encoder reads it asynchronously: hold it until the output
        // callback for this frame (otherwise a later frame's pixels can be
        // encoded under this timestamp).
        let keep = Box::into_raw(Box::new(SendBox(unsafe { CFRetained::retain(NonNull::from(frame)) })));
        let st = unsafe {
            self.session.encode_frame(
                frame,
                pts,
                dur,
                props.as_ref().map(|d| &*(objc2::rc::Retained::as_ptr(d) as *const CFDictionary)),
                keep as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if st != 0 {
            drop(unsafe { Box::from_raw(keep) });
        }
    }

    /// Emit everything still inside the encoder.
    pub fn flush(&self) {
        unsafe { self.session.complete_frames(kCMTimeInvalid) };
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            drop(Box::from_raw(self.sink));
        }
    }
}

/// SPS and PPS NAL units (without start codes) of an H.264 format.
pub fn parameter_sets(fd: &CMFormatDescription) -> Option<(Vec<u8>, Vec<u8>)> {
    let get = |i: usize| unsafe {
        let mut p: *const u8 = std::ptr::null();
        let mut n = 0usize;
        (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, i, &mut p, &mut n, std::ptr::null_mut(), std::ptr::null_mut()) == 0
            && !p.is_null())
        .then(|| std::slice::from_raw_parts(p, n).to_vec())
    };
    Some((get(0)?, get(1)?))
}

/// AVCDecoderConfigurationRecord (the FLV/MP4 "avcC" box payload).
pub fn avcc_record(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1);
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

/// AVCC (length-prefixed) to Annex B (start codes), for RTP/WebRTC.
pub fn avcc_to_annexb(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let n = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if i + n > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + n]);
        i += n;
    }
    out
}

use objc2::rc::Retained;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_conversions() {
        let avcc = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41];
        assert_eq!(avcc_to_annexb(&avcc), vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41]);
        let rec = avcc_record(&[0x67, 0x64, 0x00, 0x28, 0xac], &[0x68, 0xee]);
        assert_eq!(&rec[..6], &[1, 0x64, 0x00, 0x28, 0xff, 0xe1]);
        assert_eq!(rec.len(), 6 + 2 + 5 + 1 + 2 + 2);
    }

    #[test]
    fn encodes_keyframes_on_schedule() {
        use std::sync::Mutex;
        let got: Arc<Mutex<Vec<(f64, bool, usize)>>> = Arc::default();
        let g = got.clone();
        let cfg = VideoConfig { width: 640, height: 360, fps: 30, bitrate: 2_000_000, keyframe_secs: 1.0, cbr: true };
        let mut enc = VideoEncoder::new(cfg, Box::new(move |p| g.lock().unwrap().push((p.pts, p.key, p.data.len())))).unwrap();
        // Source frames are 1280x720: VideoToolbox scales them.
        let pool = make_pool(1280, 720, PIX_420V);
        for i in 0..90 {
            let mut buf = std::ptr::null_mut();
            unsafe { objc2_core_video::CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut buf)) };
            let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new(buf).unwrap()) };
            enc.encode(&pb, 1000.0 + i as f64 / 30.0);
        }
        enc.flush();
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 90);
        let keys: Vec<usize> = got.iter().enumerate().filter(|(_, p)| p.1).map(|(i, _)| i).collect();
        assert_eq!(keys.first(), Some(&0), "starts with a keyframe");
        assert!(keys.len() >= 3, "a keyframe at least every second: {keys:?}");
        assert!((got[1].0 - got[0].0 - 1.0 / 30.0).abs() < 1e-3);
    }
}
