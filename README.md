# rec-rs

A simple, fast, reliable screen and camera recorder for macOS, written in Rust, as an alternative
to OBS for recording YouTube videos. It records the screen, system audio, microphone and webcam
into one `.mov` or `.mp4` that plays in QuickTime and imports into Final Cut, Premiere and
DaVinci Resolve.

It uses only Apple frameworks:

- **ScreenCaptureKit** for display and window capture, system audio and the microphone. Frames
  are IOSurface-backed `CVPixelBuffer`s that are never copied.
- **AVCaptureSession** for the webcam, and optionally the microphone.
- **CoreImage on Metal** for compositing: webcam picture-in-picture and scenes.
- **VideoToolbox through AVAssetWriter** for hardware H.264, HEVC or ProRes 422, plus AAC
  audio.

There is no FFmpeg, OBS or Electron at runtime. The UI is egui on wgpu in the paper-rs style:
transparent titlebar and the Inter font.

It needs macOS 15 or later on Apple Silicon. It was developed and tested on macOS 27 on an M4
Pro MacBook Pro.

The code is a clean-room implementation, licensed MIT OR Apache-2.0. OBS (GPL-2.0) was studied
for its architecture only and no OBS code was copied. See
[analysis/OBS_ARCHITECTURE.md](analysis/OBS_ARCHITECTURE.md).

## Reverse-engineered origin

rec-rs is an independent, clean-room-style reimplementation. It was reverse engineered from
[OBS Studio](https://obsproject.com) by studying its architecture and behavior with
[REA](https://github.com/morluto/rea) and Claude Code. It is not affiliated with or endorsed by
the OBS Project. No original OBS code or assets are included in this repository.

## Build

```sh
./scripts/build.sh          # cargo build --release, then assemble and sign dist/Rec.app
dist/rec-rs --help          # dist/rec is a short alias
```

Build output goes to `~/.cache/rec-rs-target` (`CARGO_TARGET_DIR`), never into the repo,
because the repo is synced between machines. The script signs `dist/Rec.app` with your Apple
Development identity, so the macOS privacy grants survive rebuilds. Override it with
`REC_SIGN_IDENTITY`.

Tests:

```sh
cargo test                       # unit tests: pause timeline, mixer, CLI, settings, scenes
scripts/selftest.sh              # records through every pipeline and verifies each file
scripts/selftest.sh --no-screen  # skip the cases that need Screen Recording
scripts/soak.sh 1800             # 30-minute 4K60 soak with CPU/memory sampling and ffprobe checks
scripts/synccheck.py FILE        # A/V offset of a flash + beep recording
```

Test recordings go to `~/Movies/rec-rs-tests/`.

## Permissions (one time)

```sh
dist/rec-rs permissions --request
```

Then enable **Rec** under **System Settings → Privacy & Security → Screen & System Audio
Recording**. Microphone and camera are approved from the system prompts.

- `rec-rs` re-launches itself with responsibility disclaimed. macOS therefore asks about and
  remembers the signed **Rec** app rather than your terminal.
- If macOS asks whether Rec may "bypass the system private window picker", click **Allow**.
- `dist/rec-rs permissions` shows the current status.

## The app

```sh
open dist/Rec.app          # or: dist/rec-rs   (or: dist/rec-rs gui [--settings] [--start-recording])
```

The **main window** contains:

- **Live preview** of exactly what will be recorded, including the webcam overlay. Rec's own
  windows are excluded from capture.
- **Scene**:
  - Screen.
  - Screen + Cam: the webcam in a corner, rounded, circle or rectangle.
  - Camera: the webcam full frame.

  While recording you can switch scenes live, provided the recording started with a camera
  scene.
- **Sources**:
  - Display.
  - Entire display, or a single window.
  - Camera.
  - Microphone.
  - System audio.
- **Record / Stop** (⇧⌘R from anywhere), **Pause / Resume**, the recording timer, and **level
  meters** for system audio and mic with peak hold.
- **Reveal Last** and **Open Folder**.

The **settings window** (⌘, or the Settings… button) covers:

- Output folder and container (`.mov` or `.mp4`).
- Resolution: 720p, 1080p, 1440p, 4K or native.
- Frame rate, codec (auto, H.264, HEVC, ProRes) and bitrate.
- Cursor and click highlighting.
- Audio devices, track layout and gains.
- Camera device, plus overlay corner, shape, size and mirroring.

Settings persist in `~/Library/Application Support/rec-rs/settings.json`.

Rec is a regular app with a Dock icon and an app menu (Settings… ⌘,, Hide ⌘H, Quit ⌘Q, Window ▸
Minimize, Close, Show Rec Window). Closing the main window keeps Rec running, in the Dock and the
menu bar, and stops the preview. Clicking the Dock icon brings the window back. Quitting from
the Dock, the menu or a logout finalizes a running recording first.

The **menu bar item** shows a red dot and the elapsed time while recording. Its menu has
Start/Stop, Pause/Resume, Show Window, Settings, Reveal Last, Open Folder and Quit.

You can also control a running recording, from the app or the CLI, from any shell:

```sh
rec-rs pause      # SIGUSR1
rec-rs resume     # SIGUSR2
rec-rs stop       # finalizes, prints the file
```

Quitting the app or sending it SIGTERM mid-recording finalizes the file first.

⇧⌘R is registered system-wide while Rec runs. A browser's hard reload therefore won't see it
until Rec quits. If another app already owns the shortcut, the UI says so.

## Command line

```sh
# the canonical example
rec-rs record --display 1 --mic default --system-audio --cam --out file.mp4 --duration 10

# 4K60 HEVC, screen + webcam, Ctrl-C (or `rec-rs stop`) to finish
rec-rs record -r 4k --cam

# one window at native resolution, camera-only scene, ProRes
rec-rs record --window "Safari" -r native --codec prores
rec-rs record --scene cam -o talking-head.mov
```

The default output is `~/Movies/rec-YYYY-MM-DD-HHMMSS.mov`. A live status line shows the
recorded time, frame counts and peak levels. `--duration` is exact: it is counted in output
frames, and pauses don't count.

| Option | Default | Notes |
|---|---|---|
| `-o, --out, --output` | `~/Movies/rec-….mov` | `.mov` or `.mp4`. |
| `-d, --display N` | `1` | Display number from `rec-rs devices` (1 = main). |
| `--window W` | | Window ID or app/title substring. |
| `--region X,Y,W,H` | | Crop a display region (points from top-left). |
| `-r, --resolution` | `1080p` | `720p`, `1080p`, `1440p`, `4k`, `native`, `WxH`. Exact 16:9 frames, pillar-boxed when the display isn't 16:9 (or `--keep-aspect`). |
| `-f, --fps` | `60` | 24, 25, 30, 50, 60, 120. |
| `--codec` | auto | `h264` up to 1080p, `hevc` above, `prores` (422, `.mov`). |
| `--bitrate` | auto | Mb/s. About 20 for 1080p60 H.264, about 50 for 4K60 HEVC. |
| `--system-audio` / `--no-system-audio` | on | |
| `--mic DEVICE` / `--no-mic` | `default` | Unique ID or name substring. |
| `-w, --cam, --webcam [DEVICE]` | off | `default` or ID or name. Turns on the Screen + Cam scene. |
| `--scene` | `screen-cam` with a camera, else `screen` | `screen`, `screen-cam`, `cam`. |
| `--pip-size`, `--pip-corner`, `--pip-shape`, `--no-mirror` | `0.22`, bottom-right, rounded | Shape: `rounded`, `circle`, `rect`. |
| `--audio-tracks` | `both` | `both` = mix plus system and mic stems; `mix`; `stems`. |
| `--system-gain`, `--mic-gain` | `0` | dB, in the mix track. |
| `--system-audio-offset-ms`, `--mic-offset-ms` | output latency, `0` | Sync offsets. System audio is shifted by the output device's latency by default (see Output format). |
| `--no-cursor`, `--clicks` | cursor shown | |
| `-t, --duration` | | Seconds of output. |
| `--mic-backend` | `sck` | `sck` (ScreenCaptureKit) or `avcapture`. |
| `--test-pattern [--noise]` | | Synthetic source: the frame flashes white and the "system audio" beeps once a second, for self-tests and sync checks. |

Other commands:

- `rec-rs devices` lists displays, windows, cameras and mics.
- `rec-rs probe FILE` checks a file with AVFoundation, QuickTime's engine, and decodes every
  sample.
- `rec-rs permissions [--request]` shows or requests the privacy grants.
- `rec-rs stop`, `rec-rs pause` and `rec-rs resume` control a running recording.

## Output format

| Track | Contents |
|---|---|
| 1 video | H.264 High, HEVC Main (`hvc1`) or ProRes 422. Constant frame rate, BT.709 tagged, keyframe every 2 s. |
| 2 audio | **Mix** of system and mic: AAC-LC 48 kHz stereo, 256 kb/s. The default track, so players and uploaders use it. |
| 3 audio | System audio stem: AAC 48 kHz stereo. |
| 4 audio | Mic stem: AAC 48 kHz mono. |

- **Track grouping.** With one audio source there is a single audio track. The audio tracks
  share an alternate group with the mix as default, so QuickTime plays the mix and editors see
  every track.
- **Constant frame rate.** ScreenCaptureKit only sends a frame when pixels change. rec-rs
  writes on an exact 1/fps grid and repeats the newest frame when needed. Measured frame spacing
  is exactly 16.667 ms.
- **Single clock.** Screen, system audio, mic and webcam are all stamped on the host clock, and
  audio is never stretched. The flash/beep sync test measures 0.0 ms between video and audio in
  the file, including across pauses.
- **System audio latency.** ScreenCaptureKit captures system audio when it is mixed, which is
  one output latency before you hear it. rec-rs shifts it later by the output device's latency,
  read from CoreAudio (about 27 ms on built-in speakers, more on Bluetooth), so the audio lines
  up with the picture you saw. `rec-rs devices` prints the latency.
- **Webcam sync.** Emission runs a little behind real time, so the webcam frame captured at each
  instant is the one used. Webcams deliver frames 70–120 ms late, and the lag adapts to the
  slowest source.
- **Crash-safe.** Movie fragments are written every 2 s, so a crash or power loss leaves a
  playable file up to the last fragment. A normal stop finalizes a regular movie
  (`ftyp wide mdat moov`).
- **Pause.** Pauses cut whole frame slots out of the timeline. Audio is cut at the exact sample
  and shifted with the video, so there is no gap and no drift.

## Feature comparison with OBS (macOS)

| Feature | OBS 32 | rec-rs |
|---|---|---|
| Display capture | ✅ | ✅ ScreenCaptureKit, zero-copy |
| Window capture | ✅ | ✅ |
| Region / crop | ✅ crop filter | ✅ `--region` (CLI) |
| Application capture (all windows of an app) | ✅ | ❌ |
| Scenes | ✅ unlimited, free layout, transitions | ✅ three fixed scenes (screen, screen + cam, cam), switchable live, cut only |
| Multiple displays/sources at once | ✅ | ❌ one screen source plus the webcam |
| System audio | ✅ | ✅ |
| Microphone | ✅ | ✅ |
| Webcam overlay | ✅ any layout | ✅ corner PiP: rounded, circle or rect, mirrored |
| Separate audio tracks | ✅ up to 6 | ✅ mix plus system and mic stems |
| Per-source gain | ✅ | ✅ |
| Audio filters (noise suppression, compressor) | ✅ | ❌ |
| Hardware H.264 / HEVC | ✅ | ✅ |
| ProRes | ✅ | ✅ 422 |
| 10-bit / HDR | ✅ | ❌ 8-bit SDR, BT.709 |
| Constant frame rate | ✅ | ✅ exact grid |
| Crash-safe files | ✅ hybrid/fragmented MP4 | ✅ fragments every 2 s |
| Pause / resume | ✅ | ✅ sample-accurate |
| Live preview | ✅ | ✅ |
| Level meters | ✅ | ✅ peak with hold |
| Global hotkey | ✅ configurable | ✅ ⇧⌘R (fixed) |
| Menu bar control | ❌ | ✅ menu bar item plus Dock app |
| Headless CLI | ❌ (obs-cli via websocket) | ✅ `rec-rs record …`, `stop`, `pause`, `resume` |
| Replay buffer, streaming | ✅ | ❌ |
| Footprint | about 300 MB app | about 16 MB app; about 6% of one core at 4K60 screen-only, about 34% with the webcam overlay (see [PERFORMANCE.md](PERFORMANCE.md)) |
| Encoder input | frames copied back to the CPU for encoding | IOSurface passed straight to VideoToolbox |

## Known gaps

- **Scenes.** There are three fixed scenes with no free layout editor and no transitions. You
  can only switch live into a camera scene if the recording started with the camera on.
- **Capture sources.** There is no application capture or multi-display capture. Region capture
  is CLI-only.
- **Missing features.** There is no replay buffer, no streaming, no audio filters, and the
  hotkey isn't configurable.
- **8-bit SDR only.** HDR displays are tone-mapped by ScreenCaptureKit.
- **Webcam overlay.** The overlay is burned into the video; there is no separate camera track.
- **Output latency read once.** It is read when recording starts. Switching output devices
  mid-recording (to AirPods, say) leaves the old offset in place.
- **Mix track resampling.** A mic that isn't 48 kHz is resampled linearly per buffer for the mix
  track. The mic stem is converted by AVFoundation.
- **Re-confirmation prompt.** macOS periodically asks you to re-confirm screen recording for
  apps that bypass the system picker. This happens to every recorder.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI |
| `src/app/` | App: windows and UI (`ui.rs`), menu bar item and app menu, Dock quit/reopen hooks, preview scaler, theme, macOS chrome |
| `src/settings.rs` | Persisted settings → record arguments |
| `src/hotkey.rs` | Global hotkey (Carbon `RegisterEventHotKey`; needs no Accessibility permission) |
| `src/record.rs` | Sessions, pacing thread, signals, `stop`/`pause`/`resume` |
| `src/writer.rs` | AVAssetWriter sink, constant-frame-rate grid, pause timeline, track layout |
| `src/screen.rs` | ScreenCaptureKit capture, displays and windows |
| `src/camera.rs` | Webcam and AVCapture mic, device lists |
| `src/compose.rs` | CoreImage scenes and webcam overlay |
| `src/mixer.rs` | Real-time audio mixer |
| `src/tcc.rs` | Permissions and responsibility disclaim |
| `src/probe.rs` | AVFoundation verification |
| `src/synth.rs` | Test pattern with flash + beep |
| `scripts/` | Build, self-test, soak and sync check |
| `analysis/OBS_ARCHITECTURE.md` | OBS study |
