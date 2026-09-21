//! Visual system: palette, fonts, text styles, and small widget helpers.
//!
//! The window is a cafeteria tray: a mint-grey melamine surface with
//! recessed compartments for each section, mustard for the few things that
//! need attention. One palette per theme so light and dark read as the same
//! design. Fonts are the desktop's Ubuntu family when present, with
//! egui's bundled fonts as fallback. Icons are Phosphor glyphs, rendered
//! inline through the font fallback chain.

use std::sync::Arc;

use egui::epaint::Shadow;
use egui::{
    Button, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response,
    RichText, Stroke, TextStyle, Theme, Ui, Vec2, Visuals,
};

pub use egui_phosphor::regular as icons;

const MEDIUM_FAMILY: &str = "ui-medium";

pub fn medium() -> FontFamily {
    FontFamily::Name(MEDIUM_FAMILY.into())
}

pub fn title_style() -> TextStyle {
    TextStyle::Name("title".into())
}

pub fn section_style() -> TextStyle {
    TextStyle::Name("section".into())
}

pub const RADIUS: u8 = 8;
pub const WINDOW_RADIUS: u8 = 12;

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// The tray surface.
    pub bg: Color32,
    /// A recessed compartment holding one section.
    pub well: Color32,
    pub well_edge: Color32,
    pub field: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub surface_active: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    /// Mustard: attention, pins, selection. Never body text.
    pub accent: Color32,
    pub open: Color32,
    pub merged: Color32,
    pub closed: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub note: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        dark: false,
        bg: rgb(0xe9ede8),
        well: rgb(0xdfe5e0),
        well_edge: rgb(0xd0d8d2),
        field: rgb(0xf5f7f5),
        surface: rgb(0xd9e0da),
        surface_hover: rgb(0xcfd7d1),
        surface_active: rgb(0xc4cdc7),
        border: rgb(0xd0d8d2),
        border_strong: rgb(0xb5bfb8),
        text: rgb(0x18211c),
        text_weak: rgb(0x5e6a63),
        accent: rgb(0xc28f12),
        open: rgb(0x2f8f4e),
        merged: rgb(0x7b4fb0),
        closed: rgb(0xb8412f),
        warning: rgb(0xa47500),
        danger: rgb(0xb8412f),
        note: rgb(0x5e6a63),
    };

    pub const DARK: Palette = Palette {
        dark: true,
        bg: rgb(0x1d2320),
        well: rgb(0x161b18),
        well_edge: rgb(0x0e1210),
        field: rgb(0x111512),
        surface: rgb(0x262d29),
        surface_hover: rgb(0x2f3732),
        surface_active: rgb(0x38413b),
        border: rgb(0x2a322d),
        border_strong: rgb(0x3f4943),
        text: rgb(0xe6ece7),
        text_weak: rgb(0x93a098),
        accent: rgb(0xe0b23a),
        open: rgb(0x4cb56a),
        merged: rgb(0xa98bd6),
        closed: rgb(0xe0654f),
        warning: rgb(0xe0b23a),
        danger: rgb(0xe0654f),
        note: rgb(0x93a098),
    };

    pub fn for_theme(theme: Theme) -> Palette {
        match theme {
            Theme::Dark => Palette::DARK,
            Theme::Light => Palette::LIGHT,
        }
    }

    pub fn current(ui: &Ui) -> Palette {
        Palette::for_theme(ui.ctx().theme())
    }
}

/// A translucent version of `c`, for tints behind icons and selected rows.
pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

fn first_readable(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let regular = first_readable(&[
        "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
        "/usr/share/fonts/truetype/ubuntu/UbuntuSans[wdth,wght].ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ]);
    let medium_bytes = first_readable(&[
        "/usr/share/fonts/truetype/ubuntu/Ubuntu-M.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ]);
    if let Some(bytes) = regular {
        fonts
            .font_data
            .insert("ui".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "ui".into());
    }
    let mut medium_family = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = medium_bytes {
        fonts
            .font_data
            .insert(MEDIUM_FAMILY.into(), Arc::new(FontData::from_owned(bytes)));
        medium_family.insert(0, MEDIUM_FAMILY.into());
    }
    fonts.families.insert(medium(), medium_family);
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    if fonts.font_data.contains_key("phosphor") {
        fonts
            .families
            .entry(medium())
            .or_default()
            .push("phosphor".into());
    }
    fonts
}

fn visuals(theme: Theme, p: &Palette) -> Visuals {
    let mut v = match theme {
        Theme::Dark => Visuals::dark(),
        Theme::Light => Visuals::light(),
    };
    let r = CornerRadius::same(6);
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.surface;
    v.window_corner_radius = CornerRadius::same(WINDOW_RADIUS);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 140 } else { 45 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 110 } else { 30 }),
    };
    v.selection.bg_fill = tint(p.accent, 70);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.error_fg_color = p.danger;
    v.warn_fg_color = p.warning;
    v.text_cursor.stroke.color = p.accent;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = p.surface;
    w.inactive.weak_bg_fill = p.surface;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = p.surface_hover;
    w.hovered.weak_bg_fill = p.surface_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.surface_active;
    w.active.weak_bg_fill = p.surface_active;
    w.active.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open = w.hovered;
    v
}

/// Install fonts, text styles, and both theme palettes on the context.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    for theme in [Theme::Light, Theme::Dark] {
        ctx.set_visuals_of(theme, visuals(theme, &Palette::for_theme(theme)));
    }
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(18.0, medium())),
            (title_style(), FontId::new(14.0, medium())),
            (section_style(), FontId::new(11.5, medium())),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.interact_size = Vec2::new(32.0, 28.0);
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.window_margin = Margin::same(20);
        style.spacing.icon_width = 16.0;
        style.interaction.selectable_labels = false;
    });
}

/// A frameless icon button that shows its frame on hover.
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    ui.add(
        Button::new(RichText::new(icon).size(17.0))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(30.0)),
    )
    .on_hover_text(tooltip)
}

/// The main action: ink on the tray, like a stamp.
pub fn primary_button(ui: &mut Ui, icon: &str, label: &str) -> Response {
    let p = Palette::current(ui);
    ui.add(
        Button::new(RichText::new(format!("{icon}  {label}")).color(p.bg))
            .fill(p.text)
            .stroke(Stroke::NONE),
    )
}

/// A filled destructive button.
pub fn danger_button(ui: &mut Ui, icon: &str, label: &str) -> Response {
    let p = Palette::current(ui);
    ui.add(
        Button::new(RichText::new(format!("{icon}  {label}")).color(Color32::WHITE))
            .fill(p.danger)
            .stroke(Stroke::NONE),
    )
}

/// A quiet count next to a section name.
pub fn soft_count(ui: &mut Ui, count: usize) {
    let p = Palette::current(ui);
    ui.label(
        RichText::new(count.to_string())
            .text_style(TextStyle::Body)
            .color(p.text_weak),
    );
}
