//! App settings, persisted as JSON in ~/Library/Application Support/rec-rs.
//! The app turns them into the same `record` arguments the CLI takes, so
//! both share one code path (`crate::prepare`).

use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::compose::{Corner, Scene, Shape};
use crate::writer::{AudioLayout, Codec};

pub const RESOLUTIONS: [(&str, &str); 5] =
    [("720p", "720p"), ("1080p", "1080p"), ("1440p", "1440p"), ("4K", "4k"), ("Native", "native")];
pub const FPS: [u32; 5] = [24, 25, 30, 50, 60];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub output_dir: PathBuf,
    /// "mov" or "mp4".
    pub container: String,
    /// None = automatic (H.264 up to 1080p, HEVC above).
    pub codec: Option<Codec>,
    pub resolution: String,
    pub fps: u32,
    /// Mb/s; None = automatic.
    pub bitrate_mbps: Option<f64>,
    /// 1-based display number.
    pub display: usize,
    pub scene: Scene,
    /// Camera unique ID; None = system default.
    pub camera: Option<String>,
    pub mic_enabled: bool,
    /// Microphone unique ID; None = system default.
    pub mic: Option<String>,
    pub system_audio: bool,
    pub audio_tracks: AudioLayout,
    pub system_gain_db: f32,
    pub mic_gain_db: f32,
    pub pip_size: f64,
    pub pip_corner: Corner,
    pub pip_shape: Shape,
    pub mirror: bool,
    pub cursor: bool,
    pub clicks: bool,
    /// Window to record (ID from `rec-rs devices`); not persisted because
    /// window IDs change between launches.
    #[serde(skip)]
    pub window: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            output_dir: default_output_dir(),
            container: "mov".into(),
            codec: None,
            resolution: "1080p".into(),
            fps: 60,
            bitrate_mbps: None,
            display: 1,
            scene: Scene::Screen,
            camera: None,
            mic_enabled: true,
            mic: None,
            system_audio: true,
            audio_tracks: AudioLayout::Both,
            system_gain_db: 0.0,
            mic_gain_db: 0.0,
            pip_size: 0.22,
            pip_corner: Corner::BottomRight,
            pip_shape: Shape::Rounded,
            mirror: true,
            cursor: true,
            clicks: false,
            window: None,
        }
    }
}

pub fn default_output_dir() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Movies")).unwrap_or_default()
}

fn value<T: ValueEnum>(v: T) -> String {
    v.to_possible_value().map(|p| p.get_name().to_string()).unwrap_or_default()
}

impl Settings {
    /// $REC_SETTINGS, else ~/Library/Application Support/rec-rs/settings.json.
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("REC_SETTINGS") {
            return PathBuf::from(p);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join("Library/Application Support/rec-rs/settings.json")
    }

    pub fn load() -> Self {
        let mut s: Settings = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // REC_OUTPUT_DIR overrides the folder (self-tests keep files out of ~/Movies).
        if let Some(d) = std::env::var_os("REC_OUTPUT_DIR") {
            s.output_dir = PathBuf::from(d);
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default())
    }

    /// A fresh timestamped file in the output folder.
    pub fn new_output_file(&self) -> PathBuf {
        let mut buf = [0u8; 64];
        let n = unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            libc::strftime(buf.as_mut_ptr() as *mut _, buf.len(), c"rec-%Y-%m-%d-%H%M%S".as_ptr(), &tm)
        };
        let ext = if self.container == "mp4" && self.codec != Some(Codec::Prores) { "mp4" } else { "mov" };
        self.output_dir.join(format!("{}.{ext}", String::from_utf8_lossy(&buf[..n])))
    }

    /// `rec-rs record` arguments for these settings (`output` None = preview).
    pub fn record_args(&self, output: Option<&std::path::Path>) -> Vec<String> {
        let mut a: Vec<String> = vec!["-q".into(), "-r".into(), self.resolution.clone()];
        a.extend(["--fps".into(), self.fps.to_string()]);
        a.extend(["--display".into(), self.display.to_string()]);
        if let Some(w) = self.window {
            a.extend(["--window".into(), w.to_string()]);
        }
        if let Some(c) = self.codec {
            a.extend(["--codec".into(), value(c)]);
        }
        if let Some(b) = self.bitrate_mbps {
            a.extend(["--bitrate".into(), format!("{b}")]);
        }
        if let Some(o) = output {
            a.extend(["-o".into(), o.to_string_lossy().into_owned()]);
        }
        a.extend(["--scene".into(), value(self.scene)]);
        if self.scene.uses_cam() {
            a.extend(["--webcam".into(), self.camera.clone().unwrap_or_else(|| "default".into())]);
        }
        if !self.mic_enabled {
            a.push("--no-mic".into());
        } else if let Some(m) = &self.mic {
            a.extend(["--mic".into(), m.clone()]);
        }
        if !self.system_audio {
            a.push("--no-system-audio".into());
        }
        a.extend(["--audio-tracks".into(), value(self.audio_tracks)]);
        a.extend(["--system-gain".into(), format!("{}", self.system_gain_db)]);
        a.extend(["--mic-gain".into(), format!("{}", self.mic_gain_db)]);
        a.extend(["--pip-size".into(), format!("{}", self.pip_size)]);
        a.extend(["--pip-corner".into(), value(self.pip_corner)]);
        a.extend(["--pip-shape".into(), value(self.pip_shape)]);
        if !self.mirror {
            a.push("--no-mirror".into());
        }
        if !self.cursor {
            a.push("--no-cursor".into());
        }
        if self.clicks {
            a.push("--clicks".into());
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_and_defaults_for_missing_fields() {
        let mut s = Settings { codec: Some(Codec::Hevc), scene: Scene::Cam, bitrate_mbps: Some(40.0), ..Default::default() };
        s.window = Some(7);
        let t = serde_json::to_string(&s).unwrap();
        assert!(t.contains("\"hevc\"") && t.contains("\"cam\""));
        let back: Settings = serde_json::from_str(&t).unwrap();
        assert_eq!(back.window, None, "window IDs are not persisted");
        assert_eq!(Settings { window: None, ..s }, back);
        let partial: Settings = serde_json::from_str(r#"{"fps": 30}"#).unwrap();
        assert_eq!(partial.fps, 30);
        assert_eq!(partial.resolution, "1080p");
    }

    #[test]
    fn record_args_parse_with_the_cli() {
        let s = Settings {
            codec: Some(Codec::Prores),
            scene: Scene::ScreenCam,
            mic: Some("BuiltInMic".into()),
            system_gain_db: -6.0,
            window: Some(42),
            ..Default::default()
        };
        let args = s.record_args(Some(std::path::Path::new("/tmp/x.mov")));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::parse_record_args(&refs).expect("settings produce valid CLI arguments");
        assert!(args.windows(2).any(|w| w == ["--webcam", "default"]));
        assert!(args.windows(2).any(|w| w == ["--codec", "prores"]));
        assert!(args.windows(2).any(|w| w == ["--window", "42"]));
    }

    #[test]
    fn prores_forces_mov() {
        let s = Settings { container: "mp4".into(), codec: Some(Codec::Prores), ..Default::default() };
        assert!(s.new_output_file().extension().unwrap() == "mov");
        let s = Settings { container: "mp4".into(), ..Default::default() };
        assert!(s.new_output_file().extension().unwrap() == "mp4");
    }
}
