//! AAC and Opus encoding of 48 kHz stereo f32 PCM with AudioToolbox's
//! AudioConverter (hardware-independent, part of macOS).

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{Result, bail};
use objc2_core_audio_types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioCodec {
    Aac,
    Opus,
}

impl AudioCodec {
    pub fn frames_per_packet(self) -> usize {
        match self {
            AudioCodec::Aac => 1024,
            AudioCodec::Opus => 960,
        }
    }
}

/// One encoded audio packet. `pts` is in host-clock seconds.
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub codec: AudioCodec,
    pub pts: f64,
    pub frames: u32,
    pub data: Arc<Vec<u8>>,
}

type ConverterRef = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct PrimeInfo {
    leading: u32,
    trailing: u32,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(src: *const AudioStreamBasicDescription, dst: *const AudioStreamBasicDescription, out: *mut ConverterRef) -> i32;
    fn AudioConverterDispose(c: ConverterRef) -> i32;
    fn AudioConverterSetProperty(c: ConverterRef, id: u32, size: u32, data: *const c_void) -> i32;
    fn AudioConverterGetPropertyInfo(c: ConverterRef, id: u32, size: *mut u32, writable: *mut u8) -> i32;
    fn AudioConverterGetProperty(c: ConverterRef, id: u32, size: *mut u32, data: *mut c_void) -> i32;
    fn AudioConverterFillComplexBuffer(
        c: ConverterRef,
        input: unsafe extern "C" fn(ConverterRef, *mut u32, *mut AudioBufferList, *mut *mut AudioStreamPacketDescription, *mut c_void) -> i32,
        user: *mut c_void,
        out_packets: *mut u32,
        out_data: *mut AudioBufferList,
        out_desc: *mut AudioStreamPacketDescription,
    ) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const BIT_RATE: u32 = fourcc(b"brat");
const MAGIC_COOKIE: u32 = fourcc(b"cmgc"); // kAudioConverterCompressionMagicCookie
const PRIME_INFO: u32 = fourcc(b"prim");
const MAX_OUT_PACKET: u32 = fourcc(b"xops");
/// Returned by the input callback when the pending PCM is used up.
const NO_MORE_INPUT: i32 = 0x6e6d6f72; // 'nmor'

pub const RATE: f64 = 48000.0;

pub struct AudioEncoder {
    conv: ConverterRef,
    pub codec: AudioCodec,
    /// Interleaved stereo samples not yet encoded.
    pending: Vec<f32>,
    /// Host time of the first pending frame.
    pending_t: Option<f64>,
    /// Output packets so far and the encoder's priming delay.
    packets: u64,
    leading: u32,
    first_t: Option<f64>,
    max_packet: usize,
    /// Codec configuration (AAC magic cookie / esds) for containers.
    pub cookie: Vec<u8>,
    /// Scratch buffer handed to the converter in the input callback.
    feed: Vec<f32>,
}

unsafe impl Send for AudioEncoder {}

struct Feed {
    data: *mut f32,
    frames: u32,
    given: bool,
}

unsafe extern "C" fn input_proc(
    _c: ConverterRef,
    packets: *mut u32,
    abl: *mut AudioBufferList,
    _desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    unsafe {
        let feed = &mut *(user as *mut Feed);
        if feed.given || feed.frames == 0 {
            *packets = 0;
            return NO_MORE_INPUT;
        }
        feed.given = true;
        *packets = feed.frames;
        let b = &mut (*abl).mBuffers[0];
        b.mNumberChannels = 2;
        b.mDataByteSize = feed.frames * 8;
        b.mData = feed.data as *mut c_void;
        0
    }
}

impl AudioEncoder {
    /// `bitrate` in bits per second.
    pub fn new(codec: AudioCodec, bitrate: u32) -> Result<AudioEncoder> {
        let src = AudioStreamBasicDescription {
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
        let dst = AudioStreamBasicDescription {
            mSampleRate: RATE,
            mFormatID: match codec {
                AudioCodec::Aac => kAudioFormatMPEG4AAC,
                AudioCodec::Opus => kAudioFormatOpus,
            },
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: codec.frames_per_packet() as u32,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut conv: ConverterRef = std::ptr::null_mut();
        let st = unsafe { AudioConverterNew(&src, &dst, &mut conv) };
        if st != 0 || conv.is_null() {
            bail!("{codec:?} encoder unavailable (AudioConverterNew {st})");
        }
        let mut enc = AudioEncoder {
            conv,
            codec,
            pending: vec![],
            pending_t: None,
            packets: 0,
            leading: 0,
            first_t: None,
            max_packet: 2048,
            cookie: vec![],
            feed: vec![],
        };
        unsafe {
            let st = AudioConverterSetProperty(conv, BIT_RATE, 4, &bitrate as *const u32 as *const c_void);
            if st != 0 {
                eprintln!("rec: warning: {codec:?} bitrate {bitrate} not accepted ({st})");
            }
            let mut prime = PrimeInfo::default();
            let mut size = std::mem::size_of::<PrimeInfo>() as u32;
            if AudioConverterGetProperty(conv, PRIME_INFO, &mut size, &mut prime as *mut _ as *mut c_void) == 0 {
                enc.leading = prime.leading;
            }
            let mut maxp = 0u32;
            let mut size = 4;
            if AudioConverterGetProperty(conv, MAX_OUT_PACKET, &mut size, &mut maxp as *mut _ as *mut c_void) == 0 && maxp > 0 {
                enc.max_packet = maxp as usize;
            }
        }
        enc.fetch_cookie();
        Ok(enc)
    }

    /// The codec configuration; some encoders only have it after the first
    /// packet.
    fn fetch_cookie(&mut self) {
        if !self.cookie.is_empty() {
            return;
        }
        unsafe {
            let (mut size, mut w) = (0u32, 0u8);
            if AudioConverterGetPropertyInfo(self.conv, MAGIC_COOKIE, &mut size, &mut w) == 0 && size > 0 {
                let mut cookie = vec![0u8; size as usize];
                if AudioConverterGetProperty(self.conv, MAGIC_COOKIE, &mut size, cookie.as_mut_ptr() as *mut c_void) == 0 {
                    cookie.truncate(size as usize);
                    self.cookie = cookie;
                }
            }
        }
    }

    /// Encoder delay in frames (AAC priming).
    pub fn priming(&self) -> u32 {
        self.leading
    }

    /// Add interleaved stereo frames starting at host time `t`; returns
    /// the packets that are complete. A gap or overlap in `t` restarts the
    /// timeline at `t` (the encoder itself just continues).
    pub fn push(&mut self, t: f64, frames: &[[f32; 2]]) -> Vec<AudioPacket> {
        let expected = self.pending_t.map(|p| p + (self.pending.len() / 2) as f64 / RATE);
        if self.pending_t.is_none() || expected.is_some_and(|e| (e - t).abs() > 0.005) {
            if !self.pending.is_empty() {
                // Discontinuity: drop the partial packet, re-anchor.
                self.pending.clear();
            }
            self.pending_t = Some(t);
            if self.first_t.is_none() {
                self.first_t = Some(t);
            }
            // Keep packet times anchored to real time across gaps.
            let start = self.first_t.unwrap();
            let fpp = self.codec.frames_per_packet() as f64;
            // Packet n starts at start + (n*fpp - leading)/RATE, so the next
            // packet after a gap at `t` is n = (t - start)*RATE/fpp.
            self.packets = ((t - start) * RATE / fpp).round().max(self.packets as f64) as u64;
        }
        for f in frames {
            self.pending.extend_from_slice(f);
        }
        let fpp = self.codec.frames_per_packet();
        let mut out = vec![];
        while self.pending.len() >= fpp * 2 {
            self.feed.clear();
            self.feed.extend(self.pending.drain(..fpp * 2));
            self.pending_t = Some(self.pending_t.unwrap() + fpp as f64 / RATE);
            if let Some(p) = self.encode_one() {
                out.push(p);
            }
        }
        out
    }

    fn encode_one(&mut self) -> Option<AudioPacket> {
        let fpp = self.codec.frames_per_packet();
        let mut feed = Feed { data: self.feed.as_mut_ptr(), frames: fpp as u32, given: false };
        let mut buf = vec![0u8; self.max_packet];
        let mut abl = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer { mNumberChannels: 2, mDataByteSize: buf.len() as u32, mData: buf.as_mut_ptr() as *mut c_void }],
        };
        let mut n = 1u32;
        let mut desc = AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: 0 };
        let st = unsafe {
            AudioConverterFillComplexBuffer(self.conv, input_proc, &mut feed as *mut Feed as *mut c_void, &mut n, &mut abl, &mut desc)
        };
        if (st != 0 && st != NO_MORE_INPUT) || n == 0 {
            return None;
        }
        let size = abl.mBuffers[0].mDataByteSize as usize;
        buf.truncate(size);
        self.fetch_cookie();
        // Packet n (counting the priming packets) starts at frame n*fpp of
        // the encoder's output; subtract the priming delay.
        let start = self.first_t.unwrap_or(0.0);
        let pts = start + (self.packets as f64 * fpp as f64 - self.leading as f64) / RATE;
        self.packets += 1;
        Some(AudioPacket { codec: self.codec, pts, frames: fpp as u32, data: Arc::new(buf) })
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.conv) };
    }
}

const DECOMPRESSION_COOKIE: u32 = fourcc(b"dmgc");

/// Decodes AAC packets (from `AudioEncoder`) back to 48 kHz stereo f32.
pub struct AacDecoder {
    conv: ConverterRef,
}

unsafe impl Send for AacDecoder {}

struct DecFeed<'a> {
    data: &'a [u8],
    desc: AudioStreamPacketDescription,
    given: bool,
}

unsafe extern "C" fn dec_input(
    _c: ConverterRef,
    packets: *mut u32,
    abl: *mut AudioBufferList,
    desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    unsafe {
        let f = &mut *(user as *mut DecFeed);
        if f.given {
            *packets = 0;
            return NO_MORE_INPUT;
        }
        f.given = true;
        *packets = 1;
        let b = &mut (*abl).mBuffers[0];
        b.mNumberChannels = 2;
        b.mDataByteSize = f.data.len() as u32;
        b.mData = f.data.as_ptr() as *mut c_void;
        if !desc.is_null() {
            *desc = &mut f.desc;
        }
        0
    }
}

impl AacDecoder {
    pub fn new(cookie: &[u8]) -> Result<AacDecoder> {
        let src = AudioStreamBasicDescription {
            mSampleRate: RATE,
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: 1024,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let dst = AudioStreamBasicDescription {
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
        let mut conv: ConverterRef = std::ptr::null_mut();
        let st = unsafe { AudioConverterNew(&src, &dst, &mut conv) };
        if st != 0 || conv.is_null() {
            bail!("AAC decoder unavailable ({st})");
        }
        if !cookie.is_empty() {
            unsafe { AudioConverterSetProperty(conv, DECOMPRESSION_COOKIE, cookie.len() as u32, cookie.as_ptr() as *const c_void) };
        }
        Ok(AacDecoder { conv })
    }

    /// Decode one packet. The first decoded packet of a stream is the
    /// decoder's warm-up; sample timing follows `AudioPacket::pts`.
    pub fn decode(&mut self, p: &AudioPacket) -> Vec<[f32; 2]> {
        let mut out = vec![[0f32; 2]; 1024];
        let mut feed = DecFeed {
            data: &p.data,
            desc: AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: p.data.len() as u32 },
            given: false,
        };
        let mut abl = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer { mNumberChannels: 2, mDataByteSize: 1024 * 8, mData: out.as_mut_ptr() as *mut c_void }],
        };
        let mut n = 1024u32;
        let st = unsafe {
            AudioConverterFillComplexBuffer(self.conv, dec_input, &mut feed as *mut DecFeed as *mut c_void, &mut n, &mut abl, std::ptr::null_mut())
        };
        if st != 0 && st != NO_MORE_INPUT {
            n = 0;
        }
        out.truncate(n as usize);
        out
    }
}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.conv) };
    }
}

/// AudioSpecificConfig for AAC-LC at 48 kHz stereo (FLV/RTMP sequence header).
pub const AAC_ASC: [u8; 2] = [0x11, 0x90];

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize) -> Vec<[f32; 2]> {
        (0..n).map(|i| {
            let v = (i as f32 * 0.05).sin() * 0.3;
            [v, v]
        }).collect()
    }

    #[test]
    fn aac_packets_with_timestamps() {
        let mut e = AudioEncoder::new(AudioCodec::Aac, 160_000).unwrap();
        let mut pk = e.push(100.0, &tone(48000));
        assert!(!e.cookie.is_empty(), "AAC magic cookie after the first packets");
        pk.extend(e.push(101.0, &tone(48000)));
        assert!(pk.len() >= 90, "about 93 packets for 2 s, got {}", pk.len());
        assert!(pk.iter().all(|p| !p.data.is_empty() && p.data.len() < 2048));
        // Consecutive packets are 1024 frames apart.
        let d = pk[10].pts - pk[9].pts;
        assert!((d - 1024.0 / 48000.0).abs() < 1e-9);
        // Timestamps are anchored to the input clock (priming subtracted).
        let first = pk[0].pts;
        assert!((first - (100.0 - e.priming() as f64 / RATE)).abs() < 0.03, "first {first}");
    }

    #[test]
    fn opus_packets() {
        let mut e = AudioEncoder::new(AudioCodec::Opus, 128_000).unwrap();
        let pk = e.push(5.0, &tone(48000));
        assert!(pk.len() >= 45, "{} packets", pk.len());
        assert!(pk.iter().all(|p| p.frames == 960 && !p.data.is_empty()));
    }

    #[test]
    fn gap_reanchors_timeline() {
        let mut e = AudioEncoder::new(AudioCodec::Aac, 128_000).unwrap();
        let a = e.push(10.0, &tone(4800));
        let b = e.push(20.0, &tone(48000));
        let last_a = a.last().map_or(0.0, |p| p.pts);
        assert!(b.first().unwrap().pts > last_a + 9.0, "after the gap, packets follow the new time");
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    /// A click encoded and decoded again comes back at its input time when
    /// decoded samples are placed from each packet's `pts`.
    #[test]
    fn round_trip_keeps_timing() {
        let mut e = AudioEncoder::new(AudioCodec::Aac, 160_000).unwrap();
        let click = 20_000usize;
        let input: Vec<[f32; 2]> = (0..48000).map(|i| if (click..click + 48).contains(&i) { [0.9, 0.9] } else { [0.0, 0.0] }).collect();
        let pk = e.push(0.0, &input);
        let mut d = AacDecoder::new(&e.cookie).unwrap();
        // Start mid-stream, like a replay clip, with two packets of warm-up.
        let start = 10;
        let mut onset = None;
        for p in &pk[start..] {
            let out = d.decode(p);
            if p.pts < pk[start + 2].pts {
                continue;
            }
            if let Some(i) = out.iter().position(|f| f[0].abs() > 0.3) {
                onset = Some(p.pts + i as f64 / RATE);
                break;
            }
        }
        let t = onset.expect("click decoded");
        assert!((t - click as f64 / RATE).abs() < 0.0015, "click at {t:.4}s, expected {:.4}s", click as f64 / RATE);
    }
}
