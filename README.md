# rec

A native macOS screen recorder written in Rust. It records the screen, system audio, microphone
and webcam into a single `.mov` that plays in QuickTime and imports cleanly into editors.

Under the hood it uses only Apple frameworks:

- **ScreenCaptureKit** for display, window and region capture, system audio and microphone.
- **AVCaptureSession** for the webcam, and optionally the microphone.
- **CoreImage on Metal** for the webcam picture-in-picture overlay.
- **VideoToolbox via AVAssetWriter** for hardware H.264, HEVC or ProRes encoding into
  QuickTime/MP4.

There is no FFmpeg, no OBS and no Electron, and the binary is a few MB.

Requires macOS 15 or later on Apple Silicon. It was developed on macOS 27 with an M4 Pro.

## Build

```sh
./scripts/build.sh          # cargo build --release, assemble + sign dist/Rec.app
dist/rec --help
```

The script signs `dist/Rec.app` with your Apple Development identity, falling back to ad-hoc
signing. A stable signature keeps the macOS privacy grants in place across rebuilds. You can
override the identity with `REC_SIGN_IDENTITY="..."`. Put `dist/` on your `PATH` or symlink
`dist/rec` anywhere.

## Permissions (one-time)

```sh
dist/rec permissions --request
```

Then enable **Rec** under **System Settings → Privacy & Security → Screen & System Audio
Recording**. Microphone and camera are approved from the system prompts.

`rec` re-launches itself with "responsibility disclaimed". macOS therefore attributes the
permissions to the signed **Rec** app rather than to whatever terminal, tmux or IDE started it.
macOS may also ask you to confirm that Rec can "bypass the system private window picker". Click
**Allow**. The OS repeats this check periodically for every recorder, OBS included.

`dist/rec permissions` shows the current status.

## Record

```sh
# 1080p60 screen + system audio + default mic (Ctrl-C to stop)
dist/rec record

# 4K60 HEVC with the webcam in the corner, saved to a given file
dist/rec record -r 4k --webcam -o ~/Movies/demo.mov

# a specific window, at native resolution, for 30 seconds
dist/rec record --window "Safari" -r native -t 30

# stop a recording from another shell / hotkey (prints the file path)
dist/rec stop
```

The default output is `~/Movies/rec-YYYY-MM-DD-HHMMSS.mov`. A live status line shows frame
counts and peak levels for system audio and mic.

| Option | Default | Notes |
|---|---|---|
| `-r, --resolution` | `1080p` | `720p`, `1080p`, `1440p`, `4k`, `native`, or `WxH`. Presets produce exact 16:9 frames, pillar-boxed if the display is not 16:9. |
| `--keep-aspect` | off | Shrink the frame to the display's aspect ratio instead of pillar-boxing. |
| `-f, --fps` | `60` | Any divisor of 60000 (24, 25, 30, 50, 60, 120). |
| `--codec` | auto | `h264` up to 1080p, `hevc` above. `prores` gives ProRes 422 in `.mov`. |
| `--bitrate` | auto | Mb/s. Defaults to about 20 Mb/s for 1080p60 H.264 and about 50 Mb/s for 4K60 HEVC. |
| `-d, --display` | `0` | Display index from `rec devices`. |
| `--window` | | Window ID or app/title substring from `rec devices`. |
| `--region X,Y,W,H` | | Crop to a display region, in points from the top-left. |
| `--mic DEVICE` / `--no-mic` | default mic | Unique ID or name substring. |
| `--no-system-audio` | | |
| `--audio-tracks` | `both` | `both` writes the mix plus system and mic stems. `mix` writes the mix only. `stems` writes system and mic only. |
| `--system-gain`, `--mic-gain` | `0` | dB, applied to the mix track. |
| `-w, --webcam [DEVICE]` | off | Picture-in-picture overlay. Defaults to the system-preferred camera. |
| `--pip-size`, `--pip-corner`, `--pip-shape`, `--no-mirror` | `0.22`, bottom-right, rounded | The shape can be `rounded`, `circle` or `rect`. |
| `--no-cursor`, `--clicks` | cursor shown | `--clicks` highlights mouse clicks. |
| `-t, --duration` | | Stop after N seconds. |
| `--mic-backend` | `sck` | `sck` uses ScreenCaptureKit. `avcapture` uses AVCaptureSession. |
| `--test-pattern [--noise]` | | Synthetic source for self-tests. Needs no screen permission. |

Run `scripts/selftest.sh` to record and verify a clip through every pipeline. Add
`--no-screen` to skip the cases that need screen access.

Other commands:

- `rec devices` lists displays, windows, cameras and microphones.
- `rec probe FILE` checks a recording with AVFoundation, QuickTime's own engine. It reports
  playability and track layout, and decodes every video frame and audio sample.

## Output format

| Track | Contents |
|---|---|
| 1 video | H.264 High, HEVC Main (`hvc1`) or ProRes 422. Constant 60 fps, BT.709 tagged. |
| 2 audio | **Mix** of system and mic. AAC-LC 48 kHz stereo at 256 kb/s. Enabled by default, so players and uploaders use it. |
| 3 audio | System audio stem. AAC 48 kHz stereo. |
| 4 audio | Mic stem. AAC 48 kHz mono. |

With only one audio source enabled, the file has a single audio track. The three audio tracks
share one alternate group with the mix as default. QuickTime therefore plays only the mix, while
editors still see every track.

- **Constant frame rate.** ScreenCaptureKit only sends frames when pixels change. `rec` emits on
  an exact 1/fps grid and repeats the newest frame when needed, as OBS does. This keeps editors
  such as Premiere, Resolve and Final Cut in sync over long recordings.
- **Shared clock.** Screen, system audio, mic and webcam are all stamped on the host clock.
  Audio is never resampled in time, so it cannot drift from video.
- **Webcam sync.** Video emission runs slightly behind real time so the webcam frame captured at
  each instant is used. Webcams deliver frames about 70–90 ms late, and the lag adapts to the
  slowest source seen. Set `REC_DEBUG=1` to print measured latencies.
- **Crash-safe.** Movie fragments are written every 5 s, so a crash or power loss leaves a
  playable file up to the last fragment. A normal stop rewrites the file as a regular movie,
  laid out as `ftyp wide mdat moov`.

## Feature comparison with OBS (macOS)

| Feature | OBS 31 | rec |
|---|---|---|
| Full-display capture | ✅ | ✅ ScreenCaptureKit |
| Window capture | ✅ | ✅ `--window` |
| Region / crop | ✅ crop filter | ✅ `--region` |
| Application capture (all windows of an app) | ✅ | ❌ |
| Several displays or sources at once, scenes, transitions | ✅ | ❌ one source plus webcam |
| System audio | ✅ | ✅ |
| Microphone | ✅ | ✅ through ScreenCaptureKit or AVCapture |
| Webcam overlay | ✅ free layout | ✅ corner picture-in-picture: rounded, circle or rect, mirrored |
| Separate audio tracks | ✅ up to 6 | ✅ mix plus system and mic stems |
| Mixer gain | ✅ | ✅ dB per source |
| Audio filters (noise suppression, compressor) | ✅ | ❌ |
| Hardware H.264 / HEVC | ✅ | ✅ |
| ProRes | ✅ | ✅ 422 |
| 10-bit / HDR capture | ✅ | ❌ 8-bit SDR, BT.709 |
| Constant frame rate output | ✅ | ✅ |
| Crash-safe recording | ✅ with fragmented MP4/MOV | ✅ fragments every 5 s, then finalized |
| Cursor hide, click highlight | partial | ✅ both |
| Pause / resume | ✅ | ❌ |
| Replay buffer | ✅ | ❌ |
| Streaming (RTMP, WHIP) | ✅ | ❌ |
| GUI, preview, hotkeys | ✅ | ❌ CLI only. `rec stop` can be bound to a hotkey. |
| Live audio meters | ✅ | ✅ peak dBFS in the status line |
| Footprint | about 300 MB app | a few MB binary, about 30% of one core at 4K60 with webcam |
| Permissions attributed to the recorder itself | ✅ app bundle | ✅ disclaimed re-launch of signed `Rec.app` |

## Known gaps

- There is no GUI or live preview. It is a CLI, so global hotkeys need an external tool such as
  Shortcuts, Raycast or skhd running `rec stop`.
- There are no scenes, no multi-display capture and no capture of every window of an app.
- There is no pause/resume, replay buffer or streaming.
- There are no audio filters. The mic is recorded as the device delivers it.
- Capture is 8-bit SDR only. HDR displays are tone-mapped by ScreenCaptureKit.
- The webcam is a fixed-corner overlay, and it is burned into the video track. There is no
  separate webcam track or file.
- A webcam overlay switches the pipeline from 4:2:0 to BGRA plus CoreImage compositing. This
  costs more memory bandwidth but held up at 4K60 in testing.
- Mic sample-rate conversion for the mix track uses linear interpolation per buffer. The mic stem
  is converted by AVFoundation and is unaffected.
- macOS periodically re-asks you to confirm screen-recording access for apps that bypass the
  system picker. This applies to every recorder.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI |
| `src/record.rs` | Session orchestration, pacing thread, signals, `rec stop` |
| `src/screen.rs` | ScreenCaptureKit stream, displays and windows |
| `src/camera.rs` | Webcam and AVCapture mic, device enumeration |
| `src/compose.rs` | CoreImage webcam overlay |
| `src/mixer.rs` | Real-time audio mixer |
| `src/writer.rs` | AVAssetWriter sink, constant-frame-rate pacing, track layout |
| `src/tcc.rs` | Permissions and responsibility disclaim |
| `src/probe.rs` | AVFoundation verification |
| `src/synth.rs` | Test-pattern source |
| `support/Info.plist` | Usage strings and bundle ID, also embedded into the binary |
| `scripts/build.sh` | Build, bundle and sign |
