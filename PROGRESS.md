# PROGRESS

## Your fix list (2026-10-12 evening): done

Every item was measured on real recordings in `~/Movies/rec-rs-tests/goal/` (camera clips
deleted after checking).

| Item | Cause | Fix | Check |
|---|---|---|---|
| Black bars and corners | `-r 1080p`/`4k` were exact 16:9 frames, so the 1.547 display was pillarboxed (1920x1080 with black sides). Window captures arrive with transparent rounded corners, which encoded as black. The preview drew a rounded image over black. | Every size keeps the source's shape (1080p = 1670x1080 here, 4K = 3342x2160), even, never upscaled. Scaler and compositor fill edge to edge. Window corners are filled with the window's edge color. Preview drawn square. | Display at 720p/1080p/4K/native and windows at 720p/1080p/native: 0% black pixels on every edge and corner (was 100% on the sides). Preview reaches all four corners. |
| Green bar on the left | The Lanczos scaler read zeros past the source's edge: Cb = Cr = 0 is green. | Clamp edge mode. | Column 0 of a 1080p recording: (25,42,23) before, neutral (42,42,45) after. |
| Camera darker | Not the color path (camera 178.2 -> file 178.2). Each recording restarted the camera: 0.5 s of black, then a 2.5 s exposure ramp (Y 117 -> 164). | The warm camera passes preview -> recording -> preview. A cold camera gets 2 s before the file starts. | App recording: camera Y 149 from the first frame, flat. CLI cold start: 147 (was 16, then 117). |
| Audio quality | Per-buffer linear resampling for non-48 kHz devices (a buzz at buffer edges, aliasing); hard clipping of the mix; live mix ignored the gains; mic stem 128 kb/s, live 160 kb/s. | Streaming windowed-sinc resampler; -1 dBFS limiter; gains in the live mix; mic stem 192 kb/s, live 192 kb/s, live encoder at max quality. | Reference sweep/tones/noise as system audio: flat within 0.1 dB, 50 Hz-20 kHz, no clicks; mix with +6 dB gain peaks at -0.3 dBFS, 0 clipped samples. |

Notes:

- The capture's own system-audio path was already clean. The mic is the raw built-in mic: no
  noise suppression, and it picks up the speakers.
- macOS reset the camera grant; I requested it once at 18:40 and it was approved.
- Your saved settings are 4K: recordings are now 3342x2160 instead of 3840x2160 with bars.

## Dark preview (your bug report, 2026-10-12 ~11:30): fixed

- **Cause.** The new zero-copy preview texture was created as `BGRA8Unorm_sRGB`. egui treats
  texture values as gamma-encoded, so the GPU's sRGB-to-linear decode on sampling darkened
  everything. Mid-gray 128 showed as 47, and midtones were about 75% too dark. It is now a plain
  `BGRA8Unorm`, guarded by a unit test.
- **Verified against a native screenshot** of a test card (`scripts/testcard.swift ... screen
  front`) previewed by the app:

  | | Native 64 / 128 / 192 shown as | Mean luma |
  |---|---|---|
  | Before | 18 / 47 / 116 | −76% |
  | After | 64 / 127 / 192 | −0.1% |

- **The recording was not affected.** Its pixels match the screen exactly (BT.709 decode: 64 /
  128 / 191, midtones +0.2%; range, matrix and scaler are correct). QuickTime shows screen
  recordings about 9% lighter in the midtones, because it applies the BT.709 curve to
  sRGB-encoded screen pixels. That is the same for any recorder that tags BT.709. Tagging the
  sRGB curve instead would make QuickTime exact, at some risk with editors, so it's left as it
  was. Say if you want it.

## 4K quality: sharp preview, native capture, high-quality streaming (your request, 2026-10-12): done

All measured with SSIM-Y / PSNR-Y (dB) / VMAF against lossless references.
Details are in [PERFORMANCE.md](PERFORMANCE.md#picture-quality-2026-10-12).

- **Recording captures at native resolution.** A 5K display is captured at 5120x2880 and
  scaled to 4K by a Metal Lanczos filter, per YCbCr plane, with no copies (ScreenCaptureKit's own
  scaling was soft):

  | Output | Before | After |
  |---|---|---|
  | 4K | 0.9948 / 35.0 / 90.2 | 0.9971 / 40.9 / 100 |
  | 1080p | 0.9948 / 35.3 / 88.8 | 0.9934 / 39.6 / 98.5 |

  That's on ProRes, so the scaler is measured. Default encodes: 4K HEVC 42.7 dB / VMAF 100. It
  costs about 3% of a core.
- **Preview is pixel-sharp.** It is scaled to the preview's exact pixel size (2x on Retina) and
  drawn 1:1 from the frame's IOSurface, with no 960 px proxy and no CPU copies. Measured on its
  pixels:

  | Window | Before VMAF | After VMAF |
  |---|---|---|
  | Default | 45.9 | 94.0 |
  | Large | 16.6 | 94.2 |

  - CPU per frame dropped from 1.3–1.9 ms to 0.25–0.39 ms.
  - The UI and preview are color-managed (sRGB, so they're no longer oversaturated on P3
    displays).
  - Settings has a low-power preview option.
  - The app no longer burns a core while hidden or locked (100% → 2%).
- **4K streaming.**
  - **Codec:** HEVC Main10 via enhanced RTMP (YouTube) or WHIP H.265, falling back to H.264 per
    server.
  - **Bitrate presets:** YouTube 4K60 45 Mb/s H.264 or 35 HEVC; Twitch capped at 6 Mb/s and
    1080p.
  - **Encoding:** keyframes every 2 s, BT.709 limited-range tags. A smaller live size is scaled
    with the Lanczos scaler.
  - **Static-text 4K stream:** 0.9942 / 33.5 / 87.6 before, 0.9974 / 41.8 / 100 after
    (H.264 45 Mb/s) and 0.9947 / 41.6 / 100 (HEVC).
  - **4K60 motion:** VMAF 76 at the old 6 Mb/s, 94.4 at 45 Mb/s H.264, 95.3 at 35 Mb/s HEVC.
- **New tools.**
  - `scripts/testcard.swift` draws a static 5K text card.
  - `scripts/quality.sh` scores recordings and streams.
  - `scripts/encquality.sh` scores the encoder on motion.
  - `preview_pixels` is an ignored test that measures the preview.
- **Not verified.**
  - **An on-screen screenshot of the new preview.** Your screen locked during the run and the 5K
    displays were then disconnected, so the preview was measured on the exact pixels its texture
    holds. The app self-test confirms it renders frames.
  - **Streaming HEVC to YouTube itself.** It was tested against a local MediaMTX, which accepted
    enhanced-RTMP HEVC and WHIP H.265.
  - **4K streaming in the app needs the 4K output setting.** The live encode is never larger
    than the recording size.

## Pause/resume check, replay buffer and streaming (your request, 2026-10-11 ~12:30): done

- **Pause/resume in the app: verified.** In a scripted run with a 1.5 s pause, the file is
  3.17 s long against 4.66 s of wall time, and it matches the recording clock. The pause is cut
  from the file and audio stays in sync. `scripts/selftest.sh` now asserts this, so no fix was
  needed.
- **Replay buffer.**
  - Keeps the last 15–300 s (default 60) of a shared live encode in memory, starting from a
    keyframe (forced every 2 s by default, configurable 0.5–4 s).
  - Save it without stopping in any of these ways:
    - the app's **Save Replay** button (with the memory in use);
    - **⇧⌘S** from anywhere, held only while the buffer is on;
    - the menu bar menu;
    - `rec-rs save-replay` (the CLI flag is `--replay SECS`).
  - Works with or without a recording, and keeps running with the window closed.
  - Clips are H.264 + AAC MP4, and both AVFoundation and ffmpeg measure them at **0.0 ms**
    A/V offset.
  - Memory: about 45 MB for 60 s at 6 Mb/s.
- **Streaming.**
  - **Destinations:** RTMP/RTMPS (YouTube, Twitch, or a custom URL) and WHIP (WebRTC), several
    at once from **one** VideoToolbox H.264 encode, shared with the replay buffer. Audio is AAC
    (RTMP) or Opus (WHIP).
  - **Encode settings:** bitrate, keyframe interval, CBR/VBR, resolution and audio bitrate.
  - **Resilience:** auto-reconnect with backoff, and a live status showing state, bitrate,
    dropped frames and duration.
  - **Stream keys:** stored in the Keychain (`rec-rs stream-key DEST`, or the app's key field),
    never in settings, and redacted from every log line and error.
  - **Recording at the same time:** recording and streaming run together. Starting or stopping a
    recording doesn't interrupt the stream (short capture gaps are bridged).
- **Tested against a local MediaMTX** (installed with brew; all its files are under
  `~/Movies/rec-rs-tests/`):

  | Check | Result |
  |---|---|
  | RTMP | 1080p60 H.264 High + AAC, **+0.2 ms** in the server's recording |
  | RTMPS | Self-signed cert (`REC_INSECURE_TLS=1`, for local tests only) |
  | WHIP | H.264 + Opus, **−3 to −13 ms** (6.5 ms of that is the Opus pre-skip MediaMTX writes into its MP4) |
  | RTMP + WHIP | Both at once from one encode |
  | Server restart | Reconnects in about 2–6 s for both |
  | App | Stream and replay survive window close and a recording start/stop with no gap |

- **Bugs found and fixed along the way.**
  - The writer could show frames early when its queue overflowed under load, and one latency
    spike kept output delayed for the rest of a session.
  - The live encoder session is now created off the capture path.
  - MediaMTX treats the first packet of each WebRTC track as simultaneous, so WHIP now starts
    aligned.
  - The CLI printed "saved" with `--no-record`.
- **Tests.** `cargo test` passes 48 of 48. `scripts/selftest.sh` passes 17 of 17, including new
  replay, stream-rtmp, stream-whip and gui-replay cases (the stream cases need `mediamtx`).
- **Not tested: streaming to the real YouTube or Twitch** (no keys here). They use the same
  RTMPS path as the local test. A first real stream is worth a short private test. To set up:
  1. Run `rec-rs stream-key youtube`, or paste the key in the app.
  2. Choose YouTube and click Go Live.
- ⇧⌘S overrides Save As in other apps only while the replay buffer is on.

## Movable camera overlay (your request, 2026-10-11 ~11:00): done

- **Move.** In the Screen + Cam scene, drag the camera in the live preview to any position. It
  snaps to a corner when released within 5% of one, and the ↖ ↗ ↙ ↘ buttons snap it directly.
  A corner-snapped overlay stays in its corner when resized. The position persists in
  settings (`pip_place`) and goes into the recording.
- **Shapes.** Circle, rounded rectangle, square, pill and squircle (a superellipse), shown as
  icon buttons.
- **Style.** A roundness slider (0–100%) for rounded, square and squircle, plus size (10–50%),
  border width and color, drop shadow and mirror.
- **Live everywhere.** Changes apply in the preview and while recording, without restarting
  capture. The CoreImage compositor bakes them into the file with no extra encode. The mask,
  border and shadow are built once per size and style on the CPU (anti-aliased) and reused
  every frame; dragging only changes a transform.
- **CLI.** `--pip-pos X,Y`, `--pip-shape circle|rounded|square|pill|squircle`,
  `--pip-roundness`, `--pip-border`, `--pip-border-color`, `--pip-shadow`.
- **Tests.**
  - 11 new unit tests cover placement, clamping, snapping, resize-in-corner, every shape's
    coverage, border inset, shadow, CLI parsing and settings round trips.
  - A CoreImage render test bakes an overlay with border into a frame and reads pixels back:
    camera in the center, border at the edge, rounded corner showing the screen, and a live
    move.
  - The app self-test now checks that an overlay change applies without restarting capture.
  - `cargo test` passes 29 of 29 and `scripts/selftest.sh` passes 13 of 13.
- **Not verified with your face on camera.** The lid was closed during testing, so the
  built-in camera delivered no frames. I checked a 30% squircle with a red border and shadow at
  (0.4, 0.45) through the "Boom Camera" virtual camera instead, both in a recording and in the
  app preview. A physical drag with the mouse is also untested: dragging uses the same code
  path as the self-test, but no person has dragged it yet.

## Status (2026-10-11 ~10:25 EDT)

All 7 milestones of the goal are built and verified by automated tests on this MacBook Pro.
`rec-rs` records 1080p60 or 4K60 screen + mic + system audio + webcam into a `.mov`/`.mp4` that
AVFoundation (QuickTime's engine) and ffmpeg both play and decode completely.

Two checks still need a person, because they need eyes, ears or a physical keypress:

1. Press ⇧⌘R for real.
2. Open a recording in QuickTime Player's window and import it into DaVinci Resolve.

See "Your manual check" below.

**Try it:**

```sh
./scripts/build.sh
open dist/Rec.app                                  # app: preview, sources, ⇧⌘R, pause, meters, settings
dist/rec-rs record --display 1 --mic default --system-audio --cam --out ~/Movies/demo.mp4 --duration 10
```

## Goal milestones

The goal text was truncated when first pasted. The milestone labels below now follow the full
text (`~/.cache/rec_goal_full.txt`), read on 2026-10-11. Everything listed was re-verified on
2026-10-11 after the latest changes.

| # | Milestone (from the goal) | Status | Evidence |
|---|---|---|---|
| Step 1 | Study OBS (source and REA on OBS.app), write `analysis/OBS_ARCHITECTURE.md`, document the license choice | **Done** | Doc written from the source at commit `7d98beb`, cloned to `~/re/obs-studio`. REA's `inspect-plist` and `inspect-signature` ran; `rea analyze` needs a Ghidra, Hopper or IDA backend, which isn't installed, so the doc falls back to `otool` and `codesign`. Clean-room implementation, MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). |
| 1 | Display or window capture via ScreenCaptureKit at native Retina resolution, 30/60 fps, zero-copy IOSurface/CVPixelBuffer | **Done** | `--display N`, `--window`, `-r native` (4112×2658 on this Mac), `--fps 30/60`. SCK surfaces go straight to the encoder. |
| 2 | VideoToolbox H.264 and HEVC (ProRes optional), configurable bitrate/quality, constant frame rate, no dropped frames | **Done** | `--codec h264/hevc/prores`, `--bitrate`. Frame gaps measured at exactly 16.667 ms; 0 dropped frames in every test, including the 30-minute soak. |
| 3 | Mic plus system audio as separate AAC tracks plus a mixed track; A/V sync from a single clock with capture timestamps, verified by a clap/flash sync test | **Done** | Mix plus two stems. Flash/beep sync test (`scripts/synccheck.py`) measures **0.0 ms** offset, also across pauses. A real-device clap test by a person is optional (see below). |
| 4 | MP4/MOV via AVAssetWriter, crash-safe, Final Cut/Premiere/DaVinci compatible | **Done; editor import is a manual check** | Fragments every 2 s; SIGKILL leaves a playable file. Output is CFR, BT.709, `hvc1`, standard AAC, which is what editors want. Not yet opened in Resolve by a person. |
| 5 | Webcam PiP (corner, circle or rounded mask) composited on the GPU; scenes: full screen, screen + cam, cam only | **Done** | CoreImage on Metal. `--scene screen/screen-cam/cam`, switchable live in the app. The overlay is now draggable, with five shapes, roundness, border and shadow (see above). |
| 6 | Native UI in paper-rs style (transparent titlebar, Inter, egui) with menu bar icon: pick sources, live preview, start/stop with ⇧⌘R, pause/resume, timer, level meters, settings window; plus a headless CLI | **Done, except the physical ⇧⌘R keypress** | egui/wgpu windows with the paper-rs theme. A scripted app session tests preview, record via the hotkey's command, pause/resume, stop and save, the settings window, and closing the window. The exact CLI line from the goal works and yields 600 frames and 10.000 s. |
| 7 | Performance and reliability: about 0 dropped frames at 4K60, low CPU, 30-minute soak, ffprobe numbers in PERFORMANCE.md | **Done** | [PERFORMANCE.md](PERFORMANCE.md). Soak 1 (30 min, 4K60, screen + system audio + mic + webcam): 107,955 of 108,000 frames; the 45 drops all came from one 5-s encoder stall, and the writer now retries instead of dropping. Soak 2 on the fixed build (4K60, screen + system audio): **108,000 of 108,000 frames, 0 dropped**, exact 16.667 ms spacing, 6% CPU, flat memory. In both, all tracks ended within 0.1 ms. Soak 3 (webcam, fixed build) was cut by the lid closing at 10:44 (21.7-s display reconfiguration, camera unavailable); the file stayed valid and in sync. |

Tests: `cargo test` runs 18 unit tests, and `scripts/selftest.sh` passes 13 of 13 with all
permissions granted. The suite covers:

- every pipeline;
- sync, audio gaps and pause;
- Dock quit;
- the scripted app session.

**Dock icon (your request, 2026-10-11).** `support/AppIcon.icns` is copied into
`Contents/Resources`, and the icon is also set at launch. Verified:

- The app runs as a Foreground app with your icon in the Dock (checked with a Dock capture).
- The main window opens at launch.
- Clicking the Dock icon reopens a closed window.
- Dock Quit, ⌘Q and logout finalize a running recording first. Self-test `app-dock-quit`
  passes.
- Your pinned Dock tile showed the generic icon until the app ran, because the Dock had cached
  it. It shows your icon while Rec runs; after your next login it shows it at all times.

## Permissions

| Permission | Status |
|---|---|
| Screen & System Audio Recording | granted to **Rec** |
| Microphone | granted (re-approved by you after the 09:12 request) |
| Camera | granted (macOS reset it; requested once on 2026-10-12 at 18:40 and approved) |

- **Grant history.** The first request ran on 2026-10-10 at 23:32. All three grants survived
  renaming the executable and every rebuild until the Dock-app change. After macOS reset the mic
  and camera grants, I ran the request once more at 09:12. Both are granted again as of about
  10:00. Between 08:35 and 09:10 macOS had reset them, probably because the bundle changed from a
  menu bar-only app to a Dock app (`LSUIElement`). If they reset again, run
  `dist/rec-rs permissions --request`.
- **Picker prompt.** If macOS shows "Rec is requesting to bypass the system private window
  picker", click **Allow**. macOS repeats this periodically for every recorder.
- **System Events prompt.** If you see a prompt asking whether a terminal process may **control
  "System Events"**, click **Don't Allow**. On 2026-10-11 at about 08:32 I ran a simulated
  keystroke again by mistake, the same thing I had already documented as not to do. It was
  killed within seconds. Rec never needs that permission.

## Your manual check (about 3 minutes)

1. Run `open dist/Rec.app`. The Rec window opens with a live preview. Press **⇧⌘R** in any app:
   the menu bar item turns red with a timer. Click **Pause**, then **Resume**, and press
   **⇧⌘R** again to stop. Then click **Reveal Last**.
2. Open that file in **QuickTime Player** and drag it into **DaVinci Resolve**. Expect one video
   track and three audio tracks: the mix, system audio and mic.
3. Optional real-world clap test: choose the **Screen + Cam** scene, clap on camera, then step
   through the frames around the clap.

## Verification log (latest)

| Test | Result |
|---|---|
| Goal CLI line (`--display 1 --mic default --system-audio --cam --out file.mp4 --duration 10`) | 1080p60 H.264 with 600 frames and 10.000 s; 3 AAC tracks at 10.000 s |
| Pipeline sync (test pattern flashes and beeps at the same host time) | 8/8 flashes paired, offset **0.0 ms** in the mix and system tracks |
| Sync across 2 pauses | 0.0 ms in the mix and system tracks; output exactly the requested duration |
| Pause/resume via `rec-rs pause/resume` | Paused wall time removed from the output; all tracks end within 0.1 ms of each other |
| Camera-only scene | Webcam full frame, cropped to fill, mirrored |
| App, scripted (`REC_GUI_SELFTEST=1`) | Preview frames shown; recording; paused=true; preview during recording; menu bar title " 00:03"; saved and probed; settings window opens; preview stops when the window is hidden |
| App, visual check (captured with rec-rs's own window capture) | Toolbar under a transparent titlebar, live preview, scene picker, sources, meters. Rec's window is absent from its own preview. |
| `scripts/selftest.sh` | 11/11 pass |
| 30-minute soak at 4K60 HEVC (screen, system audio, mic, webcam) | 107,955 of 108,000 frames (one 5-s encoder stall, now handled); A/V end within 0.1 ms; CPU 34%; memory flat. See PERFORMANCE.md. |
| Encoder stall handling | Single 4K60 noise stream: 0 retries, 0 drops. Overload (2–3 streams): files stay valid and timed, with drops only past 0.5 s of backlog. |
| Audio gaps (every 50th buffer dropped) | 0.0 ms sync; 20 gaps filled with silence |
| Sync repeatability after the fix | 0.0 ms in 11 of 11 runs; just before the fix, 2 of 3 runs showed about -10 ms |
| Dock: launch, icon, window, quit | Foreground app with your icon; window opens; quit via `terminate:` mid-recording saves a playable file |

## Known gaps

These are tracked in the README under "Known gaps".

- **Scenes.** There are three fixed scenes, with no layout editor or transitions.
- **Missing features.** There is no application capture or audio filters, and the hotkeys
  are fixed. Streaming is RTMP/RTMPS and WHIP only (no SRT).
- **8-bit SDR only.**

## Log

- **2026-10-10 23:30–00:10:** CLI recorder.
  - Built ScreenCaptureKit capture, the constant-frame-rate writer, mixer and stems, webcam
    overlay and crash-safe fragments.
  - Ran the permissions request once, and added the self-test.
- **2026-10-11 00:30–01:10:** First menu bar app (AppKit) with preview and ⇧⌘R. Fixed the TCC
  disclaim check.
- **2026-10-11 08:00–09:10:** Re-read the full goal; the first paste had been truncated.
  - **Housekeeping.** Moved build output to `~/.cache/rec-rs-target` and test media to
    `~/Movies/rec-rs-tests`, since both had been inside the synced repo. Added the
    MIT/Apache-2.0 license.
  - **OBS study.** `analysis/OBS_ARCHITECTURE.md`, plus adopted takeaways: App Nap off,
    latency-critical activity, SCK frame interval at 0.9× the period, 2 s fragments.
  - **Recording features.** Scenes; sample-accurate pause/resume; exact `--duration`; `rec-rs`
    CLI with the goal's flags and `pause`/`resume` commands.
  - **Sync test.** Flash/beep test pattern and `synccheck.py`.
  - **App rebuilt.** Replaced the AppKit menu bar app with the egui app: main window, settings
    window, menu bar item and meters.
  - **Milestone 7.** Soak script, 30-minute soak and PERFORMANCE.md.
- **2026-10-11 09:35–10:25:**
  - Soak 2 came back clean.
  - Crash test passed with 2 s fragments.
  - An end-to-end sync test through AVPlayer found system audio about 25 ms early. The cause is
    ScreenCaptureKit capturing audio at mix time; the fix shifts it by the output latency.
  - Mic and camera are granted again, and the self-test passes 13 of 13.
  - Soak 3 (the full webcam setup) ran on the fixed build. It was disturbed when the lid was
    closed at 10:44 (clamshell mode), which made screen frames freeze for 21.7 s and the built-in
    camera disappear. The file stayed valid and in sync.
- **2026-10-11 09:05–09:35:** Your Dock icon, with the Dock quit and reopen hooks.
  - The soak's 45 drops led to the writer retrying a busy encoder instead of dropping frames.
  - While measuring I found and fixed an intermittent -10 ms audio shift (lost or early audio
    buffers). The writer now fills audio gaps with silence and trims overlaps.
  - macOS reset the mic and camera grants; re-requested once at 09:12, still pending.
  - Started soak 2 on the fixed build.
- **2026-10-11 12:30–14:00:** Pause/resume verified in the app; replay buffer; RTMP/RTMPS and
  WHIP streaming from one shared encode, with Keychain keys, reconnects and live status; app UI,
  ⇧⌘S and menu items; tested against a local MediaMTX.
- **2026-10-12 08:30–11:00:** 4K quality: native capture with Metal Lanczos scaling; HEVC
  (enhanced RTMP, WHIP) and platform presets for streams; pixel-sharp zero-copy preview; sRGB
  color management; idle while hidden. Measured with SSIM/PSNR/VMAF.
