# PERFORMANCE

All numbers come from this machine: a MacBook Pro with an M4 Pro, macOS 27, and a Retina
display at 4112×2658 pixels. Builds are release builds of `rec-rs`.

## How the numbers are measured

- **CPU and memory.** The recorder process's `ps` `%cpu` (100% = one core) and resident memory
  are sampled every 3–5 s (`scripts/soak.sh`). The figures cover the recorder only. The hardware
  video encoder and the WindowServer's capture work run outside the process and are not
  included.
- **ffprobe checks.**
  - Frame count against the expected `duration × 60`.
  - Every gap between consecutive video timestamps; at constant 60 fps each must be 16.667 ms.
  - Per-stream durations, which shows how far apart audio and video end.
- **AVFoundation.** `rec-rs probe` opens each file with QuickTime's engine and decodes every
  sample.
- **A/V sync** (`scripts/synccheck.py`). The test pattern flashes white and beeps at the same
  host-clock instant once a second. The script pairs each flash frame with the nearest beep
  onset in each audio track and reports the offset.

## 30-minute soaks (milestone 7)

| Run | Setup | Frames | Dropped | Frame gaps | A/V end | CPU mean / max | Memory |
|---|---|---|---|---|---|---|---|
| Soak 1, 08:34–09:04 | 4K60 HEVC, screen + system audio + mic + webcam overlay, 3 audio tracks | 107,955 / 108,000 | **45**, all in one 5-s window (see below) | 16.667 ms except 8 gaps of 100–133 ms at 13:27 | all 3 tracks end within 0.1 ms of the video, at 1800.000 s | 34.4% / 45.0% | 64–78 MB steady (max 151 MB at start), no growth |
| Soak 2 (after the fix), 09:29–09:59 | 4K60 HEVC, screen + system audio (mic and camera grants were reset; see PROGRESS) | *pending* | | | | | |

Both files played in AVFoundation (`playable: true`, every sample decoded) with no ffmpeg
decode errors.

**Soak 1 frame drops.** Every one of the 45 drops fell between 08:48:10 and 08:48:15, in pairs
about every 1.5 s. The other 29 min 55 s had none. During that window the hardware encoder
reported "not ready" (`isReadyForMoreMediaData == false`). Something else on the machine was
loading the media engine at the time; the Dock icon files were being created about then. The
writer used to drop a frame slot the moment the encoder was busy.

**Fix** (commit `fa747f4`). A busy encoder now makes the writer retry the same slot, with the
same timestamp, on the next pacer pass, about 8 ms later. A slot is dropped only once it is
more than 0.5 s overdue, which only happens under sustained overload. With one 4K60 stream at
worst-case content, the encoder was never busy: 0 retries and 0 drops (table below).

## Single-run measurements (60 s each, after the fix)

| Source | Resolution / codec | Frames | Dropped | Encoder-busy retries | CPU mean / max | Memory max |
|---|---|---|---|---|---|---|
| Real screen + system audio | 1080p60 H.264, 19 Mb/s | 3600 / 3600 | 0 | 0 | **5.9%** / 8.2% | 39 MB |
| Real screen + system audio | 4K60 HEVC, 50 Mb/s | 3600 / 3600 | 0 | 0 | **5.9%** / 6.2% | 40 MB |
| Real screen + system audio + mic + webcam overlay (soak 1) | 4K60 HEVC | — | — | — | 34.4% / 45.0% | 78 MB |
| Test pattern (software-drawn) + beep audio | 1080p60 H.264 | 3600 / 3600 | 0 | 0 | 12.7% / 14.3% | 53 MB |
| Test pattern | 4K60 HEVC | 3600 / 3600 | 0 | 0 | 27.1% / 28.8% | 107 MB |
| Test pattern with full-frame noise (encoder worst case, about 50 Mb/s) | 4K60 HEVC | 3600 / 3600 | 0 | 0 | 47.4% / 49.0% | 110 MB |

How to read these:

- **Screen-only capture is about 6% of one core at 4K60.** ScreenCaptureKit hands over
  IOSurface-backed frames, and rec-rs passes them to VideoToolbox without copying or converting
  them.
- **The webcam overlay is the expensive part, at about 34%.** Each output frame is composited by
  CoreImage on the GPU into a BGRA buffer, and the 4:2:0 conversion then happens in the encoder.
  This is still far below OBS, which renders every frame through its own GPU pipeline and then
  copies it back to the CPU before encoding (`analysis/OBS_ARCHITECTURE.md`, section 6).
- **Test-pattern CPU is mostly the pattern itself,** which is drawn in software at 4K60. It is
  a stress source, not a measure of the recorder's own cost.

## Overload behaviour (deliberate)

Several simultaneous 4K60 recordings of full-frame noise exceed what the hardware HEVC encoder
can sustain.

| Concurrent 4K60 noise recordings | Frames written per stream (of 1200 in 20 s) | Notes |
|---|---|---|
| 2 at default bitrate | 1042–1043 (13% dropped) | about 1600 retries each before slots aged past 0.5 s |
| 3 at 150 Mb/s | 598–603 (50% dropped) | |

In both cases the files stayed valid and correctly timed:

- Durations were exactly 20.000 s.
- Timestamps never went backwards.
- Audio stayed in sync, because it does not depend on video.

Under overload, frames are dropped at the 0.5 s limit rather than letting latency and memory
grow without bound.

## A/V sync

| Test | Result |
|---|---|
| Test pattern, 8–30 s, mix and system tracks | Offset **0.0 ms** on every flash, in every run, after the fix |
| Sync across two pauses | 0.0 ms |
| Every 50th audio buffer dropped (a simulated glitching device) | 0.0 ms. The 20 gaps were filled with silence, so audio stays on the capture clock. |
| Soak 1, 30 min, real devices | All audio tracks end within 0.1 ms of the 1800.000 s video, so there is no drift |

**Bug found while measuring, now fixed** (commit `fa747f4`). AVAssetWriter lays audio buffers
end to end and ignores gaps in their timestamps. A device buffer that arrived before the first
video frame, or one that was lost, therefore shifted all later audio earlier by its length (10
ms). This was seen intermittently as -10 ms. The writer now tracks each audio stem's written
position and fills gaps with silence or trims overlaps to the exact sample, so the error stays
under 1.5 ms. Audio that arrives before the first frame is held and trimmed rather than dropped.

## Reproduce

```sh
scripts/soak.sh 1800                    # 4K60 + webcam + mic + system audio, 30 min
scripts/soak.sh 1800 -r 4k --no-mic     # screen + system audio only
scripts/selftest.sh                     # includes sync, pause, audio-gap and app cases
REC_DEBUG=1 dist/rec-rs record ...      # prints latencies, encoder retries, gaps filled
```
