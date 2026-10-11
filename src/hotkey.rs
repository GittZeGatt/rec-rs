//! Global hotkey via Carbon `RegisterEventHotKey`. Unlike event taps this
//! needs no Accessibility permission; the callback runs on the main thread.

use std::cell::RefCell;
use std::ffi::c_void;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> i32;
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
pub const KEY_R: u32 = 15; // kVK_ANSI_R
pub const CMD: u32 = 1 << 8; // cmdKey
pub const SHIFT: u32 = 1 << 9; // shiftKey

thread_local! {
    static CALLBACK: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

extern "C" fn on_hotkey(_next: *mut c_void, _event: *mut c_void, _user: *mut c_void) -> i32 {
    CALLBACK.with(|c| {
        if let Some(f) = c.borrow().as_ref() {
            f();
        }
    });
    0
}

/// Register a system-wide hotkey. Must be called on the main thread before
/// the app run loop starts. Fails if another app already owns the combo.
pub fn register(key: u32, modifiers: u32, callback: impl Fn() + 'static) -> Result<(), i32> {
    CALLBACK.with(|c| *c.borrow_mut() = Some(Box::new(callback)));
    unsafe {
        let target = GetApplicationEventTarget();
        let spec = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
        let mut handler_ref = std::ptr::null_mut();
        let st = InstallEventHandler(target, on_hotkey, 1, &spec, std::ptr::null_mut(), &mut handler_ref);
        if st != 0 {
            return Err(st);
        }
        let id = EventHotKeyID { signature: u32::from_be_bytes(*b"REC1"), id: 1 };
        let mut hk = std::ptr::null_mut();
        let st = RegisterEventHotKey(key, modifiers, id, target, 0, &mut hk);
        if st != 0 {
            return Err(st);
        }
    }
    Ok(())
}
