//! AVAssetWriter sink: constant-frame-rate video pacing plus audio tracks.
//!
//! Video is emitted on an exact `1/fps` grid (what editors expect, like OBS),
//! repeating the newest screen frame when ScreenCaptureKit delivers nothing
//! (it only sends frames when pixels change). All timestamps stay on the host
//! clock, which ScreenCaptureKit audio/video and AVCapture camera share, so
//! audio is appended untouched and stays in sync.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_av_foundation::*;
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferPool, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
};
use objc2_foundation::NSURL;

use crate::compose::Compositor;
use crate::mixer::Mixer;
use crate::util::*;

pub const TIMESCALE: i32 = 60000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Codec {
    H264,
    Hevc,
    /// Apple ProRes 422 (intra-frame, large files, smoothest editing).
    Prores,
}

#[derive(Clone, Debug)]
pub struct VideoSpec {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub codec: Codec,
    pub bitrate: u64,
    /// Pixel format of buffers handed to the encoder.
    pub pixel_format: u32,
}

/// Audio track layout when both system audio and mic are recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AudioLayout {
    /// Track 1 = system+mic mix (what players use), tracks 2-3 = system and mic
    /// stems for editors. QuickTime plays only the mix.
    Both,
    /// Only the system+mic mix (like OBS's default single track).
    Mix,
    /// Separate system and mic tracks only.
    Stems,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioKind {
    System,
    Mic,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub screen_frames_in: u64,
    pub cam_frames_in: u64,
    pub video_frames_out: u64,
    pub video_dups: u64,
    pub video_drops: u64,
    pub audio_sys_buffers: u64,
    pub audio_mic_buffers: u64,
    pub audio_drops: u64,
    pub audio_mix_buffers: u64,
    pub audio_clipped_samples: u64,
    /// Arrival latency (host now - capture pts), max and sum, in seconds.
    pub screen_latency_max: f64,
    pub screen_latency_sum: f64,
    pub cam_latency_max: f64,
    pub cam_latency_sum: f64,
}

type Pb = SendBox<CFRetained<CVPixelBuffer>>;

pub struct Sink {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    sys: Option<Retained<AVAssetWriterInput>>,
    mic: Option<Retained<AVAssetWriterInput>>,
    mix_input: Option<Retained<AVAssetWriterInput>>,
    mixer: Option<Mixer>,
    mix_cfg: ([bool; 2], [f32; 2]),
    fps: i64,
    /// Session start on the host clock, in TIMESCALE units.
    t0: Option<i64>,
    next_slot: i64,
    screen_q: VecDeque<(f64, Pb)>,
    screen_last: Option<Pb>,
    cam_q: VecDeque<(f64, Pb)>,
    cam_last: Option<Pb>,
    compositor: Option<Compositor>,
    pub stats: Stats,
    failed: Option<String>,
    levels: [f32; 2],
}

unsafe impl Send for Sink {}

/// Minimum distance behind real time that emission runs, so late-arriving
/// frames still land in their own slot.
pub const LAG_SECS: f64 = 0.05;

fn video_settings(v: &VideoSpec) -> Retained<Dict> {
    unsafe {
        let mut compression = vec![
            (AVVideoAverageBitRateKey.unwrap(), num_i(v.bitrate as i64)),
            (AVVideoExpectedSourceFrameRateKey.unwrap(), num_i(v.fps as i64)),
            (AVVideoMaxKeyFrameIntervalDurationKey.unwrap(), num_f(2.0)),
            (AVVideoAllowFrameReorderingKey.unwrap(), num_b(false)),
        ];
        let codec = match v.codec {
            Codec::H264 => {
                compression.push((
                    AVVideoProfileLevelKey.unwrap(),
                    obj(AVVideoProfileLevelH264HighAutoLevel.unwrap()),
                ));
                AVVideoCodecTypeH264.unwrap()
            }
            Codec::Hevc => AVVideoCodecTypeHEVC.unwrap(),
            Codec::Prores => AVVideoCodecTypeAppleProRes422.unwrap(),
        };
        let color = dict(vec![
            (AVVideoColorPrimariesKey.unwrap(), obj(AVVideoColorPrimaries_ITU_R_709_2.unwrap())),
            (AVVideoTransferFunctionKey.unwrap(), obj(AVVideoTransferFunction_ITU_R_709_2.unwrap())),
            (AVVideoYCbCrMatrixKey.unwrap(), obj(AVVideoYCbCrMatrix_ITU_R_709_2.unwrap())),
        ]);
        let mut settings = vec![
            (AVVideoCodecKey.unwrap(), obj(codec)),
            (AVVideoWidthKey.unwrap(), num_i(v.width as i64)),
            (AVVideoHeightKey.unwrap(), num_i(v.height as i64)),
            (AVVideoColorPropertiesKey.unwrap(), obj(&*color)),
        ];
        // ProRes is fixed-quality intra-frame: no bitrate/GOP settings.
        if v.codec != Codec::Prores {
            settings.push((AVVideoCompressionPropertiesKey.unwrap(), obj(&*dict(compression))));
        }
        dict(settings)
    }
}

fn audio_settings(channels: i64, bitrate: i64) -> Retained<Dict> {
    // Key constants live in AVFAudio; their values equal their names.
    dict(vec![
        (&*ns("AVFormatIDKey"), num_i(fourcc(b"aac ") as i64)),
        (&*ns("AVSampleRateKey"), num_f(48000.0)),
        (&*ns("AVNumberOfChannelsKey"), num_i(channels)),
        (&*ns("AVEncoderBitRateKey"), num_i(bitrate)),
    ])
}

impl Sink {
    pub fn new(
        path: &Path,
        v: &VideoSpec,
        system_audio: bool,
        mic: bool,
        layout: AudioLayout,
        gains_db: [f32; 2],
        compositor: Option<Compositor>,
    ) -> Result<Self> {
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        let abs = std::path::absolute(path)?;
        let file_type = match path.extension().and_then(|e| e.to_str()) {
            Some("mp4") | Some("m4v") => unsafe { AVFileTypeMPEG4 }.unwrap(),
            _ => unsafe { AVFileTypeQuickTimeMovie }.unwrap(),
        };
        unsafe {
            let url = NSURL::fileURLWithPath(&ns(abs.to_str().context("non-UTF8 path")?));
            let writer = AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, file_type)
                .map_err(|e| anyhow!("AVAssetWriter: {}", e.localizedDescription()))?;
            // Fragmented writing: a crash or power loss leaves a playable file
            // up to the last fragment instead of an unreadable one.
            writer.setMovieFragmentInterval(CMTime::new(5 * TIMESCALE as i64, TIMESCALE));

            let media_video = AVMediaTypeVideo.unwrap();
            let video = AVAssetWriterInput::initWithMediaType_outputSettings(
                AVAssetWriterInput::alloc(),
                media_video,
                Some(&video_settings(v)),
            );
            video.setExpectsMediaDataInRealTime(true);
            video.setMediaTimeScale(TIMESCALE);
            let attrs = dict(vec![
                (cf_ns(kCVPixelBufferPixelFormatTypeKey), num_i(v.pixel_format as i64)),
                (cf_ns(kCVPixelBufferWidthKey), num_i(v.width as i64)),
                (cf_ns(kCVPixelBufferHeightKey), num_i(v.height as i64)),
                (cf_ns(kCVPixelBufferIOSurfacePropertiesKey), obj(&*dict(vec![]))),
            ]);
            let adaptor = AVAssetWriterInputPixelBufferAdaptor::initWithAssetWriterInput_sourcePixelBufferAttributes(
                AVAssetWriterInputPixelBufferAdaptor::alloc(),
                &video,
                Some(&attrs),
            );
            if !writer.canAddInput(&video) {
                bail!("cannot add video input (unsupported codec/size {}x{}?)", v.width, v.height);
            }
            writer.addInput(&video);

            let media_audio = AVMediaTypeAudio.unwrap();
            let mk_audio = |ch: i64, br: i64| -> Result<Retained<AVAssetWriterInput>> {
                let a = AVAssetWriterInput::initWithMediaType_outputSettings(
                    AVAssetWriterInput::alloc(),
                    media_audio,
                    Some(&audio_settings(ch, br)),
                );
                a.setExpectsMediaDataInRealTime(true);
                if !writer.canAddInput(&a) {
                    bail!("cannot add audio input");
                }
                writer.addInput(&a);
                Ok(a)
            };
            // Track order (documented in README): mix, system, mic.
            let mic_on = mic;
            let both = system_audio && mic;
            let want_mix = both && layout != AudioLayout::Stems;
            let want_stems = !both || layout != AudioLayout::Mix;
            let mix_input = if want_mix { Some(mk_audio(2, 256_000)?) } else { None };
            let sys = if system_audio && want_stems { Some(mk_audio(2, 256_000)?) } else { None };
            let mic = if mic && want_stems { Some(mk_audio(1, 128_000)?) } else { None };
            if let (Some(mixi), true) = (&mix_input, want_stems) {
                // One alternate group, mix as default: QuickTime/AVPlayer play
                // only the mix; editors still see every track.
                let mut members: Vec<&AVAssetWriterInput> = vec![mixi];
                members.extend(sys.as_deref());
                members.extend(mic.as_deref());
                let arr = objc2_foundation::NSArray::from_slice(&members);
                let group = AVAssetWriterInputGroup::initWithInputs_defaultInput(
                    AVAssetWriterInputGroup::alloc(),
                    &arr,
                    Some(mixi),
                );
                if writer.canAddInputGroup(&group) {
                    writer.addInputGroup(&group);
                } else {
                    bail!("cannot add audio track group");
                }
            }

            if !writer.startWriting() {
                bail!("startWriting failed: {}", err_text(&writer));
            }
            Ok(Sink {
                writer,
                video,
                adaptor,
                sys,
                mic,
                mix_input,
                mixer: None,
                mix_cfg: ([system_audio, mic_on], gains_db),
                fps: v.fps as i64,
                t0: None,
                next_slot: 0,
                screen_q: VecDeque::new(),
                screen_last: None,
                cam_q: VecDeque::new(),
                cam_last: None,
                compositor,
                stats: Stats::default(),
                failed: None,
                levels: [0.0; 2],
            })
        }
    }

    /// Peak levels [system, mic] since the last call (linear, 0..1).
    pub fn take_levels(&mut self) -> [f32; 2] {
        std::mem::take(&mut self.levels)
    }

    /// Current emission lag: webcams deliver frames ~70-90 ms after capture,
    /// so wait for the slowest source seen (plus margin) before emitting a
    /// slot, keeping the overlay in sync with audio.
    pub fn lag(&self) -> f64 {
        let worst = self.stats.screen_latency_max.max(self.stats.cam_latency_max);
        (worst + 0.025).clamp(LAG_SECS, 0.3)
    }

    pub fn started(&self) -> bool {
        self.t0.is_some()
    }

    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn slot_ticks(&self, slot: i64) -> i64 {
        self.t0.unwrap() + slot * TIMESCALE as i64 / self.fps
    }

    pub fn push_screen(&mut self, pts: CMTime, pb: CFRetained<CVPixelBuffer>) {
        self.stats.screen_frames_in += 1;
        let pts_s = secs(pts);
        let lat = secs(host_now()) - pts_s;
        self.stats.screen_latency_max = self.stats.screen_latency_max.max(lat);
        self.stats.screen_latency_sum += lat;
        if self.t0.is_none() {
            let t0 = (pts_s * TIMESCALE as f64).round() as i64;
            unsafe { self.writer.startSessionAtSourceTime(CMTime::new(t0, TIMESCALE)) };
            self.t0 = Some(t0);
            if self.mix_input.is_some() {
                let (act, g) = self.mix_cfg;
                self.mixer = Some(Mixer::new(t0 as f64 / TIMESCALE as f64, act[0], act[1], g));
            }
        }
        self.screen_q.push_back((pts_s, SendBox(pb)));
        // Never hold more than a few ScreenCaptureKit surfaces (its pool is small).
        while self.screen_q.len() > 4 {
            self.screen_last = self.screen_q.pop_front().map(|x| x.1);
        }
    }

    pub fn push_cam(&mut self, pts: CMTime, pb: CFRetained<CVPixelBuffer>) {
        self.stats.cam_frames_in += 1;
        let lat = secs(host_now()) - secs(pts);
        self.stats.cam_latency_max = self.stats.cam_latency_max.max(lat);
        self.stats.cam_latency_sum += lat;
        self.cam_q.push_back((secs(pts), SendBox(pb)));
        while self.cam_q.len() > 4 {
            self.cam_last = self.cam_q.pop_front().map(|x| x.1);
        }
    }

    pub fn push_audio(&mut self, kind: AudioKind, sb: &CMSampleBuffer) {
        let Some(t0) = self.t0 else { return };
        let pts = unsafe { sb.presentation_time_stamp() };
        if secs(pts) * (TIMESCALE as f64) < t0 as f64 {
            return;
        }
        if let Some((_, _, frames)) = crate::mixer::decode(sb) {
            let peak = frames.iter().fold(0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
            let i = (kind == AudioKind::Mic) as usize;
            self.levels[i] = self.levels[i].max(peak);
        }
        if let Some(m) = self.mixer.as_mut() {
            m.push(kind, sb);
            self.drain_mix(false);
        }
        let input = match kind {
            AudioKind::System => self.sys.as_ref(),
            AudioKind::Mic => self.mic.as_ref(),
        };
        let Some(input) = input else { return };
        unsafe {
            if input.isReadyForMoreMediaData() && input.appendSampleBuffer(sb) {
                match kind {
                    AudioKind::System => self.stats.audio_sys_buffers += 1,
                    AudioKind::Mic => self.stats.audio_mic_buffers += 1,
                }
            } else {
                self.stats.audio_drops += 1;
                self.check_writer();
            }
        }
    }

    fn drain_mix(&mut self, flush: bool) {
        let (Some(m), Some(input)) = (self.mixer.as_mut(), self.mix_input.as_ref()) else { return };
        while let Some(buf) = m.pull(flush) {
            unsafe {
                if input.isReadyForMoreMediaData() && input.appendSampleBuffer(&buf) {
                    self.stats.audio_mix_buffers += 1;
                } else {
                    self.stats.audio_drops += 1;
                }
            }
        }
        self.stats.audio_clipped_samples = m.clipped;
        self.check_writer();
    }

    /// Pick the newest queued frame captured at or before `t`.
    fn pick(q: &mut VecDeque<(f64, Pb)>, last: &mut Option<Pb>, t: f64) {
        while let Some((pts, _)) = q.front() {
            if *pts <= t {
                *last = q.pop_front().map(|x| x.1);
            } else {
                break;
            }
        }
    }

    /// Emit every grid slot whose time is at or before `until` (host seconds).
    pub fn emit_until(&mut self, until: f64) {
        if self.t0.is_none() || self.failed.is_some() {
            return;
        }
        let half = 0.5 / self.fps as f64;
        loop {
            let slot_t = self.slot_ticks(self.next_slot) as f64 / TIMESCALE as f64;
            if slot_t > until {
                break;
            }
            let before = self.screen_q.len();
            Self::pick(&mut self.screen_q, &mut self.screen_last, slot_t + half);
            let fresh = self.screen_q.len() != before;
            Self::pick(&mut self.cam_q, &mut self.cam_last, slot_t + half);
            let pts = unsafe { CMTime::new(self.slot_ticks(self.next_slot), TIMESCALE) };
            self.next_slot += 1;
            let Some(screen) = self.screen_last.as_ref() else { continue };
            unsafe {
                if !self.video.isReadyForMoreMediaData() {
                    self.stats.video_drops += 1;
                    self.check_writer();
                    continue;
                }
                let out: CFRetained<CVPixelBuffer> = match self.compositor.as_mut() {
                    Some(c) => {
                        let Some(pool) = self.adaptor.pixelBufferPool() else {
                            self.stats.video_drops += 1;
                            continue;
                        };
                        let mut buf = std::ptr::null_mut();
                        CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut buf));
                        let Some(buf) = std::ptr::NonNull::new(buf) else {
                            self.stats.video_drops += 1;
                            continue;
                        };
                        let buf = CFRetained::from_raw(buf);
                        c.render(&screen.0, self.cam_last.as_ref().map(|b| &*b.0), &buf);
                        buf
                    }
                    None => screen.0.clone(),
                };
                if self.adaptor.appendPixelBuffer_withPresentationTime(&out, pts) {
                    self.stats.video_frames_out += 1;
                    if !fresh {
                        self.stats.video_dups += 1;
                    }
                } else {
                    self.stats.video_drops += 1;
                    self.check_writer();
                }
            }
        }
    }

    fn check_writer(&mut self) {
        unsafe {
            if self.writer.status() == AVAssetWriterStatus::Failed && self.failed.is_none() {
                self.failed = Some(err_text(&self.writer));
            }
        }
    }

    /// Flush video up to `end` (host seconds), trim all tracks there, and
    /// finalize the file. Blocks until the movie is written.
    pub fn finish(mut self, end: f64) -> Result<Stats> {
        let Some(_) = self.t0 else {
            unsafe { self.writer.cancelWriting() };
            bail!("no screen frames were captured, nothing written");
        };
        self.emit_until(end);
        self.drain_mix(true);
        unsafe {
            let end_t = CMTime::new(self.slot_ticks(self.next_slot), TIMESCALE);
            self.writer.endSessionAtSourceTime(end_t);
            self.video.markAsFinished();
            if let Some(a) = &self.sys {
                a.markAsFinished();
            }
            if let Some(a) = &self.mic {
                a.markAsFinished();
            }
            if let Some(a) = &self.mix_input {
                a.markAsFinished();
            }
            let (tx, rx) = mpsc::channel();
            let block = RcBlock::new(move || {
                let _ = tx.send(());
            });
            self.writer.finishWritingWithCompletionHandler(&block);
            rx.recv_timeout(Duration::from_secs(60))
                .context("timed out finalizing movie")?;
            if self.writer.status() != AVAssetWriterStatus::Completed {
                bail!("writing failed: {}", err_text(&self.writer));
            }
        }
        Ok(self.stats.clone())
    }
}

fn err_text(w: &AVAssetWriter) -> String {
    unsafe {
        w.error()
            .map(|e| {
                let mut s = e.localizedDescription().to_string();
                if let Some(r) = e.localizedFailureReason() {
                    s.push_str(&format!(" ({r})"));
                }
                s.push_str(&format!(" [{} {}]", e.domain(), e.code()));
                s
            })
            .unwrap_or_else(|| "unknown error".into())
    }
}
