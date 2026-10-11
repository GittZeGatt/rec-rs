//! Presentation latency of the default output device, from the CoreAudio HAL.
//!
//! ScreenCaptureKit captures system audio when it is mixed, but it is heard
//! one output latency later, and players (AVPlayer, browsers) schedule
//! audio ahead by exactly that so it lines up with the picture on screen.
//! Screen frames are stamped when shown, so system audio must be shifted
//! later by this latency to keep the recording in sync with what the user
//! saw and heard (about 25 ms on built-in speakers, far more on Bluetooth).

use std::ffi::c_void;

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectGetPropertyDataSize(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const SYSTEM_OBJECT: u32 = 1;
const GLOBAL: u32 = fourcc(b"glob");
const OUTPUT: u32 = fourcc(b"outp");

fn get<T: Default + Copy>(obj: u32, selector: &[u8; 4], scope: u32) -> Option<T> {
    let addr = Address { selector: fourcc(selector), scope, element: 0 };
    let mut v = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(obj, &addr, 0, std::ptr::null(), &mut size, &mut v as *mut T as *mut c_void)
    };
    (st == 0).then_some(v)
}

/// Seconds between system audio being mixed and being heard on the
/// current default output device, or None if it can't be read.
pub fn output_latency() -> Option<f64> {
    let dev: u32 = get(SYSTEM_OBJECT, b"dOut", GLOBAL)?;
    let rate: f64 = get(dev, b"nsrt", GLOBAL).filter(|r: &f64| *r > 0.0)?;
    let device: u32 = get(dev, b"ltnc", OUTPUT).unwrap_or(0);
    let safety: u32 = get(dev, b"saft", OUTPUT).unwrap_or(0);
    let buffer: u32 = get(dev, b"fsiz", GLOBAL).unwrap_or(0);
    // Latency of the device's first output stream.
    let addr = Address { selector: fourcc(b"stm#"), scope: OUTPUT, element: 0 };
    let mut size = 0u32;
    let mut stream_latency = 0u32;
    unsafe {
        if AudioObjectGetPropertyDataSize(dev, &addr, 0, std::ptr::null(), &mut size) == 0 && size >= 4 {
            let mut ids = vec![0u32; size as usize / 4];
            if AudioObjectGetPropertyData(dev, &addr, 0, std::ptr::null(), &mut size, ids.as_mut_ptr() as *mut c_void) == 0
            {
                stream_latency = get(ids[0], b"ltnc", GLOBAL).unwrap_or(0);
            }
        }
    }
    let frames = device + safety + buffer + stream_latency;
    Some(frames as f64 / rate)
}
