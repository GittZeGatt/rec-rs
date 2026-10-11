#!/bin/sh
# Record short clips through every pipeline and verify them with AVFoundation
# (rec probe) and ffmpeg. Usage: scripts/selftest.sh [--no-screen]
set -eu
cd "$(dirname "$0")/.."
REC=dist/rec-rs
# Test recordings never go inside the (synced) repo.
OUT="${REC_TEST_DIR:-$HOME/Movies/rec-rs-tests}/selftest"
rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0; skip=0
PERMS=$("$REC" permissions)
has() { echo "$PERMS" | grep -q "^$1 *: granted"; }
skipped() { echo "SKIP  $1  ($2 permission not granted)"; skip=$((skip+1)); }
check() {
  name=$1; shift
  case "$name" in *.mp4) f="$OUT/$name"; name=${name%.mp4} ;; *) f="$OUT/$name.mov" ;; esac
  if "$REC" record -q -o "$f" "$@" >/dev/null 2>"$OUT/$name.log" \
     && "$REC" probe "$f" >"$OUT/$name.probe" 2>&1 \
     && { ! command -v ffmpeg >/dev/null || ffmpeg -v error -i "$f" -f null - 2>>"$OUT/$name.log"; }; then
    frames=$(awk '/vide/{getline; print $3; exit}' "$OUT/$name.probe" | tr -d '(')
    tracks=$(grep -c '^track' "$OUT/$name.probe")
    echo "PASS  $name  ($tracks tracks, $frames video frames)"; pass=$((pass+1))
  else
    echo "FAIL  $name  (see $OUT/$name.log / .probe)"; fail=$((fail+1))
  fi
}
check pattern-1080-h264   --test-pattern -t 3 --no-mic
check pattern-4k-hevc     --test-pattern --noise -r 4k -t 3 --no-mic
check pattern-prores      --test-pattern -t 2 --codec prores --no-mic
if has camera && has microphone; then check pattern-webcam-mic --test-pattern --webcam -t 3; else skipped pattern-webcam-mic "camera/microphone"; fi
if has camera; then check pattern-cam-scene --test-pattern --scene cam -t 2 --no-mic; else skipped pattern-cam-scene camera; fi
# A/V sync: the test pattern flashes white and beeps at the same instant.
if "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/sync.mov" >/dev/null 2>"$OUT/sync.log" \
   && scripts/synccheck.py "$OUT/sync.mov" --max-ms 1 >"$OUT/sync.check" 2>&1; then
  echo "PASS  sync  ($(grep -m1 'audio 0' "$OUT/sync.check" | sed 's/.*offset //'))"; pass=$((pass+1))
else
  echo "FAIL  sync  (see $OUT/sync.check)"; fail=$((fail+1))
fi
# A glitching device (every 50th audio buffer lost) must not shift audio.
if REC_SYNTH_DROP_AUDIO=50 "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/gaps.mov" >/dev/null 2>"$OUT/gaps.log" \
   && scripts/synccheck.py "$OUT/gaps.mov" --max-ms 1 >"$OUT/gaps.check" 2>&1; then
  echo "PASS  audio-gaps  (dropped device buffers filled, still in sync)"; pass=$((pass+1))
else
  echo "FAIL  audio-gaps  (see $OUT/gaps.check)"; fail=$((fail+1))
fi
# Pause/resume: 5 s of output with a pause in the middle, still in sync.
"$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/pause.mov" >/dev/null 2>"$OUT/pause.log" &
sleep 2.3; "$REC" pause || true; sleep 2; "$REC" resume || true; wait || true
if scripts/synccheck.py "$OUT/pause.mov" --max-ms 1 >"$OUT/pause.check" 2>&1 \
   && [ "$(ffprobe -v error -select_streams v -show_entries stream=nb_frames -of csv=p=0 "$OUT/pause.mov")" = 300 ]; then
  echo "PASS  pause-resume  (300 frames, audio in sync across the pause)"; pass=$((pass+1))
else
  echo "FAIL  pause-resume  (see $OUT/pause.check)"; fail=$((fail+1))
fi
# Quit like the Dock does (-[NSApp terminate:]) mid-recording: the app must
# finalize the file first. Screen only, no mic or camera.
if [ "${1:-}" != "--no-screen" ] && has "screen recording"; then
  mkdir -p "$OUT/term"; echo '{"mic_enabled": false, "scene": "screen"}' >"$OUT/term/settings.json"
  if REC_GUI_SELFTEST=terminate REC_OUTPUT_DIR="$OUT/term" REC_SETTINGS="$OUT/term/settings.json" \
       "$REC" gui >"$OUT/term.log" 2>&1 \
     && f=$(sed -n 's/.*saved on quit=Some("\(.*\)") ok=true.*/\1/p' "$OUT/term.log") && [ -n "$f" ] \
     && "$REC" probe "$f" >"$OUT/term.probe" 2>&1; then
    echo "PASS  app-dock-quit  (quit via terminate: mid-recording saved a playable file)"; pass=$((pass+1))
  else
    echo "FAIL  app-dock-quit  (see $OUT/term.log)"; fail=$((fail+1))
  fi
fi
if [ "${1:-}" != "--no-screen" ] && ! { has camera && has microphone; }; then
  echo "SKIP  screen and app cases  (they record the mic and webcam; grant both first)"; skip=$((skip+1))
elif [ "${1:-}" != "--no-screen" ]; then
  check screen-1080-all   -t 3
  check screen-4k-webcam  -r 4k --webcam -t 3
  check screen-mix.mp4    -t 2 --audio-tracks mix
  # App: scripted preview -> record (the hotkey's command) -> pause/resume
  # -> stop -> settings window -> hide window -> quit.
  if REC_GUI_SELFTEST=1 REC_OUTPUT_DIR="$OUT/gui" REC_SETTINGS="$OUT/gui/settings.json" \
       "$REC" gui >"$OUT/gui.log" 2>&1 \
     && grep -q 'idle preview session=true frame_shown=true' "$OUT/gui.log" \
     && grep -q 'paused=true' "$OUT/gui.log" \
     && grep -q 'settings window=true' "$OUT/gui.log" \
     && f=$(sed -n 's/.*saved=Some("\(.*\)").*/\1/p' "$OUT/gui.log") && [ -n "$f" ] \
     && "$REC" probe "$f" >"$OUT/gui.probe" 2>&1; then
    echo "PASS  gui-app  (preview, record, pause, saved and probed, settings window)"; pass=$((pass+1))
  else
    echo "FAIL  gui-app  (see $OUT/gui.log)"; fail=$((fail+1))
  fi
fi
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
