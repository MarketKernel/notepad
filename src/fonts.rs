//! Adds a system font as a fallback so that text in scripts not covered by
//! egui's built-in fonts (CJK, Arabic, Hebrew, Thai, …) is displayed.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

#[cfg(target_os = "macos")]
const CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/Library/Fonts/Arial Unicode.ttf",
];

#[cfg(target_os = "windows")]
const CANDIDATES: &[&str] = &[
    "C:\\Windows\\Fonts\\seguisym.ttf",
    "C:\\Windows\\Fonts\\arialuni.ttf",
    "C:\\Windows\\Fonts\\msyh.ttc",
];

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
];

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut added = false;
    for (i, path) in CANDIDATES.iter().enumerate() {
        if let Ok(bytes) = std::fs::read(path) {
            let name = format!("system-fallback-{i}");
            fonts.font_data.insert(name.clone(), FontData::from_owned(bytes).into());
            for family in [FontFamily::Monospace, FontFamily::Proportional] {
                fonts.families.entry(family).or_default().push(name.clone());
            }
            added = true;
        }
    }
    if added {
        ctx.set_fonts(fonts);
    }
}
