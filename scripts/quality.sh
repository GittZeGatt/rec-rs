#!/bin/sh
# Capture quality against a lossless reference.
#
#   scripts/quality.sh [CASE...]      (default: every case below)
#
# Shows scripts/testcard.swift's static text card 1:1 on a 5K (2x) display,
# behind other windows, takes a lossless `screencapture` of it (converted
# to sRGB, the color space recordings are made in) as the reference, then
# records that window with rec-rs in several configurations and scores the
# middle second of each against the reference scaled to the output size
# with Lanczos: SSIM (Y), PSNR (Y) and VMAF (4K model).
#
# A case is NAME:ARGS (ARGS with commas for spaces), recorded with
# `rec-rs record --window ID --no-mic --no-system-audio -t 4 ARGS`.
# Stream cases (NAME:stream:ARGS) publish to a local MediaMTX and score the
# server's recording instead.
set -eu
cd "$(dirname "$0")/.."
REC=${REC:-dist/rec-rs}
OUT="${REC_TEST_DIR:-$HOME/Movies/rec-rs-tests}/quality"
mkdir -p "$OUT"
CASES=${*:-"4k-auto:-r,4k 4k-prores:-r,4k,--codec,prores 1080-auto:-r,1080p"}

swift scripts/testcard.swift "$OUT/card.png" 600 >"$OUT/card.log" 2>&1 &
card=$!
trap 'kill $card 2>/dev/null || true' EXIT
for _ in $(seq 60); do grep -q '^window' "$OUT/card.log" 2>/dev/null && break; sleep 0.5; done
WIN=$(sed -n 's/^window //p' "$OUT/card.log")
[ -n "$WIN" ] || { cat "$OUT/card.log"; exit 1; }
sleep 1
screencapture -x -o -l "$WIN" "$OUT/ref-display.png"
sips -m "/System/Library/ColorSync/Profiles/sRGB Profile.icc" "$OUT/ref-display.png" --out "$OUT/ref.png" >/dev/null

# score FILE: the middle second against ref.png at the file's size.
score() {
  f=$1
  wh=$(ffprobe -v error -select_streams v -show_entries stream=width,height -of csv=p=0:s=x "$f")
  w=${wh%x*}; h=${wh#*x}
  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f")
  ss=$(echo "$dur" | awk '{s=$1/2-0.5; print (s>0)?s:0}')
  model=vmaf_v0.6.1; [ "$h" -ge 1440 ] && model=vmaf_4k_v0.6.1
  # Both sides to 4:2:0 BT.709 limited range, as players decode it; the
  # reference is downscaled from 5K in RGB with Lanczos.
  ff="[0:v]trim=start=$ss:duration=1,setpts=PTS-STARTPTS,scale=in_range=tv:out_range=tv,format=yuv420p,split=3[d1][d2][d3];"
  ff="$ff[1:v]format=rgb24,scale=$w:$h:flags=lanczos+accurate_rnd+full_chroma_int,scale=in_range=pc:out_range=tv:out_color_matrix=bt709:flags=accurate_rnd+full_chroma_int,format=yuv420p,loop=loop=-1:size=1,trim=duration=1,setpts=N/FRAME_RATE/TB,split=3[r1][r2][r3];"
  ff="$ff[d1][r1]ssim=stats_file=-[s];[d2][r2]psnr=stats_file=/dev/null[p];[d3][r3]libvmaf=model=version=$model:n_threads=8[v]"
  ffmpeg -v info -i "$f" -loop 1 -framerate 60 -i "$OUT/ref.png" -filter_complex "$ff" \
    -map "[s]" -f null - -map "[p]" -f null - -map "[v]" -f null - 2>"$f.metrics" >/dev/null || true
  ssim=$(grep -o 'SSIM Y:[0-9.]*' "$f.metrics" | tail -1 | cut -d: -f2)
  psnr=$(grep -o 'PSNR y:[0-9.inf]*' "$f.metrics" | tail -1 | cut -d: -f2)
  vmaf=$(grep -o 'VMAF score: [0-9.]*' "$f.metrics" | tail -1 | awk '{print $3}')
  rate=$(ffprobe -v error -select_streams v -show_entries stream=bit_rate,codec_name,profile -of csv=p=0 "$f" | head -1)
  printf '%-14s %-11s SSIM-Y %-8s PSNR-Y %-6s VMAF %-6s  (%s)\n' "$2" "${w}x$h" "$ssim" "$psnr" "$vmaf" "$rate"
}

for c in $CASES; do
  name=${c%%:*}; rest=${c#*:}
  case "$rest" in
    stream:*)
      args=$(echo "${rest#stream:}" | tr ',' ' ')
      M="$OUT/mediamtx"; mkdir -p "$M"; rm -rf "${M:?}/rec"
      printf 'logLevel: warn\nrtsp: false\nhls: false\nsrt: false\nmoq: false\nrtmpAddress: :19350\nwebrtcAddress: :18889\nwebrtcLocalUDPAddress: :18189\npathDefaults:\n  record: yes\n  recordPath: %s/rec/%%path/%%Y-%%m-%%d_%%H-%%M-%%S-%%f\n  recordFormat: fmp4\npaths:\n  all_others:\n' "$M" >"$M/mediamtx.yml"
      (cd "$M" && exec mediamtx mediamtx.yml >"$M/server.log" 2>&1) &
      mpid=$!; sleep 2
      # shellcheck disable=SC2086
      REC_STREAM_KEY=quality "$REC" record -q --window "$WIN" --no-mic --no-system-audio --no-record -t 8 -o "$OUT/none.mov" $args >/dev/null 2>"$OUT/$name.log" || true
      sleep 2; kill "$mpid" 2>/dev/null || true; wait "$mpid" 2>/dev/null || true
      f=$(find "$M/rec" -name '*.mp4' | head -1)
      [ -n "$f" ] && cp -f "$f" "$OUT/$name.mp4" && score "$OUT/$name.mp4" "$name" || echo "$name: no stream recording (see $OUT/$name.log)"
      ;;
    *)
      args=$(echo "$rest" | tr ',' ' ')
      f="$OUT/$name.mov"
      rm -f "${OUT:?}/$name.mov" "${OUT:?}/$name.mov.metrics"
      # shellcheck disable=SC2086
      "$REC" record -q --window "$WIN" --no-mic --no-system-audio -t 4 -o "$f" $args >/dev/null 2>"$OUT/$name.log" \
        && score "$f" "$name" || echo "$name: recording failed (see $OUT/$name.log)"
      ;;
  esac
done
