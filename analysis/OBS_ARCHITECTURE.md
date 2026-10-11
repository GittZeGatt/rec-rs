# OBS Studio on macOS: architecture notes for rec-rs

This document studies how OBS Studio is built on macOS, so rec-rs can learn from its design. It
is written in our own words. It cites file paths and function or struct names only, and contains
no OBS source code. See section 9 for the licensing position.

**Sources studied**

- OBS source: `git clone --depth 1 https://github.com/obsproject/obs-studio ~/re/obs-studio`,
  commit `7d98beb` (2026-10-08). Paths below are relative to that checkout.
- Installed app: `/Applications/OBS.app` 32.2.2 (build 31845296735), arm64, signed by Developer
  ID "Wizards of OBS LLC (2MMRE5MTB8)". The bundle was not modified.
- REA `rea@6.4.0` (`npx -y rea-agents@latest`), with output in `~/re/obs-rea/`:
  - `rea inspect-plist /Applications/OBS.app --format json` and
    `rea inspect-signature /Applications/OBS.app --format json` both worked (`plist.json`,
    `sig.json`, including entitlements).
  - `rea analyze /Applications/OBS.app` failed with `provider_unavailable`; no Ghidra, Hopper or
    IDA backend is installed.
  - `rea inspect-binary-layout` failed; its profile is Linux x64 only.
  - The bundle inventory therefore comes from `otool -L`, `ls` and `codesign`
    (`otool-OBS.txt`).

---

## 1. Runtime and plugin structure of OBS.app

### How OBS does it

**Bundle layout (observed).**

- `Contents/MacOS/OBS` is the Qt 6 frontend. `Contents/MacOS/obs-ffmpeg-mux` is a separate muxer
  executable that links only libav* and libobs.
- `Contents/Frameworks/` holds:
  - `libobs.framework`, plus two graphics backends: `libobs-opengl.dylib` and
    `libobs-metal.dylib`.
  - `obs-frontend-api.dylib` and `obs-scripting.dylib`.
  - Qt, Sparkle, Syphon, and CEF with four `OBS Helper*.app` bundles for the browser source.
  - The FFmpeg dylibs, x264, srt, rist, mbedTLS and libdatachannel.
- `Contents/PlugIns/` holds one `*.plugin` bundle per module:
  - Mac-specific: `mac-capture`, `mac-avcapture` (+ `-legacy`), `mac-videotoolbox`,
    `coreaudio-encoder`, `mac-syphon`, `mac-virtualcam`.
  - Cross-platform: `obs-outputs`, `obs-ffmpeg`, `obs-filters`, `obs-browser`,
    `obs-websocket`, etc.
- `Contents/Library/SystemExtensions/` holds the CoreMediaIO camera extension for the virtual
  camera.
- `otool -L` shows a clean split by framework:
  - `mac-capture` links ScreenCaptureKit, CoreAudio, CoreMedia, CoreVideo and IOSurface.
  - `mac-avcapture` links AVFoundation and CoreMediaIO.
  - `mac-videotoolbox` links VideoToolbox.
  - `coreaudio-encoder` links only CoreAudio and libobs.
  - `libobs-metal` is Swift and links Metal, QuartzCore and IOSurface.

**Module loading.**

- `libobs/obs-module.c`: `obs_open_module` calls `os_dlopen` on the plugin.
  `load_module_exports` resolves `obs_module_load`, `obs_module_set_pointer` and
  `obs_module_ver` (required), plus optional unload, post-load, locale and description hooks.
- A plugin uses `OBS_DECLARE_MODULE()` (`libobs/obs-module.h`). It registers C vtables in
  `obs_module_load`:
  - `obs_register_source(obs_source_info)`, `obs_register_encoder(obs_encoder_info)` and
    `obs_register_output(obs_output_info)`.
  - Callbacks include `create`, `video_tick`, `video_render`, `encode` and `encoded_packet`.
  - Flags include `OBS_SOURCE_VIDEO`, `OBS_SOURCE_ASYNC_VIDEO`, `OBS_SOURCE_AUDIO` and
    `OBS_SOURCE_CUSTOM_DRAW`.
- Core modules are a build-time list (`libobs/obs-core-modules.c.in`, filled by
  `set_obs_core_modules` in `cmake/common/helpers_common.cmake`). On macOS,
  `load_core_modules` (`libobs/obs-core-modules-loader-mac.m`) resolves each one at
  `<builtInPlugInsURL>/<name>.plugin/Contents/MacOS/<name>`, with data in `Contents/Resources/`.
- User plugins load from `~/Library/Application Support/obs-studio/plugins/<name>.plugin`, or the
  legacy `.so` layout (`frontend/plugin-manager/PluginModuleLoader_MacOS.mm`).
- `plugins/mac-capture/plugin-main.c` registers by OS version:
  - The SCK `screen_capture` source on 12.5 and later.
  - The SCK audio source on 13 and later. On 13+, the legacy display, window and CoreAudio
    output capture sources are marked `OBS_SOURCE_DEPRECATED`.
- Renderer choice: `OBSApp::GetRenderModule` (`frontend/OBSApp.cpp`) defaults to OpenGL. Metal is
  an "Experimental" option on Apple Silicon only; `libobs-metal/README.md` calls it alpha.

**Info.plist (REA).**

- Bundle `com.obsproject.obs-studio`, minimum macOS 13.0, built with the 26.5 SDK.
- Usage strings (from `cmake/macos/helpers.cmake`): `NSCameraUsageDescription`,
  `NSMicrophoneUsageDescription`, and `NSAppleEventsUsageDescription` (for hotkeys).
- There is no screen-capture key; SCK is gated by TCC alone.
- Also: `LSAppNapIsDisabled = true`, `NSCameraReactionEffectGesturesEnabledDefault = false`
  (which stops macOS gesture reactions being burned into camera frames), and Sparkle `SU*`
  keys.

**Entitlements (REA; source `frontend/cmake/macos/entitlements.plist`).**

- Hardened runtime, with `device.camera` and `device.audio-input`.
- `cs.disable-library-validation` (third-party plugins) and
  `cs.allow-unsigned-executable-memory` (CEF and scripting).
- `system-extension.install` and an app group, for the virtual camera.

### What rec-rs does and what we take away

- rec-rs is one binary with no plugin ABI, which is right for a focused recorder. OBS's
  sync/async source split is still a good model: some sources expose only their newest surface
  and are polled by the compositor, while others queue frames with timestamps.
- rec-rs needs neither `disable-library-validation` nor `allow-unsigned-executable-memory`, so
  its hardened-runtime posture stays stronger than OBS's.
- **Action items:**
  - **App Nap.** Add `LSAppNapIsDisabled` to `support/Info.plist`, or hold an
    `NSProcessInfo beginActivityWithOptions` token while recording. Rec is an `LSUIElement`
    menu-bar app with usually no visible window, exactly the case where App Nap throttles the
    pacer thread's timers.
  - **Camera reactions.** Consider setting `NSCameraReactionEffectGesturesEnabledDefault = false`,
    so a thumbs-up in the webcam overlay doesn't put fireworks in the recording.

---

## 2. macOS screen capture (`plugins/mac-capture`)

### How OBS does it

**Files.** `mac-sck-video-capture.m` is the video source. `mac-sck-audio-capture.m` is the
audio-only source. `mac-sck-common.{h,m}` holds the shared `struct screen_capture`,
`ScreenCaptureDelegate` and content lists. The legacy implementations are
`mac-display-capture.m` and `mac-window-capture.m`.

**Stream setup (`init_screen_stream`).** One source covers three modes, each with its own
`SCContentFilter`:

- **`ScreenCaptureDisplayStream`.** Captures the whole display. With "hide OBS" on, OBS excludes
  its own `SCRunningApplication`, matched by bundle ID. Output size is the display mode's pixel
  size (`CGDisplayModeGetPixelWidth/Height`), so Retina is captured at native resolution.
- **`ScreenCaptureWindowStream`.** Uses a desktop-independent window filter sized from
  `window.frame`. Child windows are included on 14.2+.
- **`ScreenCaptureApplicationStream`.** Uses a display filter with a single included app. The
  menu bar is included on 14.2+.

`SCStreamConfiguration` settings:

- `queueDepth` 8, `showsCursor` per setting, **Display P3** color space, and a clear background.
- `minimumFrameInterval` is **0.9 × the output frame interval**, so SCK is never the rate
  limiter.
- The pixel format is **`64RGBAHalf` on macOS 15+**, otherwise `32BGRA`.
- On 13+, `capturesAudio` is on, with `excludesCurrentProcessAudio` and 2 channels. One stream
  delivers both screen and audio outputs.
- Shareable content is fetched asynchronously, guarded by the `shareable_content_available`
  semaphore.

**Frame delivery (`screen_stream_video_update`).**

- The delegate takes the pixel buffer's `IOSurfaceRef`, retains it, increments its use count,
  and swaps it into `sc->current` under a mutex. The previous surface is released.
- No copy is made and no timestamp is kept: screen capture is a *sync* source, so the newest
  frame wins.
- For window capture it reads the `SCStreamFrameInfoContentRect`, `ContentScale` and
  `ScaleFactor` attachments. On a size change it calls `updateConfiguration` on the live stream.
- `SCStreamFrameInfoStatus` is not checked explicitly. Idle frames carry no image buffer, so
  they fall through as a null surface.

**Into libobs.**

- `sck_video_capture_tick` runs on the graphics thread each render tick. It promotes
  `current` → `prev` and wraps the surface with `gs_texture_create_from_iosurface`, or reuses
  the texture object with `gs_texture_rebind_iosurface`.
- The GL backend binds the surface as a `GL_TEXTURE_RECTANGLE` via `CGLTexImageIOSurface2D`
  (`libobs-opengl/gl-cocoa.m`). The Metal backend wraps it as an `MTLTexture`
  (`device_texture_create_from_iosurface` in `libobs-metal/metal-texture2d.swift`).
- `sck_video_capture_render` draws with the `DrawD65P3` technique, converting P3 to the canvas
  color space.
- `didStopWithError` sets `capture_failed`. The UI then offers `reactivate_capture`.
- Legacy `CGDisplayStream` capture uses the same IOSurface handoff and reads
  `CGDisplayStreamUpdateGetDropCount`.

### What rec-rs does and what we take away

- **Same zero-copy model.** `src/screen.rs` keeps the newest `CVPixelBuffer` for the
  compositor. It already goes further than OBS: it checks
  `SCStreamFrameInfoStatus == Complete` and excludes rec's own windows.
- **Frame-interval headroom.** Rec sets `minimumFrameInterval` to exactly 1/fps. OBS's
  0.9 × headroom stops SCK delivery jitter from causing a missed slot at 60 fps. The idea is
  worth adopting.
- **Color.** OBS asks for P3 and half-float and converts on the GPU. Rec should tag the SCK color
  space explicitly so CoreImage color-matches correctly into sRGB. Keep 8-bit or `420v`;
  half-float doubles bandwidth and only pays off for HDR or P3 output.
- **Window resize.** Mirror OBS's handling for `--window`: on a size change, call
  `updateConfiguration` rather than letterboxing a stale size.

---

## 3. Camera (`plugins/mac-avcapture`)

### How OBS does it

**Two sources** are registered in `plugin-main.m`:

- `macos-avcapture` is *async* (`OBS_SOURCE_ASYNC_VIDEO`). It copies frames into libobs with
  timestamps.
- `macos-avcapture-fast` is *sync* (`OBS_SOURCE_VIDEO | CUSTOM_DRAW | SRGB`). It holds the newest
  IOSurface, just like screen capture.

**Session (`createSession` in `OBSAVCapture.m`).**

- `AVCaptureVideoDataOutput` and `AVCaptureAudioDataOutput`, each on its own serial queue, plus a
  session queue.
- Hot-plug is handled with `deviceConnected:` and `deviceDisconnected:`.

**Format selection (`configureSession`).**

- The user picks an exact `AVCaptureDeviceFormat`, serialized via
  `AVCaptureDeviceFormat+OBSListable.m`, plus a frame rate.
- The rate is validated against `videoSupportedFrameRateRanges`, with fallback via
  `fallbackFrameRateForFormat`. OBS then sets `activeFormat` and `activeVideoMinFrameDuration`
  under `lockForConfiguration`.
- A preset path (`configureSessionWithPreset`) and migration of legacy settings also exist.
- The fast path requires `32BGRA` or `ARGB2101010LEPacked` and otherwise reports
  `OBSAVCaptureError_SampleBufferFormat`.

**Timestamps (`captureOutput:didOutputSampleBuffer:fromConnection:`).**

- The output PTS is converted to ns with `CMTimeConvertScale`. These are host-clock times.
- On the async path, planes are copied from the locked pixel buffer into an `obs_source_frame`
  and passed to `obs_source_output_video`. Audio goes to `obs_source_output_audio` on the same
  time base.
- `obs_source_set_async_unbuffered` toggles libobs's pacing:
  - Unbuffered shows the newest frame.
  - Buffered (`ready_async_frame` in `libobs/obs-source.c`) releases frames by timestamp against
    the video clock. It drops late frames and avoids duplicates within 2 ms.

### What rec-rs does and what we take away

- `src/camera.rs` uses the newest frame per output slot. That is OBS's "fast" model, and right
  for a PiP overlay.
- **Worth adopting:**
  - Explicit format choice: validate `activeFormat` and `activeVideoMinFrameDuration` against
    `videoSupportedFrameRateRanges` instead of relying on presets.
  - Hot-plug and disconnect handling, so unplugging a camera doesn't end the recording.

---

## 4. Audio

### How OBS does it

**CoreAudio input (`plugins/mac-capture/mac-audio.c`).**

- An AUHAL (`kAudioUnitSubType_HALOutput`) has input enabled via
  `kAudioOutputUnitProperty_EnableIO`.
- `input_callback` calls `AudioUnitRender` and stamps buffers with
  `AudioConvertHostTimeToNanos(mHostTime)`. That is the device's own timestamp, not "now", so
  callback scheduling jitter doesn't enter the timeline.
- Property listeners (`coreaudio_init_hooks`) and a `reconnect_thread` survive device changes
  and device loss.

**SCK audio (`screen_stream_audio_update`).**

- Plane pointers go into `obs_source_audio` as `AUDIO_FORMAT_FLOAT_PLANAR`, with the timestamp
  set from the buffer's output PTS (host clock).
- `sck_audio_capture` is the audio-only variant, for desktop or single-app audio. It adds a dummy
  screen output so SCK stays quiet.

**Per-source ingest (`libobs/obs-source.c`).**

- `process_audio` resamples each source to the output format (swresample wrapper in
  `libobs/media-io/audio-resampler-ffmpeg.c`) and tracks the resampler delay as
  `resample_offset`.
- `source_output_audio_data` then applies the timing rules:
  - **Direct timestamps.** A timestamp within `MAX_TS_VAR` (2 s, in `obs-internal.h`) of
    `os_gettime_ns()` is trusted as absolute. Otherwise `timing_adjust` maps the source clock
    onto system time.
  - **Smoothing.** A buffer starting within `TS_SMOOTHING_THRESHOLD` (**70 ms**) of where the
    last one ended (`next_audio_ts_min`) is snapped to be contiguous.
  - **Jumps.** A jump of more than 2 s triggers `handle_ts_jump`, which resets timing and
    flushes the buffer.
  - **Offsets.** The user `sync_offset` is added and the resampler delay subtracted.
  - **Storage.** Data is appended (`source_output_audio_push_back`) or placed at its sample
    offset (`source_output_audio_place`) in `audio_input_buf`.

**Global mixer (`libobs/media-io/audio-io.c`, `libobs/obs-audio.c`).**

- `audio_thread` ticks every `AUDIO_OUTPUT_FRAMES` (1024 frames, about 21.3 ms at 48 kHz). The
  deadline is start time plus samples so far, and it sleeps with `os_sleepto_ns_fast`. The audio
  clock is a sample counter anchored to `os_gettime_ns`, so it cannot drift.
- `audio_callback` runs each tick:
  - It builds the render tree from active sources and renders each one
    (`obs_source_audio_render`), applying volume, mute and the per-source **mixer bitmask**.
  - It finds the minimum source timestamp.
  - `mix_audio` sums each source into up to **`MAX_AUDIO_MIXES` = 6 tracks** at the right sample
    start point, then consumed data is discarded.
- **Adaptive buffering.** If a source's data is older than the current window,
  `add_audio_buffering` delays the whole output timeline by whole ticks.
  - The cap is `max_buffering_ticks`: by default 45 ticks, about 960 ms (`libobs/obs.c`).
  - Once maxed, stale audio is dropped (`ignore_audio`). Sources still waiting for data are
    skipped as `audio_pending`.
  - A fixed-buffering mode pre-allocates the maximum instead.
- `clamp_audio_output` clamps to [-1, 1] and scrubs NaN, keeping an unclamped copy.
  `do_audio_output` then feeds each active mix to its encoders, each with an optional resampler.

### What rec-rs does and what we take away

- `src/mixer.rs` is the same design in miniature:
  - One 48 kHz sample timeline from host-clock PTS.
  - A continuity snap within 2 ms (`SNAP`).
  - Emission anyway after a 0.5 s stall (`MAX_WAIT`).
  - A mix track plus stems.
- **Snap width.** The main tuning difference is the snap: 70 ms in OBS, 2 ms in rec. Ours is
  fine for SCK and AUHAL host stamps. Widen it only on evidence; log snap and gap counts per
  source first.
- **Robustness to copy:**
  - Mic reconnect on device loss, as with `reconnect_thread` (AirPods disconnects).
  - NaN scrubbing before clamping.
  - If Bluetooth mics arrive late, replace the fixed 0.5 s wait with OBS-style growth in whole
    ticks up to a cap, and log it.
- The six mixes with per-source bitmasks are generality rec doesn't need.

---

## 5. Scene compositing, render loop, and CFR

### How OBS does it

**Scenes (`libobs/obs-scene.c`).**

- A scene is itself a source (`scene_info`). Its `obs_sceneitem`s carry transform, bounds
  (`OBS_BOUNDS_*`), crop, blend mode and visibility.
- `update_item_transform` precomputes `draw_transform`. `scene_video_render` → `render_item`
  draws the items in order. Crop and scale-filtered items go through an intermediate texture
  first (`render_item_texture`).

**Graphics.** The `gs_*` API (`libobs/graphics/`) sits on `libobs-opengl` (the macOS default) or
`libobs-metal`. Shaders are HLSL-like `.effect` files, transpiled per backend.

**Render loop.** `obs_graphics_thread` → `obs_graphics_thread_loop` (in `libobs/obs-video.c`, run
in an autorelease pool on macOS). Each tick:

1. `tick_sources` runs every `video_tick`, which is where sync sources rebind their newest
   IOSurface.
2. `output_frame` for each video mix:
   - `render_main_texture` draws the scene at canvas resolution.
   - `render_output_texture` scales it.
   - `render_convert_texture` converts RGB to NV12/I420/P010 on the GPU.
   - `stage_output_texture` copies to staging surfaces.
   - `download_frame` reads back the *previous* tick's surface (`NUM_TEXTURES` = 2). The CPU
     never stalls on the GPU, at the cost of one frame of latency.
3. `render_displays` draws the previews.
4. `video_sleep` sleeps to the next tick.

**How CFR is achieved.**

- `video_time` is a fixed grid starting at `os_gettime_ns()`. `video_sleep` sleeps to the
  absolute deadline `video_time + interval` with `os_sleepto_ns`.
- If the tick overran, it computes how many intervals passed (`count`) and advances
  `video_time` by `count × interval`. The `count - 1` missed intervals are recorded as **lagged
  frames**.
- The frame carries `count` into `video_output_lock_frame` (`libobs/media-io/video-io.c`). The
  video-io thread then delivers it `count` times, adding `frame_time` to the timestamp each
  time (`video_output_cur_frame`). **Late renders become duplicate frames with exact
  timestamps.**
- If the frame cache is full because encoders are behind, the last frame's `count` and
  `skipped` are bumped instead. These are reported as **skipped frames**.
- Sources never set frame timing. A per-encoder `frame_rate_divisor` (`video_output_connect2`)
  lets one canvas feed encoders at, say, 60 and 30 fps.

### What rec-rs does and what we take away

- Rec is pull-based with a lag:
  - A pacer thread wakes every half period and calls `emit_until(now - lag)`.
  - For each CFR slot it picks the newest screen and camera buffers at or before the slot,
    composites them with CoreImage on Metal, and duplicates the previous output when nothing new
    arrived.
  - Timestamps come from slot index × frame duration, never from capture PTS, which is the same
    invariant OBS keeps.
- **Absolute deadlines.** Sleep to absolute slot deadlines (`mach_wait_until`, next slot + lag)
  instead of `sleep(period/2)`. That halves wakeups and removes phase wobble.
- **Split counters.** Split rec's `dup` counter into "no new capture frame" (benign) and "pacer
  late" (a real problem), the way OBS separates lagged from skipped frames.
- **Don't block on the GPU.** OBS's double-buffered readback says don't stall on the GPU inside
  a tick. Let CoreImage render into writer-pool buffers without waiting synchronously, where the
  append ordering allows it.

---

## 6. Encoders

### How OBS does it

**VideoToolbox (`plugins/mac-videotoolbox/encoder.c`).**

- One encoder is registered per available VT encoder ID: H.264, HEVC, and ProRes
  (hardware and software lists, `vt_prores_*_encoder_list`).
- `create_encoder` does the following:
  - `VTCompressionSessionCreate` with `kVTVideoEncoderSpecification_EncoderID` and a minimal
    pixel-buffer spec.
  - Checks `UsingHardwareAcceleratedVideoEncoder`.
  - Sets `MaxKeyFrameIntervalDuration` (`keyint_sec`), `MaxKeyFrameInterval`
    (`keyint_sec × fps`) and `ExpectedFrameRate`.
  - Sets `AllowFrameReordering` from the B-frames toggle.
  - Sets `ProfileLevel` via `obs_to_vt_profile` (Main10 for P010, 4:2:2 profiles for 4:2:2).
  - Sets `RealTime` to **false**, deliberately, because libobs does the pacing.
  - Sets color primaries, transfer and matrix, plus HDR mastering and content-light-level
    metadata (`session_set_colorspace`).
- Rate control (`session_set_bitrate`):
  - CBR uses `ConstantBitRate`, falling back to the average rate.
  - ABR uses `AverageBitRate` with optional `DataRateLimits`.
  - CRF uses `Quality` plus `SpatialAdaptiveQPLevel`.
  - Bitrate is changeable live (`OBS_ENCODER_CAP_DYN_BITRATE`).
- `vt_encode` is a **CPU-input path**. It takes a buffer from the session's pool
  (`get_cached_pixel_buffer`), `memcpy`s each plane of the GPU-converted, read-back frame into
  it, and calls `VTCompressionSessionEncodeFrame` with PTS = frame index / fps.
  - There is no zero-copy `encode_texture` path for VT on macOS; `obs-video-gpu-encode.c` serves
    other platforms.
- `parse_sample` emits Annex-B packets with parameter sets on keyframes (`handle_keyframe`). With
  B-frames it shifts DTS back two frames.

**AAC.**

- `CoreAudio_AAC` (`plugins/coreaudio-encoder/encoder.cpp`) wraps an `AudioConverter`. The
  frontend prefers it on macOS over `libfdk_aac` and `ffmpeg_aac`
  (`OBSBasicSettings_Stream.cpp`).
- It sets the bitrate-control mode and codec quality, validated against
  `kAudioConverterApplicableEncodeBitRates`.
- It reads `kAudioConverterPrimeInfo` and shifts packet timestamps by the priming frames, which
  it exposes via `get_priming_samples` so the muxer can write the edit list.

### What rec-rs does and what we take away

- Rec hands IOSurface-backed pixel buffers straight to AVAssetWriter → VT. That is **zero-copy
  end to end**, strictly better than OBS's readback + `memcpy` path on macOS. Keep it.
- Rec already matches OBS's recording defaults: a 2 s `MaxKeyFrameIntervalDuration`, no frame
  reordering, and `ExpectedSourceFrameRate`.
- **Quality mode.** OBS's CRF mode (VT `Quality` + spatial AQ) is the better default for local
  files. A `--quality` option mapped to `AVVideoQualityKey` could replace bitrate guessing.
- **Real-time flags.** `expectsMediaDataInRealTime = true` is correct for AVAssetWriter, which
  has no external pacing buffer. OBS's `RealTime = false` relies on libobs's own queueing.
- **AAC priming.** AVAssetWriter writes AAC priming edit lists itself, which OBS has to do by
  hand through `PrimeInfo`.

---

## 7. Muxing and the recording pipeline

### How OBS does it

**Interleaving (`libobs/obs-output.c`).**

- `initialize_interleaved_packets` computes per-track `video_offsets` and `audio_offsets`, so all
  tracks start at zero.
- `prune_premature_packets` drops leading packets until audio and video overlap within one frame.
- `send_interleaved` releases packets in DTS order once every track has a later packet
  (`highest_video_ts`, `highest_audio_ts`).

**Native hybrid MP4/MOV muxer (`plugins/obs-outputs/mp4-mux.c`, `mp4-output.c`).**

- This is the **default recording format**: `hybrid_mov` on macOS, `hybrid_mp4` elsewhere
  (`DEFAULT_CONTAINER` in `frontend/widgets/OBSBasic.cpp`). It is registered as `mp4_output` and
  `mov_output`.
- **While recording** it writes fragmented MP4:
  - First `ftyp`, then a 16-byte `free` (or `wide`) placeholder, then an initial `moov` with
    `mvex`.
  - `mp4_mux_submit_packet` sets `next_frag_pts` at each video keyframe. Once all tracks pass
    it, `mp4_flush_fragment` writes `moof` + `mdat`. So there is one fragment per GOP.
  - **A crash leaves a valid fragmented file**, complete up to the last GOP.
- **On stop** (`mp4_mux_finalise`):
  - It appends a complete non-fragmented `moov` (`stts`, `stss`, `ctts`, `stsc`, `stsz`,
    `stco`/`co64`) that indexes the fragment data already on disk.
  - It seeks to the start, rewrites `ftyp`, and turns the placeholder into one `mdat` header
    (64-bit size when over 4 GiB) that swallows all the fragments.
  - The result is a normal, seekable file with no remux pass.
- **Extras:**
  - `edts`/`elst` edit lists.
  - Chapters (an `add_chapter` proc, queued until the matching frame).
  - Keyframe-aligned splitting by time, size or manual trigger (`should_split`).
  - `mdta` metadata.
  - I/O through a threaded `buffered_file_serializer`.

**FFmpeg mux process (`plugins/obs-ffmpeg/obs-ffmpeg-mux.c`, `ffmpeg-mux/ffmpeg-mux.c`).**

- This is used for MKV, FLV, TS, plain MP4/MOV and the replay buffer.
- OBS spawns `obs-ffmpeg-mux` via `os_process_pipe_create2`, with arguments from
  `build_command_line`. It streams packets to the child's stdin as an `ffm_packet_info` header
  (pts, dts, size, index, type, keyframe) plus the payload.
- The child calls `av_interleaved_write_frame` for each packet. On stdin EOF, **including an OBS
  crash**, `ffmpeg_mux_free` calls `av_write_trailer`. Process isolation is the crash-safety
  mechanism for non-fragmented formats.

**Remux.** `media_remux_job_*` (`libobs/media-io/media-remux.c`) copies streams between
containers with libavformat. `OBSBasic::AutoRemux` can convert finished recordings.

### What rec-rs does and what we take away

- Rec uses AVAssetWriter with `movieFragmentInterval` = 5 s. That gives the same contract as
  OBS's hybrid mode: playable up to the last fragment after a crash, and a normal movie after
  `finishWriting`. MOV is also OBS's macOS default.
- **Fragment interval.** OBS fragments every GOP (about 2 s), while rec can lose up to 5 s on a
  crash. Setting the interval to 2 s, aligned with keyint, costs almost nothing.
- **Crash test.** Add a test that `kill -9`s rec mid-recording and checks the file with
  `rec probe`. It proves the crash-safety claim.
- **Start alignment.** Rec achieves OBS-style alignment by starting the session at the first CFR
  slot, with audio on the same host origin (`t0`). Keep a regression test that the first audio
  and video PTS agree within one audio buffer.
- **Back-pressure.** `isReadyForMoreMediaData` is AVAssetWriter's equivalent of OBS's skipped
  frames. Rec already counts the resulting drops.

---

## 8. A/V sync

### How OBS does it

- **One clock.** On macOS, `os_gettime_ns()` is `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)`
  (`libobs/util/platform-cocoa.m`). That is mach absolute time in ns, the same base as the host
  time clock, SCK PTS, AVCapture PTS and CoreAudio `mHostTime`. Capture timestamps are therefore
  "direct" and used as-is.
- **Video.** `video_time` is a fixed grid: frame N = start + N × interval. Duplicates keep the
  grid exact.
- **Audio.** Each audio window is `[prev_time, audio_time)`, derived from the same clock plus a
  sample count. Sources are placed into it by their smoothed, offset host timestamps. Buffering
  shifts the timeline later in whole ticks; it never stretches it.
- **Encoder start.**
  - Video encoders take `start_ts` from their first raw frame (`receive_video` in
    `libobs/obs-encoder.c`).
  - The paired audio encoder (`buffer_audio`) drops audio before that point and trims the first
    buffer to start exactly there (`calc_offset_size`, `start_from_buffer`).
  - `send_off_encoder_packet` sets `dts_usec` = `start_ts` + encoder DTS, so every encoder shares
    the system time base.
- **User offsets.** `obs_source_set_sync_offset` sets a per-source offset in ns. Async video uses
  `timing_adjust` to map onto `video_time`.
- **No drift resampling.** Drift between a device's sample clock and host time shows up as small
  discontinuities. Those are absorbed by the 70 ms snap, or by a reset beyond it, not by
  adaptive resampling.

### What rec-rs does and what we take away

- Rec uses the same principle: `host_now()` (mach time) is the only clock. Video slots and the
  mixer timeline share one origin, and capture PTS only *place* data; they never *time* output.
- **Long-recording drift.** OBS's snap-then-reset means long recordings with devices that don't
  stamp host time can drift up to 70 ms before correcting. Rec's 2 ms snap corrects more often,
  which is better for lip-sync but risks clicks. If clicks are reported, the principled fix is a
  slow per-source drift estimator with fractional resampling, not a wider snap. OBS doesn't have
  this either.
- **Per-source offset.** A flag such as `--mic-offset-ms` is cheap, and it is the knob OBS users
  rely on for Bluetooth latency.

---

## 9. Licensing note

- OBS Studio is **GPL-2.0** (file headers say "version 2 ... or any later version"). rec-rs is a
  clean-room implementation licensed **MIT OR Apache-2.0**.
- OBS was studied for architecture only:
  - Its source was read in a clone outside this repository (`~/re/obs-studio`).
  - The installed app was inspected non-destructively (REA, `otool`, `codesign`, `plutil`).
- **No OBS source code was copied** into rec-rs or into this document. Only file paths,
  function, struct and constant names, and descriptions in our own words appear here.
- rec-rs is built directly on Apple's public frameworks: ScreenCaptureKit, AVFoundation,
  CoreImage/Metal, VideoToolbox and CoreAudio.
- This markdown file is the only OBS-related item in the repository. The clone and the REA
  output stay in `~/re/`.

---

## Summary: the most useful takeaways for rec-rs

1. **Keep the core.** One host clock, a CFR slot grid with duplicate-on-late, a sample-timeline
   mixer, and a crash-safe fragmented container all match OBS's design. Rec's zero-copy
   IOSurface → AVAssetWriter/VT path beats OBS's readback + `memcpy` path on macOS.
2. **Pacing.** Sleep to absolute slot deadlines. Set SCK `minimumFrameInterval` to about 0.9 ×
   the frame period. Split stats into "no new frame" and "pacer late", as OBS splits lagged and
   skipped frames.
3. **App hygiene.** Disable App Nap while recording (`LSAppNapIsDisabled` or a `beginActivity`
   token), and disable camera reaction gestures
   (`NSCameraReactionEffectGesturesEnabledDefault = false`).
4. **Robustness.**
   - Reconnect mic and camera after device loss, and restart SCK on `didStopWithError`.
   - Call `updateConfiguration` when a captured window resizes.
   - Use a 2 s fragment interval and add a kill-9 crash test.
5. **Knobs.** A quality-based VT mode (`AVVideoQualityKey`), per-source audio sync offsets, and
   adaptive audio buffering in whole ticks if Bluetooth sources arrive late.
