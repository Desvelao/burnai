//! Visual style taken from the design mockups (dark, flat cards, outlined badges).
use eframe::egui::{self, Color32, CornerRadius, FontId, Stroke, TextStyle};

pub const WINDOW: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x1f);
pub const TITLE_BAR: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x21, 0x22, 0x26);
pub const CARD: Color32 = Color32::from_rgb(0x25, 0x26, 0x2b);
pub const CARD_BORDER: Color32 = Color32::from_rgb(0x3a, 0x3b, 0x42);
pub const INSET: Color32 = Color32::from_rgb(0x16, 0x17, 0x1a);
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe6, 0xea);
pub const MUTED: Color32 = Color32::from_rgb(0xa0, 0xa0, 0xaa);
pub const PRIMARY: Color32 = Color32::from_rgb(0x33, 0x41, 0x5c);
pub const PRIMARY_HOVER: Color32 = Color32::from_rgb(0x41, 0x52, 0x74);
pub const SUCCESS_BTN: Color32 = Color32::from_rgb(0x2f, 0x6f, 0x4f);
pub const DANGER_BTN: Color32 = Color32::from_rgb(0xc9, 0x3c, 0x37);
pub const NEUTRAL_BTN: Color32 = Color32::from_rgb(0x3a, 0x3b, 0x42);
pub const BAR_BLUE: Color32 = Color32::from_rgb(0x5a, 0xa2, 0xff);
pub const BAR_LOW: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
pub const CLAY: Color32 = Color32::from_rgb(0xd9, 0x77, 0x57);
pub const OTHER_PROVIDER: Color32 = Color32::from_rgb(0x6b, 0x6c, 0x75);
pub const ROW_HIGHLIGHT: Color32 = Color32::from_rgb(0x2b, 0x2a, 0x22);

/// egui's default fonts plus DejaVu Sans as the last fallback, so symbols the defaults lack
/// (arrows, dots, dingbats) draw as glyphs instead of boxes.
pub fn fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "symbols".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/DejaVuSans.ttf"
        ))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("symbols".into());
    }
    fonts
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    let mut v = egui::Visuals::dark();
    v.panel_fill = WINDOW;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    v.window_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = INSET;
    v.faint_bg_color = Color32::from_rgb(0x20, 0x21, 0x25);
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = Color32::from_rgb(0x8e, 0xc0, 0xff);
    v.selection.bg_fill = PRIMARY;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    let r = CornerRadius::same(4);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    v.widgets.inactive.bg_fill = NEUTRAL_BTN;
    v.widgets.inactive.weak_bg_fill = NEUTRAL_BTN;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_fill = PRIMARY_HOVER;
    v.widgets.hovered.weak_bg_fill = PRIMARY_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    v.widgets.active.bg_fill = PRIMARY;
    v.widgets.active.weak_bg_fill = PRIMARY;
    v.widgets.open.bg_fill = PRIMARY;
    v.widgets.open.weak_bg_fill = PRIMARY;
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 28.0;
        s.spacing.window_margin = egui::Margin::same(16);
        s.text_styles
            .insert(TextStyle::Heading, FontId::proportional(22.0));
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(13.0));
        s.text_styles
            .insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.0));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_font_is_the_last_fallback_in_both_families() {
        let f = fonts();
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = &f.families[&fam];
            assert_eq!(list.last().map(String::as_str), Some("symbols"));
            assert!(list.len() > 1, "default fonts stay first");
        }
    }
}
