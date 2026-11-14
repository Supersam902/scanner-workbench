use egui::{Color32, FontData, FontFamily, FontId, RichText, Stroke, TextStyle, Vec2};
use std::sync::Arc;

pub(super) const CANVAS: Color32 = Color32::from_rgb(27, 25, 23);
pub(super) const CHROME: Color32 = Color32::from_rgb(20, 18, 17);
pub(super) const SURFACE: Color32 = Color32::from_rgb(37, 33, 30);
pub(super) const RAISED: Color32 = Color32::from_rgb(47, 41, 36);
pub(super) const BORDER: Color32 = Color32::from_rgb(71, 61, 53);
pub(super) const SELECTED: Color32 = Color32::from_rgb(74, 43, 29);
pub(super) const INK: Color32 = Color32::from_rgb(248, 240, 228);
pub(super) const MUTED: Color32 = Color32::from_rgb(185, 171, 158);
pub(super) const ACCENT: Color32 = Color32::from_rgb(248, 145, 88);
pub(super) const AMBER: Color32 = Color32::from_rgb(247, 200, 110);
pub(super) const RED: Color32 = Color32::from_rgb(255, 137, 127);
pub(super) const GREEN: Color32 = Color32::from_rgb(156, 205, 164);

const ICON: &[u8] = include_bytes!("../../assets/scanner/workbench-icon.png");
const SPLASH: &[u8] = include_bytes!("../../assets/scanner/workbench-splash.png");

pub fn icon_data() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(ICON).ok()
}

fn texture(ctx: &egui::Context, id: &'static str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let key = egui::Id::new(id);
    if let Some(texture) = ctx.data(|data| data.get_temp::<egui::TextureHandle>(key)) {
        return Some(texture);
    }
    let decoded = eframe::icon_data::from_png_bytes(bytes).ok()?;
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [decoded.width as usize, decoded.height as usize],
        &decoded.rgba,
    );
    let texture = ctx.load_texture(id, image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(key, texture.clone()));
    Some(texture)
}

pub(super) fn logo(ui: &mut egui::Ui, size: f32) {
    if let Some(icon) = texture(ui.ctx(), "workbench-icon", ICON) {
        ui.add(egui::Image::new((icon.id(), Vec2::splat(size))));
    }
}

fn display_family() -> FontFamily {
    FontFamily::Name("Barlow Semi Condensed".into())
}

pub(super) fn title(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).heading().size(size)
}

pub(super) fn label(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).text_style(TextStyle::Button).size(size)
}

pub(super) fn configure(ctx: &egui::Context) {
    let key = egui::Id::new("workbench-appearance");
    if ctx.data(|data| data.get_temp::<bool>(key).unwrap_or(false)) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(key, true));
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        (
            "Barlow",
            include_bytes!("../../assets/fonts/Barlow-Regular.ttf").as_slice(),
        ),
        (
            "Barlow Semibold",
            include_bytes!("../../assets/fonts/Barlow-SemiBold.ttf").as_slice(),
        ),
        (
            "Barlow Semi Condensed",
            include_bytes!("../../assets/fonts/BarlowSemiCondensed-Bold.ttf").as_slice(),
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    let mut proportional = fonts.families[&FontFamily::Proportional].clone();
    for (family, name) in [
        (display_family(), "Barlow Semi Condensed"),
        (
            FontFamily::Name("Barlow Semibold".into()),
            "Barlow Semibold",
        ),
    ] {
        let mut fallback = proportional.clone();
        fallback.insert(0, name.into());
        fonts.families.insert(family, fallback);
    }
    proportional.insert(0, "Barlow".into());
    fonts
        .families
        .insert(FontFamily::Proportional, proportional);
    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::dark();
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        let visuals = &mut style.visuals;
        visuals.panel_fill = CANVAS;
        visuals.window_fill = SURFACE;
        visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
        visuals.extreme_bg_color = CHROME;
        visuals.faint_bg_color = Color32::from_rgb(43, 38, 34);
        visuals.override_text_color = Some(INK);
        visuals.selection.bg_fill = SELECTED;
        visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
        visuals.hyperlink_color = ACCENT;
        visuals.warn_fg_color = AMBER;
        visuals.error_fg_color = RED;
        for (widget, fill, border) in [
            (&mut visuals.widgets.inactive, SURFACE, BORDER),
            (&mut visuals.widgets.hovered, RAISED, MUTED),
            (&mut visuals.widgets.active, SELECTED, ACCENT),
            (&mut visuals.widgets.open, RAISED, BORDER),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = Stroke::new(1.0_f32, border);
            widget.fg_stroke.color = INK;
            widget.corner_radius = egui::CornerRadius::same(6);
        }
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(13.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(16.0));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Name("Barlow Semibold".into())),
        );
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::new(32.0, display_family()));
    });
}

pub(super) fn splash(ctx: &egui::Context) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(CHROME))
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            let height = (rect.width() / 2.0).min(rect.height());
            let photo =
                egui::Rect::from_center_size(rect.center(), Vec2::new(height * 2.0, height));
            if let Some(texture) = texture(ctx, "workbench-splash", SPLASH) {
                ui.painter().image(
                    texture.id(),
                    photo,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            let origin = photo.left_center() + Vec2::new(42.0, -100.0);
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                    origin,
                    Vec2::new(photo.width() * 0.36, 200.0),
                )),
                |ui| {
                    logo(ui, 58.0);
                    ui.add_space(10.0);
                    ui.label(title("Scanner Workbench", 40.0));
                    ui.label(label("OBD-II diagnostics", 18.0).color(INK));
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                        ui.label(RichText::new("Opening workbench").color(INK));
                    });
                },
            );
        });
}
