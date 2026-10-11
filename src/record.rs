//! Recording session orchestration.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::camera::{self, Camera, Mic};
use crate::synth::Synth;
use crate::compose::{Compositor, PipSpec};
use crate::screen::{self, ScreenCapture, ScreenConfig};
use crate::tcc::{self, Status};
use crate::util::*;
use crate::writer::{AudioLayout, Codec, Sink, Stats, VideoSpec};

pub struct Shared {
    pub sink: Mutex<Option<Sink>>,
    pub error: Mutex<Option<String>>,
}

impl Shared {
    pub fn with(&self, f: impl FnOnce(&mut Sink)) {
        if let Some(s) = self.sink.lock().unwrap().as_mut() {
            f(s);
        }
    }
    pub fn fail(&self, msg: String) {
        let mut e = self.error.lock().unwrap();
        if e.is_none() {
            *e = Some(msg);
        }
        STOP.store(true, Ordering::SeqCst);
    }
}

static STOP: AtomicBool = AtomicBool::new(false);
static SIGNALS: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    // A second Ctrl-C while finalizing aborts immediately.
    if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 {
        unsafe { libc::_exit(130) };
    }
}

pub struct Options {
    pub output: PathBuf,
    pub size: (usize, usize),
    pub fps: u32,
    pub codec: Codec,
    pub bitrate: u64,
    pub system_audio: bool,
    pub mic: Option<Option<String>>,
    pub webcam: Option<Option<String>>,
    pub pip: PipSpec,
    pub cursor: bool,
    pub clicks: bool,
    pub duration: Option<f64>,
    pub quiet: bool,
    /// Replace the screen with a generated pattern (Some(noise)).
    pub test_pattern: Option<bool>,
    pub audio_layout: AudioLayout,
    /// Gain in dB for [system, mic] in the mix track.
    pub gains_db: [f32; 2],
    pub mic_backend: MicBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum MicBackend {
    /// ScreenCaptureKit microphone capture (same clock/stream as the screen).
    Sck,
    /// AVCaptureSession audio input.
    Avcapture,
}

pub fn check_permissions(o: &Options) -> Result<()> {
    let mut missing = vec![];
    if o.test_pattern.is_none() && tcc::screen_status() != Status::Granted {
        missing.push("Screen & System Audio Recording");
    }
    if o.mic.is_some() && tcc::mic_status() != Status::Granted {
        missing.push("Microphone");
    }
    if o.webcam.is_some() && tcc::camera_status() != Status::Granted {
        missing.push("Camera");
    }
    if !missing.is_empty() {
        bail!(
            "missing permission(s): {}\n  run `rec permissions --request`, then enable Rec in System Settings > Privacy & Security",
            missing.join(", ")
        );
    }
    Ok(())
}

pub fn run(o: Options, target: Option<screen::Target>) -> Result<Stats> {
    let (w, h) = o.size;
    let pixel_format = if o.webcam.is_some() { PIX_BGRA } else { PIX_420V };
    let spec = VideoSpec { width: w, height: h, fps: o.fps, codec: o.codec, bitrate: o.bitrate, pixel_format };
    let compositor = o.webcam.as_ref().map(|_| Compositor::new(w, h, o.pip.clone()));
    let sink = Sink::new(&o.output, &spec, o.system_audio, o.mic.is_some(), o.audio_layout, o.gains_db, compositor)?;
    let shared = Arc::new(Shared { sink: Mutex::new(Some(sink)), error: Mutex::new(None) });

    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    }

    let cleanup_partial = |shared: &Arc<Shared>| {
        if let Some(s) = shared.sink.lock().unwrap().take() {
            let _ = s.finish(0.0);
        }
        let _ = std::fs::remove_file(&o.output);
    };

    let cam = match &o.webcam {
        Some(sel) => match Camera::start(sel.as_deref(), shared.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                cleanup_partial(&shared);
                return Err(e);
            }
        },
        None => None,
    };
    let mic_desc = match &o.mic {
        None => "off".to_string(),
        Some(None) => "default".to_string(),
        Some(Some(id)) => camera::microphones()
            .into_iter()
            .find(|m| &m.id == id)
            .map(|m| m.name)
            .unwrap_or(id.clone()),
    };
    let sck_mic = if o.mic_backend == MicBackend::Sck { o.mic.clone() } else { None };
    let mut av_mic = None;
    if o.mic_backend == MicBackend::Avcapture
        && let Some(sel) = &o.mic {
            match Mic::start(sel.as_deref(), shared.clone()) {
                Ok(m) => av_mic = Some(m),
                Err(e) => {
                    if let Some(c) = &cam {
                        c.stop();
                    }
                    cleanup_partial(&shared);
                    return Err(e);
                }
            }
        }
    let mut synth = None;
    let mut screen = None;
    let source_desc;
    match (o.test_pattern, target) {
        (Some(noise), _) => {
            synth = Some(Synth::start(w, h, o.fps, pixel_format, noise, shared.clone()));
            source_desc = format!("test pattern{}", if noise { " (noise)" } else { "" });
        }
        (None, Some(target)) => {
            source_desc = target.describe();
            let cfg = ScreenConfig {
                target,
                width: w,
                height: h,
                fps: o.fps,
                pixel_format,
                cursor: o.cursor,
                clicks: o.clicks,
                system_audio: o.system_audio,
                mic: sck_mic,
            };
            match ScreenCapture::start(&cfg, shared.clone()) {
                Ok(s) => screen = Some(s),
                Err(e) => {
                    if let Some(c) = &cam {
                        c.stop();
                    }
                    if let Some(m) = &av_mic {
                        m.stop();
                    }
                    cleanup_partial(&shared);
                    return Err(e);
                }
            }
        }
        (None, None) => bail!("internal: no capture target"),
    }

    if !o.quiet {
        eprintln!(
            "rec: recording {} -> {} ({}x{} @ {}fps {:?}{}, system audio {}, mic {}, webcam {})",
            source_desc,
            o.output.display(),
            w,
            h,
            o.fps,
            o.codec,
            if o.codec == Codec::Prores { String::new() } else { format!(" {} Mb/s", o.bitrate / 1_000_000) },
            if o.system_audio { "on" } else { "off" },
            mic_desc,
            cam.as_ref().map(|c| c.name.as_str()).unwrap_or("off"),
        );
        eprintln!("rec: press Ctrl-C to stop");
    }

    // Pacer: emit constant-frame-rate video a little behind real time.
    let pacer_shared = shared.clone();
    let fps = o.fps;
    let pacer = std::thread::spawn(move || {
        let period = Duration::from_secs_f64(1.0 / fps as f64 / 2.0);
        while !STOP.load(Ordering::SeqCst) {
            let now = secs(host_now());
            // Plain threads have no autorelease pool; without one every
            // autoreleased CoreImage/AVFoundation object would leak.
            objc2::rc::autoreleasepool(|_| {
                pacer_shared.with(|s| {
                    s.emit_until(now - s.lag());
                    if let Some(f) = s.failure() {
                        let f = f.to_string();
                        let ps = pacer_shared.clone();
                        std::thread::spawn(move || ps.fail(format!("writer failed: {f}")));
                    }
                });
            });
            std::thread::sleep(period);
        }
    });

    let pidfile = pidfile_path();
    let _ = std::fs::write(&pidfile, format!("{}\n{}\n", std::process::id(), o.output.display()));

    let started = Instant::now();
    let mut last_status = Instant::now();
    let mut warned_no_frames = false;
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
        let el = started.elapsed().as_secs_f64();
        if let Some(d) = o.duration
            && el >= d {
                break;
            }
        if !warned_no_frames && el > 3.0 {
            let mut started_ok = true;
            shared.with(|s| started_ok = s.started());
            if !started_ok {
                warned_no_frames = true;
                eprintln!("rec: warning: no screen frames after 3s (is Screen Recording permission granted?)");
            }
        }
        if !o.quiet && last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let mut st = Stats::default();
            let mut lv = [0f32; 2];
            shared.with(|s| {
                st = s.stats.clone();
                lv = s.take_levels();
            });
            let db = |p: f32| if p <= 1e-5 { "  -inf".to_string() } else { format!("{:>6.1}", 20.0 * p.log10()) };
            eprint!(
                "\r\x1b[2K  {:02}:{:02}  video {} fr ({} dup, {} drop)  sys {} dB  mic {} dB  ",
                el as u64 / 60,
                el as u64 % 60,
                st.video_frames_out,
                st.video_dups,
                st.video_drops,
                if o.system_audio { db(lv[0]) } else { "   off".into() },
                if o.mic.is_some() { db(lv[1]) } else { "   off".into() },
            );
        }
    }
    let end = secs(host_now());
    STOP.store(true, Ordering::SeqCst);
    if !o.quiet {
        eprintln!("\nrec: stopping...");
    }
    let _ = pacer.join();
    let stop_err = screen.as_ref().and_then(|s| s.stop().err());
    if let Some(s) = synth.as_mut() {
        s.stop();
    }
    if let Some(c) = &cam {
        c.stop();
    }
    if let Some(m) = &av_mic {
        m.stop();
    }
    let sink = shared.sink.lock().unwrap().take();
    let result = match sink {
        Some(s) => objc2::rc::autoreleasepool(|_| s.finish(end)),
        None => bail!("internal: sink missing"),
    };
    let _ = std::fs::remove_file(&pidfile);
    if let Some(e) = shared.error.lock().unwrap().take() {
        eprintln!("rec: error during recording: {e}");
    }
    if let Some(e) = stop_err {
        eprintln!("rec: warning: stopping screen capture: {e}");
    }
    result
}

pub fn pidfile_path() -> PathBuf {
    std::env::temp_dir().join("rec.pid")
}

/// Ask a running `rec record` to stop (like Ctrl-C) and wait for it to finish.
pub fn stop_running() -> Result<()> {
    let path = pidfile_path();
    let text = std::fs::read_to_string(&path).map_err(|_| anyhow::anyhow!("no recording in progress"))?;
    let mut lines = text.lines();
    let pid: i32 = lines.next().unwrap_or("").trim().parse()?;
    let file = lines.next().unwrap_or("").to_string();
    if unsafe { libc::kill(pid, 0) } != 0 {
        let _ = std::fs::remove_file(&path);
        bail!("no recording in progress (stale pidfile removed)");
    }
    unsafe { libc::kill(pid, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(90);
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() > deadline {
            bail!("recorder (pid {pid}) did not exit within 90s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("{file}");
    Ok(())
}
