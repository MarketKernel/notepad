//! Fonts and text look.
//!
//! egui's built-in UI font (Ubuntu Light) is thin and its default text colors
//! are gray, which is hard to read. The system UI / monospace fonts are used
//! instead where available, text is a bit larger and closer to black (white in
//! dark mode). A system font with wide script coverage is added as a fallback
//! so that CJK, Arabic, Hebrew, Thai, … text is displayed.

use eframe::egui::epaint::{FontColorTransferFunction, text::VariationCoords};
use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, TextStyle, Theme};

#[cfg(target_os = "macos")]
const UI: &[&str] = &["/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/Supplemental/Arial.ttf"];
#[cfg(target_os = "macos")]
const MONO: &[&str] = &["/System/Library/Fonts/SFNSMono.ttf"];
#[cfg(target_os = "macos")]
const FALLBACK: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/Library/Fonts/Arial Unicode.ttf",
];

#[cfg(target_os = "windows")]
const UI: &[&str] = &["C:\\Windows\\Fonts\\segoeui.ttf"];
#[cfg(target_os = "windows")]
const MONO: &[&str] = &["C:\\Windows\\Fonts\\consola.ttf"];
#[cfg(target_os = "windows")]
const FALLBACK: &[&str] = &[
    "C:\\Windows\\Fonts\\seguisym.ttf",
    "C:\\Windows\\Fonts\\arialuni.ttf",
    "C:\\Windows\\Fonts\\msyh.ttc",
];

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const UI: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO: &[&str] = &[];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const FALLBACK: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
];

/// Loads the first readable file of `paths` as `name`. Variable fonts (SF) are
/// pinned to the regular weight.
fn load(fonts: &mut FontDefinitions, name: &str, paths: &[&str]) -> bool {
    let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) else {
        return false;
    };
    let mut data = FontData::from_owned(bytes);
    data.tweak.coords = VariationCoords::new([("wght", 400.0), ("opsz", 14.0)]);
    fonts.font_data.insert(name.to_owned(), data.into());
    true
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for (name, paths, family) in [("system-ui", UI, FontFamily::Proportional), ("system-mono", MONO, FontFamily::Monospace)] {
        if load(&mut fonts, name, paths) {
            fonts.families.entry(family).or_default().insert(0, name.to_owned());
        }
    }
    for (i, path) in FALLBACK.iter().enumerate() {
        if let Ok(bytes) = std::fs::read(path) {
            let name = format!("system-fallback-{i}");
            fonts.font_data.insert(name.clone(), FontData::from_owned(bytes).into());
            for family in [FontFamily::Monospace, FontFamily::Proportional] {
                fonts.families.entry(family).or_default().push(name.clone());
            }
        }
    }
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        use FontFamily::{Monospace, Proportional};
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.0, Proportional)),
            (TextStyle::Body, FontId::new(14.0, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Heading, FontId::new(20.0, Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, Monospace)),
        ]
        .into();
        style.spacing.interact_size.y = 22.0;
        // Slightly bolder glyph edges, in light mode too.
        style.visuals.text_options.color_transfer_function = FontColorTransferFunction::TwoCoverageMinusCoverageSq;
        style.visuals.weak_text_alpha = 0.7;
    });
    ctx.style_mut_of(Theme::Light, |style| {
        let w = &mut style.visuals.widgets;
        w.noninteractive.fg_stroke.color = Color32::from_gray(20);
        w.inactive.fg_stroke.color = Color32::from_gray(10);
    });
    ctx.style_mut_of(Theme::Dark, |style| {
        let w = &mut style.visuals.widgets;
        w.noninteractive.fg_stroke.color = Color32::from_gray(215);
        w.inactive.fg_stroke.color = Color32::from_gray(230);
    });
}
