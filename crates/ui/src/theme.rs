use egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x2E);
pub const BORDER: Color32 = Color32::from_rgb(0x45, 0x47, 0x5A);

pub fn apply_dark_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 2.0);
    style.visuals.window_fill = BG;
    style.visuals.window_stroke = egui::Stroke::new(1.0_f32, BORDER);
    style.visuals.window_shadow = egui::epaint::Shadow::NONE;
    style.visuals.window_rounding = egui::Rounding::same(8.0);
    ctx.set_style(style);
}
