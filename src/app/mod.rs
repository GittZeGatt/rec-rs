//! The app: a main window (live preview, sources, record/pause, timer,
//! level meters), a settings window and a menu bar item, in egui on wgpu
//! with a transparent titlebar and Inter (the paper-rs look). ⇧⌘R toggles
//! recording from anywhere.

mod dock;
mod macos;
mod menubar;
mod preview;
mod theme;
mod ui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::compose::Scene;
use crate::record::{self, Session};
use crate::settings::Settings;
use crate::writer::Stats;

/// Things that happen to the app from menus, the hotkey, signals and
/// buttons; handled on the main thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    ToggleRecording,
    TogglePause,
    ShowWindow,
    ShowSettings,
    RevealLast,
    OpenFolder,
    Quit,
}

static QUEUE: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());
static PROXY: OnceLock<Mutex<EventLoopProxy<()>>> = OnceLock::new();

/// Wake the event loop (from any thread).
fn wake() {
    if let Some(p) = PROXY.get() {
        let _ = p.lock().unwrap().send_event(());
    }
}

pub fn push(c: Cmd) {
    QUEUE.lock().unwrap().push(c);
    wake();
}

/// SIGINT (`rec-rs stop`) stops the recording; SIGTERM/SIGHUP quit after
/// saving it. SIGUSR1/2 (`rec-rs pause`/`resume`) use record's flags.
static SIG_STOP: AtomicBool = AtomicBool::new(false);
static SIG_QUIT: AtomicBool = AtomicBool::new(false);
extern "C" fn on_sigint(_: libc::c_int) {
    SIG_STOP.store(true, Ordering::SeqCst);
}
extern "C" fn on_sigterm(_: libc::c_int) {
    SIG_QUIT.store(true, Ordering::SeqCst);
}

/// One OS window rendered with egui.
struct Win {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    egui: egui_winit::State,
    ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
    repaint_at: Option<Instant>,
}

struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Level meter state for one source.
#[derive(Default, Clone, Copy)]
pub struct Meter {
    /// Displayed level in dBFS (decays smoothly).
    pub db: f32,
    pub peak_db: f32,
    pub peak_at: Option<Instant>,
}

impl Meter {
    fn feed(&mut self, linear: f32, dt: f32) {
        let db = if linear <= 1e-5 { -90.0 } else { 20.0 * linear.log10() };
        self.db = db.max(self.db - 24.0 * dt).max(-90.0);
        if db >= self.peak_db || self.peak_at.is_none_or(|t| t.elapsed() > Duration::from_millis(1500)) {
            self.peak_db = db;
            self.peak_at = Some(Instant::now());
        }
    }
}

#[derive(Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// Everything the UI shows and edits.
pub struct State {
    pub settings: Settings,
    pub recording: Option<Session>,
    pub preview: Option<Session>,
    /// Settings the running preview session was started with.
    preview_settings: Option<Settings>,
    pub finishing: bool,
    pub last_file: Option<PathBuf>,
    pub error: Option<String>,
    pub meters: [Meter; 2],
    pub displays: Vec<String>,
    pub windows: Vec<(u32, String)>,
    pub cameras: Vec<Device>,
    pub mics: Vec<Device>,
    pub hotkey_ok: bool,
    pub preview_tex: Option<egui::TextureHandle>,
    pub preview_aspect: f32,
    /// Preview frames shown (for the self-test).
    frames_shown: u64,
    /// UI requests handled after the frame.
    pub cmds: Vec<Cmd>,
    pub refresh_devices: bool,
    pub permissions_missing: bool,
}

impl State {
    pub fn is_paused(&self) -> bool {
        self.recording.as_ref().is_some_and(|s| s.is_paused())
    }

    /// Recorded time (excluding pauses) of the running recording.
    pub fn clock(&self) -> String {
        self.recording.as_ref().map(|s| record::clock(s.recorded_secs())).unwrap_or_default()
    }

    /// Whether the running session can switch to `scene` live.
    pub fn can_switch_live(&self, scene: Scene) -> bool {
        let cam_running = self.recording.as_ref().is_some_and(|s| s.camera_name().is_some());
        !scene.uses_cam() || cam_running
    }
}

/// A finalized recording: (file, outcome).
type Finished = (PathBuf, Result<Stats, String>);

struct App {
    gpu: Option<Gpu>,
    main: Option<Win>,
    settings_win: Option<Win>,
    main_visible: bool,
    st: State,
    menubar: Option<menubar::MenuBar>,
    _main_menu: Option<objc2::rc::Retained<menubar::Target>>,
    scaler: preview::Scaler,
    last_tick: Instant,
    last_meter: Instant,
    start_recording: bool,
    selftest: Option<SelfTest>,
    finished: Arc<Mutex<Option<Finished>>>,
}

fn load_devices(st: &mut State) {
    st.displays = crate::screen::displays()
        .map(|ds| {
            ds.iter()
                .enumerate()
                .map(|(i, d)| format!("Display {} — {}×{}", i + 1, d.pixels.0, d.pixels.1))
                .collect()
        })
        .unwrap_or_default();
    st.windows = crate::screen::windows()
        .map(|ws| {
            ws.into_iter()
                .filter(|w| !w.title.is_empty())
                .map(|w| (w.id, format!("{} — {}", w.app, w.title)))
                .collect()
        })
        .unwrap_or_default();
    st.cameras = crate::camera::cameras().into_iter().map(|d| Device { id: d.id, name: d.name }).collect();
    st.mics = crate::camera::microphones().into_iter().map(|d| Device { id: d.id, name: d.name }).collect();
}

impl App {
    fn new(start_recording: bool) -> App {
        let mut st = State {
            settings: Settings::load(),
            recording: None,
            preview: None,
            preview_settings: None,
            finishing: false,
            last_file: None,
            error: None,
            meters: Default::default(),
            displays: vec![],
            windows: vec![],
            cameras: vec![],
            mics: vec![],
            hotkey_ok: true,
            preview_tex: None,
            preview_aspect: 16.0 / 9.0,
            frames_shown: 0,
            cmds: vec![],
            refresh_devices: false,
            permissions_missing: false,
        };
        load_devices(&mut st);
        App {
            gpu: None,
            main: None,
            settings_win: None,
            main_visible: true,
            st,
            menubar: None,
            _main_menu: None,
            scaler: preview::Scaler::new(),
            last_tick: Instant::now(),
            last_meter: Instant::now(),
            start_recording,
            selftest: std::env::var_os("REC_GUI_SELFTEST").map(|_| SelfTest::default()),
            finished: Arc::new(Mutex::new(None)),
        }
    }

    fn gpu(&mut self, surface: &wgpu::Surface<'static>) -> &Gpu {
        if self.gpu.is_none() {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(surface),
                ..Default::default()
            }))
            .expect("no compatible GPU adapter");
            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("rec-rs"),
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                ..Default::default()
            }))
            .expect("create device");
            self.gpu = Some(Gpu { instance, adapter, device, queue });
        }
        self.gpu.as_ref().unwrap()
    }

    fn open_window(&mut self, el: &ActiveEventLoop, title: &str, size: (f64, f64), min: (f64, f64)) -> Win {
        use winit::platform::macos::WindowAttributesExtMacOS;
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(size.0, size.1))
            .with_min_inner_size(winit::dpi::LogicalSize::new(min.0, min.1))
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true);
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        // A throwaway instance creates the first surface; the shared one
        // after that (surfaces must come from the adapter's instance).
        let surface = match &self.gpu {
            Some(g) => g.instance.create_surface(window.clone()).expect("surface"),
            None => {
                let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
                let s = instance.create_surface(window.clone()).expect("surface");
                let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&s),
                    ..Default::default()
                }))
                .expect("no compatible GPU adapter");
                let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("rec-rs"),
                    required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                    ..Default::default()
                }))
                .expect("create device");
                self.gpu = Some(Gpu { instance, adapter, device, queue });
                s
            }
        };
        let g = self.gpu(&surface);
        let caps = surface.get_capabilities(&g.adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let px = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: px.width.max(1),
            height: px.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&g.device, &config);
        let renderer = egui_wgpu::Renderer::new(&g.device, format, egui_wgpu::RendererOptions::default());
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let egui = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(g.device.limits().max_texture_dimension_2d as usize),
        );
        let (x, y) = theme::TRAFFIC_LIGHTS;
        macos::inset_traffic_lights(&window, x, y);
        window.request_redraw();
        Win { window, surface, config, egui, ctx, renderer, repaint_at: None }
    }

    fn show_main(&mut self, el: &ActiveEventLoop) {
        if self.main.is_none() {
            let w = self.open_window(el, "Rec", (1040.0, 660.0), (860.0, 560.0));
            self.main = Some(w);
        }
        if let Some(w) = &self.main {
            w.window.set_visible(true);
            w.window.focus_window();
            w.window.request_redraw();
        }
        self.main_visible = true;
        preview::set_wanted(true);
        macos::activate();
        self.sync_preview();
    }

    fn hide_main(&mut self) {
        if let Some(w) = &self.main {
            w.window.set_visible(false);
        }
        self.main_visible = false;
        preview::set_wanted(false);
        self.sync_preview();
    }

    fn show_settings(&mut self, el: &ActiveEventLoop) {
        if self.settings_win.is_none() {
            let w = self.open_window(el, "Rec Settings", (520.0, 640.0), (460.0, 480.0));
            self.settings_win = Some(w);
        }
        if let Some(w) = &self.settings_win {
            w.window.focus_window();
            w.window.request_redraw();
        }
        macos::activate();
    }

    fn options(&self, output: Option<&std::path::Path>) -> Result<(record::Options, Option<crate::screen::Target>)> {
        let args = self.st.settings.record_args(output);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::prepare(crate::parse_record_args(&refs)?)
    }

    /// Run a preview session while the window is visible and nothing is
    /// recording; restart it when the settings change.
    fn sync_preview(&mut self) {
        let want = self.main_visible && self.st.recording.is_none() && !self.st.finishing;
        let stale = self.st.preview_settings.as_ref() != Some(&self.st.settings);
        if self.st.preview.is_some() && (!want || stale) {
            if let Some(s) = self.st.preview.take() {
                let _ = s.finish();
            }
            self.st.preview_settings = None;
        }
        if want && self.st.preview.is_none() && self.st.error.is_none() {
            let cb = preview::callback(Arc::new(wake));
            match self.options(None).and_then(|(o, t)| Session::start(&o, t, false, Some(cb))) {
                Ok(s) => {
                    self.st.preview = Some(s);
                    self.st.permissions_missing = false;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.st.permissions_missing = msg.contains("missing permission");
                    self.st.error = Some(format!("Preview unavailable: {msg}"));
                }
            }
            self.st.preview_settings = Some(self.st.settings.clone());
        }
    }

    fn toggle_recording(&mut self) {
        if self.st.finishing {
            return;
        }
        if self.st.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(&mut self) {
        // One capture session at a time: the recording feeds the preview.
        if let Some(s) = self.st.preview.take() {
            let _ = s.finish();
        }
        self.st.preview_settings = None;
        self.st.error = None;
        let file = self.st.settings.new_output_file();
        let cb = preview::callback(Arc::new(wake));
        let r = std::fs::create_dir_all(&self.st.settings.output_dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| self.options(Some(&file)))
            .and_then(|(o, t)| Session::start(&o, t, true, Some(cb)));
        match r {
            Ok(sess) => {
                let _ = std::fs::write(record::pidfile_path(), format!("{}\n{}\n", std::process::id(), file.display()));
                self.st.recording = Some(sess);
                record::PAUSE_REQ.store(false, Ordering::SeqCst);
                record::RESUME_REQ.store(false, Ordering::SeqCst);
            }
            Err(e) => {
                eprintln!("rec: could not start recording: {e:#}");
                self.st.error = Some(format!("Could not start recording: {e:#}"));
                self.sync_preview();
            }
        }
    }

    fn stop_recording(&mut self) {
        let Some(sess) = self.st.recording.take() else { return };
        let file = sess.output.clone().unwrap_or_default();
        self.st.finishing = true;
        let done = self.finished.clone();
        // Finalizing can take a moment; keep the UI responsive.
        std::thread::spawn(move || {
            let r = objc2::rc::autoreleasepool(|_| sess.finish()).map_err(|e| format!("{e:#}"));
            *done.lock().unwrap() = Some((file, r));
            let _ = std::fs::remove_file(record::pidfile_path());
            wake();
        });
    }

    fn toggle_pause(&mut self) {
        if let Some(s) = &self.st.recording {
            if s.is_paused() {
                s.resume();
            } else {
                s.pause();
            }
        }
    }

    fn quit(&mut self, el: &ActiveEventLoop) {
        if let Some(s) = self.st.recording.take() {
            let file = s.output.clone();
            let r = objc2::rc::autoreleasepool(|_| s.finish());
            if self.selftest.is_some() {
                println!("selftest: saved on quit={file:?} ok={}", r.is_ok());
            }
            let _ = std::fs::remove_file(record::pidfile_path());
        }
        while self.st.finishing && self.finished.lock().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(s) = self.st.preview.take() {
            let _ = s.finish();
        }
        dock::allow_terminate();
        el.exit();
    }

    fn handle(&mut self, el: &ActiveEventLoop, c: Cmd) {
        match c {
            Cmd::ToggleRecording => self.toggle_recording(),
            Cmd::TogglePause => self.toggle_pause(),
            Cmd::ShowWindow => self.show_main(el),
            Cmd::ShowSettings => self.show_settings(el),
            Cmd::RevealLast => {
                if let Some(f) = &self.st.last_file {
                    macos::reveal(f);
                }
            }
            Cmd::OpenFolder => macos::open_folder(&self.st.settings.output_dir),
            Cmd::Quit => self.quit(el),
        }
    }

    /// Periodic work: commands, signals, finished recordings, failures,
    /// menu bar, meters.
    fn tick(&mut self, el: &ActiveEventLoop) {
        if let Some(mut t) = self.selftest.take() {
            t.step(self);
            self.selftest = Some(t);
        }
        if SIG_QUIT.swap(false, Ordering::SeqCst) {
            self.quit(el);
            return;
        }
        if SIG_STOP.swap(false, Ordering::SeqCst) && self.st.recording.is_some() {
            self.stop_recording();
        }
        if self.st.recording.as_ref().is_some_and(|s| s.has_frames()) {
            if record::PAUSE_REQ.swap(false, Ordering::SeqCst) {
                self.st.recording.as_ref().unwrap().pause();
            }
            if record::RESUME_REQ.swap(false, Ordering::SeqCst) {
                self.st.recording.as_ref().unwrap().resume();
            }
        }
        let cmds: Vec<Cmd> = std::mem::take(&mut *QUEUE.lock().unwrap());
        let ui_cmds = std::mem::take(&mut self.st.cmds);
        for c in cmds.into_iter().chain(ui_cmds) {
            self.handle(el, c);
        }
        let done = self.finished.lock().unwrap().take();
        if let Some((file, r)) = done {
            self.st.finishing = false;
            match r {
                Ok(_) => self.st.last_file = Some(file),
                Err(e) => self.st.error = Some(format!("Recording failed: {e}")),
            }
        }
        let failed = self.st.recording.as_ref().is_some_and(|s| s.failed());
        if failed {
            let err = self.st.recording.as_ref().and_then(|s| s.error()).unwrap_or_default();
            self.stop_recording();
            self.st.error = Some(format!("Recording stopped: {err}"));
        }
        if self.st.preview.as_ref().is_some_and(|s| s.failed()) {
            let err = self.st.preview.as_ref().and_then(|s| s.error()).unwrap_or_default();
            if let Some(s) = self.st.preview.take() {
                let _ = s.finish();
            }
            self.st.error = Some(format!("Preview stopped: {err}"));
        }
        if self.st.refresh_devices {
            self.st.refresh_devices = false;
            load_devices(&mut self.st);
        }
        // Meters.
        let dt = self.last_meter.elapsed().as_secs_f32();
        self.last_meter = Instant::now();
        let sess = self.st.recording.as_ref().or(self.st.preview.as_ref());
        let lv = sess.map(|s| s.poll().1).unwrap_or([0.0; 2]);
        for (m, l) in self.st.meters.iter_mut().zip(lv) {
            m.feed(l, dt);
        }
        self.sync_preview();
        let clock = self.st.clock();
        let view = menubar::View {
            recording: self.st.recording.is_some(),
            paused: self.st.is_paused(),
            finishing: self.st.finishing,
            clock: &clock,
            has_last: self.st.last_file.is_some(),
        };
        if let Some(m) = &mut self.menubar {
            m.update(&view);
        }
    }

    fn redraw(&mut self, which: WindowId) {
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == which);
        if is_main
            && let Some(img) = self.scaler.take()
        {
            self.st.preview_aspect = img.size[0] as f32 / img.size[1] as f32;
            let w = self.main.as_ref().unwrap();
            match &mut self.st.preview_tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.st.preview_tex = Some(w.ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
            self.st.frames_shown += 1;
        }
        let Some(gpu) = self.gpu.as_ref() else { return };
        let win = if is_main { self.main.as_mut() } else { self.settings_win.as_mut() };
        let Some(win) = win else { return };
        let input = win.egui.take_egui_input(&win.window);
        let st = &mut self.st;
        let out = win.ctx.run_ui(input, |ui| {
            if is_main {
                ui::main_window(ui, st);
            } else {
                ui::settings_window(ui, st);
            }
        });
        win.egui.handle_platform_output(&win.window, out.platform_output);
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay);
        win.repaint_at = if delay < Duration::from_secs(60) { Some(Instant::now() + delay) } else { None };
        let ppp = out.pixels_per_point;
        let prims = win.ctx.tessellate(out.shapes, ppp);
        // Upload every frame, even when the surface frame is skipped, or a
        // later partial update targets a texture that was never allocated.
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                win.renderer.update_texture(&gpu.device, &gpu.queue, *id, d);
            }
        }
        let frame = match win.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                win.surface.configure(&gpu.device, &win.config);
                for id in &out.textures_delta.free {
                    win.renderer.free_texture(id);
                }
                return;
            }
        };
        let (x, y) = theme::TRAFFIC_LIGHTS;
        macos::inset_traffic_lights(&win.window, x, y);
        let view = frame.texture.create_view(&Default::default());
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [win.config.width, win.config.height], pixels_per_point: ppp };
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        let extra = win.renderer.update_buffers(&gpu.device, &gpu.queue, &mut enc, &prims, &sd);
        {
            let bg = theme::palette(win.ctx.theme() == egui::Theme::Dark).window;
            let [r, g, b, _] = bg.to_normalized_gamma_f32();
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            win.renderer.render(&mut pass, &prims, &sd);
        }
        gpu.queue.submit(extra.into_iter().chain([enc.finish()]));
        win.window.pre_present_notify();
        gpu.queue.present(frame);
        for id in &out.textures_delta.free {
            win.renderer.free_texture(id);
        }
        // Drag the window by the toolbar (the titlebar is transparent).
        if ui::take_drag_request() {
            let _ = win.window.drag_window();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.menubar.is_some() {
            return;
        }
        let mtm = objc2::MainThreadMarker::new().expect("main thread");
        let mb = menubar::MenuBar::new(mtm);
        let ok = crate::hotkey::register(crate::hotkey::KEY_R, crate::hotkey::CMD | crate::hotkey::SHIFT, || {
            push(Cmd::ToggleRecording)
        });
        if let Err(code) = ok {
            eprintln!("rec: could not register ⇧⌘R (error {code}); use the menu or the window instead");
            mb.hotkey_unavailable();
            self.st.hotkey_ok = false;
        }
        self.menubar = Some(mb);
        self._main_menu = Some(menubar::install_main_menu(mtm));
        macos::set_dock_icon();
        if std::env::var_os("REC_GUI_HIDDEN").is_some() {
            self.main_visible = false;
        } else {
            self.show_main(el);
        }
        if self.start_recording {
            push(Cmd::ToggleRecording);
        }
        eprintln!("rec: app running (⇧⌘R toggles recording)");
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        self.tick(el);
        if let Some(w) = &self.main
            && self.main_visible
        {
            w.window.request_redraw();
        }
        if let Some(w) = &self.settings_win {
            w.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == id);
        let win = if is_main { self.main.as_mut() } else { self.settings_win.as_mut() };
        let Some(win) = win else { return };
        let resp = win.egui.on_window_event(&win.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                if is_main {
                    self.hide_main();
                } else {
                    self.settings_win = None;
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let s = win.window.inner_size();
                win.config.width = s.width.max(1);
                win.config.height = s.height.max(1);
                if let Some(g) = &self.gpu {
                    win.surface.configure(&g.device, &win.config);
                }
                win.window.request_redraw();
            }
            WindowEvent::ThemeChanged(_) => {
                theme::set_accent(theme::system_accent());
                theme::apply(&win.ctx);
                win.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.redraw(id);
                if !self.st.cmds.is_empty() {
                    self.tick(el);
                }
            }
            _ => {
                if resp.repaint {
                    win.window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.last_tick = Instant::now();
            self.tick(el);
        }
        // Animate while capturing (preview, meters, timer); otherwise idle
        // with a slow tick for signals.
        let live = self.st.recording.is_some() || self.st.preview.is_some() || self.st.finishing;
        let now = Instant::now();
        let mut next = now + Duration::from_millis(if live { 100 } else { 250 });
        for w in [self.main.as_mut(), self.settings_win.as_mut()].into_iter().flatten() {
            let visible = w.window.is_visible().unwrap_or(true);
            if live && visible {
                // Timer, meters: ~15 fps; preview frames wake us on arrival.
                w.window.request_redraw();
            }
            if let Some(t) = w.repaint_at {
                if t <= now {
                    w.repaint_at = None;
                    w.window.request_redraw();
                } else {
                    next = next.min(t);
                }
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(next));
    }
}

/// REC_GUI_SELFTEST=1: drive the real app paths on a timer (preview,
/// record via the hotkey's command, pause/resume, stop, settings window,
/// close) and print what happened.
#[derive(Default)]
struct SelfTest {
    t0: Option<Instant>,
    step: u32,
}

impl SelfTest {
    fn step(&mut self, app: &mut App) {
        let t = self.t0.get_or_insert_with(Instant::now).elapsed().as_secs_f64();
        // REC_GUI_SELFTEST=terminate: quit the way the Dock does
        // (-[NSApp terminate:]) mid-recording; the file must be finalized.
        if std::env::var("REC_GUI_SELFTEST").as_deref() == Ok("terminate") {
            match self.step {
                0 if t > 2.0 => {
                    push(Cmd::ToggleRecording);
                    self.step = 1;
                }
                1 if t > 5.0 => {
                    println!("selftest: recording={} -> NSApp terminate:", app.st.recording.is_some());
                    let mtm = objc2::MainThreadMarker::new().unwrap();
                    objc2_app_kit::NSApplication::sharedApplication(mtm).terminate(None);
                    self.step = 2;
                }
                _ => {}
            }
            return;
        }
        match self.step {
            0 if t > 3.0 => {
                println!("selftest: idle preview session={} frame_shown={}", app.st.preview.is_some(), app.st.frames_shown > 0);
                app.st.frames_shown = 0;
                push(Cmd::ToggleRecording);
                self.step = 1;
            }
            1 if t > 4.0 => {
                println!("selftest: recording={} error={:?}", app.st.recording.is_some(), app.st.error);
                push(Cmd::TogglePause);
                self.step = 2;
            }
            2 if t > 5.5 => {
                println!("selftest: paused={}", app.st.is_paused());
                push(Cmd::TogglePause);
                self.step = 3;
            }
            3 if t > 8.0 => {
                println!("selftest: preview during recording frame_shown={}", app.st.frames_shown > 0);
                println!(
                    "selftest: status title={:?} recorded={:.2}s",
                    app.menubar.as_ref().and_then(|m| m.title()),
                    app.st.recording.as_ref().map_or(0.0, |s| s.recorded_secs())
                );
                push(Cmd::ToggleRecording);
                self.step = 4;
            }
            4 if !app.st.finishing && app.st.recording.is_none() => {
                println!("selftest: saved={:?} error={:?}", app.st.last_file, app.st.error);
                push(Cmd::ShowSettings);
                self.step = 5;
            }
            5 if t > 10.0 => {
                println!(
                    "selftest: settings window={} preview_restarted={}",
                    app.settings_win.is_some(),
                    app.st.preview.is_some()
                );
                app.hide_main();
                println!("selftest: after close preview_session={}", app.st.preview.is_some());
                push(Cmd::Quit);
                self.step = 6;
            }
            _ => {}
        }
    }
}

pub fn run(start_recording: bool, settings: bool) -> Result<()> {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, on_sigterm as *const () as usize);
        libc::signal(libc::SIGHUP, on_sigterm as *const () as usize);
    }
    record::install_pause_signals();
    let el = EventLoop::<()>::with_user_event()
        // A regular app: Dock icon (support/AppIcon.icns) and a menu bar.
        .with_activation_policy(ActivationPolicy::Regular)
        .with_default_menu(false)
        .build()?;
    let _ = PROXY.set(Mutex::new(el.create_proxy()));
    dock::install();
    let mut app = App::new(start_recording);
    if settings {
        push(Cmd::ShowSettings);
    }
    el.run_app(&mut app)?;
    let _ = std::fs::remove_file(record::pidfile_path());
    Ok(())
}
