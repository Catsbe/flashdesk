//! FlashDesk visual system.
//!
//! One dark theme, forced regardless of the Windows light/dark setting. Before this, egui
//! followed the OS theme: in Windows light mode the page turned light grey while the cards kept
//! their hard-coded dark fills, leaving dark text on dark cards. Everything is built from a small
//! palette, a real bold CJK face for headings, and shared building blocks (cards, buttons,
//! chips, segmented controls, toggles, banners) so every page has the same rhythm and spacing.

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontData,
    FontDefinitions, FontFamily, FontId, Frame, Layout, Margin, Response, RichText, Sense, Shadow,
    Stroke, StrokeKind, TextStyle, TextWrapMode, Theme, Ui, WidgetText,
};

/// Palette.
pub mod c {
    use eframe::egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(13, 16, 22);
    pub const SURFACE: Color32 = Color32::from_rgb(21, 25, 33);
    pub const SURFACE_2: Color32 = Color32::from_rgb(28, 33, 43);
    pub const SURFACE_3: Color32 = Color32::from_rgb(38, 45, 58);
    pub const FIELD: Color32 = Color32::from_rgb(12, 15, 20);
    pub const BORDER: Color32 = Color32::from_rgb(36, 42, 54);
    pub const BORDER_STRONG: Color32 = Color32::from_rgb(52, 60, 76);
    pub const TEXT: Color32 = Color32::from_rgb(230, 234, 241);
    pub const TEXT_2: Color32 = Color32::from_rgb(156, 166, 182);
    pub const TEXT_3: Color32 = Color32::from_rgb(108, 118, 135);
    pub const ACCENT: Color32 = Color32::from_rgb(45, 200, 170);
    pub const ACCENT_HOVER: Color32 = Color32::from_rgb(70, 218, 190);
    pub const ACCENT_PRESS: Color32 = Color32::from_rgb(34, 170, 145);
    pub const ACCENT_TEXT: Color32 = Color32::from_rgb(6, 28, 24);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(20, 56, 52);
    pub const OK: Color32 = Color32::from_rgb(72, 212, 142);
    pub const WARN: Color32 = Color32::from_rgb(242, 184, 84);
    pub const DANGER: Color32 = Color32::from_rgb(240, 104, 104);
    pub const INFO: Color32 = Color32::from_rgb(112, 170, 255);
}

pub const RADIUS: u8 = 8;
pub const CARD_RADIUS: u8 = 12;
/// Max width of the main content column (wider windows get side margins instead of
/// stretching every row edge to edge).
pub const CONTENT_MAX_WIDTH: f32 = 900.0;

pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Installs fonts + the dark theme. Call once at startup.
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style_of(Theme::Dark)).clone();
    apply_style(&mut style);
    ctx.set_style_of(Theme::Dark, style.clone());
    // Same look even if something flips the preference to light.
    ctx.set_style_of(Theme::Light, style);
}

fn load_font(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let regular = load_font(&[
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ]);
    let bold = load_font(&[
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\msyhbd.ttf",
        r"C:\Windows\Fonts\Dengb.ttf",
    ]);
    let mono = load_font(&[r"C:\Windows\Fonts\consola.ttf"]);

    let defaults = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = regular {
        fonts
            .font_data
            .insert("cjk".into(), std::sync::Arc::new(FontData::from_owned(bytes)));
        if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
            list.insert(0, "cjk".into());
        }
        if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
            list.push("cjk".into());
        }
    }
    let mut bold_list = Vec::new();
    if let Some(bytes) = bold {
        fonts
            .font_data
            .insert("cjk-bold".into(), std::sync::Arc::new(FontData::from_owned(bytes)));
        bold_list.push("cjk-bold".to_string());
    }
    if fonts.font_data.contains_key("cjk") {
        bold_list.push("cjk".into());
    }
    bold_list.extend(defaults);
    fonts.families.insert(bold_family(), bold_list);
    if let Some(bytes) = mono {
        fonts
            .font_data
            .insert("mono".into(), std::sync::Arc::new(FontData::from_owned(bytes)));
        if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
            list.insert(0, "mono".into());
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_style(style: &mut egui::Style) {
    style.text_styles = [
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(20.0, bold_family())),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = vec2(8.0, 8.0);
    s.button_padding = vec2(12.0, 5.0);
    s.interact_size = vec2(36.0, 30.0);
    s.window_margin = Margin::same(16);
    s.menu_margin = Margin::same(8);
    s.combo_width = 170.0;
    s.text_edit_width = 240.0;
    s.slider_width = 170.0;
    s.icon_width = 16.0;
    s.icon_width_inner = 9.0;
    s.icon_spacing = 6.0;
    s.scroll = egui::style::ScrollStyle::floating();

    style.interaction.selectable_labels = false;

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = c::BG;
    v.window_fill = c::SURFACE;
    v.window_stroke = Stroke::new(1.0_f32, c::BORDER_STRONG);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(130),
    };
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = c::FIELD;
    v.faint_bg_color = c::SURFACE_2;
    v.code_bg_color = c::SURFACE_2;
    v.hyperlink_color = c::INFO;
    v.warn_fg_color = c::WARN;
    v.error_fg_color = c::DANGER;
    v.selection.bg_fill = Color32::from_rgb(28, 98, 88);
    v.selection.stroke = Stroke::new(1.0_f32, c::TEXT);
    v.slider_trailing_fill = true;
    v.indent_has_left_vline = false;
    v.striped = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c::SURFACE;
    w.noninteractive.weak_bg_fill = c::SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, c::BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, c::TEXT);
    w.noninteractive.corner_radius = CornerRadius::same(RADIUS);

    w.inactive.bg_fill = c::SURFACE_3;
    w.inactive.weak_bg_fill = c::SURFACE_2;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, c::BORDER_STRONG);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, c::TEXT);
    w.inactive.corner_radius = CornerRadius::same(RADIUS);
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = Color32::from_rgb(50, 59, 75);
    w.hovered.weak_bg_fill = c::SURFACE_3;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(74, 86, 106));
    w.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.hovered.corner_radius = CornerRadius::same(RADIUS);
    w.hovered.expansion = 0.0;

    w.active.bg_fill = c::ACCENT_PRESS;
    w.active.weak_bg_fill = Color32::from_rgb(34, 70, 66);
    w.active.bg_stroke = Stroke::new(1.0_f32, c::ACCENT);
    w.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.active.corner_radius = CornerRadius::same(RADIUS);
    w.active.expansion = 0.0;

    w.open.bg_fill = c::SURFACE_3;
    w.open.weak_bg_fill = c::SURFACE_3;
    w.open.bg_stroke = Stroke::new(1.0_f32, c::ACCENT);
    w.open.fg_stroke = Stroke::new(1.0_f32, c::TEXT);
    w.open.corner_radius = CornerRadius::same(RADIUS);
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

pub fn bold(text: impl Into<String>) -> RichText {
    RichText::new(text).family(bold_family()).color(c::TEXT)
}

/// Card / section title.
pub fn title(text: impl Into<String>) -> RichText {
    bold(text).size(16.5)
}

/// Large page heading.
pub fn h1(text: impl Into<String>) -> RichText {
    bold(text).size(22.0)
}

/// Secondary explanatory text.
pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(c::TEXT_2).size(13.0)
}

/// Tertiary text (field labels, captions).
pub fn faint(text: impl Into<String>) -> RichText {
    RichText::new(text).color(c::TEXT_3).size(12.5)
}

pub fn mono(text: impl Into<String>) -> RichText {
    RichText::new(text).monospace().color(c::TEXT_2)
}

// ---------------------------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------------------------

/// A content card filling the available width.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(c::SURFACE)
        .stroke(Stroke::new(1.0_f32, c::BORDER))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A recessed row inside a card (device rows, list items).
pub fn inset<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(c::SURFACE_2)
        .stroke(Stroke::new(1.0_f32, c::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Card header: bold title + optional subtitle on the left, widgets on the right.
pub fn card_header(
    ui: &mut Ui,
    heading: &str,
    subtitle: Option<&str>,
    right: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(title(heading));
            if let Some(sub) = subtitle {
                ui.label(muted(sub));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(6.0);
}

/// Thin horizontal divider with breathing room.
pub fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, c::BORDER);
    ui.add_space(4.0);
}

// ---------------------------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    /// Borderless text/icon button.
    Ghost,
}

/// Painted button with proper hover/pressed/disabled states for every kind.
pub fn button(ui: &mut Ui, text: impl Into<String>, kind: Kind) -> Response {
    button_sized(ui, text, kind, 32.0)
}

pub fn small_button(ui: &mut Ui, text: impl Into<String>, kind: Kind) -> Response {
    button_sized(ui, text, kind, 26.0)
}

fn button_sized(ui: &mut Ui, text: impl Into<String>, kind: Kind, height: f32) -> Response {
    let enabled = ui.is_enabled();
    let rich = match kind {
        Kind::Primary => RichText::new(text).family(bold_family()),
        _ => RichText::new(text),
    };
    let size_pt = if height < 30.0 { 13.0 } else { 14.0 };
    let galley = WidgetText::from(rich.size(size_pt)).into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Button,
    );
    let pad_x = if kind == Kind::Ghost { 8.0 } else { 14.0 };
    let size = vec2(galley.size().x + pad_x * 2.0, height.max(galley.size().y + 6.0));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let pressed = enabled && resp.is_pointer_button_down_on();
        let (fill, stroke, text_color) = match kind {
            Kind::Primary => (
                if pressed {
                    c::ACCENT_PRESS
                } else if hovered {
                    c::ACCENT_HOVER
                } else {
                    c::ACCENT
                },
                Stroke::NONE,
                c::ACCENT_TEXT,
            ),
            Kind::Secondary => (
                if pressed {
                    Color32::from_rgb(46, 54, 69)
                } else if hovered {
                    c::SURFACE_3
                } else {
                    c::SURFACE_2
                },
                Stroke::new(
                    1.0_f32,
                    if hovered {
                        Color32::from_rgb(74, 86, 106)
                    } else {
                        c::BORDER_STRONG
                    },
                ),
                c::TEXT,
            ),
            Kind::Danger => (
                if pressed {
                    Color32::from_rgb(96, 36, 40)
                } else if hovered {
                    Color32::from_rgb(78, 32, 36)
                } else {
                    Color32::from_rgb(52, 26, 30)
                },
                Stroke::new(1.0_f32, c::DANGER.gamma_multiply(0.55)),
                Color32::from_rgb(255, 150, 150),
            ),
            Kind::Ghost => (
                if pressed {
                    c::SURFACE_3
                } else if hovered {
                    c::SURFACE_2
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::NONE,
                if hovered { c::TEXT } else { c::TEXT_2 },
            ),
        };
        let (fill, text_color) = if enabled {
            (fill, text_color)
        } else {
            (fill.gamma_multiply(0.45), text_color.gamma_multiply(0.45))
        };
        ui.painter()
            .rect(rect, CornerRadius::same(RADIUS), fill, stroke, StrokeKind::Inside);
        let text_pos = rect.center() - galley.size() / 2.0;
        ui.painter().galley(text_pos, galley, text_color);
    }
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Top navigation tab: text with an accent underline when selected.
pub fn nav_tab(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let galley = WidgetText::from(if selected {
        bold(text).size(14.5)
    } else {
        RichText::new(text).size(14.5)
    })
    .into_galley(ui, Some(TextWrapMode::Extend), f32::INFINITY, TextStyle::Button);
    let size = vec2(galley.size().x + 24.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if hovered && !selected {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS), c::SURFACE_2);
        }
        let color = if selected || hovered { c::TEXT } else { c::TEXT_2 };
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
        if selected {
            let bar = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 12.0, rect.bottom() - 3.0),
                vec2(rect.width() - 24.0, 3.0),
            );
            ui.painter().rect_filled(bar, CornerRadius::same(2), c::ACCENT);
        }
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Segmented control; returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    Frame::new()
        .fill(c::FIELD)
        .stroke(Stroke::new(1.0_f32, c::BORDER_STRONG))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (opt, label) in options {
                    let selected = *value == *opt;
                    let galley = WidgetText::from(if selected {
                        bold(*label).size(13.0)
                    } else {
                        RichText::new(*label).size(13.0)
                    })
                    .into_galley(ui, Some(TextWrapMode::Extend), f32::INFINITY, TextStyle::Button);
                    let size = vec2((galley.size().x + 22.0).max(44.0), 24.0);
                    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                    let hovered = resp.hovered();
                    let fill = if selected {
                        c::ACCENT_SOFT
                    } else if hovered {
                        c::SURFACE_2
                    } else {
                        Color32::TRANSPARENT
                    };
                    let stroke = if selected {
                        Stroke::new(1.0_f32, c::ACCENT.gamma_multiply(0.6))
                    } else {
                        Stroke::NONE
                    };
                    ui.painter()
                        .rect(rect, CornerRadius::same(6), fill, stroke, StrokeKind::Inside);
                    let color = if selected {
                        c::ACCENT_HOVER
                    } else if hovered {
                        c::TEXT
                    } else {
                        c::TEXT_2
                    };
                    ui.painter()
                        .galley(rect.center() - galley.size() / 2.0, galley, color);
                    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() && !selected {
                        *value = *opt;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// iOS-style toggle switch; returns the response (clicked toggles `on`).
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = vec2(38.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    if ui.is_rect_visible(rect) {
        let track = Color32::from_rgb(
            lerp(58, 45, t),
            lerp(66, 200, t),
            lerp(82, 170, t),
        );
        ui.painter()
            .rect_filled(rect, CornerRadius::same(11), track);
        let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), 8.0, Color32::WHITE);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
}

// ---------------------------------------------------------------------------------------------
// Small display widgets
// ---------------------------------------------------------------------------------------------

/// Rounded pill label.
pub fn chip(ui: &mut Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> Response {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.5).color(fg));
        })
        .response
}

/// Neutral chip (grey on dark).
pub fn tag(ui: &mut Ui, text: impl Into<String>) -> Response {
    chip(ui, text, c::TEXT_2, c::SURFACE_3)
}

pub fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
    ui.painter()
        .circle_filled(rect.center(), 4.5 + 2.5, color.gamma_multiply(0.18));
}

#[derive(Clone, Copy)]
pub enum Tone {
    Info,
    Ok,
    Warn,
    Danger,
}

fn tone_colors(tone: Tone) -> (Color32, Color32) {
    match tone {
        Tone::Info => (c::INFO, Color32::from_rgb(20, 32, 52)),
        Tone::Ok => (c::OK, Color32::from_rgb(18, 44, 36)),
        Tone::Warn => (c::WARN, Color32::from_rgb(52, 40, 18)),
        Tone::Danger => (c::DANGER, Color32::from_rgb(56, 24, 28)),
    }
}

/// Read-only monospace box (keys, tokens) with a copy button. Returns true when copied.
pub fn code_box(ui: &mut Ui, text: &str) -> bool {
    let mut copied = false;
    Frame::new()
        .fill(c::FIELD)
        .stroke(Stroke::new(1.0_f32, c::BORDER))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if small_button(ui, "复制", Kind::Secondary).clicked() {
                    ui.ctx().copy_text(text.to_string());
                    copied = true;
                }
                ui.with_layout(Layout::left_to_right(Align::Center).with_main_wrap(true), |ui| {
                    ui.add(egui::Label::new(mono(text).color(c::TEXT)).wrap());
                });
            });
        });
    copied
}

/// Single-line text input with comfortable padding. Allocates exactly `width` × 32 so it keeps
/// its size inside `Grid`s too (a plain `desired_width` collapses to the column's last width).
pub fn text_input(ui: &mut Ui, value: &mut String, width: f32, hint: &str) -> Response {
    ui.add_sized(
        vec2(width, 32.0),
        egui::TextEdit::singleline(value)
            .margin(Margin::symmetric(8, 6))
            .hint_text(RichText::new(hint).color(c::TEXT_3)),
    )
}

/// True when a text field finished editing (focus lost or Enter pressed).
pub fn committed(resp: &Response) -> bool {
    resp.lost_focus()
}

/// Width of a label column in settings-style forms.
pub const FORM_LABEL_WIDTH: f32 = 120.0;

/// Settings-style row: fixed-width label on the left, controls on the right.
pub fn form_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    form_row_w(ui, label, FORM_LABEL_WIDTH, add);
}

pub fn form_row_w(ui: &mut Ui, label: &str, label_width: f32, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(
            vec2(label_width, ui.spacing().interact_size.y),
            Sense::hover(),
        );
        add(ui);
        // Painted after the controls so the label can be centred on the finished row (taller
        // controls such as segmented buttons grow it); multi-line rows keep it on the first line.
        let row = ui.min_rect();
        let y = if row.height() <= 40.0 {
            row.center().y
        } else {
            rect.center().y
        };
        ui.painter().text(
            pos2(rect.left(), y),
            Align2::LEFT_CENTER,
            label,
            FontId::new(13.5, FontFamily::Proportional),
            c::TEXT_2,
        );
    });
}

/// Vertical gap between cards.
pub fn gap(ui: &mut Ui) {
    ui.add_space(10.0);
}

/// Small section heading inside a card.
pub fn subheading(text: impl Into<String>) -> RichText {
    bold(text).size(14.0).color(c::TEXT_2)
}

/// Selectable card used as a radio option (e.g. picking the relay line).
pub fn option_card(
    ui: &mut Ui,
    selected: bool,
    heading: &str,
    subtitle: &str,
    width: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 56.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let fill = if selected {
            c::ACCENT_SOFT
        } else if hovered {
            c::SURFACE_3
        } else {
            c::SURFACE_2
        };
        let stroke = if selected {
            Stroke::new(1.0_f32, c::ACCENT.gamma_multiply(0.75))
        } else {
            Stroke::new(1.0_f32, c::BORDER)
        };
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);

        let center = pos2(rect.left() + 20.0, rect.center().y);
        painter.circle_stroke(
            center,
            7.0,
            Stroke::new(1.5_f32, if selected { c::ACCENT } else { c::TEXT_3 }),
        );
        if selected {
            painter.circle_filled(center, 3.5, c::ACCENT);
        }

        let left = rect.left() + 38.0;
        let max_w = (rect.right() - 12.0 - left).max(10.0);
        let t = WidgetText::from(bold(heading).size(14.0)).into_galley(
            ui,
            Some(TextWrapMode::Truncate),
            max_w,
            TextStyle::Body,
        );
        let s = WidgetText::from(RichText::new(subtitle).size(12.5).color(c::TEXT_3))
            .into_galley(ui, Some(TextWrapMode::Truncate), max_w, TextStyle::Small);
        let (th, sh) = (t.size().y, s.size().y);
        let top = rect.center().y - (th + 2.0 + sh) / 2.0;
        let painter = ui.painter();
        painter.galley(pos2(left, top), t, c::TEXT);
        painter.galley(pos2(left, top + th + 2.0), s, c::TEXT_3);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Rounded square with a glyph (device icon), optionally with an "online" dot.
pub fn avatar(ui: &mut Ui, glyph: &str, online: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(11), c::SURFACE_3);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::new(20.0, FontFamily::Proportional),
        c::TEXT_2,
    );
    if online {
        let p = pos2(rect.right() - 4.0, rect.bottom() - 4.0);
        painter.circle_filled(p, 6.0, c::SURFACE_2);
        painter.circle_filled(p, 4.0, c::OK);
    }
}

/// Centered placeholder for empty lists.
pub fn empty_state(ui: &mut Ui, glyph: &str, heading: &str, body: &str) {
    Frame::new()
        .fill(c::SURFACE_2)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(16, 18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.label(RichText::new(glyph).size(24.0).color(c::TEXT_3));
                ui.label(bold(heading).size(14.5).color(c::TEXT_2));
                ui.label(muted(body));
            });
        });
}

/// Compact toolbar button with an "on" state and an optional dropdown chevron.
pub fn pill(ui: &mut Ui, text: &str, on: bool, chevron: bool) -> Response {
    let galley = WidgetText::from(RichText::new(text).size(13.0)).into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Button,
    );
    let chevron_w = if chevron { 14.0 } else { 0.0 };
    let size = vec2(galley.size().x + 20.0 + chevron_w, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let fill = if on {
            c::ACCENT_SOFT
        } else if hovered {
            c::SURFACE_3
        } else {
            Color32::TRANSPARENT
        };
        let stroke = if on {
            Stroke::new(1.0_f32, c::ACCENT.gamma_multiply(0.5))
        } else {
            Stroke::NONE
        };
        let color = if on {
            c::ACCENT_HOVER
        } else if hovered {
            c::TEXT
        } else {
            c::TEXT_2
        };
        ui.painter()
            .rect(rect, CornerRadius::same(RADIUS), fill, stroke, StrokeKind::Inside);
        let text_pos = pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(text_pos, galley, color);
        if chevron {
            paint_chevron(
                ui.painter(),
                pos2(rect.right() - 13.0, rect.center().y),
                false,
                color,
            );
        }
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Square ghost button showing an up/down chevron.
pub fn chevron_button(ui: &mut Ui, up: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if hovered {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS), c::SURFACE_3);
        }
        paint_chevron(
            ui.painter(),
            rect.center(),
            up,
            if hovered { c::TEXT } else { c::TEXT_2 },
        );
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Small ghost "×" button, painted (the ✕ glyph is missing from the CJK UI font).
pub fn close_button(ui: &mut Ui) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if hovered {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(6), c::SURFACE_3);
        }
        let stroke = Stroke::new(1.5_f32, if hovered { c::TEXT } else { c::TEXT_2 });
        let (ctr, r) = (rect.center(), 4.0);
        ui.painter()
            .line_segment([ctr + vec2(-r, -r), ctr + vec2(r, r)], stroke);
        ui.painter()
            .line_segment([ctr + vec2(-r, r), ctr + vec2(r, -r)], stroke);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn paint_chevron(painter: &egui::Painter, center: egui::Pos2, up: bool, color: Color32) {
    let (dx, dy) = (4.0, 2.2);
    let s = if up { -1.0 } else { 1.0 };
    painter.line(
        vec![
            pos2(center.x - dx, center.y - dy * s),
            pos2(center.x, center.y + dy * s),
            pos2(center.x + dx, center.y - dy * s),
        ],
        Stroke::new(1.6_f32, color),
    );
}

// ---------------------------------------------------------------------------------------------
// Toast notifications
// ---------------------------------------------------------------------------------------------

/// Short-lived notification shown floating at the bottom of the window, so feedback is visible
/// no matter how far down a long page the button was.
#[derive(Clone)]
pub struct Toast {
    tone: Tone,
    text: String,
    expires: f64,
}

impl Toast {
    pub fn new(ctx: &egui::Context, tone: Tone, text: impl Into<String>) -> Self {
        let secs = match tone {
            Tone::Warn | Tone::Danger => 8.0,
            _ => 4.0,
        };
        Self {
            tone,
            text: text.into(),
            expires: ctx.input(|i| i.time) + secs,
        }
    }
}

/// Draws the toast (if any) and clears it once expired or dismissed.
pub fn toast_area(ctx: &egui::Context, slot: &mut Option<Toast>) {
    let Some(toast) = slot.as_ref() else {
        return;
    };
    let now = ctx.input(|i| i.time);
    if now >= toast.expires {
        *slot = None;
        return;
    }
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(toast.expires - now));
    let (fg, bg) = tone_colors(toast.tone);
    let mut close = false;
    egui::Area::new(egui::Id::new("flashdesk_toast"))
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -26.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            Frame::new()
                .fill(bg)
                .stroke(Stroke::new(1.0_f32, fg.gamma_multiply(0.55)))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(14, 8))
                .shadow(Shadow {
                    offset: [0, 6],
                    blur: 20,
                    spread: 0,
                    color: Color32::from_black_alpha(150),
                })
                .show(ui, |ui| {
                    ui.set_max_width(620.0);
                    ui.horizontal(|ui| {
                        status_dot(ui, fg);
                        ui.add(egui::Label::new(RichText::new(&toast.text).color(c::TEXT)).wrap());
                        if close_button(ui).clicked() {
                            close = true;
                        }
                    });
                });
        });
    if close {
        *slot = None;
    }
}
