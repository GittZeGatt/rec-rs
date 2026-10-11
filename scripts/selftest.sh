#!/bin/sh
# Record short clips through every pipeline and verify them with AVFoundation
# (rec probe) and ffmpeg. Usage: scripts/selftest.sh [--no-screen]
set -eu
cd "$(dirname "$0")/.."
REC=dist/rec
OUT=analysis/raw/selftest
rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0
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
check pattern-webcam-mic  --test-pattern --webcam -t 3
if [ "${1:-}" != "--no-screen" ]; then
  check screen-1080-all   -t 3
  check screen-4k-webcam  -r 4k --webcam -t 3
  check screen-mix.mp4    -t 2 --audio-tracks mix
  # Menu bar app: scripted preview -> record (same toggle as the hotkey) -> stop -> quit.
  if REC_GUI_SELFTEST=1 REC_GUI_NO_ALERTS=1 "$REC" gui >"$OUT/gui.log" 2>&1 \
     && grep -q 'frame_shown=true' "$OUT/gui.log" \
     && f=$(sed -n 's/.*saved=Some("\(.*\)").*/\1/p' "$OUT/gui.log") && [ -n "$f" ] \
     && "$REC" probe "$f" >"$OUT/gui.probe" 2>&1; then
    echo "PASS  gui-menubar  (preview shown, recording saved and probed)"; pass=$((pass+1))
    rm -f "$f"
  else
    echo "FAIL  gui-menubar  (see $OUT/gui.log)"; fail=$((fail+1))
  fi
fi
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
