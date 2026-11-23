use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke};

pub fn setup_fonts_and_style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\Deng.ttf",
    ];

    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".into(), std::sync::Arc::new(FontData::from_owned(bytes)));
            if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
                list.insert(0, "cjk".into());
            }
            if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
                list.push("cjk".into());
            }
            break;
        }
    }

    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.window_fill = Color32::from_rgb(16, 19, 25);
    style.visuals.panel_fill = Color32::from_rgb(16, 19, 25);
    style.visuals.extreme_bg_color = Color32::from_rgb(11, 13, 18);
    style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(24, 29, 38);
    style.visuals.widgets.noninteractive.bg_stroke =
        Stroke::new(1.0_f32, Color32::from_rgb(44, 52, 66));
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(33, 40, 52);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 56, 74);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(18, 128, 120);
    style.visuals.selection.bg_fill = Color32::from_rgb(20, 150, 138);
    style.visuals.window_corner_radius = CornerRadius::same(8);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(6);
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    ctx.set_style(style);
}
