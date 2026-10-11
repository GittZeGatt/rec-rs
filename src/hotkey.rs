//! Global hotkeys via Carbon `RegisterEventHotKey`. Unlike event taps this
//! needs no Accessibility permission; callbacks run on the main thread.

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
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
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
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");
const SIGNATURE: u32 = u32::from_be_bytes(*b"REC1");
pub const KEY_R: u32 = 15; // kVK_ANSI_R
pub const KEY_S: u32 = 1; // kVK_ANSI_S
pub const CMD: u32 = 1 << 8; // cmdKey
pub const SHIFT: u32 = 1 << 9; // shiftKey

struct Registered {
    id: u32,
    hotkey: *mut c_void,
    callback: Box<dyn Fn()>,
}

thread_local! {
    static HOTKEYS: RefCell<Vec<Registered>> = const { RefCell::new(Vec::new()) };
    static HANDLER: RefCell<bool> = const { RefCell::new(false) };
}

extern "C" fn on_hotkey(_next: *mut c_void, event: *mut c_void, _user: *mut c_void) -> i32 {
    let mut hk = EventHotKeyID { signature: 0, id: 0 };
    let st = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            std::ptr::null_mut(),
            &mut hk as *mut _ as *mut c_void,
        )
    };
    if st != 0 || hk.signature != SIGNATURE {
        return 0;
    }
    HOTKEYS.with(|h| {
        if let Some(r) = h.borrow().iter().find(|r| r.id == hk.id) {
            (r.callback)();
        }
    });
    0
}

/// Register a system-wide hotkey under `id` (replacing one with the same
/// id). Must be called on the main thread. Fails if another app already
/// owns the combo.
pub fn register(id: u32, key: u32, modifiers: u32, callback: impl Fn() + 'static) -> Result<(), i32> {
    unregister(id);
    unsafe {
        let target = GetApplicationEventTarget();
        if !HANDLER.with(|h| *h.borrow()) {
            let spec = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
            let mut handler_ref = std::ptr::null_mut();
            let st = InstallEventHandler(target, on_hotkey, 1, &spec, std::ptr::null_mut(), &mut handler_ref);
            if st != 0 {
                return Err(st);
            }
            HANDLER.with(|h| *h.borrow_mut() = true);
        }
        let mut hotkey = std::ptr::null_mut();
        let st = RegisterEventHotKey(key, modifiers, EventHotKeyID { signature: SIGNATURE, id }, target, 0, &mut hotkey);
        if st != 0 {
            return Err(st);
        }
        HOTKEYS.with(|h| h.borrow_mut().push(Registered { id, hotkey, callback: Box::new(callback) }));
    }
    Ok(())
}

/// Release the hotkey registered under `id` (no-op if none).
pub fn unregister(id: u32) {
    HOTKEYS.with(|h| {
        h.borrow_mut().retain(|r| {
            if r.id == id {
                unsafe { UnregisterEventHotKey(r.hotkey) };
                false
            } else {
                true
            }
        })
    });
}
