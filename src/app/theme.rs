//! macOS-flavoured egui look shared with paper-rs: Inter font, system accent,
//! light/dark palettes that follow the system appearance.

use egui::{
    Color32, CornerRadius, FontData, FontFamily, FontId, Shadow, Stroke,
    vec2,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub static INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
pub static INTER_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Inter-Medium.ttf");
pub static INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// macOS system blue, used when the system accent can't be read.
pub const MAC_BLUE: Color32 = Color32::from_rgb(0x00, 0x7a, 0xff);

/// Body text (13pt) and secondary text (11pt), as in AppKit.
pub const BODY: f32 = 13.0;
pub const SECONDARY: f32 = 11.0;
/// Height of the unified toolbar.
pub const TOOLBAR_H: f32 = 40.0;
/// Traffic lights: left inset and the height of the bar they are centred in (points).
pub const TRAFFIC_LIGHTS: (f64, f64) = (14.0, TOOLBAR_H as f64);
/// Horizontal room the traffic lights need from the window's left edge.
pub const TRAFFIC_LIGHTS_W: f32 = 76.0;

static ACCENT: AtomicU32 = AtomicU32::new(u32::from_be_bytes([0x00, 0x7a, 0xff, 0xff]));

pub fn accent() -> Color32 {
    let [r, g, b, _] = ACCENT.load(Ordering::Relaxed).to_be_bytes();
    Color32::from_rgb(r, g, b)
}

pub fn set_accent(c: Color32) {
    ACCENT.store(
        u32::from_be_bytes([c.r(), c.g(), c.b(), 0xff]),
        Ordering::Relaxed,
    );
}

/// The user's accent colour from System Settings › Appearance (falls back to system blue).
pub fn system_accent() -> Color32 {
    #[cfg(target_os = "macos")]
    {
        // `AppleAccentColor` is absent for the default (blue) and multicolour settings.
        let out = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleAccentColor"])
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            return match String::from_utf8_lossy(&o.stdout).trim() {
                "-1" => Color32::from_rgb(0x8c, 0x8c, 0x8c),
                "0" => Color32::from_rgb(0xff, 0x3b, 0x30),
                "1" => Color32::from_rgb(0xff, 0x95, 0x00),
                "2" => Color32::from_rgb(0xff, 0xcc, 0x00),
                "3" => Color32::from_rgb(0x28, 0xcd, 0x41),
                "5" => Color32::from_rgb(0xaf, 0x52, 0xde),
                "6" => Color32::from_rgb(0xff, 0x2d, 0x55),
                _ => MAC_BLUE,
            };
        }
    }
    MAC_BLUE
}

/// Colours for one appearance, loosely after AppKit's semantic colours.
#[derive(Clone, Copy)]
pub struct Palette {
    pub window: Color32,
    pub sidebar: Color32,
    pub inspector: Color32,
    pub control: Color32,
    pub control_border: Color32,
    pub field: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub tertiary: Color32,
    pub separator: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub popover: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            window: Color32::from_rgb(0x2b, 0x2b, 0x2d),
            sidebar: Color32::from_rgb(0x26, 0x26, 0x28),
            inspector: Color32::from_rgb(0x25, 0x25, 0x27),
            control: Color32::from_rgb(0x3a, 0x3a, 0x3c),
            control_border: Color32::from_white_alpha(22),
            field: Color32::from_rgb(0x1f, 0x1f, 0x21),
            text: Color32::from_rgb(0xec, 0xec, 0xee),
            secondary: Color32::from_rgb(0x98, 0x98, 0x9d),
            tertiary: Color32::from_rgb(0x63, 0x63, 0x66),
            separator: Color32::from_rgb(0x3a, 0x3a, 0x3c),
            hover: Color32::from_white_alpha(16),
            pressed: Color32::from_white_alpha(30),
            popover: Color32::from_rgb(0x32, 0x32, 0x34),
        }
    } else {
        Palette {
            window: Color32::from_rgb(0xf6, 0xf6, 0xf6),
            sidebar: Color32::from_rgb(0xee, 0xee, 0xf0),
            inspector: Color32::from_rgb(0xf6, 0xf6, 0xf6),
            control: Color32::WHITE,
            control_border: Color32::from_black_alpha(30),
            field: Color32::WHITE,
            text: Color32::from_rgb(0x1d, 0x1d, 0x1f),
            secondary: Color32::from_rgb(0x86, 0x86, 0x8b),
            tertiary: Color32::from_rgb(0xb8, 0xb8, 0xbc),
            separator: Color32::from_rgb(0xdc, 0xdc, 0xde),
            hover: Color32::from_black_alpha(12),
            pressed: Color32::from_black_alpha(24),
            popover: Color32::from_rgb(0xfc, 0xfc, 0xfc),
        }
    }
}

pub fn pal(ui: &egui::Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

/// Inter (Regular/Medium/SemiBold) as the proportional family, egui's fonts as fallbacks.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        ("Inter", INTER_REGULAR),
        ("Inter-Medium", INTER_MEDIUM),
        ("Inter-SemiBold", INTER_SEMIBOLD),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    let fallbacks: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let with = |first: &str| {
        let mut v = vec![first.to_string()];
        v.extend(fallbacks.iter().cloned());
        v
    };
    fonts
        .families
        .insert(FontFamily::Proportional, with("Inter"));
    fonts
        .families
        .insert(FontFamily::Name("medium".into()), with("Inter-Medium"));
    fonts
        .families
        .insert(FontFamily::Name("semibold".into()), with("Inter-SemiBold"));
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.push("Inter".into());
    }
    ctx.set_fonts(fonts);
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn style_for(s: &mut egui::Style, dark: bool) {
    use egui::TextStyle as T;
    let p = palette(dark);
    let acc = accent();
    s.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    s.text_styles = [
        (T::Small, FontId::proportional(SECONDARY)),
        (T::Body, FontId::proportional(BODY)),
        (T::Button, FontId::proportional(BODY)),
        (T::Heading, semibold(BODY)),
        (T::Monospace, FontId::monospace(12.0)),
    ]
    .into();
    s.spacing.item_spacing = vec2(6.0, 4.0);
    s.spacing.button_padding = vec2(8.0, 3.0);
    s.spacing.interact_size = vec2(24.0, 22.0);
    s.spacing.menu_margin = egui::Margin::same(5);
    s.spacing.window_margin = egui::Margin::same(12);
    s.spacing.scroll = egui::style::ScrollStyle {
        bar_width: 7.0,
        handle_min_length: 24.0,
        ..egui::style::ScrollStyle::floating()
    };
    s.interaction.selectable_labels = false;

    let v = &mut s.visuals;
    v.override_text_color = None;
    v.weak_text_color = Some(p.secondary);
    v.hyperlink_color = acc;
    v.panel_fill = p.inspector;
    v.window_fill = p.popover;
    v.faint_bg_color = if dark {
        Color32::from_white_alpha(6)
    } else {
        Color32::from_black_alpha(6)
    };
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.window_stroke = Stroke::new(1.0, p.control_border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 110 } else { 40 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 90 } else { 30 }),
    };
    v.selection.bg_fill = acc;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.indent_has_left_vline = false;
    v.striped = false;
    v.interact_cursor = None;
    v.collapsing_header_frame = false;

    let r6 = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.inspector;
    w.noninteractive.weak_bg_fill = p.inspector;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r6;
    w.inactive.bg_fill = p.control;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = Stroke::new(1.0, p.control_border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r6;
    w.inactive.expansion = 0.0;
    w.hovered.bg_fill = p.control;
    w.hovered.weak_bg_fill = blend(p.control, p.hover);
    w.hovered.bg_stroke = Stroke::new(1.0, p.control_border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r6;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = blend(p.control, p.pressed);
    w.active.weak_bg_fill = blend(p.control, p.pressed);
    w.active.bg_stroke = Stroke::new(1.0, p.control_border);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = r6;
    w.active.expansion = 0.0;
    w.open = w.active;
}

/// `over` composited on `base`.
pub fn blend(base: Color32, over: Color32) -> Color32 {
    let a = over.a() as f32 / 255.0;
    let [or, og, ob, _] = over.to_srgba_unmultiplied();
    let mix = |b: u8, o: u8| (b as f32 * (1.0 - a) + o as f32 * a).round() as u8;
    Color32::from_rgb(mix(base.r(), or), mix(base.g(), og), mix(base.b(), ob))
}

/// Fonts plus light and dark styles. egui picks the one matching the system appearance
/// (reported by egui-winit); headless rendering falls back to light.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.options_mut(|o| {
        o.theme_preference = egui::ThemePreference::System;
        o.fallback_theme = egui::Theme::Light;
    });
    ctx.style_mut_of(egui::Theme::Light, |s| style_for(s, false));
    ctx.style_mut_of(egui::Theme::Dark, |s| style_for(s, true));
}

