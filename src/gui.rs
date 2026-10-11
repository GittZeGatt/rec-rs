//! Menu bar app: status item + menu, live preview window, global Cmd+Shift+R.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_core_foundation::{CFRetained, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetIOSurface};
use objc2_foundation::*;
use objc2_quartz_core::{CALayer, CATransaction, kCAGravityResizeAspect};

use crate::record::{self, Session};
use crate::util::*;
use crate::writer::{PreviewFn, Stats};

const RESOLUTIONS: [(&str, &str); 4] = [("1080p", "1080p"), ("1440p", "1440p"), ("4K", "4k"), ("Native", "native")];

/// Recording/preview settings, persisted in the app's user defaults.
#[derive(Clone)]
struct Settings {
    resolution: String,
    webcam: bool,
    mic: bool,
    system_audio: bool,
}

impl Settings {
    fn load() -> Self {
        let d = NSUserDefaults::standardUserDefaults();
        let get_b = |k: &str, def: bool| {
            if d.objectForKey(&ns(k)).is_some() { d.boolForKey(&ns(k)) } else { def }
        };
        Settings {
            resolution: d.stringForKey(&ns("resolution")).map(|s| s.to_string()).unwrap_or_else(|| "1080p".into()),
            webcam: get_b("webcam", false),
            mic: get_b("mic", true),
            system_audio: get_b("systemAudio", true),
        }
    }

    fn save(&self) {
        let d = NSUserDefaults::standardUserDefaults();
        unsafe {
            d.setObject_forKey(Some(&ns(&self.resolution)), &ns("resolution"));
        }
        d.setBool_forKey(self.webcam, &ns("webcam"));
        d.setBool_forKey(self.mic, &ns("mic"));
        d.setBool_forKey(self.system_audio, &ns("systemAudio"));
    }

    fn record_args(&self) -> Vec<String> {
        let mut a = vec!["-q".to_string(), "-r".into(), self.resolution.clone()];
        if self.webcam {
            a.push("--webcam".into());
        }
        if !self.mic {
            a.push("--no-mic".into());
        }
        if !self.system_audio {
            a.push("--no-system-audio".into());
        }
        a
    }
}

#[derive(Default)]
struct Ui {
    status_item: Option<Retained<NSStatusItem>>,
    record_item: Option<Retained<NSMenuItem>>,
    preview_item: Option<Retained<NSMenuItem>>,
    res_items: Vec<Retained<NSMenuItem>>,
    webcam_item: Option<Retained<NSMenuItem>>,
    mic_item: Option<Retained<NSMenuItem>>,
    sys_item: Option<Retained<NSMenuItem>>,
    reveal_item: Option<Retained<NSMenuItem>>,
    hotkey_item: Option<Retained<NSMenuItem>>,
    window: Option<Retained<NSWindow>>,
}

struct State {
    settings: Settings,
    ui: Ui,
    /// Recording session (writes a file).
    recording: Option<Session>,
    /// Preview-only session while idle with the preview window open.
    preview: Option<Session>,
    /// A recording is being finalized on a background thread.
    finishing: bool,
    last_file: Option<PathBuf>,
}

pub struct Ivars {
    state: RefCell<State>,
}

// Preview frames cross from the pacer thread to the main thread.
static PREVIEW_VISIBLE: AtomicBool = AtomicBool::new(false);
static PREVIEW_PENDING: AtomicBool = AtomicBool::new(false);
static PREVIEW_FRAME: Mutex<Option<SendBox<CFRetained<CVPixelBuffer>>>> = Mutex::new(None);
/// Result of a background finalize: (file, outcome).
static FINISHED: Mutex<Option<(PathBuf, Result<Stats, String>)>> = Mutex::new(None);

/// Signals: SIGINT (`rec stop`) stops the recording, SIGTERM/SIGHUP quit
/// after finalizing. Handled on the next timer tick.
static SIG_STOP: AtomicBool = AtomicBool::new(false);
static SIG_QUIT: AtomicBool = AtomicBool::new(false);
/// Start/stop requested from outside the main thread (e.g. --start-recording).
static SIG_TOGGLE: AtomicBool = AtomicBool::new(false);
extern "C" fn on_sigint(_: libc::c_int) {
    SIG_STOP.store(true, Ordering::SeqCst);
}
extern "C" fn on_sigterm(_: libc::c_int) {
    SIG_QUIT.store(true, Ordering::SeqCst);
}

thread_local! {
    static PREVIEW_LAYER: RefCell<Option<Retained<CALayer>>> = const { RefCell::new(None) };
}

fn preview_fn() -> PreviewFn {
    Arc::new(|frame: CFRetained<CVPixelBuffer>| {
        if !PREVIEW_VISIBLE.load(Ordering::Relaxed) {
            return;
        }
        *PREVIEW_FRAME.lock().unwrap() = Some(SendBox(frame));
        // Coalesce: at most one pending main-thread update.
        if PREVIEW_PENDING.swap(true, Ordering::AcqRel) {
            return;
        }
        DispatchQueue::main().exec_async(|| {
            PREVIEW_PENDING.store(false, Ordering::Release);
            let Some(frame) = PREVIEW_FRAME.lock().unwrap().take() else { return };
            let Some(surface) = CVPixelBufferGetIOSurface(Some(&frame.0)) else { return };
            PREVIEW_LAYER.with(|l| {
                if let Some(layer) = l.borrow().as_ref() {
                    CATransaction::begin();
                    CATransaction::setDisableActions(true);
                    let obj: &AnyObject = unsafe { &*(&*surface as *const _ as *const AnyObject) };
                    unsafe { layer.setContents(Some(obj)) };
                    CATransaction::commit();
                }
            });
        });
    })
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RecAppDelegate"]
    #[ivars = Ivars]
    pub struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    impl Delegate {
        #[unsafe(method(toggleRecording:))]
        fn toggle_recording_action(&self, _sender: Option<&AnyObject>) {
            self.toggle_recording();
        }

        #[unsafe(method(togglePreview:))]
        fn toggle_preview(&self, _sender: Option<&AnyObject>) {
            if PREVIEW_VISIBLE.load(Ordering::Relaxed) {
                self.close_preview();
            } else {
                self.open_preview();
            }
        }

        #[unsafe(method(setResolution:))]
        fn set_resolution(&self, sender: Option<&NSMenuItem>) {
            let Some(item) = sender else { return };
            let idx = item.tag() as usize;
            self.state().borrow_mut().settings.resolution = RESOLUTIONS[idx].1.to_string();
            self.settings_changed();
        }

        #[unsafe(method(toggleWebcam:))]
        fn toggle_webcam(&self, _sender: Option<&AnyObject>) {
            let mut st = self.state().borrow_mut();
            st.settings.webcam = !st.settings.webcam;
            drop(st);
            self.settings_changed();
        }

        #[unsafe(method(toggleMic:))]
        fn toggle_mic(&self, _sender: Option<&AnyObject>) {
            let mut st = self.state().borrow_mut();
            st.settings.mic = !st.settings.mic;
            drop(st);
            self.settings_changed();
        }

        #[unsafe(method(toggleSystemAudio:))]
        fn toggle_system_audio(&self, _sender: Option<&AnyObject>) {
            let mut st = self.state().borrow_mut();
            st.settings.system_audio = !st.settings.system_audio;
            drop(st);
            self.settings_changed();
        }

        #[unsafe(method(revealLast:))]
        fn reveal_last(&self, _sender: Option<&AnyObject>) {
            if let Some(p) = self.state().borrow().last_file.clone() {
                let url = NSURL::fileURLWithPath(&ns(&p.to_string_lossy()));
                NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
            }
        }

        #[unsafe(method(openFolder:))]
        fn open_folder(&self, _sender: Option<&AnyObject>) {
            let dir = std::env::var("HOME").map(|h| format!("{h}/Movies")).unwrap_or_default();
            let url = NSURL::fileURLWithPath(&ns(&dir));
            NSWorkspace::sharedWorkspace().openURL(&url);
        }

        #[unsafe(method(quit:))]
        fn quit_app(&self, _sender: Option<&AnyObject>) {
            self.do_quit();
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: Option<&NSTimer>) {
            self.on_tick();
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: Option<&NSNotification>) {
            PREVIEW_VISIBLE.store(false, Ordering::Relaxed);
            self.stop_preview_session();
            self.refresh_menu();
        }
    }

    unsafe impl NSWindowDelegate for Delegate {}
);

fn item(title: &str, action: Option<Sel>, key: &str, target: &Delegate, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let it = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), action, &ns(key))
    };
    if action.is_some() {
        unsafe { it.setTarget(Some(target)) };
    }
    it
}

impl Delegate {
    fn do_quit(&self) {
        // Finalize an in-progress recording before exiting.
        let rec = self.state().borrow_mut().recording.take();
        if let Some(sess) = rec {
            let _ = sess.finish();
        }
        let prev = self.state().borrow_mut().preview.take();
        if let Some(sess) = prev {
            let _ = sess.finish();
        }
        NSApplication::sharedApplication(self.mtm()).terminate(None);
    }

    fn create(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            state: RefCell::new(State {
                settings: Settings::load(),
                ui: Ui::default(),
                recording: None,
                preview: None,
                finishing: false,
                last_file: None,
            }),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn state(&self) -> &RefCell<State> {
        &self.ivars().state
    }

    fn build_ui(&self) {
        let mtm = self.mtm();
        let bar = NSStatusBar::systemStatusBar();
        let status_item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let record_item = item("Start Recording", Some(sel!(toggleRecording:)), "r", self, mtm);
        record_item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
        menu.addItem(&record_item);
        let preview_item = item("Show Preview", Some(sel!(togglePreview:)), "p", self, mtm);
        menu.addItem(&preview_item);
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let res_menu = NSMenu::new(mtm);
        let mut res_items = vec![];
        for (i, (label, _)) in RESOLUTIONS.iter().enumerate() {
            let it = item(label, Some(sel!(setResolution:)), "", self, mtm);
            it.setTag(i as isize);
            res_menu.addItem(&it);
            res_items.push(it);
        }
        let res_parent = item("Resolution", None, "", self, mtm);
        res_parent.setSubmenu(Some(&res_menu));
        menu.addItem(&res_parent);
        let webcam_item = item("Webcam Overlay", Some(sel!(toggleWebcam:)), "", self, mtm);
        menu.addItem(&webcam_item);
        let mic_item = item("Microphone", Some(sel!(toggleMic:)), "", self, mtm);
        menu.addItem(&mic_item);
        let sys_item = item("System Audio", Some(sel!(toggleSystemAudio:)), "", self, mtm);
        menu.addItem(&sys_item);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let reveal_item = item("Reveal Last Recording", Some(sel!(revealLast:)), "", self, mtm);
        menu.addItem(&reveal_item);
        menu.addItem(&item("Open Recordings Folder", Some(sel!(openFolder:)), "", self, mtm));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let hotkey_item = item("Global hotkey: ⇧⌘R", None, "", self, mtm);
        hotkey_item.setEnabled(false);
        menu.addItem(&hotkey_item);
        menu.addItem(&item("Quit Rec", Some(sel!(quit:)), "q", self, mtm));
        status_item.setMenu(Some(&menu));

        let mut st = self.state().borrow_mut();
        st.ui = Ui {
            status_item: Some(status_item),
            record_item: Some(record_item),
            preview_item: Some(preview_item),
            res_items,
            webcam_item: Some(webcam_item),
            mic_item: Some(mic_item),
            sys_item: Some(sys_item),
            reveal_item: Some(reveal_item),
            hotkey_item: Some(hotkey_item),
            window: None,
        };
        drop(st);
        self.refresh_menu();
    }

    fn refresh_menu(&self) {
        let st = self.state().borrow();
        let on = |b: bool| if b { NSControlStateValueOn } else { NSControlStateValueOff };
        let recording = st.recording.is_some();
        if let Some(it) = &st.ui.record_item {
            it.setTitle(&ns(if st.finishing {
                "Saving…"
            } else if recording {
                "Stop Recording"
            } else {
                "Start Recording"
            }));
            it.setEnabled(!st.finishing);
        }
        if let Some(it) = &st.ui.preview_item {
            it.setTitle(&ns(if PREVIEW_VISIBLE.load(Ordering::Relaxed) { "Hide Preview" } else { "Show Preview" }));
        }
        for (i, it) in st.ui.res_items.iter().enumerate() {
            it.setState(on(RESOLUTIONS[i].1 == st.settings.resolution));
            it.setEnabled(!recording);
        }
        for (it, val) in [
            (&st.ui.webcam_item, st.settings.webcam),
            (&st.ui.mic_item, st.settings.mic),
            (&st.ui.sys_item, st.settings.system_audio),
        ] {
            if let Some(it) = it {
                it.setState(on(val));
                it.setEnabled(!recording);
            }
        }
        if let Some(it) = &st.ui.reveal_item {
            it.setEnabled(st.last_file.is_some());
        }
        drop(st);
        self.refresh_status();
    }

    fn refresh_status(&self) {
        let st = self.state().borrow();
        let Some(si) = &st.ui.status_item else { return };
        let Some(button) = si.button(self.mtm()) else { return };
        let (symbol, title, red) = if st.finishing {
            ("record.circle", " Saving…".to_string(), false)
        } else if let Some(sess) = &st.recording {
            ("record.circle.fill", format!(" {}", record::clock(sess.started.elapsed().as_secs_f64())), true)
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

    fn alert(&self, title: &str, text: &str) {
        eprintln!("rec: {title}: {text}");
        if std::env::var_os("REC_GUI_NO_ALERTS").is_some() {
            return;
        }
        let mtm = self.mtm();
        let a = NSAlert::new(mtm);
        a.setMessageText(&ns(title));
        a.setInformativeText(&ns(text));
        NSApplication::sharedApplication(mtm).activate();
        a.runModal();
    }

    fn make_options(&self, write: bool) -> Result<(record::Options, Option<crate::screen::Target>)> {
        let args = self.state().borrow().settings.record_args();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let a = crate::parse_record_args(&refs)?;
        let (opts, target) = crate::prepare(a)?;
        if write && let Some(parent) = opts.output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok((opts, target))
    }

    fn toggle_recording(&self) {
        if self.state().borrow().finishing {
            return;
        }
        if self.state().borrow().recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(&self) {
        // One capture session at a time: hand the preview over to the recording.
        self.stop_preview_session();
        let result = self
            .make_options(true)
            .and_then(|(opts, target)| Session::start(&opts, target, true, Some(preview_fn())));
        match result {
            Ok(sess) => {
                let file = sess.output.clone().unwrap_or_default();
                let _ = std::fs::write(record::pidfile_path(), format!("{}\n{}\n", std::process::id(), file.display()));
                self.state().borrow_mut().recording = Some(sess);
                self.update_window_title();
            }
            Err(e) => {
                self.alert("Could not start recording", &format!("{e:#}"));
                self.start_preview_session_if_visible();
            }
        }
        self.refresh_menu();
    }

    fn stop_recording(&self) {
        let Some(sess) = self.state().borrow_mut().recording.take() else { return };
        let file = sess.output.clone().unwrap_or_default();
        self.state().borrow_mut().finishing = true;
        let pidfile = record::pidfile_path();
        // Finalizing can take a moment for long recordings; keep the UI live.
        std::thread::spawn(move || {
            let r = objc2::rc::autoreleasepool(|_| sess.finish()).map_err(|e| format!("{e:#}"));
            *FINISHED.lock().unwrap() = Some((file, r));
            let _ = std::fs::remove_file(pidfile);
        });
        self.refresh_menu();
    }

    /// REC_GUI_SELFTEST=1: drive the real GUI code paths on a timer
    /// (preview, record via the same toggle the hotkey uses, stop, quit).
    fn selftest_step(&self) {
        thread_local! { static T0: std::time::Instant = std::time::Instant::now(); static STEP: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }
        let t = T0.with(|t| t.elapsed().as_secs_f64());
        let step = STEP.with(|s| s.get());
        let layer_has_contents = || PREVIEW_LAYER.with(|l| l.borrow().as_ref().is_some_and(|l| unsafe { l.contents() }.is_some()));
        let next = |n| STEP.with(|s| s.set(n));
        match step {
            0 if t > 1.0 => { self.open_preview(); next(1) }
            1 if t > 3.0 => {
                println!("selftest: idle preview session={} frame_shown={}", self.state().borrow().preview.is_some(), layer_has_contents());
                PREVIEW_LAYER.with(|l| if let Some(l) = l.borrow().as_ref() { unsafe { l.setContents(None) } });
                self.toggle_recording();
                println!("selftest: recording={}", self.state().borrow().recording.is_some());
                next(2)
            }
            2 if t > 8.0 => {
                println!("selftest: preview during recording frame_shown={}", layer_has_contents());
                let title = self.state().borrow().ui.status_item.as_ref().and_then(|s| s.button(self.mtm())).map(|b| b.title().to_string());
                println!("selftest: status title={title:?}");
                self.toggle_recording();
                next(3)
            }
            3 if !self.state().borrow().finishing => {
                println!("selftest: saved={:?} preview_restarted={}", self.state().borrow().last_file, self.state().borrow().preview.is_some());
                next(4)
            }
            4 if t > 10.0 => {
                self.close_preview();
                println!("selftest: after close preview_session={}", self.state().borrow().preview.is_some());
                self.do_quit();
            }
            _ => {}
        }
    }

    fn on_tick(&self) {
        if std::env::var_os("REC_GUI_SELFTEST").is_some() {
            self.selftest_step();
        }
        if SIG_QUIT.swap(false, Ordering::SeqCst) {
            self.do_quit();
            return;
        }
        if SIG_TOGGLE.swap(false, Ordering::SeqCst) {
            self.toggle_recording();
        }
        if SIG_STOP.swap(false, Ordering::SeqCst) && self.state().borrow().recording.is_some() {
            self.stop_recording();
        }
        let done = FINISHED.lock().unwrap().take();
        if let Some((file, r)) = done {
            self.state().borrow_mut().finishing = false;
            match r {
                Ok(_) => self.state().borrow_mut().last_file = Some(file),
                Err(e) => self.alert("Recording failed", &e),
            }
            self.update_window_title();
            self.start_preview_session_if_visible();
            self.refresh_menu();
        }
        let failed = self.state().borrow().recording.as_ref().is_some_and(|s| s.failed());
        if failed {
            let err = self.state().borrow().recording.as_ref().and_then(|s| s.error()).unwrap_or_default();
            self.stop_recording();
            self.alert("Recording stopped", &err);
        }
        let preview_failed = self.state().borrow().preview.as_ref().is_some_and(|s| s.failed());
        if preview_failed {
            self.stop_preview_session();
        }
        self.refresh_status();
    }

    fn settings_changed(&self) {
        self.state().borrow().settings.save();
        // Restart the preview so it reflects the new settings.
        if self.state().borrow().preview.is_some() {
            self.stop_preview_session();
            self.start_preview_session_if_visible();
        }
        self.refresh_menu();
    }

    fn open_preview(&self) {
        let mtm = self.mtm();
        if self.state().borrow().ui.window.is_none() {
            let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(640.0, 360.0));
            let style = NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Resizable
                | NSWindowStyleMask::Miniaturizable;
            let win = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    rect,
                    style,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { win.setReleasedWhenClosed(false) };
            win.setLevel(NSFloatingWindowLevel);
            win.setContentAspectRatio(CGSize::new(16.0, 9.0));
            win.setDelegate(Some(ProtocolObject::from_ref(self)));
            win.setFrameAutosaveName(&ns("RecPreview"));
            win.center();
            if let Some(view) = win.contentView() {
                view.setWantsLayer(true);
                if let Some(layer) = view.layer() {
                    layer.setContentsGravity(unsafe { kCAGravityResizeAspect });
                    let black = objc2_core_graphics::CGColor::new_generic_gray(0.0, 1.0);
                    layer.setBackgroundColor(Some(&black));
                    PREVIEW_LAYER.with(|l| *l.borrow_mut() = Some(layer));
                }
            }
            self.state().borrow_mut().ui.window = Some(win);
        }
        PREVIEW_VISIBLE.store(true, Ordering::Relaxed);
        self.update_window_title();
        if let Some(w) = &self.state().borrow().ui.window {
            NSApplication::sharedApplication(mtm).activate();
            w.makeKeyAndOrderFront(None);
        }
        self.start_preview_session_if_visible();
        self.refresh_menu();
    }

    fn close_preview(&self) {
        let win = self.state().borrow().ui.window.clone();
        if let Some(w) = win {
            w.close(); // triggers windowWillClose:
        }
    }

    fn update_window_title(&self) {
        let st = self.state().borrow();
        if let Some(w) = &st.ui.window {
            let mode = if st.recording.is_some() { "recording" } else { "preview (not recording)" };
            w.setTitle(&ns(&format!("Rec — {mode}")));
        }
    }

    fn start_preview_session_if_visible(&self) {
        let busy = {
            let st = self.state().borrow();
            st.recording.is_some() || st.preview.is_some() || st.finishing
        };
        if busy || !PREVIEW_VISIBLE.load(Ordering::Relaxed) {
            return;
        }
        match self.make_options(false).and_then(|(opts, target)| Session::start(&opts, target, false, Some(preview_fn())))
        {
            Ok(sess) => self.state().borrow_mut().preview = Some(sess),
            Err(e) => eprintln!("rec: preview unavailable: {e:#}"),
        }
    }

    fn stop_preview_session(&self) {
        let sess = self.state().borrow_mut().preview.take();
        if let Some(s) = sess {
            let _ = s.finish();
        }
    }
}

pub fn run(start_recording: bool) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow::anyhow!("GUI must run on the main thread"))?;
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, on_sigterm as *const () as usize);
        libc::signal(libc::SIGHUP, on_sigterm as *const () as usize);
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::create(mtm);
    delegate.build_ui();

    let d2 = delegate.clone();
    if let Err(code) = crate::hotkey::register(crate::hotkey::KEY_R, crate::hotkey::CMD | crate::hotkey::SHIFT, move || {
        d2.toggle_recording()
    }) {
        eprintln!("rec: could not register ⇧⌘R (error {code}); use the menu instead");
        if let Some(it) = &delegate.state().borrow().ui.hotkey_item {
            it.setTitle(&ns("Global hotkey ⇧⌘R unavailable"));
        }
    }

    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.25,
            &delegate,
            sel!(tick:),
            None,
            true,
        )
    };
    let _ = CGFloat::default();
    eprintln!("rec: menu bar app running (⇧⌘R toggles recording)");
    if start_recording {
        SIG_TOGGLE.store(true, Ordering::SeqCst);
    }
    app.run();
    Ok(())
}
