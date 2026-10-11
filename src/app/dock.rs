//! Dock behaviour winit doesn't provide: quitting from the Dock, the menu or
//! a logout goes through the app's own quit (which finalizes a running
//! recording first), and clicking the Dock icon reopens the main window.

use std::sync::atomic::{AtomicBool, Ordering};

use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::sel;

use super::Cmd;

/// Set once the app has saved everything and may terminate.
static ALLOW_TERMINATE: AtomicBool = AtomicBool::new(false);

pub fn allow_terminate() {
    ALLOW_TERMINATE.store(true, Ordering::SeqCst);
}

/// -applicationShouldTerminate: → NSTerminateNow (1) once allowed, else
/// NSTerminateCancel (0) and a Quit command that saves and then exits.
extern "C-unwind" fn should_terminate(_this: &AnyObject, _sel: Sel, _app: &AnyObject) -> usize {
    if ALLOW_TERMINATE.load(Ordering::SeqCst) {
        return 1;
    }
    super::push(Cmd::Quit);
    0
}

/// -applicationShouldHandleReopen:hasVisibleWindows: (Dock icon click).
extern "C-unwind" fn should_reopen(_this: &AnyObject, _sel: Sel, _app: &AnyObject, visible: Bool) -> Bool {
    if !visible.as_bool() {
        super::push(Cmd::ShowWindow);
    }
    Bool::YES
}

/// Add both methods to winit's application delegate class. Call after the
/// event loop is built (the class is registered then).
pub fn install() {
    let Some(cls) = AnyClass::get(c"WinitApplicationDelegate") else {
        eprintln!("rec: warning: winit app delegate not found; Dock quit won't finalize recordings");
        return;
    };
    let cls = cls as *const AnyClass as *mut AnyClass;
    unsafe {
        let term: Imp = std::mem::transmute(should_terminate as extern "C-unwind" fn(_, _, _) -> _);
        objc2::ffi::class_addMethod(cls, sel!(applicationShouldTerminate:), term, c"Q@:@".as_ptr());
        let reopen: Imp = std::mem::transmute(should_reopen as extern "C-unwind" fn(_, _, _, _) -> _);
        objc2::ffi::class_addMethod(cls, sel!(applicationShouldHandleReopen:hasVisibleWindows:), reopen, c"B@:@B".as_ptr());
    }
}
