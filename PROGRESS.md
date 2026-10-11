# PROGRESS

## Status (2026-10-11 ~00:10 EDT): CLI recorder working end to end; GUI milestone in progress

`rec` records a 1080p60 or 4K60 screen, system audio, mic and webcam overlay into one `.mov`.
AVFoundation (QuickTime's engine) reports every file playable and decodes every frame and audio
sample. ffmpeg also decodes them cleanly.

**Try it:**

```sh
./scripts/build.sh
dist/rec record -r 4k --webcam            # Ctrl-C (or `dist/rec stop`) to finish
open -a "QuickTime Player" ~/Movies/rec-*.mov
```

A 10-second reference clip is at `analysis/raw/sample-4k60.mov`. It contains 4K60 HEVC, the
webcam overlay, a mix track, and system and mic stems.

## Goal milestones: plain status

**Caveat:** the original task message arrived truncated. It began mid-sentence at "...est once,
write clearly in PROGRESS.md what to approve...", so I never saw the numbered milestone list.
Milestones 1–5 below are inferred from the stated "done" condition, not copied from your list.
Milestone 6 is as you described it on 2026-10-11.

| # | Milestone | Status |
|---|---|---|
| 1 (inferred) | Permissions requested once and documented, with a stable signed identity | **Done** |
| 2 (inferred) | Screen capture at 1080p60 and 4K60, hardware H.264/HEVC, constant 60 fps | **Done**, verified |
| 3 (inferred) | System audio and mic, mix track plus stems, in sync | **Done**, verified by tooling; no human clap test yet |
| 4 (inferred) | Webcam overlay | **Done**, verified |
| 5 (inferred) | Plays in QuickTime and imports into an editor; README with OBS table and gaps | **Partly done**. AVFoundation and ffmpeg verified. Not yet opened in QuickTime's window or imported into Resolve by a person. |
| 6 | GUI with preview, menu bar icon, Cmd+Shift+R hotkey | **Not done** as of the first report. In progress now, see below. |

## Permissions: all granted

| Permission | Status |
|---|---|
| Screen & System Audio Recording | granted (enabled for **Rec** in System Settings) |
| Microphone | granted |
| Camera | granted |

The request ran once, on 2026-10-10 at 23:32. Mic and camera were approved from the prompts, and
screen recording was switched on in System Settings at about 23:42.

- If macOS shows **"Rec is requesting to bypass the system private window picker…"**, click
  **Allow**. macOS repeats this periodically for every recorder that skips the system picker.
- The grants are tied to the signed `com.heyfabrika.rec` bundle, which uses the Apple
  Development identity. They survived several rebuilds. Signing with a different identity would
  reset them.

## Your manual check (2 minutes)

1. Open `analysis/raw/sample-4k60.mov`, or a fresh recording, in **QuickTime Player**. The
   video, webcam corner and both voice and system sound should play together.
2. Drag the same file into **DaVinci Resolve**, which is installed (the App Store edition).
   - Expect one video track and three audio tracks: the mix, system audio and mic.
   - Resolve's App Store edition has no external scripting. I did not drive its GUI while you
     were using the Mac, so this import has not been exercised in Resolve itself.
3. Optional sync check: record yourself clapping on camera with `--webcam`, then step through
   the frames around the clap.
   - The design keeps every source on the host clock, and the overlay waits out the camera's
     measured ~70–90 ms delivery delay.
   - An end-to-end clap test has not been done by a human yet.

## Verification log

| Test | Result |
|---|---|
| 1080p60 H.264, screen and system audio | 305/305 frames at exactly 1/60 s spacing. BT.709 tagged. ffmpeg decodes cleanly. |
| 4K60 HEVC (`hvc1`), screen, system audio, mic and webcam | 0 dropped frames. About 30% of one core. |
| Stress: 4K60 HEVC, noisy test pattern, webcam and mic | 902 frames, 0 dropped, about 100 Mb/s. |
| 3-minute soak at 4K60 with webcam | 10,806 frames, 0 dropped. Audio and video end within 1 ms, so no drift. Memory plateaued at 100–165 MB with no leak. |
| Mix track vs stems | Cross-correlation lag of 0–1 samples at 8 kHz, about 0.1 ms. |
| Track layout | Mix enabled and default. Stems are in the same alternate group and disabled, so QuickTime plays only the mix. |
| Crash safety | After SIGKILL at 13 s, the first 10 s were playable. The fragment interval is now 5 s. |
| Stop paths | Ctrl-C/SIGINT, SIGTERM to the wrapper process, `rec stop` and `-t` all produce finalized files. |
| Window capture, region capture, ProRes 422, MP4 output | Each works and probes clean. |
| `scripts/selftest.sh` | 7/7 pass. It covers the test pattern, webcam plus mic, real screen, 4K and MP4. |

## Not done / known gaps

These are tracked in the README under "Known gaps".

- There is no GUI, live preview, pause/resume, scenes, audio filters or streaming.
- The webcam is burned into the video. There is no separate webcam track.
- Capture is 8-bit SDR only.
- The work is not committed to git. Nothing has been committed. Run `git add -A && git commit`
  when you are happy with it.

## Log

- 2026-10-10 23:30: Scaffolded the Rust project with objc2 bindings. Added the permissions
  module, embedded Info.plist, and a signed `dist/Rec.app` that disclaims responsibility.
  Permission request issued once.
- 23:35–23:45: ScreenCaptureKit capture, constant-frame-rate AVAssetWriter sink, webcam overlay
  with CoreImage, test-pattern source, AVCapture mic backend. Screen permission granted.
- 23:45–00:10: Real-time mixer with mix and stem tracks, window and region capture, `rec stop`,
  level meters, ProRes, atomic rebuilds, adaptive emission lag for webcam sync, self-test
  script, soak test, docs.
