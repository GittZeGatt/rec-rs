//! Small Objective-C / CoreFoundation helpers.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CFString;
use objc2_core_media::{CMClock, CMTime};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

pub type Dict = NSDictionary<NSString, AnyObject>;

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Toll-free bridge a CFString to NSString.
pub fn cf_ns(s: &CFString) -> &NSString {
    unsafe { &*(s as *const CFString as *const NSString) }
}

pub fn num_i(v: i64) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_i64(v))))
}
pub fn num_f(v: f64) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_f64(v))))
}
pub fn num_b(v: bool) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_bool(v))))
}
pub fn obj<T: objc2::Message>(v: &T) -> Retained<AnyObject> {
    unsafe { Retained::retain(v as *const T as *mut AnyObject) }.unwrap()
}

pub fn dict(pairs: Vec<(&NSString, Retained<AnyObject>)>) -> Retained<Dict> {
    let keys: Vec<&NSString> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<Retained<AnyObject>> = pairs.iter().map(|p| p.1.clone()).collect();
    NSDictionary::from_retained_objects(&keys, &vals)
}

pub fn host_now() -> CMTime {
    unsafe { CMClock::host_time_clock().time() }
}

pub fn secs(t: CMTime) -> f64 {
    unsafe { t.seconds() }
}

/// Wrapper asserting a CoreFoundation/ObjC value may cross threads. All uses
/// are serialized by a Mutex or are documented thread-safe CF types.
pub struct SendBox<T>(pub T);
unsafe impl<T> Send for SendBox<T> {}
unsafe impl<T> Sync for SendBox<T> {}

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}
pub const PIX_420V: u32 = fourcc(b"420v");
pub const PIX_BGRA: u32 = fourcc(b"BGRA");

pub fn make_pool(w: usize, h: usize, pix: u32) -> objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBufferPool> {
    let attrs = dict(vec![
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferPixelFormatTypeKey }), num_i(pix as i64)),
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferWidthKey }), num_i(w as i64)),
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferHeightKey }), num_i(h as i64)),
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey }), obj(&*dict(vec![]))),
    ]);
    let attrs_cf: &objc2_core_foundation::CFDictionary =
        unsafe { &*(&*attrs as *const Dict as *const objc2_core_foundation::CFDictionary) };
    let mut pool = std::ptr::null_mut();
    unsafe {
        objc2_core_video::CVPixelBufferPool::create(None, None, Some(attrs_cf), std::ptr::NonNull::from(&mut pool));
        objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new(pool).expect("pixel buffer pool"))
    }
}

