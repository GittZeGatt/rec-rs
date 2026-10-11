#!/usr/bin/env python3
"""A/V sync check for a recording of a flash + beep source.

Finds the frames where the picture flashes to white and the onsets of beeps
in each audio track, pairs each flash with the nearest beep, and reports the
offset (audio minus video, ms). Sources: `rec-rs record --test-pattern`
(pipeline check) or a played-back flash/beep clip (end-to-end capture check).

Usage: synccheck.py FILE [--max-ms 20] [--audio 0,1] [--region x,y,w,h (0..1 fractions)]
Exit status 1 if no flashes are paired or any |offset| exceeds --max-ms.
"""
import json, subprocess, sys, struct, argparse

def run(cmd):
    return subprocess.run(cmd, check=True, capture_output=True).stdout

def probe(path):
    j = json.loads(run(["ffprobe", "-v", "error", "-show_streams", "-of", "json", path]))
    return j["streams"]

def flashes(path, fps, start, region):
    vf = "scale=64:36"
    if region:
        x, y, w, h = region
        vf = f"crop=iw*{w}:ih*{h}:iw*{x}:ih*{y}," + vf
    raw = run(["ffmpeg", "-v", "error", "-i", path, "-map", "0:v:0", "-vf", vf,
               "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
    n = 64 * 36
    lum = [sum(raw[i:i + n]) / n for i in range(0, len(raw) - n + 1, n)]
    out, prev = [], 0.0
    for i, l in enumerate(lum):
        if l > 180 and prev < 140:
            out.append(start + i / fps)
        prev = l
    return out, len(lum)

def onsets(path, idx, start=0.0, thresh=0.1, quiet=0.2):
    raw = run(["ffmpeg", "-v", "error", "-i", path, "-map", f"0:a:{idx}", "-ac", "1",
               "-ar", "48000", "-f", "f32le", "-"])
    s = struct.unpack(f"<{len(raw) // 4}f", raw)
    out, last = [], -1e9
    for i, v in enumerate(s):
        if abs(v) > thresh:
            if (i - last) / 48000 > quiet:
                out.append(start + i / 48000)
            last = i
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--max-ms", type=float, default=20.0)
    ap.add_argument("--region", help="x,y,w,h as fractions of the frame")
    ap.add_argument("--audio", help="audio track indexes to check (default: all with beeps)")
    ap.add_argument("-v", action="store_true", help="print every flash/beep pair")
    a = ap.parse_args()
    region = [float(v) for v in a.region.split(",")] if a.region else None
    streams = probe(a.file)
    v = next(s for s in streams if s["codec_type"] == "video")
    num, den = map(int, v["avg_frame_rate"].split("/"))
    fps = num / den
    fl, nframes = flashes(a.file, fps, float(v.get("start_time", 0)), region)
    print(f"video: {nframes} frames @ {fps:g} fps, {len(fl)} flashes")
    ok, any_pair = True, False
    audio = [s for s in streams if s["codec_type"] == "audio"]
    pick = {int(x) for x in a.audio.split(",")} if a.audio else None
    for i, s in enumerate(audio):
        if pick is not None and i not in pick:
            continue
        on = onsets(a.file, i, float(s.get("start_time", 0)))
        if not on:
            print(f"audio {i}: no beeps (skipped)")
            continue
        offs = []
        for f in fl:
            b = min(on, key=lambda t: abs(t - f))
            if abs(b - f) < 0.2:
                offs.append((b - f) * 1000)
                if a.v:
                    print(f"  flash {f:7.3f}s  beep {b:7.3f}s  {1000*(b-f):+6.1f} ms")
        if not offs:
            print(f"audio {i}: {len(on)} beeps, none near a flash")
            ok = False
            continue
        any_pair = True
        worst = max(offs, key=abs)
        mean = sum(offs) / len(offs)
        bad = abs(worst) > a.max_ms
        ok &= not bad
        print(f"audio {i}: {len(offs)}/{len(fl)} flashes paired, offset mean {mean:+.1f} ms, "
              f"worst {worst:+.1f} ms, range [{min(offs):+.1f}, {max(offs):+.1f}]"
              + ("  FAIL" if bad else ""))
    if not any_pair:
        print("no flash/beep pairs found")
        ok = False
    print("SYNC OK" if ok else "SYNC FAIL")
    sys.exit(0 if ok else 1)

main()
