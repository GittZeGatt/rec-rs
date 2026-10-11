//! Synthetic "screen": a generated moving test pattern on the host clock.
//! Used for self-tests and encoder stress tests without Screen Recording access.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_video::*;

use crate::record::Shared;
use crate::util::*;

pub struct Synth {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

/// Length of the beep that starts with every white flash.
pub const BEEP_SECS: f64 = 0.04;

/// Samples [start, start+n) of the synthetic "system audio": a 1 kHz beep at
/// the start of every second (when the video flashes), silence otherwise.
pub fn beep_samples(start: i64, n: usize) -> Vec<f32> {
    let rate = crate::mixer::RATE as i64;
    let len = (BEEP_SECS * rate as f64) as i64;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n as i64 {
        let s = start + i;
        let in_sec = s.rem_euclid(rate);
        let v = if in_sec < len {
            0.5 * (2.0 * std::f64::consts::PI * 1000.0 * in_sec as f64 / rate as f64).sin() as f32
        } else {
            0.0
        };
        out.push(v);
        out.push(v);
    }
    out
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Draw frame `n`. Every `fps` frames (once per second) the frame is solid
/// white so A/V offsets can be measured against an audio click.
fn draw(pb: &CVPixelBuffer, n: u64, fps: u64, noise: bool, rng: &mut Rng) {
    unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0));
        let w = CVPixelBufferGetWidth(pb);
        let h = CVPixelBufferGetHeight(pb);
        let flash = n.is_multiple_of(fps);
        let box_x = ((n * 12) as usize) % w.saturating_sub(200).max(1);
        let box_y = h / 3;
        let planar = CVPixelBufferGetPlaneCount(pb) > 0;
        let block = 32usize;
        let row_noise = |rng: &mut Rng, x: usize, y: usize| -> u8 {
            if noise && ((x / block) + (y / block) + n as usize).is_multiple_of(3) {
                (rng.next() & 0xff) as u8
            } else {
                0
            }
        };
        if planar {
            let y_base = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *mut u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
            for y in 0..h {
                let row = std::slice::from_raw_parts_mut(y_base.add(y * y_stride), w);
                for (x, px) in row.iter_mut().enumerate() {
                    let in_box = x >= box_x && x < box_x + 200 && y >= box_y && y < box_y + 200;
                    *px = if flash || in_box {
                        235
                    } else {
                        let g = (((x + n as usize * 4) % 256) as u8 / 2).wrapping_add(16);
                        g ^ row_noise(rng, x, y)
                    };
                }
            }
            let uv_base = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *mut u8;
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
            for y in 0..h / 2 {
                let row = std::slice::from_raw_parts_mut(uv_base.add(y * uv_stride), w);
                for (i, px) in row.iter_mut().enumerate() {
                    *px = if flash { 128 } else if i % 2 == 0 { 100 + (y % 56) as u8 } else { 150 };
                }
            }
        } else {
            let base = CVPixelBufferGetBaseAddress(pb) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(pb);
            for y in 0..h {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride) as *mut u32, w);
                for (x, px) in row.iter_mut().enumerate() {
                    let in_box = x >= box_x && x < box_x + 200 && y >= box_y && y < box_y + 200;
                    *px = if flash || in_box {
                        0xffff_ffff
                    } else {
                        let g = ((x + n as usize * 4) % 256) as u32;
                        let nz = row_noise(rng, x, y) as u32;
                        0xff00_0000 | ((g ^ nz) << 16) | (((y * 255 / h) as u32) << 8) | (120 ^ nz)
                    };
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(0));
    }
}

impl Synth {
    /// `audio`: also generate system audio with a beep on every flash.
    pub fn start(w: usize, h: usize, fps: u32, pix: u32, noise: bool, audio: bool, shared: Arc<Shared>) -> Synth {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let pool = SendBox(make_pool(w, h, pix));
        // Video and audio share this origin, so flash k and beep k are both
        // stamped exactly start + k seconds.
        let start = secs(host_now()).ceil();
        let mut threads = vec![];
        if audio {
            let (stop3, shared3) = (stop.clone(), shared.clone());
            threads.push(std::thread::spawn(move || {
                let fmt = crate::mixer::stereo_format();
                let rate = crate::mixer::RATE;
                let chunk = 480usize; // 10 ms
                let mut k = 0i64;
                // Self-test hook: drop every Nth buffer like a glitching device.
                let drop_every: i64 = std::env::var("REC_SYNTH_DROP_AUDIO").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                while !stop3.load(Ordering::SeqCst) {
                    let t = start + (k * chunk as i64) as f64 / rate;
                    let now = secs(host_now());
                    // Deliver each chunk once it has "played", like a real device.
                    let ready = t + chunk as f64 / rate;
                    if ready > now {
                        std::thread::sleep(Duration::from_secs_f64(ready - now));
                    }
                    objc2::rc::autoreleasepool(|_| {
                        let pts = unsafe { objc2_core_media::CMTime::new(start as i64 * rate as i64 + k * chunk as i64, rate as i32) };
                        let dropped = drop_every > 0 && k % drop_every == drop_every - 1;
                        if dropped {
                        } else if let Some(sb) = crate::mixer::pcm_buffer(&fmt, beep_samples(k * chunk as i64, chunk), pts) {
                            shared3.with(|s| s.push_audio(crate::writer::AudioKind::System, &sb));
                        }
                    });
                    k += 1;
                }
            }));
        }
        threads.push(std::thread::spawn(move || {
            let pool = pool;
            let mut rng = Rng(0x9e3779b97f4a7c15);
            let period = 1.0 / fps as f64;
            let mut n = 0u64;
            while !stop2.load(Ordering::SeqCst) {
                let due = start + n as f64 * period;
                let now = secs(host_now());
                if due > now {
                    std::thread::sleep(Duration::from_secs_f64(due - now));
                }
                objc2::rc::autoreleasepool(|_| {
                    let mut buf = std::ptr::null_mut();
                    unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool.0, std::ptr::NonNull::from(&mut buf)) };
                    if let Some(buf) = std::ptr::NonNull::new(buf) {
                        let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(buf) };
                        draw(&pb, n, fps as u64, noise, &mut rng);
                        let pts = unsafe { objc2_core_media::CMTime::with_seconds(due, 1_000_000_000) };
                        shared.with(|s| s.push_screen(pts, pb));
                    }
                });
                n += 1;
            }
        }));
        Synth { stop, threads }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beep_starts_on_each_second() {
        let s = beep_samples(48000 - 2, 6);
        assert_eq!(&s[..4], &[0.0; 4], "silent before the second");
        assert_eq!(s[4], 0.0, "sine starts at zero phase");
        assert!(s[6] > 0.0);
        let len = (BEEP_SECS * 48000.0) as i64;
        assert_eq!(beep_samples(len, 1), vec![0.0, 0.0]);
    }
}
