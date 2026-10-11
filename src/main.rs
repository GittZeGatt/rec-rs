mod camera;
mod compose;
mod gui;
mod hotkey;
mod mixer;
mod probe;
mod record;
mod screen;
mod synth;
mod tcc;
mod util;
mod writer;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use compose::{Corner, PipSpec, Shape};
use writer::Codec;

#[derive(Parser)]
#[command(name = "rec", version, about = "Native macOS screen recorder (ScreenCaptureKit + VideoToolbox + AVAssetWriter)")]
struct Cli {
    /// Without a subcommand, rec starts the menu bar app.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record the screen (plus system audio, mic, and optional webcam overlay).
    Record(RecordArgs),
    /// Menu bar app with live preview and a global Cmd+Shift+R hotkey
    /// (also what `open dist/Rec.app` starts).
    Gui {
        /// Start recording immediately (e.g. from a Login Item or script).
        #[arg(long)]
        start_recording: bool,
    },
    /// List displays, windows, cameras and microphones.
    Devices,
    /// Stop a running `rec record` (e.g. from a hotkey) and print its file.
    Stop,
    /// Show (and optionally request) screen, microphone and camera permissions.
    Permissions {
        /// Trigger the system permission prompts for anything not yet granted.
        #[arg(long)]
        request: bool,
        /// Seconds to wait for prompt answers when requesting.
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    /// Check a recording with AVFoundation (the engine QuickTime uses).
    Probe { file: PathBuf },
}

#[derive(Args)]
pub struct RecordArgs {
    /// Output file (.mov or .mp4). Default: ~/Movies/rec-<timestamp>.mov
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Display index from `rec devices` (0 = main display).
    #[arg(short, long, default_value_t = 0)]
    display: usize,
    /// Record only this region of the display: X,Y,WIDTH,HEIGHT in points
    /// from the display's top-left corner.
    #[arg(long, value_name = "X,Y,W,H", conflicts_with = "window")]
    region: Option<String>,
    /// Record a single window: window ID or app/title substring (`rec devices`).
    #[arg(long, value_name = "WINDOW")]
    window: Option<String>,
    /// Output size: 720p, 1080p, 1440p, 4k, native, or WIDTHxHEIGHT.
    #[arg(short, long, default_value = "1080p")]
    resolution: String,
    /// Shrink the frame to the display's aspect ratio instead of letterboxing
    /// into an exact 16:9 frame.
    #[arg(long)]
    keep_aspect: bool,
    /// Frames per second.
    #[arg(short, long, default_value_t = 60)]
    fps: u32,
    /// Video codec. Default: h264 up to 1080p, hevc above.
    #[arg(long, value_enum)]
    codec: Option<Codec>,
    /// Video bitrate in Mb/s. Default scales with resolution and fps.
    #[arg(long)]
    bitrate: Option<f64>,
    /// Do not record system audio.
    #[arg(long)]
    no_system_audio: bool,
    /// Microphone: unique ID or name substring (default: system default mic).
    #[arg(long, value_name = "DEVICE")]
    mic: Option<String>,
    /// Do not record the microphone.
    #[arg(long, conflicts_with = "mic")]
    no_mic: bool,
    /// Overlay a webcam (optionally: unique ID or name substring).
    #[arg(short, long, value_name = "DEVICE", num_args = 0..=1, default_missing_value = "")]
    webcam: Option<String>,
    /// Webcam overlay width as a fraction of the frame width.
    #[arg(long, default_value_t = 0.22)]
    pip_size: f64,
    #[arg(long, value_enum, default_value_t = Corner::BottomRight)]
    pip_corner: Corner,
    #[arg(long, value_enum, default_value_t = Shape::Rounded)]
    pip_shape: Shape,
    /// Do not mirror the webcam.
    #[arg(long)]
    no_mirror: bool,
    /// Hide the mouse cursor.
    #[arg(long)]
    no_cursor: bool,
    /// Highlight mouse clicks.
    #[arg(long)]
    clicks: bool,
    /// Stop automatically after this many seconds.
    #[arg(short = 't', long)]
    duration: Option<f64>,
    /// Do not print live status.
    #[arg(short, long)]
    quiet: bool,
    /// Audio tracks when recording both system audio and mic.
    #[arg(long, value_enum, default_value_t = writer::AudioLayout::Both)]
    audio_tracks: writer::AudioLayout,
    /// System audio gain in the mix track (dB).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    system_gain: f32,
    /// Microphone gain in the mix track (dB).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    mic_gain: f32,
    /// Microphone capture backend.
    #[arg(long, value_enum, default_value_t = record::MicBackend::Sck)]
    mic_backend: record::MicBackend,
    /// Record a generated test pattern instead of the screen (self-test;
    /// needs no Screen Recording permission). Implies --no-system-audio and
    /// --mic-backend avcapture.
    #[arg(long)]
    test_pattern: bool,
    /// With --test-pattern: add random noise blocks to stress the encoder.
    #[arg(long, requires = "test_pattern")]
    noise: bool,
}

fn even(x: f64) -> usize {
    ((x / 2.0).round() as usize * 2).max(2)
}

fn parse_size(s: &str, native: (usize, usize), keep_aspect: bool) -> Result<(usize, usize)> {
    let lower = s.to_lowercase();
    let boxed = match lower.as_str() {
        "720p" => (1280, 720),
        "1080p" => (1920, 1080),
        "1440p" => (2560, 1440),
        "4k" | "2160p" | "uhd" => (3840, 2160),
        "native" => return Ok((even(native.0 as f64), even(native.1 as f64))),
        _ => {
            let Some((w, h)) = lower.split_once('x') else {
                bail!("bad --resolution {s:?}: use 720p/1080p/1440p/4k/native/WxH")
            };
            (w.trim().parse()?, h.trim().parse()?)
        }
    };
    if !keep_aspect {
        return Ok((even(boxed.0 as f64), even(boxed.1 as f64)));
    }
    let s = (boxed.0 as f64 / native.0 as f64).min(boxed.1 as f64 / native.1 as f64);
    Ok((even(native.0 as f64 * s), even(native.1 as f64 * s)))
}

fn default_output() -> PathBuf {
    let mut buf = [0u8; 64];
    let n = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        libc::strftime(buf.as_mut_ptr() as *mut _, buf.len(), c"rec-%Y-%m-%d-%H%M%S.mov".as_ptr(), &tm)
    };
    let name = String::from_utf8_lossy(&buf[..n]).to_string();
    let dir = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Movies")).unwrap_or_default();
    dir.join(name)
}

/// Turn record arguments into session options and a capture target
/// (shared by the CLI and the GUI).
pub fn prepare(a: RecordArgs) -> Result<(record::Options, Option<screen::Target>)> {
    let mic = if a.no_mic {
        None
    } else {
        Some(a.mic.as_deref().map(camera::resolve_mic).transpose()?)
    };
    let webcam = a.webcam.map(|s| if s.is_empty() { None } else { Some(s) });
    let mut opts = record::Options {
        output: a.output.unwrap_or_else(default_output),
        size: (0, 0),
        fps: a.fps,
        codec: Codec::H264,
        bitrate: 0,
        system_audio: !a.no_system_audio,
        mic,
        webcam,
        pip: PipSpec { size: a.pip_size.clamp(0.05, 0.9), corner: a.pip_corner, shape: a.pip_shape, mirror: !a.no_mirror },
        cursor: !a.no_cursor,
        clicks: a.clicks,
        duration: a.duration,
        quiet: a.quiet,
        test_pattern: a.test_pattern.then_some(a.noise),
        audio_layout: a.audio_tracks,
        gains_db: [a.system_gain, a.mic_gain],
        mic_backend: if a.test_pattern { record::MicBackend::Avcapture } else { a.mic_backend },
    };
    if a.test_pattern {
        opts.system_audio = false;
    }
    record::check_permissions(&opts)?;
    if !(1..=240).contains(&a.fps) || 60000 % a.fps != 0 {
        bail!("--fps must divide 60000 (e.g. 24, 25, 30, 50, 60, 120)");
    }
    let (target, native) = if a.test_pattern {
        (None, (3840, 2160))
    } else {
        let mut displays = screen::displays()?;
        if a.display >= displays.len() {
            bail!("display {} not found ({} available, see `rec devices`)", a.display, displays.len());
        }
        let d = displays.swap_remove(a.display);
        let scale = d.pixels.0 as f64 / d.points.0.max(1) as f64;
        if let Some(sel) = &a.window {
            let w = screen::find_window(sel)?;
            let native = ((w.size.0 * scale) as usize, (w.size.1 * scale) as usize);
            (Some(screen::Target::Window(w)), native)
        } else if let Some(r) = &a.region {
            let v: Vec<f64> = r.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>()?;
            if v.len() != 4 || v[2] <= 0.0 || v[3] <= 0.0 {
                bail!("--region must be X,Y,WIDTH,HEIGHT (points)");
            }
            let rect = objc2_core_foundation::CGRect::new(
                objc2_core_foundation::CGPoint::new(v[0], v[1]),
                objc2_core_foundation::CGSize::new(v[2], v[3]),
            );
            let native = ((v[2] * scale) as usize, (v[3] * scale) as usize);
            (Some(screen::Target::Display(d, Some(rect))), native)
        } else {
            let native = d.pixels;
            (Some(screen::Target::Display(d, None)), native)
        }
    };
    let (w, h) = parse_size(&a.resolution, native, a.keep_aspect)?;
    let codec = a.codec.unwrap_or(if w * h > 1920 * 1080 { Codec::Hevc } else { Codec::H264 });
    // Bits per pixel per frame tuned for screen content (sharp text).
    let bpp = match codec {
        Codec::H264 => 0.16,
        Codec::Hevc => 0.10,
        Codec::Prores => 0.0, // fixed quality; bitrate unused
    };
    let bitrate = a
        .bitrate
        .map(|m| (m * 1e6) as u64)
        .unwrap_or_else(|| ((w * h) as f64 * a.fps as f64 * bpp) as u64)
        .clamp(1_000_000, 400_000_000);
    if codec == Codec::Prores && opts.output.extension().is_some_and(|e| e == "mp4") {
        bail!("ProRes needs a .mov output");
    }
    opts.size = (w, h);
    opts.codec = codec;
    opts.bitrate = bitrate;
    if let Some(parent) = opts.output.parent()
        && !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    Ok((opts, target))
}

/// Parse a `rec record ...` argument list (used by the GUI).
pub fn parse_record_args(args: &[&str]) -> Result<RecordArgs> {
    let mut v = vec!["rec", "record"];
    v.extend_from_slice(args);
    match Cli::try_parse_from(v)?.cmd {
        Some(Cmd::Record(a)) => Ok(a),
        _ => bail!("internal: not a record command"),
    }
}

fn record(a: RecordArgs) -> Result<()> {
    let (opts, target) = prepare(a)?;
    let out = opts.output.clone();
    let quiet = opts.quiet;
    let stats = record::run(opts, target)?;
    if !quiet {
        eprintln!(
            "rec: saved {} ({} video frames, {} duplicated, {} dropped; audio buffers sys {} mic {} mix {}, {} dropped, {} clipped samples)",
            out.display(),
            stats.video_frames_out,
            stats.video_dups,
            stats.video_drops,
            stats.audio_sys_buffers,
            stats.audio_mic_buffers,
            stats.audio_mix_buffers,
            stats.audio_drops,
            stats.audio_clipped_samples
        );
    }
    if std::env::var_os("REC_DEBUG").is_some() {
        let avg = |sum: f64, n: u64| if n == 0 { 0.0 } else { sum / n as f64 * 1000.0 };
        eprintln!(
            "rec: debug: frame arrival latency  screen avg {:.1} ms max {:.1} ms ({} frames)  camera avg {:.1} ms max {:.1} ms ({} frames)",
            avg(stats.screen_latency_sum, stats.screen_frames_in),
            stats.screen_latency_max * 1000.0,
            stats.screen_frames_in,
            avg(stats.cam_latency_sum, stats.cam_frames_in),
            stats.cam_latency_max * 1000.0,
            stats.cam_frames_in
        );
    }
    println!("{}", out.display());
    Ok(())
}

fn devices() -> Result<()> {
    println!("Displays:");
    match screen::displays() {
        Ok(ds) => {
            for (i, d) in ds.iter().enumerate() {
                println!(
                    "  [{i}] id {}  {}x{} pixels ({}x{} points)",
                    d.id, d.pixels.0, d.pixels.1, d.points.0, d.points.1
                );
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Windows (for --window):");
    match screen::windows() {
        Ok(ws) => {
            for w in ws {
                println!("  {:>6}  {} — {}  ({}x{} pt)", w.id, w.app, w.title, w.size.0, w.size.1);
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Cameras:");
    for c in camera::cameras() {
        println!("  {}  [{}]", c.name, c.id);
    }
    println!("Microphones:");
    for m in camera::microphones() {
        println!("  {}  [{}]", m.name, m.id);
    }
    Ok(())
}

fn main() {
    // Behave like a normal CLI when piped into `head` etc.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    tcc::disclaim_and_reexec();
    let cli = Cli::parse();
    let r = match cli.cmd.unwrap_or(Cmd::Gui { start_recording: false }) {
        Cmd::Gui { start_recording } => gui::run(start_recording),
        Cmd::Record(a) => record(a),
        Cmd::Devices => devices(),
        Cmd::Stop => record::stop_running(),
        Cmd::Permissions { request, wait } => {
            if request {
                tcc::request_all(true, true, true, std::time::Duration::from_secs(wait));
            }
            let (s, m, c) = tcc::report();
            println!("screen recording : {s}");
            println!("microphone       : {m}");
            println!("camera           : {c}");
            Ok(())
        }
        Cmd::Probe { file } => probe::probe(&file),
    };
    if let Err(e) = r {
        eprintln!("rec: error: {e:#}");
        std::process::exit(1);
    }
}
