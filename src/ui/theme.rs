use egui::{Color32, CornerRadius, Context, Stroke, Visuals};

pub const PINK: Color32 = Color32::from_rgb(0xe8, 0x43, 0x93);
pub const CYAN: Color32 = Color32::from_rgb(0x00, 0xce, 0xc9);
pub const ERROR: Color32 = Color32::from_rgb(239, 68, 68);
pub const WARNING: Color32 = Color32::from_rgb(234, 179, 8);
pub const SUCCESS: Color32 = Color32::from_rgb(0x00, 0xce, 0xc9);
pub const GREEN: Color32 = Color32::from_rgb(34, 197, 94);
pub const PINK_DIM: Color32 = Color32::from_rgba_premultiplied(0x2a, 0x16, 0x24, 220);

pub const BG: Color32 = Color32::from_rgb(0x0f, 0x17, 0x2a);
pub const PANEL: Color32 = Color32::from_rgb(0x17, 0x1f, 0x2e);
pub const CARD: Color32 = Color32::from_rgba_premultiplied(0x1c, 0x25, 0x36, 230);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x22, 0x2c, 0x40);
pub const BORDER: Color32 = Color32::from_rgba_premultiplied(25, 25, 25, 25);
pub const TEXT: Color32 = Color32::from_rgb(0xf1, 0xf5, 0xf9);
pub const MUTED: Color32 = Color32::from_rgb(0x94, 0xa3, 0xb8);

pub fn apply(ctx: &Context) {
    let mut v = Visuals::dark();

    v.hyperlink_color = CYAN;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(0xe8, 0x43, 0x93, 55);
    v.selection.stroke = Stroke::new(1.0_f32, PINK);

    v.widgets.active.bg_fill = Color32::from_rgba_unmultiplied(0x80, 0x20, 0x50, 200);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, PINK);
    v.widgets.active.fg_stroke = Stroke::new(2.0_f32, PINK);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, CYAN);

    v.panel_fill = PANEL;
    v.window_fill = BG;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);

    ctx.set_visuals(v);
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
}
