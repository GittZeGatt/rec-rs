//! Menu bar status item: record icon (red with the elapsed time while
//! recording) and a menu. Menu actions are queued as `Cmd`s for the app.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;

use super::Cmd;
use crate::util::ns;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RecMenuTarget"]
    pub struct Target;

    impl Target {
        #[unsafe(method(toggleRecording:))]
        fn toggle_recording(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::ToggleRecording);
        }
        #[unsafe(method(togglePause:))]
        fn toggle_pause(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::TogglePause);
        }
        #[unsafe(method(showWindow:))]
        fn show_window(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::ShowWindow);
        }
        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::ShowSettings);
        }
        #[unsafe(method(revealLast:))]
        fn reveal_last(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::RevealLast);
        }
        #[unsafe(method(openFolder:))]
        fn open_folder(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::OpenFolder);
        }
        #[unsafe(method(quit:))]
        fn quit(&self, _s: Option<&AnyObject>) {
            super::push(Cmd::Quit);
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

pub struct MenuBar {
    status: Retained<NSStatusItem>,
    record: Retained<NSMenuItem>,
    pause: Retained<NSMenuItem>,
    reveal: Retained<NSMenuItem>,
    hotkey: Retained<NSMenuItem>,
    _target: Retained<Target>,
    last: Option<(bool, bool, bool, String)>,
}

/// What the menu bar shows.
pub struct View<'a> {
    pub recording: bool,
    pub paused: bool,
    pub finishing: bool,
    pub clock: &'a str,
    pub has_last: bool,
}

impl MenuBar {
    pub fn new(mtm: MainThreadMarker) -> MenuBar {
        let target = Target::new(mtm);
        let item = |title: &str, action: Option<Sel>, key: &str| {
            let it = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), action, &ns(key))
            };
            if action.is_some() {
                unsafe { it.setTarget(Some(&target)) };
            }
            it
        };
        let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let record = item("Start Recording", Some(sel!(toggleRecording:)), "r");
        record.setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
        menu.addItem(&record);
        let pause = item("Pause", Some(sel!(togglePause:)), "");
        menu.addItem(&pause);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&item("Show Rec Window", Some(sel!(showWindow:)), ""));
        menu.addItem(&item("Settings…", Some(sel!(showSettings:)), ","));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let reveal = item("Reveal Last Recording", Some(sel!(revealLast:)), "");
        menu.addItem(&reveal);
        menu.addItem(&item("Open Recordings Folder", Some(sel!(openFolder:)), ""));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let hotkey = item("Global hotkey: ⇧⌘R", None, "");
        hotkey.setEnabled(false);
        menu.addItem(&hotkey);
        menu.addItem(&item("Quit Rec", Some(sel!(quit:)), "q"));
        status.setMenu(Some(&menu));
        let mut mb = MenuBar { status, record, pause, reveal, hotkey, _target: target, last: None };
        mb.update(&View { recording: false, paused: false, finishing: false, clock: "", has_last: false });
        mb
    }

    pub fn hotkey_unavailable(&self) {
        self.hotkey.setTitle(&ns("Global hotkey ⇧⌘R unavailable"));
    }

    pub fn title(&self) -> Option<String> {
        let mtm = MainThreadMarker::new()?;
        self.status.button(mtm).map(|b| b.title().to_string())
    }

    pub fn update(&mut self, v: &View) {
        let key = (v.recording, v.paused, v.finishing, v.clock.to_string());
        if self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);
        self.record.setTitle(&ns(if v.finishing {
            "Saving…"
        } else if v.recording {
            "Stop Recording"
        } else {
            "Start Recording"
        }));
        self.record.setEnabled(!v.finishing);
        self.pause.setTitle(&ns(if v.paused { "Resume" } else { "Pause" }));
        self.pause.setEnabled(v.recording);
        self.reveal.setEnabled(v.has_last);
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some(button) = self.status.button(mtm) else { return };
        let (symbol, title, red) = if v.finishing {
            ("record.circle", " Saving…".to_string(), false)
        } else if v.paused {
            ("pause.circle.fill", format!(" {}", v.clock), false)
        } else if v.recording {
            ("record.circle.fill", format!(" {}", v.clock), true)
        } else {
            ("record.circle", String::new(), false)
        };
        if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(symbol), Some(&ns("Rec"))) {
            img.setTemplate(!red);
            button.setImage(Some(&img));
        }
        button.setContentTintColor(if red { Some(NSColor::systemRedColor()) } else { None }.as_deref());
        button.setImagePosition(NSCellImagePosition::ImageLeft);
        button.setTitle(&ns(&title));
    }
}
