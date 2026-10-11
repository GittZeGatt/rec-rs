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
    pub clipped: u64,
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

fn resample(src: Vec<[f32; 2]>, from: f64) -> Vec<[f32; 2]> {
    if (from - RATE).abs() < 0.5 || from <= 0.0 || src.len() < 2 {
        return src;
    }
    let n = ((src.len() as f64) * RATE / from).round() as usize;
    let step = from / RATE;
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let j = (x as usize).min(src.len() - 1);
            let k = (j + 1).min(src.len() - 1);
            let t = (x - j as f64) as f32;
            [src[j][0] + (src[k][0] - src[j][0]) * t, src[j][1] + (src[k][1] - src[j][1]) * t]
        })
        .collect()
}

impl Mixer {
    pub fn new(t0_secs: f64, system: bool, mic: bool, gain_db: [f32; 2]) -> Self {
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
        let fmt = unsafe {
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
        };
        Mixer {
            t0: (t0_secs * RATE).round() as i64,
            base: 0,
            buf: VecDeque::new(),
            horizon: [None, None],
            active: [system, mic],
            gain: gain_db.map(|db| 10f32.powf(db / 20.0)),
            fmt,
            clipped: 0,
        }
    }

    pub fn push(&mut self, kind: AudioKind, sb: &CMSampleBuffer) {
        let Some((pts, rate, frames)) = decode(sb) else { return };
        let frames = resample(frames, rate);
        let i = idx(kind);
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

    /// Mixed PCM ready to encode: everything all active sources have reached,
    /// or anything older than MAX_WAIT behind the newest source.
    pub fn pull(&mut self, flush: bool) -> Option<CFRetained<CMSampleBuffer>> {
        let hs: Vec<i64> = (0..2).filter(|&i| self.active[i]).map(|i| self.horizon[i].unwrap_or(0)).collect();
        let max_h = hs.iter().copied().max().unwrap_or(0);
        let until = if flush { max_h } else { hs.iter().copied().min().unwrap_or(0).max(max_h - MAX_WAIT) };
        let n = (until - self.base).min(self.buf.len() as i64);
        if n <= 0 {
            return None;
        }
        let n = n as usize;
        let mut data: Vec<f32> = Vec::with_capacity(n * 2);
        for fr in self.buf.drain(..n) {
            for s in fr {
                if s.abs() > 1.0 {
                    self.clipped += 1;
                }
                data.push(s.clamp(-1.0, 1.0));
            }
        }
        let pts = unsafe { CMTime::new(self.t0 + self.base, RATE as i32) };
        self.base += n as i64;
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
                &self.fmt,
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
}
