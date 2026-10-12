//! Real-time audio mixer: system audio + microphone -> one stereo track.
//!
//! Each source's buffers are decoded to 48 kHz stereo f32 and placed on a
//! common sample timeline derived from their host-clock timestamps, so the mix
//! stays in sync with video. Per-source continuity snapping avoids clicks from
//! sub-millisecond timestamp jitter.

use std::collections::VecDeque;
use std::ptr::NonNull;

use objc2_core_audio_types::*;
use objc2_core_foundation::CFRetained;
use objc2_core_media::*;

use crate::util::*;
use crate::writer::AudioKind;

pub const RATE: f64 = 48000.0;
/// Emit even if a source is silent/stalled for longer than this.
const MAX_WAIT: i64 = 24000; // 0.5 s
/// Treat timestamp jumps up to this many samples as continuous.
const SNAP: i64 = 96; // 2 ms

pub struct Mixer {
    t0: i64, // session start in samples on the host clock
    base: i64,
    buf: VecDeque<[f32; 2]>,
    horizon: [Option<i64>; 2],
    active: [bool; 2],
    gain: [f32; 2],
    fmt: CFRetained<CMAudioFormatDescription>,
    /// Samples that would have clipped (the limiter held them back).
    pub clipped: u64,
    resamplers: [Option<Resampler>; 2],
    limiter: Limiter,
}

unsafe impl Send for Mixer {}

fn idx(k: AudioKind) -> usize {
    match k {
        AudioKind::System => 0,
        AudioKind::Mic => 1,
    }
}

/// Decode a PCM sample buffer to (pts seconds, sample rate, stereo frames).
pub fn decode(sb: &CMSampleBuffer) -> Option<(f64, f64, Vec<[f32; 2]>)> {
    unsafe {
        let fd = sb.format_description()?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fd).as_ref()?;
        if asbd.mFormatID != kAudioFormatLinearPCM {
            return None;
        }
        let frames = sb.num_samples() as usize;
        if frames == 0 {
            return None;
        }
        let mut needed = 0usize;
        sb.audio_buffer_list_with_retained_block_buffer(&mut needed, std::ptr::null_mut(), 0, None, None, 0, std::ptr::null_mut());
        if needed == 0 {
            return None;
        }
        let mut storage = vec![0u64; needed.div_ceil(8)];
        let abl = storage.as_mut_ptr() as *mut AudioBufferList;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let st = sb.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            abl,
            needed,
            None,
            None,
            1, // kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment
            &mut block,
        );
        if st != 0 {
            return None;
        }
        let _block = NonNull::new(block).map(|b| CFRetained::from_raw(b));
        let nbuf = (*abl).mNumberBuffers as usize;
        let bufs = std::slice::from_raw_parts((*abl).mBuffers.as_ptr(), nbuf);
        let is_float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
        let non_inter = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        let bits = asbd.mBitsPerChannel;
        let ch = asbd.mChannelsPerFrame.max(1) as usize;
        let read = |b: &AudioBuffer, i: usize| -> f32 {
            match (is_float, bits) {
                (true, 32) => *(b.mData as *const f32).add(i),
                (true, 64) => *(b.mData as *const f64).add(i) as f32,
                (false, 16) => *(b.mData as *const i16).add(i) as f32 / 32768.0,
                (false, 32) => *(b.mData as *const i32).add(i) as f32 / 2147483648.0,
                (false, 24) => {
                    let p = (b.mData as *const u8).add(i * 3);
                    let v = ((*p as i32) | ((*p.add(1) as i32) << 8) | ((*p.add(2) as i32) << 16)) << 8 >> 8;
                    v as f32 / 8388608.0
                }
                _ => 0.0,
            }
        };
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            let (l, r) = if non_inter {
                let l = read(&bufs[0], f);
                let r = if nbuf > 1 { read(&bufs[1], f) } else { l };
                (l, r)
            } else {
                let l = read(&bufs[0], f * ch);
                let r = if ch > 1 { read(&bufs[0], f * ch + 1) } else { l };
                (l, r)
            };
            out.push([l, r]);
        }
        Some((secs(sb.presentation_time_stamp()), asbd.mSampleRate, out))
    }
}

/// Taps on each side of a resampled output sample.
const HALF_TAPS: usize = 24;

/// Streaming sinc resampler to 48 kHz for one source (e.g. a 44.1 kHz USB
/// mic or a 16/24 kHz Bluetooth headset). Kaiser-windowed sinc, low-passed
/// below the lower Nyquist rate, with its filter history and fractional
/// position carried across buffers, so buffer boundaries are seamless.
struct Resampler {
    from: f64,
    /// Input samples not yet fully used, starting at absolute input index
    /// `first`.
    hist: Vec<[f32; 2]>,
    first: i64,
    /// Absolute input position of the next output sample.
    pos: f64,
    /// Host time (seconds) of input sample 0.
    t0: f64,
    /// Absolute input index the next buffer is expected to start at.
    next_in: i64,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..32 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

impl Resampler {
    fn new(from: f64, t0: f64) -> Self {
        Resampler { from, hist: vec![], first: 0, pos: 0.0, t0, next_in: 0 }
    }

    /// Resample a buffer starting at host time `pts`. Returns the output
    /// frames and the host time of the first one. A buffer that does not
    /// continue the stream (gap, overlap, rate change) restarts it.
    fn push(&mut self, pts: f64, rate: f64, src: Vec<[f32; 2]>) -> (f64, Vec<[f32; 2]>) {
        let expected = self.t0 + self.next_in as f64 / self.from;
        if rate != self.from || (pts - expected).abs() > SNAP as f64 / RATE {
            *self = Resampler::new(rate, pts);
        }
        self.next_in += src.len() as i64;
        self.hist.extend(src);
        let step = self.from / RATE;
        // Cutoff (cycles per input sample): below both Nyquist rates.
        let fc = 0.5 * (RATE / self.from).min(1.0) * 0.94;
        let beta = 8.0;
        let norm = bessel_i0(beta);
        let mut out = vec![];
        let t_out = self.t0 + self.pos / self.from;
        let h = HALF_TAPS as f64 / (2.0 * fc).min(1.0);
        let reach = h.ceil() as i64;
        // Taps before the stream's start read as silence.
        while (self.pos.floor() as i64) + reach < self.first + self.hist.len() as i64 {
            let c = self.pos.floor() as i64;
            let (mut l, mut r, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
            for j in (c - reach + 1)..=(c + reach) {
                let x = j as f64 - self.pos;
                if x.abs() >= h {
                    continue;
                }
                let arg = 2.0 * fc * x;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg) };
                let w = bessel_i0(beta * (1.0 - (x / h).powi(2)).max(0.0).sqrt()) / norm;
                let k = 2.0 * fc * sinc * w;
                wsum += k;
                if j >= self.first {
                    let s = self.hist[(j - self.first) as usize];
                    l += k * s[0] as f64;
                    r += k * s[1] as f64;
                }
            }
            // Unity gain at DC whatever the fractional phase.
            let g = if wsum > 0.0 { 1.0 / wsum } else { 0.0 };
            out.push([(l * g) as f32, (r * g) as f32]);
            self.pos += step;
        }
        let keep_from = (self.pos.floor() as i64 - reach).max(self.first);
        let drop = (keep_from - self.first) as usize;
        self.hist.drain(..drop.min(self.hist.len()));
        self.first += drop as i64;
        (t_out, out)
    }
}

/// Mix bus limiter: keeps summed sources under the ceiling with smooth gain
/// changes (fast attack, slow release) and a soft knee for the rare peak
/// the envelope misses, instead of clipping them flat.
struct Limiter {
    gain: f32,
}

/// -1 dBFS: headroom for the AAC encoder, whose output overshoots its
/// input peaks slightly.
const CEILING: f32 = 0.891;
const ATTACK: f32 = 0.35; // per sample, ~0.06 ms
const RELEASE: f32 = 1.0 / (0.15 * RATE as f32); // full recovery in ~150 ms

impl Limiter {
    fn process(&mut self, fr: [f32; 2]) -> [f32; 2] {
        let peak = fr[0].abs().max(fr[1].abs());
        let want = if peak > CEILING { CEILING / peak } else { 1.0 };
        self.gain = if want < self.gain { self.gain + (want - self.gain) * ATTACK } else { (self.gain + RELEASE).min(want) };
        fr.map(|s| soft_clip(s * self.gain))
    }
}

/// Identity below the knee, then a smooth curve that never passes 1.0.
fn soft_clip(s: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = s.abs();
    if a <= KNEE {
        return s;
    }
    let room = 1.0 - KNEE;
    (KNEE + room * ((a - KNEE) / room).tanh()).copysign(s)
}

impl Mixer {
    pub fn new(t0_secs: f64, system: bool, mic: bool, gain_db: [f32; 2]) -> Self {
        Mixer {
            t0: (t0_secs * RATE).round() as i64,
            base: 0,
            buf: VecDeque::new(),
            horizon: [None, None],
            active: [system, mic],
            gain: gain_db.map(|db| 10f32.powf(db / 20.0)),
            fmt: stereo_format(),
            clipped: 0,
            resamplers: [None, None],
            limiter: Limiter { gain: 1.0 },
        }
    }
    /// Add decoded frames that start at `pts` (seconds on the output timeline).
    pub fn push(&mut self, kind: AudioKind, pts: f64, rate: f64, frames: Vec<[f32; 2]>) {
        let i = idx(kind);
        let (pts, frames) = if (rate - RATE).abs() < 0.5 || rate <= 0.0 {
            self.resamplers[i] = None;
            (pts, frames)
        } else {
            self.resamplers[i].get_or_insert_with(|| Resampler::new(rate, pts)).push(pts, rate, frames)
        };
        if frames.is_empty() {
            return;
        }
        let mut start = (pts * RATE).round() as i64 - self.t0;
        if let Some(h) = self.horizon[i]
            && (start - h).abs() <= SNAP {
                start = h;
            }
        let end = start + frames.len() as i64;
        self.horizon[i] = Some(self.horizon[i].map_or(end, |h| h.max(end)));
        let need = (end - self.base).max(0) as usize;
        if self.buf.len() < need {
            self.buf.resize(need, [0.0, 0.0]);
        }
        let g = self.gain[i];
        for (n, fr) in frames.iter().enumerate() {
            let pos = start + n as i64 - self.base;
            if pos < 0 {
                continue; // already emitted
            }
            let slot = &mut self.buf[pos as usize];
            slot[0] += fr[0] * g;
            slot[1] += fr[1] * g;
        }
    }

    /// Mixed frames ready (see `pull`) with their start time in host seconds.
    pub fn pull_frames(&mut self, flush: bool) -> Option<(f64, Vec<[f32; 2]>)> {
        let hs: Vec<i64> = (0..2).filter(|&i| self.active[i]).map(|i| self.horizon[i].unwrap_or(0)).collect();
        let max_h = hs.iter().copied().max().unwrap_or(0);
        let until = if flush { max_h } else { hs.iter().copied().min().unwrap_or(0).max(max_h - MAX_WAIT) };
        let n = (until - self.base).min(self.buf.len() as i64);
        if n <= 0 {
            return None;
        }
        let t = (self.t0 + self.base) as f64 / RATE;
        self.base += n;
        let mut frames: Vec<[f32; 2]> = self.buf.drain(..n as usize).collect();
        for fr in &mut frames {
            self.clipped += fr.iter().filter(|s| s.abs() > 1.0).count() as u64;
            *fr = self.limiter.process(*fr);
        }
        Some((t, frames))
    }

    /// Mixed PCM ready to encode: everything all active sources have reached,
    /// or anything older than MAX_WAIT behind the newest source.
    pub fn pull(&mut self, flush: bool) -> Option<CFRetained<CMSampleBuffer>> {
        let (t, frames) = self.pull_frames(flush)?;
        let data: Vec<f32> = frames.into_iter().flatten().collect();
        let pts = unsafe { CMTime::new((t * RATE).round() as i64, RATE as i32) };
        pcm_buffer(&self.fmt, data, pts)
    }
}

/// 48 kHz interleaved stereo f32 PCM.
pub fn stereo_format() -> CFRetained<CMAudioFormatDescription> {
    let asbd = AudioStreamBasicDescription {
        mSampleRate: RATE,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut fd: *const CMAudioFormatDescription = std::ptr::null();
    unsafe {
        CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(&asbd),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            None,
            NonNull::from(&mut fd),
        );
        CFRetained::from_raw(NonNull::new(fd as *mut CMAudioFormatDescription).expect("audio format"))
    }
}

/// Wrap interleaved stereo samples (`stereo_format`) starting at `pts` in a
/// sample buffer.
pub fn pcm_buffer(
    fmt: &CMAudioFormatDescription,
    mut data: Vec<f32>,
    pts: CMTime,
) -> Option<CFRetained<CMSampleBuffer>> {
    let n = data.len() / 2;
    unsafe {
        let bytes = data.len() * 4;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        if CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            bytes,
            None,
            std::ptr::null(),
            0,
            bytes,
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        ) != 0
        {
            return None;
        }
        let block = CFRetained::from_raw(NonNull::new(block)?);
        CMBlockBuffer::replace_data_bytes(NonNull::new(data.as_mut_ptr() as *mut _)?, &block, 0, bytes);
        let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
        if CMAudioSampleBufferCreateReadyWithPacketDescriptions(
            None,
            &block,
            fmt,
            n as isize,
            pts,
            std::ptr::null(),
            NonNull::from(&mut out),
        ) != 0
        {
            return None;
        }
        Some(CFRetained::from_raw(NonNull::new(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull_all(m: &mut Mixer, flush: bool) -> (Option<f64>, Vec<[f32; 2]>) {
        let mut first = None;
        let mut out = vec![];
        while let Some(b) = m.pull(flush) {
            let (pts, rate, frames) = decode(&b).expect("mix buffer decodes");
            assert_eq!(rate, RATE);
            first.get_or_insert(pts);
            out.extend(frames);
        }
        (first, out)
    }

    #[test]
    fn sums_sources_on_a_shared_timeline() {
        let mut m = Mixer::new(10.0, true, true, [0.0, 0.0]);
        m.push(AudioKind::System, 10.0, RATE, vec![[0.25, 0.25]; 480]);
        // Nothing until the mic reaches the same point (it may still arrive).
        assert!(m.pull(false).is_none());
        // Mic starts 5 ms later, mono duplicated to both channels.
        m.push(AudioKind::Mic, 10.005, RATE, vec![[0.5, 0.5]; 480]);
        let (pts, out) = pull_all(&mut m, false);
        assert_eq!(pts, Some(10.0));
        assert_eq!(out.len(), 480, "only up to the slower source's horizon");
        assert_eq!(out[0], [0.25, 0.25]);
        assert_eq!(out[240], [0.75, 0.75], "overlap is summed");
        let (pts, rest) = pull_all(&mut m, true);
        assert_eq!(pts, Some(10.01));
        assert_eq!(rest.len(), 240);
        assert_eq!(rest[0], [0.5, 0.5]);
    }

    #[test]
    fn gain_and_limiting() {
        // +6 dB: 0.2 -> 0.399, untouched (under the ceiling).
        let mut m = Mixer::new(0.0, false, true, [0.0, 6.0]);
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.2, -0.3]; 100]);
        let (_, out) = pull_all(&mut m, true);
        assert!((out[50][0] - 0.399).abs() < 0.01);
        assert!((out[50][1] + 0.599).abs() < 0.01);
        assert_eq!(m.clipped, 0);
        // Too loud: held under full scale (both channels by the same gain,
        // so the stereo image keeps), never flattened at 1.0.
        let mut m = Mixer::new(0.0, false, true, [0.0, 6.0]);
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.4, -0.6]; 4800]);
        let (_, out) = pull_all(&mut m, true);
        assert!(out.iter().all(|f| f[0].abs() < 1.0 && f[1].abs() < 1.0));
        let last = out[4799];
        assert!((last[1].abs() - CEILING).abs() < 0.01, "{last:?}");
        assert!((last[0] / last[1] + 0.4 / 0.6).abs() < 0.01);
        assert_eq!(m.clipped, 4800);
    }

    /// A sine through the resampler in uneven buffers: the right length and
    /// pitch, seamless across buffers (error against the ideal 48 kHz sine).
    fn resampled_sine_error(from: f64, freq: f64, chunk: usize) -> (usize, f64) {
        let mut m = Mixer::new(0.0, false, true, [0.0, 0.0]);
        let n = from as usize; // 1 s
        let sine = |t: f64| (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5;
        let mut i = 0;
        while i < n {
            let len = chunk + (i / chunk) % 7; // uneven buffer sizes
            let end = (i + len).min(n);
            let frames: Vec<[f32; 2]> = (i..end).map(|k| { let v = sine(k as f64 / from); [v, v] }).collect();
            m.push(AudioKind::Mic, i as f64 / from, from, frames);
            i = end;
        }
        let (pts, out) = pull_all(&mut m, true);
        let t0 = pts.unwrap();
        // Compare away from the stream's start and end (filter ramp).
        let err = (2400..out.len() - 2400)
            .map(|k| (out[k][0] - sine(t0 + k as f64 / RATE)).abs() as f64)
            .fold(0.0, f64::max);
        (out.len(), err)
    }

    #[test]
    fn resamples_seamlessly() {
        for (from, chunk) in [(44100.0, 441), (16000.0, 160), (24000.0, 512), (96000.0, 1024)] {
            let (len, err) = resampled_sine_error(from, 1000.0, chunk);
            // The last ~1.6 ms wait in the filter for input that never came.
            assert!((len as i64 - 48000).abs() <= 100, "{from}: {len} samples");
            assert!(err < 0.002, "{from}: max error {err}");
        }
    }

    #[test]
    fn resampler_removes_content_above_nyquist() {
        // 20 kHz at 96 kHz cannot pass to 48 kHz unaliased, but is under
        // 24 kHz; 30 kHz must be filtered out rather than fold to 18 kHz.
        let mut m = Mixer::new(0.0, false, true, [0.0, 0.0]);
        let frames: Vec<[f32; 2]> = (0..96000)
            .map(|k| { let v = (2.0 * std::f64::consts::PI * 30000.0 * k as f64 / 96000.0).sin() as f32 * 0.5; [v, v] })
            .collect();
        m.push(AudioKind::Mic, 0.0, 96000.0, frames);
        let (_, out) = pull_all(&mut m, true);
        let peak = out[2400..out.len() - 2400].iter().fold(0f32, |a, f| a.max(f[0].abs()));
        assert!(peak < 0.005, "aliased residue {peak}");
    }

    #[test]
    fn small_jitter_snaps_to_continuous() {
        let mut m = Mixer::new(0.0, true, false, [0.0, 0.0]);
        m.push(AudioKind::System, 0.0, RATE, vec![[0.1, 0.1]; 480]);
        // 1 ms late: within SNAP, so no gap of silence is inserted.
        m.push(AudioKind::System, 0.011, RATE, vec![[0.1, 0.1]; 480]);
        let (_, out) = pull_all(&mut m, true);
        assert_eq!(out.len(), 960);
        assert!(out.iter().all(|f| f[0] == 0.1));
    }


}
