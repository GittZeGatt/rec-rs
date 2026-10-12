//! Hardware H.264 / HEVC encoding with a VideoToolbox compression session,
//! for the replay buffer and streaming (the recording keeps its own
//! AVAssetWriter encode at its own quality).
//!
//! Real time (a stream can't fall behind), asking for quality over speed
//! and power (no measurable effect on an M4 Pro, whose real-time mode
//! already favors quality), BT.709 limited-range tags in the bitstream, and
//! HEVC Main10: from the same 8-bit frames it scored +1.4 dB PSNR-Y and +1.4
//! VMAF over Main on 4K60 motion at 35 Mb/s (scripts/encquality.sh).
//! `REC_LIVE_BASIC=1` restores the plain settings for A/B measurements.

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
    /// HEVC instead of H.264.
    pub hevc: bool,
}

/// One encoded H.264 access unit: AVCC (4-byte length-prefixed NAL units).
pub struct VideoPacket {
    /// Host-clock seconds.
    pub pts: f64,
    pub hevc: bool,
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
            hevc: self.hevc,
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
        let hevc = is_hevc(&fd);
        sink(VideoPacket { pts, hevc, key: is_keyframe(&sb), data: Arc::new(data), sample: SendBox(sb), format: SendBox(fd) });
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
                if cfg.hevc { kCMVideoCodecType_HEVC } else { kCMVideoCodecType_H264 },
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
            let basic = std::env::var_os("REC_LIVE_BASIC").is_some();
            let profile = match (cfg.hevc, basic) {
                (false, _) => kVTProfileLevel_H264_High_AutoLevel,
                (true, true) => kVTProfileLevel_HEVC_Main_AutoLevel,
                (true, false) => kVTProfileLevel_HEVC_Main10_AutoLevel,
            };
            set(&session, kVTCompressionPropertyKey_ProfileLevel, &(obj(cf_ns(profile))));
            if !basic {
                set(&session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, &(num_b(false)));
                set(&session, kVTCompressionPropertyKey_MaximizePowerEfficiency, &(num_b(false)));
                // BT.709, video (limited) range: the VUI says so, so players
                // and services don't guess.
                set(&session, kVTCompressionPropertyKey_ColorPrimaries, &(obj(cf_ns(kCMFormatDescriptionColorPrimaries_ITU_R_709_2))));
                set(&session, kVTCompressionPropertyKey_TransferFunction, &(obj(cf_ns(kCMFormatDescriptionTransferFunction_ITU_R_709_2))));
                set(&session, kVTCompressionPropertyKey_YCbCrMatrix, &(obj(cf_ns(kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2))));
            }
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

/// Whether a format is HEVC.
pub fn is_hevc(fd: &CMFormatDescription) -> bool {
    unsafe { fd.media_sub_type() == kCMVideoCodecType_HEVC }
}

/// Parameter set NAL units (without start codes): SPS and PPS for H.264,
/// VPS, SPS and PPS for HEVC.
pub fn parameter_sets(fd: &CMFormatDescription) -> Option<Vec<Vec<u8>>> {
    let hevc = is_hevc(fd);
    let get = |i: usize, count: &mut usize| unsafe {
        let mut p: *const u8 = std::ptr::null();
        let mut n = 0usize;
        let st = if hevc {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fd, i, &mut p, &mut n, count, std::ptr::null_mut())
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, i, &mut p, &mut n, count, std::ptr::null_mut())
        };
        (st == 0 && !p.is_null()).then(|| std::slice::from_raw_parts(p, n).to_vec())
    };
    let mut count = 0usize;
    let first = get(0, &mut count)?;
    let mut sets = vec![first];
    for i in 1..count {
        sets.push(get(i, &mut 0)?);
    }
    Some(sets)
}

/// The decoder configuration record (`avcC` or `hvcC`, as in MP4 and
/// FLV) of a format.
pub fn config_record(fd: &CMFormatDescription) -> Option<Vec<u8>> {
    use objc2_core_foundation::{CFData, CFDictionary};
    unsafe {
        let atoms = fd.extension(kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms)?;
        let atoms: &CFDictionary = atoms.downcast_ref()?;
        let key = CFString::from_static_str(if is_hevc(fd) { "hvcC" } else { "avcC" });
        let data = atoms.value(&*key as *const CFString as *const c_void);
        let data: &CFData = (data as *const CFData).as_ref()?;
        Some(data.to_vec())
    }
}

/// AVCDecoderConfigurationRecord (the FLV/MP4 "avcC" box payload).
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
    }

    /// Both codecs give their parameter sets and configuration record
    /// (what RTMP's sequence header and WHIP's keyframes carry).
    #[test]
    fn parameter_sets_and_records() {
        for hevc in [false, true] {
            let got: Arc<std::sync::Mutex<Vec<VideoPacket>>> = Arc::default();
            let g = got.clone();
            let cfg = VideoConfig { width: 640, height: 360, fps: 30, bitrate: 2_000_000, keyframe_secs: 1.0, cbr: true, hevc };
            let mut enc = VideoEncoder::new(cfg, Box::new(move |p| g.lock().unwrap().push(p))).unwrap();
            let pool = make_pool(640, 360, PIX_420V);
            for i in 0..5 {
                let mut buf = std::ptr::null_mut();
                unsafe { objc2_core_video::CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut buf)) };
                let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new(buf).unwrap()) };
                enc.encode(&pb, 10.0 + i as f64 / 30.0);
            }
            enc.flush();
            let got = got.lock().unwrap();
            let p = &got[0];
            assert!(p.key && p.hevc == hevc);
            let sets = parameter_sets(&p.format.0).unwrap();
            let rec = config_record(&p.format.0).unwrap();
            assert_eq!(rec[0], 1, "configuration version");
            if hevc {
                assert_eq!(sets.len(), 3, "VPS, SPS, PPS");
                assert_eq!(sets[0][0] >> 1 & 0x3f, 32, "VPS NAL type");
                assert_eq!(rec[1] & 0x1f, 2, "Main10 profile");
            } else {
                assert_eq!(sets.len(), 2, "SPS, PPS");
                assert_eq!(sets[0][0] & 0x1f, 7, "SPS NAL type");
                assert_eq!(rec[1], 100, "High profile");
            }
        }
    }

    #[test]
    fn encodes_keyframes_on_schedule() {
        use std::sync::Mutex;
        let got: Arc<Mutex<Vec<(f64, bool, usize)>>> = Arc::default();
        let g = got.clone();
        let cfg = VideoConfig { width: 640, height: 360, fps: 30, bitrate: 2_000_000, keyframe_secs: 1.0, cbr: true, hevc: false };
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

    /// Encoder quality harness (scripts/encquality.sh): encodes the raw
    /// BGRA frames of REC_ENC_SRC (a lossless file, decoded by ffmpeg) with
    /// REC_ENC_BITRATE (b/s), REC_ENC_HEVC=1, REC_ENC_FPS and writes an
    /// Annex B stream to REC_ENC_OUT.
    #[test]
    #[ignore]
    fn encode_file() {
        use std::io::{Read, Write};
        let src = std::env::var("REC_ENC_SRC").expect("REC_ENC_SRC");
        let out = std::env::var("REC_ENC_OUT").expect("REC_ENC_OUT");
        let bitrate: u64 = std::env::var("REC_ENC_BITRATE").unwrap().parse().unwrap();
        let hevc = std::env::var("REC_ENC_HEVC").is_ok_and(|v| v == "1");
        let fps: u32 = std::env::var("REC_ENC_FPS").map_or(60, |v| v.parse().unwrap());
        let (w, h) = (3840usize, 2160usize);
        let mut ff = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i", &src, "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = ff.stdout.take().unwrap();
        let packets = Arc::new(std::sync::Mutex::new(Vec::<(bool, Vec<u8>, Vec<Vec<u8>>)>::new()));
        let p2 = packets.clone();
        let cfg = VideoConfig { width: w, height: h, fps, bitrate, keyframe_secs: 2.0, cbr: true, hevc };
        let mut enc = VideoEncoder::new(
            cfg,
            Box::new(move |p| {
                let sets = parameter_sets(&p.format.0).unwrap_or_default();
                p2.lock().unwrap().push((p.key, avcc_to_annexb(&p.data), sets));
            }),
        )
        .unwrap();
        let pool = make_pool(w, h, PIX_BGRA);
        let mut frame = vec![0u8; w * h * 4];
        let mut n = 0;
        while pipe.read_exact(&mut frame).is_ok() {
            let mut buf = std::ptr::null_mut();
            unsafe { objc2_core_video::CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut buf)) };
            let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new(buf).unwrap()) };
            unsafe {
                use objc2_core_video::*;
                CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
                let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRow(&pb);
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(frame.as_ptr().add(y * w * 4), base.add(y * stride), w * 4);
                }
                CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
            }
            crate::scale::tag_bt709(&pb);
            enc.encode(&pb, 1000.0 + n as f64 / fps as f64);
            n += 1;
            // Real time: the encoder sees frames at the stream's pace.
            std::thread::sleep(std::time::Duration::from_secs_f64(1.0 / fps as f64));
        }
        enc.flush();
        drop(enc);
        let mut f = std::fs::File::create(&out).unwrap();
        for (key, data, sets) in packets.lock().unwrap().iter() {
            if *key {
                for ps in sets {
                    f.write_all(&[0, 0, 0, 1]).unwrap();
                    f.write_all(ps).unwrap();
                }
            }
            f.write_all(data).unwrap();
        }
        println!("encoded {n} frames -> {out}");
    }

}
