#!/bin/sh
# Live-encoder quality: encode lossless 4K60 motion clips with the stream
# encoder (src/live/video.rs) and score them with VMAF (4K model) and PSNR-Y.
#
#   scripts/encquality.sh [CASE...]
#
# CASE is NAME:CODEC:KBPS[:basic], CODEC h264 or hevc; "basic" sets
# REC_LIVE_BASIC=1 (the encoder settings before the quality work).
# Sources (made once, ffv1): the text card (scripts/testcard.swift)
# scrolling 6 px a frame, and ffmpeg's testsrc2 (moving graphics, text and
# gradients over the whole frame).
set -eu
cd "$(dirname "$0")/.."
OUT="${REC_TEST_DIR:-$HOME/Movies/rec-rs-tests}/encquality"
mkdir -p "$OUT"
CASES=${*:-"h264-6m:h264:6000:basic h264-6m:h264:6000 h264-40m:h264:40000:basic h264-40m:h264:40000 hevc-30m:hevc:30000"}
[ -f "$OUT/card.png" ] || swift scripts/testcard.swift "$OUT/card.png"
[ -f "$OUT/scroll.mkv" ] || ffmpeg -v error -loop 1 -framerate 60 -i "$OUT/card.png" -t 2 \
  -vf "scale=3840:2160:flags=lanczos,scroll=v=0.0028,format=bgra" -c:v ffv1 "$OUT/scroll.mkv"
[ -f "$OUT/motion.mkv" ] || ffmpeg -v error -f lavfi -i "testsrc2=s=3840x2160:r=60" -t 2 \
  -vf format=bgra -c:v ffv1 "$OUT/motion.mkv"
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.cache/rec-rs-target}
export CARGO_TARGET_DIR
cargo test --release --no-run -q 2>/dev/null
for src in scroll motion; do
  for c in $CASES; do
    name=${c%%:*}; rest=${c#*:}; codec=${rest%%:*}; rest=${rest#*:}; kbps=${rest%%:*}
    basic=""; case "$c" in *:basic) basic=1; name="$name-basic" ;; esac
    f="$OUT/$src-$name.$codec"
    REC_LIVE_BASIC=${basic:+1} REC_ENC_SRC="$OUT/$src.mkv" REC_ENC_OUT="$f" REC_ENC_BITRATE=$((kbps * 1000)) \
      REC_ENC_HEVC=$([ "$codec" = hevc ] && echo 1 || echo 0) \
      sh -c '[ -n "$REC_LIVE_BASIC" ] || unset REC_LIVE_BASIC; cargo test --release -q encode_file -- --ignored --exact live::video::tests::encode_file' >/dev/null 2>&1
    # Pair frames by index: Matroska's millisecond timestamps put 60 fps
    # frames 16/17/17 ms apart, which would mismatch every third frame.
    # Read every stream as BT.709 limited range (what the encoder writes),
    # tagged or not, so tags can't change the comparison.
    ff="[0:v]setpts=N/(60*TB),setparams=range=tv:colorspace=bt709:color_primaries=bt709:color_trc=bt709,format=yuv420p,split=2[d1][d2];"
    ff="$ff[1:v]setpts=N/(60*TB),format=rgb24,scale=in_range=pc:out_range=tv:out_color_matrix=bt709:flags=accurate_rnd+full_chroma_int,format=yuv420p,split=2[r1][r2];"
    ff="$ff[d1][r1]psnr[p];[d2][r2]libvmaf=model=version=vmaf_4k_v0.6.1:n_threads=8[v]"
    ffmpeg -v info -r 60 -i "$f" -i "$OUT/$src.mkv" -filter_complex "$ff" -map "[p]" -f null - -map "[v]" -f null - 2>"$f.metrics" >/dev/null || true
    psnr=$(grep -o 'PSNR y:[0-9.inf]*' "$f.metrics" | tail -1 | cut -d: -f2)
    vmaf=$(grep -o 'VMAF score: [0-9.]*' "$f.metrics" | tail -1 | awk '{print $3}')
    size=$(wc -c <"$f")
    printf '%-7s %-16s %5s kb/s  PSNR-Y %-7s VMAF %-7s (%s Mb/s actual)\n' "$src" "$name" "$kbps" "$psnr" "$vmaf" \
      "$(echo "$size" | awk '{printf "%.1f", $1*8/2/1e6}')"
  done
done
