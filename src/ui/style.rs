//! Fonts, colours and spacing.

use eframe::egui::{self, Color32, FontData, FontFamily, FontId, TextStyle, Theme};
use std::path::PathBuf;
use std::sync::Arc;

const BOLD_FAMILY: &str = "bold";

/// The font family for headings and names.
pub fn bold() -> FontFamily {
    FontFamily::Name(BOLD_FAMILY.into())
}

/// Colours that differ between the light and the dark theme.
pub struct Palette {
    pub background: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub good: Color32,
    pub good_bg: Color32,
    pub warn: Color32,
    pub bad: Color32,
}

impl Palette {
    pub fn of(dark_mode: bool) -> Self {
        if dark_mode {
            Self {
                background: Color32::from_rgb(0x1c, 0x1c, 0x1e),
                card: Color32::from_rgb(0x2a, 0x2a, 0x2d),
                card_stroke: Color32::from_rgb(0x3b, 0x3b, 0x40),
                text: Color32::from_rgb(0xec, 0xec, 0xee),
                muted: Color32::from_rgb(0x9c, 0x9c, 0xa3),
                accent: Color32::from_rgb(0x4f, 0x66, 0xe8),
                on_accent: Color32::WHITE,
                good: Color32::from_rgb(0x6f, 0xd0, 0x8c),
                good_bg: Color32::from_rgb(0x1f, 0x3a, 0x29),
                warn: Color32::from_rgb(0xe8, 0xb3, 0x4a),
                bad: Color32::from_rgb(0xf0, 0x7a, 0x70),
            }
        } else {
            Self {
                background: Color32::from_rgb(0xf4, 0xf4, 0xf1),
                card: Color32::WHITE,
                card_stroke: Color32::from_rgb(0xe2, 0xe2, 0xdd),
                text: Color32::from_rgb(0x1f, 0x1f, 0x22),
                muted: Color32::from_rgb(0x6a, 0x6a, 0x70),
                accent: Color32::from_rgb(0x40, 0x59, 0xd6),
                on_accent: Color32::WHITE,
                good: Color32::from_rgb(0x1c, 0x7c, 0x3f),
                good_bg: Color32::from_rgb(0xdf, 0xf3, 0xe5),
                warn: Color32::from_rgb(0x9a, 0x62, 0x00),
                bad: Color32::from_rgb(0xc0, 0x39, 0x2b),
            }
        }
    }
}

/// Install fonts and the look shared by both themes.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(20.0, bold())),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        let widgets = &mut style.visuals.widgets;
        for state in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            state.corner_radius = 8.into();
        }
        style.visuals.window_corner_radius = 12.into();
    });

    for theme in [Theme::Light, Theme::Dark] {
        let palette = Palette::of(theme == Theme::Dark);
        ctx.style_mut_of(theme, |style| {
            style.visuals.panel_fill = palette.background;
            style.visuals.window_fill = palette.card;
            // Text fields sit on cards, so they take the page colour to stand out.
            style.visuals.text_edit_bg_color = Some(palette.background);
            style.visuals.override_text_color = Some(palette.text);
            style.visuals.weak_text_color = Some(palette.muted);
            style.visuals.warn_fg_color = palette.warn;
            style.visuals.error_fg_color = palette.bad;
            style.visuals.selection.bg_fill = palette.accent.gamma_multiply(0.35);
        });
    }
}

/// Use the operating system's UI font. egui's built-in font lacks most Vietnamese letters,
/// and mixing it with a fallback font inside one word looks broken.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    if let Some(regular) = load_system_font(false) {
        fonts
            .font_data
            .insert("system-ui".to_owned(), Arc::new(regular));
        proportional.insert(0, "system-ui".to_owned());
    }
    // The bold family must exist even without a bold face: egui panics on an unknown one.
    let mut bold_family = proportional.clone();
    if let Some(bold) = load_system_font(true) {
        fonts
            .font_data
            .insert("system-ui-bold".to_owned(), Arc::new(bold));
        bold_family.insert(0, "system-ui-bold".to_owned());
    }

    fonts
        .families
        .insert(FontFamily::Proportional, proportional);
    fonts.families.insert(bold(), bold_family);
    ctx.set_fonts(fonts);
}

struct Face {
    path: PathBuf,
    /// Weight to select in a variable font; `None` for a static face.
    weight: Option<f32>,
}

fn load_system_font(bold: bool) -> Option<FontData> {
    system_faces(bold).into_iter().find_map(|face| {
        let bytes = std::fs::read(&face.path).ok()?;
        let mut data = FontData::from_owned(bytes);
        if let Some(weight) = face.weight {
            data.tweak.coords = egui::epaint::text::VariationCoords::new([(b"wght", weight)]);
        }
        Some(data)
    })
}

#[cfg(target_os = "macos")]
fn system_faces(bold: bool) -> Vec<Face> {
    let supplemental = if bold { "Arial Bold.ttf" } else { "Arial.ttf" };
    vec![
        // San Francisco is one variable font; the weight is an axis.
        Face {
            path: "/System/Library/Fonts/SFNS.ttf".into(),
            weight: Some(if bold { 650.0 } else { 400.0 }),
        },
        Face {
            path: PathBuf::from("/System/Library/Fonts/Supplemental").join(supplemental),
            weight: None,
        },
    ]
}

#[cfg(windows)]
fn system_faces(bold: bool) -> Vec<Face> {
    let fonts = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let names: &[&str] = if bold {
        &["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"]
    } else {
        &["segoeui.ttf", "arial.ttf", "tahoma.ttf"]
    };
    names
        .iter()
        .map(|name| Face {
            path: fonts.join(name),
            weight: None,
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_faces(_bold: bool) -> Vec<Face> {
    // eframe's system-font fallback covers missing glyphs here.
    Vec::new()
}
