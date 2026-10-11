//! Syntax highlighting of the visible lines only (syntect).
//!
//! Highlighting state is started a few lines above the first visible line, so
//! a file of any size costs the same. Multi-line constructs that begin far
//! above the screen may be highlighted imprecisely — an accepted trade-off.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use eframe::egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

struct Assets {
    syntaxes: SyntaxSet,
    dark: Theme,
    light: Theme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults().themes;
        Assets {
            syntaxes: SyntaxSet::load_defaults_nonewlines(),
            dark: themes.remove("base16-ocean.dark").unwrap_or_default(),
            light: themes.remove("InspiredGitHub").unwrap_or_default(),
        }
    })
}

/// Names of all languages, sorted, for the menu.
pub fn language_names() -> Vec<String> {
    let mut names: Vec<String> = assets()
        .syntaxes
        .syntaxes()
        .iter()
        .filter(|s| !s.file_extensions.is_empty() && s.name != "Plain Text")
        .map(|s| s.name.clone())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

/// Guess the language from the file name, or from the first line (`#!...`).
pub fn detect(path: Option<&Path>, first_line: &str) -> Option<String> {
    let ss = &assets().syntaxes;
    let by_ext = path.and_then(|p| {
        let name = p.file_name()?.to_str()?;
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or(name);
        ss.find_syntax_by_extension(ext)
            .or_else(|| ss.find_syntax_by_extension(name))
    });
    let syntax = by_ext.or_else(|| ss.find_syntax_by_first_line(first_line))?;
    (syntax.name != "Plain Text").then(|| syntax.name.clone())
}

pub type Spans = Vec<(Range<usize>, Color32)>;

/// Highlights consecutive lines. Lines longer than `max_len` are skipped
/// (returned with empty spans) to keep the UI fast.
pub fn highlight(language: &str, dark: bool, lines: &[&str], max_len: usize) -> Vec<Spans> {
    let a = assets();
    let Some(syntax) = a.syntaxes.find_syntax_by_name(language) else {
        return vec![Vec::new(); lines.len()];
    };
    let theme = if dark { &a.dark } else { &a.light };
    let default_fg = theme.settings.foreground;
    let mut h = HighlightLines::new(syntax, theme);
    lines
        .iter()
        .map(|line| {
            if line.len() > max_len {
                return Vec::new();
            }
            let Ok(regions) = h.highlight_line(line, &a.syntaxes) else {
                return Vec::new();
            };
            let mut pos = 0;
            regions
                .into_iter()
                .map(|(style, text)| {
                    let c = style.foreground;
                    let range = pos..pos + text.len();
                    pos += text.len();
                    // Ordinary text uses the editor's own (high-contrast) text color.
                    let color = if Some(c) == default_fg {
                        Color32::PLACEHOLDER
                    } else {
                        Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
                    };
                    (range, color)
                })
                .collect()
        })
        .collect()
}
