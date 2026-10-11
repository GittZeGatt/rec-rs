//! egui layout of the main window and the settings window.

use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Align, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke, Ui, vec2};

use super::theme::{self, BODY, SECONDARY, TOOLBAR_H, TRAFFIC_LIGHTS_W};
use super::{Cmd, Meter, State};
use crate::compose::{self, Corner, Place, Scene, Shape};
use crate::settings::Settings;
use crate::settings::{FPS, RESOLUTIONS};
use crate::writer::{AudioLayout, Codec};

const RED: Color32 = Color32::from_rgb(0xff, 0x3b, 0x30);
const SIDEBAR_W: f32 = 290.0;

static DRAG: AtomicBool = AtomicBool::new(false);

/// Whether the toolbar asked to drag the window this frame.
pub fn take_drag_request() -> bool {
    DRAG.swap(false, Ordering::Relaxed)
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).font(theme::semibold(SECONDARY)).color(theme::pal(ui).secondary));
    ui.add_space(2.0);
}

/// Toolbar strip: empty space drags the window.
fn toolbar(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let rect = ui.max_rect();
    let bg = ui.interact(rect, ui.id().with("drag"), Sense::click_and_drag());
    if bg.drag_started() {
        DRAG.store(true, Ordering::Relaxed);
    }
    ui.horizontal_centered(|ui| {
        ui.add_space(TRAFFIC_LIGHTS_W);
        add(ui);
    });
    let p = theme::pal(ui);
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.separator));
}

fn pill(ui: &mut Ui, text: &str, fill: Color32, fg: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::semibold(SECONDARY), fg);
    let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(16.0, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
}

pub fn main_window(ui: &mut Ui, st: &mut State) {
    let p = theme::pal(ui);
    let recording = st.recording.is_some();
    egui::Panel::top("toolbar")
        .exact_size(TOOLBAR_H)
        .frame(egui::Frame::NONE.fill(p.window).inner_margin(Margin::symmetric(12, 0)))
        .show(ui, |ui| {
            toolbar(ui, |ui| {
                ui.label(RichText::new("Rec").font(theme::semibold(BODY)));
                ui.add_space(6.0);
                if st.finishing {
                    pill(ui, "SAVING…", p.control, p.secondary);
                } else if st.is_paused() {
                    pill(ui, &format!("PAUSED  {}", st.clock()), Color32::from_rgb(0xff, 0x95, 0x00), Color32::WHITE);
                } else if recording {
                    pill(ui, &format!("● REC  {}", st.clock()), RED, Color32::WHITE);
                } else if st.preview.is_some() {
                    pill(ui, "PREVIEW", p.control, p.secondary);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Settings…").on_hover_text("Output, codec, resolution, audio (⌘,)").clicked() {
                        st.cmds.push(Cmd::ShowSettings);
                    }
                    if !st.hotkey_ok {
                        ui.label(RichText::new("⇧⌘R unavailable").color(p.secondary));
                    }
                });
            });
        });
    egui::Panel::bottom("controls")
        .exact_size(68.0)
        .frame(egui::Frame::NONE.fill(p.window).inner_margin(Margin::symmetric(16, 10)))
        .show(ui, |ui| controls(ui, st));
    egui::Panel::right("sources")
        .exact_size(SIDEBAR_W)
        .frame(egui::Frame::NONE.fill(p.sidebar).inner_margin(Margin { left: 14, right: 4, top: 6, bottom: 6 }))
        .show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.set_max_width(SIDEBAR_W - 32.0);
                sources(ui, st)
            });
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(p.window).inner_margin(Margin::same(16)))
        .show(ui, |ui| preview(ui, st));
    // Keyboard: ⌘, settings, ⌘W close (handled as hide by the app), space
    // does nothing (avoid accidental recordings).
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
        st.cmds.push(Cmd::ShowSettings);
    }
}

fn preview(ui: &mut Ui, st: &mut State) {
    let p = theme::pal(ui);
    if let Some(err) = st.error.clone() {
        egui::Frame::NONE
            .fill(Color32::from_rgba_unmultiplied(0xff, 0x3b, 0x30, 30))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(err).color(p.text));
                    if st.permissions_missing && ui.button("Request Permissions").clicked() {
                        crate::tcc::request_all(true, true, true, std::time::Duration::from_secs(0));
                    }
                    if ui.button("Dismiss").clicked() {
                        st.error = None;
                    }
                });
            });
        ui.add_space(10.0);
    }
    let avail = ui.available_size();
    let aspect = st.preview_aspect.max(0.1);
    let mut size = vec2(avail.x, avail.x / aspect);
    if size.y > avail.y {
        size = vec2(avail.y * aspect, avail.y);
    }
    ui.vertical_centered(|ui| {
        ui.add_space(((avail.y - size.y) / 2.0).max(0.0));
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let r = CornerRadius::same(10);
        ui.painter().rect_filled(rect, r, Color32::BLACK);
        let live = st.recording.is_some() || st.preview.is_some();
        match &st.preview_tex {
            Some(t) if live => {
                egui::Image::new((t.id(), size)).corner_radius(r).paint_at(ui, rect);
            }
            _ => {
                let msg = if st.finishing { "Saving…" } else { "No preview" };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(BODY), Color32::GRAY);
            }
        }
        if st.recording.is_some() {
            let border = if st.is_paused() { Color32::from_rgb(0xff, 0x95, 0x00) } else { RED };
            ui.painter().rect_stroke(rect, r, Stroke::new(2.0, border), egui::StrokeKind::Inside);
        }
        if live && st.settings.scene == Scene::ScreenCam {
            drag_overlay(ui, st, rect);
        }
    });
}

/// Output-pixel frame the overlay geometry is computed in (any size with
/// the preview's aspect ratio; positions are fractions of it).
fn frame_size(st: &State) -> (f64, f64) {
    (3840.0, (3840.0 / st.preview_aspect.max(0.1) as f64).round())
}

fn overlay_ui_rect(st: &State, spec: &compose::PipSpec, preview: egui::Rect) -> egui::Rect {
    let (w, h) = frame_size(st);
    let [x, y, ow, oh] = compose::overlay_rect(spec, w, h, st.cam_aspect);
    let (sx, sy) = (preview.width() as f64 / w, preview.height() as f64 / h);
    egui::Rect::from_min_size(
        preview.min + vec2((x * sx) as f32, (y * sy) as f32),
        vec2((ow * sx) as f32, (oh * sy) as f32),
    )
}

/// Drag the camera overlay in the preview. It moves live (in the preview
/// and the recording) and snaps to a corner when released near one.
fn drag_overlay(ui: &mut Ui, st: &mut State, preview: egui::Rect) {
    const SNAP: f64 = 0.05;
    let spec = st.settings.pip_spec();
    let ov = overlay_ui_rect(st, &spec, preview);
    let resp = ui.interact(ov, ui.id().with("pip-drag"), Sense::drag());
    let accent = theme::accent();
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        ui.painter().rect_stroke(ov, CornerRadius::same(4), Stroke::new(1.5, accent), egui::StrokeKind::Outside);
    }
    if resp.dragged() {
        let d = resp.drag_delta();
        let (cx, cy) = ((ov.center().x - preview.min.x) / preview.width(), (ov.center().y - preview.min.y) / preview.height());
        let at = [(cx + d.x / preview.width()).clamp(0.0, 1.0) as f64, (cy + d.y / preview.height()).clamp(0.0, 1.0) as f64];
        st.settings.pip_place = Place::At(at);
        // Show where it would snap.
        let (w, h) = frame_size(st);
        if let Some(c) = compose::snap_corner(&st.settings.pip_spec(), w, h, st.cam_aspect, SNAP) {
            let ghost = overlay_ui_rect(st, &compose::PipSpec { place: Place::Corner(c), ..st.settings.pip_spec() }, preview);
            ui.painter().rect_stroke(ghost, CornerRadius::same(4), Stroke::new(1.0, accent.gamma_multiply(0.7)), egui::StrokeKind::Outside);
        }
        st.apply_pip();
    }
    if resp.drag_stopped() {
        let (w, h) = frame_size(st);
        if let Some(c) = compose::snap_corner(&st.settings.pip_spec(), w, h, st.cam_aspect, SNAP) {
            st.settings.pip_place = Place::Corner(c);
        }
        st.apply_pip();
        let _ = st.settings.save();
    }
}

/// A small icon of `shape` (with the current roundness).
fn shape_icon(ui: &Ui, rect: egui::Rect, shape: Shape, roundness: f64, color: Color32) {
    let c = rect.center();
    let (w, h) = if shape.is_square() { (16.0, 16.0) } else { (22.0, 13.0) };
    let r = egui::Rect::from_center_size(c, vec2(w, h));
    let painter = ui.painter();
    let stroke = Stroke::new(1.5, color);
    match shape {
        Shape::Circle => {
            painter.circle_stroke(c, 8.0, stroke);
        }
        Shape::Squircle => {
            let n = compose::squircle_exponent(roundness);
            let pts: Vec<egui::Pos2> = (0..64)
                .map(|i| {
                    let t = i as f64 / 64.0 * std::f64::consts::TAU;
                    let (ct, s) = (t.cos(), t.sin());
                    let x = ct.signum() * ct.abs().powf(2.0 / n) * 8.0;
                    let y = s.signum() * s.abs().powf(2.0 / n) * 8.0;
                    c + vec2(x as f32, y as f32)
                })
                .collect();
            painter.add(egui::Shape::closed_line(pts, stroke));
        }
        _ => {
            let rad = match shape {
                Shape::Pill => h / 2.0,
                Shape::Rect => 0.0,
                _ => roundness as f32 * h / 2.0,
            };
            painter.rect_stroke(r, CornerRadius::same(rad.round() as u8), stroke, egui::StrokeKind::Middle);
        }
    }
}

/// Camera overlay look and position. Live: also while recording.
fn overlay_controls(ui: &mut Ui, s: &mut Settings) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let bw = ((ui.available_width() - 16.0) / 5.0).max(30.0);
        for shape in Shape::PICKER {
            let sel = s.pip_shape == shape;
            let (rect, resp) = ui.allocate_exact_size(vec2(bw, 30.0), Sense::click());
            let fill = if sel { theme::accent() } else if resp.hovered() { p.hover } else { p.control };
            ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
            shape_icon(ui, rect, shape, s.pip_roundness, if sel { Color32::WHITE } else { p.text });
            if resp.on_hover_text(shape.label()).clicked() {
                s.pip_shape = shape;
            }
        }
    });
    ui.add_space(4.0);
    // Label (64) + slider + value box (about 52) + spacing must fit.
    let slider_w = (ui.available_width() - 64.0 - 60.0 - 40.0).max(60.0);
    let row = |ui: &mut Ui, label: &str, add: &mut dyn FnMut(&mut Ui)| {
        ui.horizontal(|ui| {
            ui.add_sized(vec2(64.0, 18.0), egui::Label::new(RichText::new(label).size(SECONDARY).color(p.secondary)));
            add(ui);
        });
    };
    ui.spacing_mut().slider_width = slider_w;
    row(ui, "Size", &mut |ui| {
        let mut pct = s.pip_size * 100.0;
        if ui.add(egui::Slider::new(&mut pct, 10.0..=50.0).suffix(" %").integer()).changed() {
            s.pip_size = pct / 100.0;
        }
    });
    row(ui, "Roundness", &mut |ui| {
        let mut pct = s.pip_roundness * 100.0;
        if ui.add_enabled(s.pip_shape.uses_roundness(), egui::Slider::new(&mut pct, 0.0..=100.0).suffix(" %").integer()).changed() {
            s.pip_roundness = pct / 100.0;
        }
    });
    row(ui, "Border", &mut |ui| {
        // Narrower slider: the color swatch shares the row.
        ui.spacing_mut().slider_width = slider_w - 30.0;
        ui.add(egui::Slider::new(&mut s.pip_border, 0.0..=20.0).suffix(" px").integer());
        ui.add_enabled_ui(s.pip_border > 0.0, |ui| {
            ui.spacing_mut().interact_size = vec2(22.0, 18.0);
            ui.color_edit_button_srgb(&mut s.pip_border_color);
        });
    });
    row(ui, "Position", &mut |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        for (c, label, tip) in [
            (Corner::TopLeft, "↖", "Top left"),
            (Corner::TopRight, "↗", "Top right"),
            (Corner::BottomLeft, "↙", "Bottom left"),
            (Corner::BottomRight, "↘", "Bottom right"),
        ] {
            let sel = s.pip_place == Place::Corner(c);
            if ui.add(egui::Button::selectable(sel, label).min_size(vec2(26.0, 22.0))).on_hover_text(tip).clicked() {
                s.pip_place = Place::Corner(c);
            }
        }
        if matches!(s.pip_place, Place::At(_)) {
            ui.label(RichText::new("custom").size(SECONDARY).color(p.secondary));
        }
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut s.pip_shadow, "Shadow");
        ui.checkbox(&mut s.mirror, "Mirror");
    });
    ui.label(RichText::new("Drag the camera in the preview to move it; it snaps to corners.").size(SECONDARY).color(p.secondary));
}

fn controls(ui: &mut Ui, st: &mut State) {
    let p = theme::pal(ui);
    let recording = st.recording.is_some();
    ui.horizontal_centered(|ui| {
        // Record / stop: a round button like a camera's.
        let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::click());
        let c = rect.center();
        let painter = ui.painter();
        painter.circle_stroke(c, 21.0, Stroke::new(3.0, if resp.hovered() { p.text } else { p.secondary }));
        if st.finishing {
            painter.circle_filled(c, 15.0, p.tertiary);
        } else if recording {
            painter.rect_filled(egui::Rect::from_center_size(c, vec2(18.0, 18.0)), CornerRadius::same(4), RED);
        } else {
            painter.circle_filled(c, 16.0, RED);
        }
        let tip = if recording { "Stop recording (⇧⌘R)" } else { "Start recording (⇧⌘R)" };
        if resp.on_hover_text(tip).clicked() && !st.finishing {
            st.cmds.push(Cmd::ToggleRecording);
        }
        ui.add_space(6.0);
        let paused = st.is_paused();
        if ui
            .add_enabled(recording, egui::Button::new(if paused { "Resume" } else { "Pause" }).min_size(vec2(72.0, 26.0)))
            .clicked()
        {
            st.cmds.push(Cmd::TogglePause);
        }
        ui.add_space(10.0);
        let clock = if recording { st.clock() } else { "00:00".into() };
        let color = if paused { p.secondary } else if recording { RED } else { p.tertiary };
        ui.label(RichText::new(clock).font(egui::FontId::monospace(22.0)).color(color));
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.add_space(4.0);
            meter(ui, "System", st.meters[0], st.settings.system_audio);
            meter(ui, "Mic", st.meters[1], st.settings.mic_enabled);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Open Folder").clicked() {
                st.cmds.push(Cmd::OpenFolder);
            }
            if st.last_file.is_some() && ui.button("Reveal Last").clicked() {
                st.cmds.push(Cmd::RevealLast);
            }
        });
    });
}

/// Horizontal peak meter, -60..0 dBFS, green/yellow/red with a peak tick.
fn meter(ui: &mut Ui, label: &str, m: Meter, on: bool) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.add_sized(vec2(44.0, 14.0), egui::Label::new(RichText::new(label).size(SECONDARY).color(p.secondary)));
        let (rect, _) = ui.allocate_exact_size(vec2(220.0, 8.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(3), p.control);
        if !on {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "off", egui::FontId::proportional(9.0), p.tertiary);
            return;
        }
        let frac = |db: f32| ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let x = |f: f32| rect.left() + rect.width() * f;
        let level = frac(m.db);
        for (lo, hi, col) in [
            (0.0, frac(-18.0), Color32::from_rgb(0x34, 0xc7, 0x59)),
            (frac(-18.0), frac(-6.0), Color32::from_rgb(0xff, 0xcc, 0x00)),
            (frac(-6.0), 1.0, RED),
        ] {
            let hi = hi.min(level);
            if hi > lo {
                let r = egui::Rect::from_x_y_ranges(x(lo)..=x(hi), rect.y_range());
                painter.rect_filled(r, CornerRadius::same(2), col);
            }
        }
        if m.peak_db > -60.0 {
            let px = x(frac(m.peak_db));
            painter.vline(px, rect.y_range(), Stroke::new(2.0, p.text));
        }
        ui.label(
            RichText::new(if m.db <= -89.0 { "-∞".to_string() } else { format!("{:.0}", m.db) })
                .size(SECONDARY)
                .color(p.secondary),
        );
    });
}

fn combo<T: Clone + PartialEq>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, String)], enabled: bool) -> bool {
    let current = options.iter().find(|(v, _)| v == value).map(|(_, l)| l.clone()).unwrap_or_default();
    let mut changed = false;
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(id).width(ui.available_width()).selected_text(current).show_ui(ui, |ui| {
            for (v, label) in options {
                if ui.selectable_label(v == value, label).clicked() && v != value {
                    *value = v.clone();
                    changed = true;
                }
            }
        });
    });
    changed
}

fn sources(ui: &mut Ui, st: &mut State) {
    let recording = st.recording.is_some() || st.finishing;
    let idle = !recording;
    let mut s = st.settings.clone();

    section(ui, "SCENE");
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let w = (ui.available_width() - 8.0) / 3.0;
        for scene in Scene::ALL {
            // While recording, only scenes the running session supports.
            let ok = idle || st.can_switch_live(scene);
            let label = match scene {
                Scene::Screen => "Screen",
                Scene::ScreenCam => "Screen + Cam",
                Scene::Cam => "Camera",
            };
            let sel = s.scene == scene;
            let b = egui::Button::selectable(sel, RichText::new(label).size(SECONDARY + 1.0)).min_size(vec2(w, 26.0));
            let resp = ui.add_enabled(ok, b);
            let resp = if ok { resp } else { resp.on_disabled_hover_text("Start the recording with a camera scene to switch live") };
            if resp.clicked() && !sel {
                s.scene = scene;
                if let Some(r) = &st.recording {
                    r.set_scene(scene);
                }
            }
        }
    });

    section(ui, "SCREEN");
    let displays: Vec<(usize, String)> = st.displays.iter().enumerate().map(|(i, d)| (i + 1, d.clone())).collect();
    combo(ui, "display", &mut s.display, &displays, idle);
    ui.add_space(4.0);
    let mut targets: Vec<(Option<u32>, String)> = vec![(None, "Entire display".into())];
    targets.extend(st.windows.iter().map(|(id, l)| (Some(*id), l.clone())));
    ui.horizontal(|ui| {
        let w = ui.available_width() - 30.0;
        ui.allocate_ui(vec2(w, 24.0), |ui| {
            combo(ui, "window", &mut s.window, &targets, idle);
        });
        if ui.add_enabled(idle, egui::Button::new("↻")).on_hover_text("Refresh windows and devices").clicked() {
            st.refresh_devices = true;
        }
    });

    section(ui, "CAMERA");
    let mut cams: Vec<(Option<String>, String)> = vec![(None, "Default camera".into())];
    cams.extend(st.cameras.iter().map(|d| (Some(d.id.clone()), d.name.clone())));
    combo(ui, "camera", &mut s.camera, &cams, idle && s.scene.uses_cam());
    if !s.scene.uses_cam() {
        ui.label(RichText::new("Pick a camera scene to use the webcam.").size(SECONDARY).color(theme::pal(ui).secondary));
    }
    if s.scene == Scene::ScreenCam {
        section(ui, "CAMERA OVERLAY");
        overlay_controls(ui, &mut s);
    }

    section(ui, "AUDIO");
    ui.add_enabled(idle, egui::Checkbox::new(&mut s.system_audio, "System audio"));
    ui.add_enabled(idle, egui::Checkbox::new(&mut s.mic_enabled, "Microphone"));
    let mut mics: Vec<(Option<String>, String)> = vec![(None, "Default microphone".into())];
    mics.extend(st.mics.iter().map(|d| (Some(d.id.clone()), d.name.clone())));
    combo(ui, "mic", &mut s.mic, &mics, idle && s.mic_enabled);

    section(ui, "OUTPUT");
    let codec = s.codec.map_or("Auto codec".to_string(), |c| format!("{c:?}").replace("H264", "H.264").replace("Hevc", "HEVC").replace("Prores", "ProRes 422"));
    ui.label(
        RichText::new(format!("{} · {} fps · {} · .{}", s.resolution.to_uppercase(), s.fps, codec, s.container))
            .size(SECONDARY)
            .color(theme::pal(ui).secondary),
    );
    ui.label(RichText::new(s.output_dir.display().to_string()).size(SECONDARY).color(theme::pal(ui).secondary));

    if s != st.settings {
        st.settings = s;
        let _ = st.settings.save();
    }
}

pub fn settings_window(ui: &mut Ui, st: &mut State) {
    let p = theme::pal(ui);
    let idle = st.recording.is_none() && !st.finishing;
    egui::Panel::top("settings-toolbar")
        .exact_size(TOOLBAR_H)
        .frame(egui::Frame::NONE.fill(p.window).inner_margin(Margin::symmetric(12, 0)))
        .show(ui, |ui| {
            toolbar(ui, |ui| {
                ui.label(RichText::new("Settings").font(theme::semibold(BODY)));
            });
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(p.window).inner_margin(Margin::symmetric(20, 12)))
        .show(ui, |ui| {
            if !idle {
                ui.label(RichText::new("Settings are locked while recording.").color(p.secondary));
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.add_enabled_ui(idle, |ui| settings_form(ui, st));
                // The overlay stays editable while recording (applied live).
                section(ui, "CAMERA OVERLAY");
                let mut s = st.settings.clone();
                overlay_controls(ui, &mut s);
                if s != st.settings {
                    st.settings = s;
                    st.apply_pip();
                    let _ = st.settings.save();
                }
            });
        });
}

fn settings_form(ui: &mut Ui, st: &mut State) {
    let mut s = st.settings.clone();
    let grid = |ui: &mut Ui, id: &str, add: &mut dyn FnMut(&mut Ui)| {
        egui::Grid::new(id).num_columns(2).spacing(vec2(12.0, 8.0)).min_col_width(110.0).show(ui, |ui| add(ui));
    };

    section(ui, "OUTPUT");
    grid(ui, "out", &mut |ui| {
        ui.label("Folder");
        ui.horizontal(|ui| {
            ui.label(RichText::new(s.output_dir.display().to_string()).size(SECONDARY));
            if ui.button("Choose…").clicked()
                && let Some(d) = rfd::FileDialog::new().set_directory(&s.output_dir).pick_folder()
            {
                s.output_dir = d;
            }
        });
        ui.end_row();
        ui.label("Format");
        let opts = [("mov".to_string(), "QuickTime (.mov)".to_string()), ("mp4".to_string(), "MPEG-4 (.mp4)".to_string())];
        combo(ui, "container", &mut s.container, &opts, s.codec != Some(Codec::Prores));
        ui.end_row();
    });

    section(ui, "VIDEO");
    grid(ui, "video", &mut |ui| {
        ui.label("Resolution");
        let opts: Vec<(String, String)> = RESOLUTIONS.iter().map(|(l, v)| (v.to_string(), l.to_string())).collect();
        combo(ui, "res", &mut s.resolution, &opts, true);
        ui.end_row();
        ui.label("Frame rate");
        let opts: Vec<(u32, String)> = FPS.iter().map(|f| (*f, format!("{f} fps"))).collect();
        combo(ui, "fps", &mut s.fps, &opts, true);
        ui.end_row();
        ui.label("Codec");
        let opts = vec![
            (None, "Auto (H.264 ≤1080p, HEVC above)".to_string()),
            (Some(Codec::H264), "H.264".to_string()),
            (Some(Codec::Hevc), "HEVC (H.265)".to_string()),
            (Some(Codec::Prores), "ProRes 422 (.mov)".to_string()),
        ];
        combo(ui, "codec", &mut s.codec, &opts, true);
        ui.end_row();
        ui.label("Bitrate");
        ui.horizontal(|ui| {
            let mut auto = s.bitrate_mbps.is_none();
            if ui.add_enabled(s.codec != Some(Codec::Prores), egui::Checkbox::new(&mut auto, "Auto")).changed() {
                s.bitrate_mbps = if auto { None } else { Some(30.0) };
            }
            if let Some(b) = &mut s.bitrate_mbps {
                ui.add(egui::Slider::new(b, 2.0..=200.0).suffix(" Mb/s").logarithmic(true).integer());
            }
        });
        ui.end_row();
        ui.label("Cursor");
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.cursor, "Show");
            ui.add_enabled(s.cursor, egui::Checkbox::new(&mut s.clicks, "Highlight clicks"));
        });
        ui.end_row();
    });

    section(ui, "AUDIO");
    grid(ui, "audio", &mut |ui| {
        ui.label("System audio");
        ui.checkbox(&mut s.system_audio, "Record");
        ui.end_row();
        ui.label("Microphone");
        let mut mics: Vec<(Option<String>, String)> = vec![(None, "Default microphone".into())];
        mics.extend(st.mics.iter().map(|d| (Some(d.id.clone()), d.name.clone())));
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.mic_enabled, "");
            combo(ui, "mic2", &mut s.mic, &mics, s.mic_enabled);
        });
        ui.end_row();
        ui.label("Tracks");
        let opts = vec![
            (AudioLayout::Both, "Mix + separate system & mic".to_string()),
            (AudioLayout::Mix, "Mix only".to_string()),
            (AudioLayout::Stems, "Separate only".to_string()),
        ];
        combo(ui, "tracks", &mut s.audio_tracks, &opts, true);
        ui.end_row();
        ui.label("System gain");
        ui.add(egui::Slider::new(&mut s.system_gain_db, -24.0..=12.0).suffix(" dB").step_by(0.5));
        ui.end_row();
        ui.label("Mic gain");
        ui.add(egui::Slider::new(&mut s.mic_gain_db, -24.0..=24.0).suffix(" dB").step_by(0.5));
        ui.end_row();
    });

    section(ui, "CAMERA");
    grid(ui, "cam", &mut |ui| {
        ui.label("Camera");
        let mut cams: Vec<(Option<String>, String)> = vec![(None, "Default camera".into())];
        cams.extend(st.cameras.iter().map(|d| (Some(d.id.clone()), d.name.clone())));
        combo(ui, "cam2", &mut s.camera, &cams, true);
        ui.end_row();
    });
    ui.add_space(12.0);
    if ui.button("Restore Defaults").clicked() {
        s = crate::settings::Settings { output_dir: s.output_dir.clone(), ..Default::default() };
    }
    if s != st.settings {
        st.settings = s;
        let _ = st.settings.save();
    }
}
