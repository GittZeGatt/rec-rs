//! Live outputs: one shared H.264 + AAC/Opus encode of the composited
//! output, fanned out to the replay buffer and any streams (RTMP, WHIP).
//!
//! The capture session feeds every output frame and every audio buffer in
//! (on the host clock, independent of recording pauses). Nothing is encoded
//! while no live output is active.

pub mod audio;
pub mod keychain;
pub mod replay;
pub mod rtmp;
pub mod video;
pub mod whip;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};

use crate::mixer::Mixer;
use crate::writer::AudioKind;
use audio::{AudioCodec, AudioEncoder, AudioPacket};
use video::{VideoConfig, VideoEncoder, VideoPacket};

/// Settings of the shared live encode.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveConfig {
    /// Encode size (frames of another size are scaled by VideoToolbox).
    pub width: usize,
    pub height: usize,
    /// Video bits per second.
    pub bitrate: u64,
    pub keyframe_secs: f64,
    pub cbr: bool,
    /// AAC bits per second (Opus for WHIP uses the same).
    pub audio_bitrate: u32,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig { width: 1920, height: 1080, bitrate: 6_000_000, keyframe_secs: 1.0, cbr: true, audio_bitrate: 160_000 }
    }
}

#[derive(Clone)]
pub enum Packet {
    Video(VideoPacket),
    Audio(AudioPacket),
}

/// Codec configuration the outputs need (sequence headers).
#[derive(Clone, Default)]
pub struct Formats {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub aac_cookie: Vec<u8>,
}

/// A stream destination fed with packets.
pub trait Output: Send {
    fn packet(&mut self, p: &Packet, f: &Formats);
    fn status(&self) -> OutputStatus;
    /// Wants Opus audio (WebRTC) instead of AAC.
    fn wants_opus(&self) -> bool {
        false
    }
    /// Asked for a keyframe (just connected or reconnected).
    fn take_keyframe_request(&mut self) -> bool {
        false
    }
    fn stop(&mut self);
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputStatus {
    pub name: String,
    pub state: String,
    pub live: bool,
    /// Measured send rate over the last seconds, bits per second.
    pub bitrate: f64,
    pub dropped_frames: u64,
    pub reconnects: u64,
    /// Seconds since the stream started.
    pub duration: f64,
    pub error: Option<String>,
}

#[derive(Default)]
struct Hub {
    formats: Formats,
    replay: Option<replay::Replay>,
    outputs: Vec<(u64, Box<dyn Output>)>,
}

impl Hub {
    fn dispatch(&mut self, p: Packet) {
        match &p {
            Packet::Video(v) => {
                if v.key
                    && self.formats.sps.is_empty()
                    && let Some((sps, pps)) = video::parameter_sets(&v.format.0)
                {
                    self.formats.sps = sps;
                    self.formats.pps = pps;
                }
                if let Some(r) = &mut self.replay {
                    r.push_video(v.clone());
                }
            }
            Packet::Audio(a) => {
                if a.codec == AudioCodec::Aac
                    && let Some(r) = &mut self.replay
                {
                    r.push_audio(a.clone());
                }
            }
        }
        for (_, o) in &mut self.outputs {
            o.packet(&p, &self.formats);
        }
    }
}

struct Encoders {
    video: Option<VideoEncoder>,
    aac: Option<AudioEncoder>,
    opus: Option<AudioEncoder>,
    mixer: Option<Mixer>,
    /// [system, mic] captured by the current session.
    sources: [bool; 2],
    /// Without any audio source, silence is generated up to here.
    silence_t: Option<f64>,
    /// A video encoder is being created in the background.
    creating: bool,
    /// Last frame encoded and its time, repeated over short gaps (a
    /// capture session being swapped) so streams see no hole.
    last_frame: Option<(f64, crate::util::SendBox<objc2_core_foundation::CFRetained<CVPixelBuffer>>)>,
    /// End time of the audio encoded so far; short gaps get silence.
    audio_end: Option<f64>,
}

/// Gaps up to this long are filled (repeated frame, silence); longer ones
/// restart the timeline.
const MAX_FILL: f64 = 3.0;

type Made = Arc<Mutex<Option<Result<VideoEncoder>>>>;

pub struct Live {
    hub: Arc<Mutex<Hub>>,
    /// A video encoder finished in the background.
    made: Made,
    enc: Mutex<Encoders>,
    cfg: Mutex<LiveConfig>,
    active: AtomicBool,
    next_id: Mutex<u64>,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            hub: Arc::default(),
            enc: Mutex::new(Encoders {
                video: None,
                aac: None,
                opus: None,
                mixer: None,
                sources: [false; 2],
                silence_t: None,
                creating: false,
                last_frame: None,
                audio_end: None,
            }),
            made: Arc::default(),
            cfg: Mutex::new(LiveConfig::default()),
            active: AtomicBool::new(false),
            next_id: Mutex::new(1),
        }
    }
}

impl Live {
    pub fn new() -> Arc<Live> {
        Arc::new(Live::default())
    }

    /// Something consumes the encode (replay buffer or a stream).
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Call without holding the hub lock (lock order: encoders, then hub).
    fn update_active(&self) {
        let on = {
            let hub = self.hub.lock().unwrap();
            hub.replay.is_some() || !hub.outputs.is_empty()
        };
        self.active.store(on, Ordering::Relaxed);
        if !on {
            // Free the hardware encoder when idle.
            let mut e = self.enc.lock().unwrap();
            e.video = None;
            e.aac = None;
            e.opus = None;
            e.mixer = None;
            e.last_frame = None;
            e.audio_end = None;
        }
    }

    pub fn config(&self) -> LiveConfig {
        self.cfg.lock().unwrap().clone()
    }

    /// Change the encode; the encoder restarts with a keyframe. The replay
    /// buffer starts over (a clip holds one set of parameter sets).
    pub fn set_config(&self, c: LiveConfig) {
        let mut cfg = self.cfg.lock().unwrap();
        if *cfg != c {
            *cfg = c;
            let mut e = self.enc.lock().unwrap();
            e.video = None;
            e.aac = None;
            e.opus = None;
            e.last_frame = None;
            e.audio_end = None;
            // Lock order: encoders, then hub.
            let mut hub = self.hub.lock().unwrap();
            hub.formats = Formats::default();
            if let Some(r) = &mut hub.replay {
                r.clear();
            }
        }
    }

    /// A capture session starts: which audio sources it delivers.
    pub fn begin_session(&self, system: bool, mic: bool) {
        let mut e = self.enc.lock().unwrap();
        e.sources = [system, mic];
        e.mixer = None;
        e.silence_t = None;
    }

    /// One composited output frame at host time `t` (seconds).
    pub fn video(&self, frame: &CVPixelBuffer, t: f64, fps: u32) {
        if !self.active() {
            return;
        }
        let cfg = self.config();
        let want_key = self.hub.lock().unwrap().outputs.iter_mut().fold(false, |k, (_, o)| o.take_keyframe_request() | k);
        let mut e = self.enc.lock().unwrap();
        if e.video.as_ref().is_none_or(|v| v.cfg.fps != fps) {
            // Creating a hardware session takes ~100 ms: do it off the
            // capture path (frames until then are skipped).
            if !e.creating {
                e.creating = true;
                let vc = VideoConfig {
                    width: cfg.width.min(CVPixelBufferGetWidth(frame)).max(16) & !1,
                    height: cfg.height.min(CVPixelBufferGetHeight(frame)).max(16) & !1,
                    fps,
                    bitrate: cfg.bitrate,
                    keyframe_secs: cfg.keyframe_secs,
                    cbr: cfg.cbr,
                };
                let hub = self.hub.clone();
                let made = self.made.clone();
                std::thread::spawn(move || {
                    let r = VideoEncoder::new(vc, Box::new(move |p| hub.lock().unwrap().dispatch(Packet::Video(p))));
                    *made.lock().unwrap() = Some(r);
                });
            }
            let ready = self.made.lock().unwrap().take();
            match ready {
                Some(Ok(v)) if v.cfg.fps == fps => {
                    e.video = Some(v);
                    e.creating = false;
                }
                Some(Ok(_)) => e.creating = false, // settings changed meanwhile
                Some(Err(err)) => {
                    eprintln!("rec: live video encoder: {err:#}");
                    e.creating = false;
                    return;
                }
                None => return,
            }
        }
        let last = e.last_frame.take();
        if let Some(v) = e.video.as_mut() {
            if want_key {
                v.force_keyframe();
            }
            // A short hole (sessions swapped): repeat the last frame on the
            // frame grid up to this one.
            let step = 1.0 / fps.max(1) as f64;
            if let Some((lt, lf)) = &last
                && t - lt > 1.5 * step
                && t - lt < MAX_FILL
            {
                let mut ft = lt + step;
                while ft < t - 0.5 * step {
                    v.encode(&lf.0, ft);
                    ft += step;
                }
            }
            v.encode(frame, t);
            e.last_frame = Some((t, crate::util::SendBox(objc2_core_foundation::Type::retain(frame))));
        }
        // No audio source: keep a silent track so players and services
        // that require audio accept the stream.
        if e.sources == [false, false] {
            let from = e.silence_t.unwrap_or(t);
            let n = ((t - from) * audio::RATE).round() as usize;
            if n > 0 || e.silence_t.is_none() {
                e.silence_t = Some(from + n as f64 / audio::RATE);
                drop(e);
                self.encode_audio(from, &vec![[0.0f32; 2]; n]);
            }
        }
    }

    /// Captured audio (decoded to stereo f32) of `kind` at host time `pts`.
    pub fn audio(&self, kind: AudioKind, pts: f64, rate: f64, frames: Vec<[f32; 2]>) {
        if !self.active() {
            return;
        }
        let mixed = {
            let mut e = self.enc.lock().unwrap();
            let sources = e.sources;
            let m = e.mixer.get_or_insert_with(|| Mixer::new(pts, sources[0], sources[1], [0.0, 0.0]));
            m.push(kind, pts, rate, frames);
            let mut out = vec![];
            while let Some(x) = m.pull_frames(false) {
                out.push(x);
            }
            out
        };
        for (t, frames) in mixed {
            self.encode_audio(t, &frames);
        }
    }

    fn encode_audio(&self, t: f64, frames: &[[f32; 2]]) {
        let (cfg, wants_opus) = {
            let hub = self.hub.lock().unwrap();
            (self.config(), hub.outputs.iter().any(|(_, o)| o.wants_opus()))
        };
        let mut packets = vec![];
        {
            let mut e = self.enc.lock().unwrap();
            // A short hole (sessions swapped): fill it with silence so the
            // audio timeline stays continuous.
            let mut t = t;
            let mut filled;
            let mut frames = frames;
            if let Some(end) = e.audio_end
                && t - end > 0.005
                && t - end < MAX_FILL
            {
                let n = ((t - end) * audio::RATE).round() as usize;
                filled = vec![[0.0f32; 2]; n];
                filled.extend_from_slice(frames);
                frames = &filled;
                t = end;
            }
            e.audio_end = Some(t + frames.len() as f64 / audio::RATE);
            if e.aac.is_none() {
                match AudioEncoder::new(AudioCodec::Aac, cfg.audio_bitrate) {
                    Ok(a) => e.aac = Some(a),
                    Err(err) => eprintln!("rec: live audio encoder: {err:#}"),
                }
            }
            if let Some(a) = e.aac.as_mut() {
                packets.extend(a.push(t, frames));
                if !a.cookie.is_empty() {
                    let mut hub = self.hub.lock().unwrap();
                    if hub.formats.aac_cookie.is_empty() {
                        hub.formats.aac_cookie = a.cookie.clone();
                    }
                }
            }
            if wants_opus {
                if e.opus.is_none() {
                    e.opus = AudioEncoder::new(AudioCodec::Opus, cfg.audio_bitrate.min(256_000)).ok();
                }
                if let Some(o) = e.opus.as_mut() {
                    packets.extend(o.push(t, frames));
                }
            } else {
                e.opus = None;
            }
        }
        let mut hub = self.hub.lock().unwrap();
        for p in packets {
            hub.dispatch(Packet::Audio(p));
        }
    }

    // --- Replay buffer -------------------------------------------------

    /// Keep the last `secs` seconds (None turns the buffer off).
    pub fn set_replay(&self, secs: Option<f64>) {
        {
            let mut hub = self.hub.lock().unwrap();
            match (secs, hub.replay.as_mut()) {
                (Some(s), Some(r)) => r.secs = s,
                (Some(s), None) => hub.replay = Some(replay::Replay::new(s)),
                (None, _) => hub.replay = None,
            }
        }
        self.update_active();
    }

    /// (seconds held, bytes held), if the replay buffer is on.
    pub fn replay_stats(&self) -> Option<(f64, usize)> {
        self.hub.lock().unwrap().replay.as_ref().map(|r| (r.held(), r.bytes()))
    }

    /// Save the buffer to `path` (an .mp4) on a background thread. The
    /// buffer keeps running.
    pub fn save_replay(&self, path: PathBuf) -> Result<std::thread::JoinHandle<Result<PathBuf>>> {
        // Make sure everything captured so far is in the snapshot.
        if let Some(v) = self.enc.lock().unwrap().video.as_ref() {
            v.flush();
        }
        let (v, a, cookie) = {
            let hub = self.hub.lock().unwrap();
            let r = hub.replay.as_ref().ok_or_else(|| anyhow::anyhow!("the replay buffer is off"))?;
            let (v, a) = r.snapshot();
            (v, a, hub.formats.aac_cookie.clone())
        };
        if v.is_empty() {
            anyhow::bail!("the replay buffer is still empty");
        }
        Ok(std::thread::spawn(move || objc2::rc::autoreleasepool(|_| replay::write_mp4(&path, &v, &a, &cookie))))
    }

    // --- Streams -------------------------------------------------------

    pub fn add_output(&self, o: Box<dyn Output>) -> u64 {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        self.hub.lock().unwrap().outputs.push((id, o));
        self.update_active();
        // A new viewer needs a keyframe and the codec headers.
        if let Some(v) = self.enc.lock().unwrap().video.as_mut() {
            v.force_keyframe();
        }
        id
    }

    pub fn remove_output(&self, id: u64) {
        let removed = {
            let mut hub = self.hub.lock().unwrap();
            let i = hub.outputs.iter().position(|(i, _)| *i == id);
            i.map(|i| hub.outputs.remove(i))
        };
        self.update_active();
        if let Some((_, mut o)) = removed {
            o.stop();
        }
    }

    pub fn statuses(&self) -> Vec<(u64, OutputStatus)> {
        self.hub.lock().unwrap().outputs.iter().map(|(i, o)| (*i, o.status())).collect()
    }

    pub fn stop_all(&self) {
        let outs = std::mem::take(&mut self.hub.lock().unwrap().outputs);
        for (_, mut o) in outs {
            o.stop();
        }
        self.update_active();
    }
}

/// Redact a secret (stream key) from text meant for logs and the UI.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.len() < 4 { text.to_string() } else { text.replace(secret, "••••") }
}

/// Stream URL for a destination: `youtube` and `twitch` expand to their
/// RTMPS ingest URLs; anything else is used as given.
pub fn destination_url(dest: &str) -> String {
    match dest.trim().to_lowercase().as_str() {
        "youtube" => "rtmps://a.rtmps.youtube.com:443/live2".into(),
        "twitch" => "rtmps://live.twitch.tv:443/app".into(),
        _ => dest.trim().to_string(),
    }
}

/// (URL, key) for a destination: the key comes from $REC_STREAM_KEY or the
/// Keychain. WHIP endpoints may have no key.
pub fn resolve_destination(dest: &str) -> Result<(String, String)> {
    let url = destination_url(dest);
    let key = std::env::var("REC_STREAM_KEY").ok().filter(|k| !k.is_empty()).or_else(|| keychain::get(&url));
    match key {
        Some(k) => Ok((url, k)),
        None if url.starts_with("http") => Ok((url, String::new())),
        None => anyhow::bail!("no stream key for {url}: set REC_STREAM_KEY or run `rec-rs stream-key {dest}`"),
    }
}

/// Start an output for `url` (rtmp://, rtmps:// or http(s):// WHIP).
pub fn start_output(url: &str, key: &str) -> Result<Box<dyn Output>> {
    if url.starts_with("rtmp://") || url.starts_with("rtmps://") {
        Ok(Box::new(rtmp::RtmpOutput::start(url, key)?))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Ok(Box::new(whip::WhipOutput::start(url, key)?))
    } else {
        anyhow::bail!("unsupported stream URL {url:?} (use rtmp://, rtmps:// or a WHIP http(s):// URL)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::*;

    /// Feed `secs` of 30 fps frames and audio through the hub. Every whole
    /// second the frame is white and the audio beeps, like the test pattern.
    fn feed(live: &Live, secs: f64, t0: f64) {
        feed_with(live, secs, t0, 30, 1600);
    }

    /// `chunk`: audio frames per call (audio arrives in pieces unrelated to
    /// video frames).
    fn feed_with(live: &Live, secs: f64, t0: f64, fps: usize, chunk: usize) {
        use objc2_core_video::*;
        let pool = make_pool(640, 360, PIX_BGRA);
        let n = (secs * fps as f64) as usize;
        let mut audio_done = 0usize;
        // The encoder starts in the background: let it come up first, as it
        // does while a real capture warms up.
        if live.enc.lock().unwrap().video.is_none() {
            let mut buf = std::ptr::null_mut();
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut buf)) };
            let pb: objc2_core_foundation::CFRetained<CVPixelBuffer> =
                unsafe { objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new(buf).unwrap()) };
            live.video(&pb, t0 - 1.0, fps as u32);
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        for i in 0..n {
            let t = t0 + i as f64 / fps as f64;
            let flash = i % fps == 0;
            let mut buf = std::ptr::null_mut();
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut buf)) };
            let pb: objc2_core_foundation::CFRetained<CVPixelBuffer> =
                unsafe { objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new(buf).unwrap()) };
            unsafe {
                CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
                let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
                std::ptr::write_bytes(base, if flash { 255 } else { 20 }, CVPixelBufferGetBytesPerRow(&pb) * 360);
                CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
            }
            live.video(&pb, t, fps as u32);
            // Audio up to this frame's time, in `chunk`-sized pieces; a
            // 10 ms beep starts on every whole second.
            let until = ((i + 1) as f64 / fps as f64 * 48000.0) as usize;
            while audio_done + chunk <= until {
                let frames: Vec<[f32; 2]> = (audio_done..audio_done + chunk)
                    .map(|k| {
                        let in_sec = k % 48000;
                        let v = if in_sec < 480 { 0.5 * (in_sec as f32 * 0.13).sin() } else { 0.0 };
                        [v, v]
                    })
                    .collect();
                live.audio(AudioKind::System, t0 + audio_done as f64 / 48000.0, 48000.0, frames);
                audio_done += chunk;
            }
            if i % fps == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// Records every packet's (is video, pts).
    struct Collect(Arc<Mutex<Vec<(bool, f64)>>>);

    impl Output for Collect {
        fn packet(&mut self, p: &Packet, _f: &Formats) {
            let item = match p {
                Packet::Video(v) => (true, v.pts),
                Packet::Audio(a) => (false, a.pts),
            };
            self.0.lock().unwrap().push(item);
        }
        fn status(&self) -> OutputStatus {
            OutputStatus::default()
        }
        fn stop(&mut self) {}
    }

    /// Swapping capture sessions leaves a hole of a few hundred ms; the
    /// stream must stay continuous (repeated frame, silence).
    #[test]
    fn short_gaps_are_filled() {
        let live = Live::new();
        live.set_config(LiveConfig { width: 640, height: 360, bitrate: 1_000_000, keyframe_secs: 1.0, ..Default::default() });
        let got = Arc::new(Mutex::new(vec![]));
        live.add_output(Box::new(Collect(got.clone())));
        live.begin_session(true, false);
        feed(&live, 2.0, 7000.0);
        live.begin_session(true, false);
        feed(&live, 2.0, 7002.25);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let got = got.lock().unwrap();
        let mut video: Vec<f64> = got.iter().filter(|p| p.0).map(|p| p.1).collect();
        video.sort_by(f64::total_cmp);
        let worst = video.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(video.len() > 110, "{} frames", video.len());
        // The new session's frames are on another phase of the frame grid:
        // at most one and a half frame intervals apart.
        assert!(worst <= 1.5 / 30.0 + 1e-6, "video gap {worst}");
        let audio: Vec<f64> = got.iter().filter(|p| !p.0).map(|p| p.1).collect();
        let worst = audio.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(worst < 1024.0 / 48000.0 + 0.001, "audio gap {worst}");
    }

    #[test]
    #[ignore]
    fn sync_probe_60fps_10ms_chunks() {
        let live = Live::new();
        live.set_config(LiveConfig { width: 640, height: 360, bitrate: 1_000_000, keyframe_secs: 1.0, ..Default::default() });
        live.begin_session(true, false);
        live.set_replay(Some(15.0));
        feed_with(&live, 12.0, 6000.0, 60, 480);
        let saved = live.save_replay(std::path::PathBuf::from("/tmp/rec_replay_60.mp4")).unwrap().join().unwrap().unwrap();
        println!("saved {}", saved.display());
    }

    #[test]
    fn replay_saves_a_playable_mp4() {
        let live = Live::new();
        live.set_config(LiveConfig { width: 640, height: 360, bitrate: 1_000_000, keyframe_secs: 1.0, ..Default::default() });
        live.begin_session(true, false);
        live.set_replay(Some(15.0));
        feed(&live, 20.0, 5000.0);
        let (held, bytes) = live.replay_stats().unwrap();
        assert!((15.0..17.0).contains(&held), "held {held}");
        assert!(bytes > 100_000);
        let dir = std::env::temp_dir().join(format!("rec-replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("replay.mp4");
        let saved = live.save_replay(path.clone()).unwrap().join().unwrap().unwrap();
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name,duration", "-of", "csv=p=0"])
            .arg(&saved)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if std::env::var_os("REC_KEEP_REPLAY_TEST").is_some() {
            std::fs::copy(&saved, "/tmp/rec_replay_test.mp4").unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("h264"), "{text}");
        assert!(text.contains("aac"), "{text}");
        let durs: Vec<f64> = text.lines().filter_map(|l| l.split(',').nth(1)?.parse().ok()).collect();
        assert!(durs.iter().all(|d| (14.5..17.5).contains(d)), "durations {durs:?}");
    }
}
