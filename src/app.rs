use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui::{
    self, Align, Key, KeyboardShortcut, Layout, Modifiers, RichText, ThemePreference, ViewportCommand,
};

use crate::buffer::{Document, Original};
use crate::editor::{DEFAULT_FONT_SIZE, Editor};
use crate::highlight;
use crate::settings;

const REPO_URL: &str = "https://github.com/MarketKernel/notepad";

struct Loading {
    path: PathBuf,
    rx: Receiver<std::io::Result<Original>>,
    progress: Arc<AtomicU64>,
    total: u64,
    cancel: Arc<AtomicBool>,
}

#[cfg(target_os = "macos")]
enum PanelResult {
    Saved,
    Deleted,
    Cancelled,
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Sets or clears the execute bits (for everyone who may read the file).
#[cfg(any(target_os = "macos", all(test, unix)))]
fn set_executable(path: &Path, on: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    let mode = perm.mode();
    let new = if on { mode | ((mode & 0o444) >> 2) } else { mode & !0o111 };
    if new != mode {
        perm.set_mode(new);
        std::fs::set_permissions(path, perm)?;
    }
    Ok(())
}

enum Choice {
    Save,
    Discard,
    Cancel,
}

pub struct NotepadApp {
    editor: Editor,
    path: Option<PathBuf>,
    loading: Option<Loading>,
    find_open: bool,
    find_query: String,
    replace_open: bool,
    replace_text: String,
    match_case: bool,
    find_focus: bool,
    goto_open: bool,
    goto_text: String,
    message: Option<String>,
    allow_close: bool,
    title: String,
    languages: Option<Vec<String>>,
    theme: ThemePreference,
    about_open: bool,
    about_icon: Option<egui::TextureHandle>,
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);

fn sc(m: Modifiers, k: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(m, k)
}

/// Cmd+Option+F on macOS (as in Xcode / VS Code), Ctrl+H elsewhere.
fn replace_shortcut() -> KeyboardShortcut {
    if cfg!(target_os = "macos") {
        sc(CMD.plus(Modifiers::ALT), Key::F)
    } else {
        sc(CMD, Key::H)
    }
}

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

impl NotepadApp {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        // Cmd +/- change the editor font size instead of zooming the whole UI.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let theme = settings::load_theme();
        cc.egui_ctx.set_theme(theme);
        let mut app = Self {
            editor: Editor::new(Document::new()),
            path: None,
            loading: None,
            find_open: false,
            find_query: String::new(),
            replace_open: false,
            replace_text: String::new(),
            match_case: false,
            find_focus: false,
            goto_open: false,
            goto_text: String::new(),
            message: None,
            allow_close: false,
            title: String::new(),
            languages: None,
            theme,
            about_open: false,
            about_icon: None,
        };
        if let Some(p) = path {
            app.start_loading(p);
        }
        app.editor.request_focus(&cc.egui_ctx);
        app
    }

    fn file_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("Untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    // ---- files -----------------------------------------------------------------

    fn start_loading(&mut self, path: PathBuf) {
        let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let (path, progress, cancel) = (path.clone(), progress.clone(), cancel.clone());
            std::thread::spawn(move || {
                let _ = tx.send(Original::open(&path, &progress, &cancel));
            });
        }
        self.loading = Some(Loading {
            path,
            rx,
            progress,
            total,
            cancel,
        });
    }

    fn poll_loading(&mut self, ctx: &egui::Context) {
        let Some(l) = &self.loading else { return };
        match l.rx.try_recv() {
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
            Err(TryRecvError::Disconnected) => self.loading = None,
            Ok(result) => {
                let l = self.loading.take().unwrap();
                match result {
                    Ok(orig) => {
                        let doc = Document::from_original(orig);
                        let mut ed = Editor::new(doc);
                        ed.wrap = self.editor.wrap;
                        ed.line_numbers = self.editor.line_numbers;
                        ed.font_size = self.editor.font_size;
                        let first = String::from_utf8_lossy(&ed.doc.read(0, 512)).into_owned();
                        let first = first.lines().next().unwrap_or("").to_owned();
                        ed.language = highlight::detect(Some(&l.path), &first);
                        self.editor = ed;
                        self.path = Some(l.path);
                        self.message = None;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => self.message = Some(format!("Cannot open {}: {e}", l.path.display())),
                }
                self.editor.request_focus(ctx);
            }
        }
    }

    /// Asks whether to save unsaved changes. Returns `true` if it is OK to
    /// proceed (saved or discarded).
    fn confirm_discard(&mut self) -> bool {
        if !self.editor.is_modified() {
            return true;
        }
        // A new document on macOS: one combined sheet "keep this new document?"
        // with the save panel and Delete / Cancel / Save, like Mac document apps.
        #[cfg(target_os = "macos")]
        if self.path.is_none() {
            let message = format!(
                "Do you want to keep this new document \u{201c}{}\u{201d}?\n\
                 You can choose to save your changes, or delete this document immediately. \
                 You can\u{2019}t undo this action.",
                self.file_name()
            );
            return match self.run_save_panel(Some(message)) {
                PanelResult::Saved => true,
                PanelResult::Deleted => true,
                PanelResult::Cancelled => false,
            };
        }
        match self.ask_save() {
            Choice::Save => self.save(),
            Choice::Discard => true,
            Choice::Cancel => false,
        }
    }

    fn ask_save(&self) -> Choice {
        use rfd::{MessageButtons, MessageDialog, MessageDialogResult as R, MessageLevel};
        let dialog = MessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title(format!(
                "Do you want to save the changes made to the document \u{201c}{}\u{201d}?",
                self.file_name()
            ))
            .set_description("Your changes will be lost if you don\u{2019}t save them.");
        #[cfg(windows)]
        let dialog = dialog.set_buttons(MessageButtons::YesNoCancel);
        #[cfg(not(windows))]
        let dialog = dialog.set_buttons(MessageButtons::YesNoCancelCustom(
            "Save".into(),
            "Cancel".into(),
            "Don\u{2019}t Save".into(),
        ));
        match dialog.show() {
            R::Yes | R::Ok => Choice::Save,
            R::No => Choice::Discard,
            R::Custom(s) if s == "Save" => Choice::Save,
            R::Custom(s) if s == "Don\u{2019}t Save" => Choice::Discard,
            _ => Choice::Cancel,
        }
    }

    fn save(&mut self) -> bool {
        match self.path.clone() {
            Some(p) => self.save_to(&p),
            None => self.save_as(),
        }
    }

    #[cfg(target_os = "macos")]
    fn save_as(&mut self) -> bool {
        matches!(self.run_save_panel(None), PanelResult::Saved)
    }

    #[cfg(not(target_os = "macos"))]
    fn save_as(&mut self) -> bool {
        let mut dialog = rfd::FileDialog::new().set_file_name(self.suggested_name());
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            dialog = dialog.set_directory(dir);
        }
        match dialog.save_file() {
            Some(p) => self.save_to(&p),
            None => false,
        }
    }

    /// File name offered in the save panel.
    fn suggested_name(&self) -> String {
        match &self.path {
            Some(_) => self.file_name(),
            None => format!("{}.txt", self.file_name()),
        }
    }

    /// Native macOS save panel. With `keep_message`, it is the "keep this new
    /// document?" sheet that also offers Delete.
    #[cfg(target_os = "macos")]
    fn run_save_panel(&mut self, keep_message: Option<String>) -> PanelResult {
        use crate::mac_save::{Outcome, Request, run, set_tags};
        let executable = match &self.path {
            Some(p) => is_executable(p),
            None => self.editor.doc.read(0, 2) == b"#!",
        };
        let name = self.suggested_name();
        let outcome = run(Request {
            delete_button: keep_message.is_some(),
            message: keep_message,
            file_name: &name,
            directory: self.path.as_ref().and_then(|p| p.parent()),
            executable,
        });
        match outcome {
            Outcome::Delete => PanelResult::Deleted,
            Outcome::Cancel => PanelResult::Cancelled,
            Outcome::Save { path, executable, tags } => {
                if !self.save_to(&path) {
                    return PanelResult::Cancelled;
                }
                if let Err(e) = set_executable(&path, executable) {
                    self.message = Some(format!("Saved, but cannot change permissions: {e}"));
                }
                set_tags(&path, &tags);
                PanelResult::Saved
            }
        }
    }

    fn save_to(&mut self, path: &Path) -> bool {
        match self.editor.doc.save(path) {
            Ok(()) => {
                self.editor.mark_saved();
                if self.editor.language.is_none() {
                    self.editor.language = highlight::detect(Some(path), "");
                }
                self.path = Some(path.to_owned());
                self.message = Some(format!("Saved {}", human_size(self.editor.doc.len())));
                true
            }
            Err(e) => {
                self.message = Some(format!("Cannot save {}: {e}", path.display()));
                false
            }
        }
    }

    fn open_dialog(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(p) = dialog.pick_file() {
            self.start_loading(p);
        }
    }

    fn open_path(&mut self, p: PathBuf) {
        if self.confirm_discard() {
            self.start_loading(p);
        }
    }

    fn new_document(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        let mut ed = Editor::new(Document::new());
        ed.wrap = self.editor.wrap;
        ed.line_numbers = self.editor.line_numbers;
        ed.font_size = self.editor.font_size;
        self.editor = ed;
        self.path = None;
        self.message = None;
    }

    /// A new window (a separate process), optionally opening `path` in it.
    fn new_window(&mut self, path: Option<&Path>) {
        let result = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(path).spawn());
        if let Err(e) = result {
            self.message = Some(format!("Cannot start a new window: {e}"));
        }
    }

    /// A file opened from Finder: it replaces a blank window, otherwise it
    /// gets a window of its own.
    #[cfg(target_os = "macos")]
    fn open_from_finder(&mut self, p: PathBuf) {
        if self.path.as_ref() == Some(&p) {
            return;
        }
        if self.path.is_none() && self.loading.is_none() && !self.editor.is_modified() {
            self.start_loading(p);
        } else {
            self.new_window(Some(&p));
        }
    }

    // ---- actions ---------------------------------------------------------------

    fn find(&mut self, backwards: bool) {
        if self.find_query.is_empty() {
            self.find_open = true;
            self.find_focus = true;
            return;
        }
        let q = self.find_query.clone();
        self.message = if self.editor.find(&q, self.match_case, backwards) {
            None
        } else {
            Some(format!("\"{q}\" not found"))
        };
    }

    fn open_find(&mut self, replace: bool) {
        self.find_open = true;
        self.replace_open = replace;
        self.find_focus = true;
    }

    fn replace_one(&mut self, now: f64) {
        if self.find_query.is_empty() {
            return self.open_find(true);
        }
        let (q, r) = (self.find_query.clone(), self.replace_text.clone());
        self.message = if self.editor.replace(&q, &r, self.match_case, now) {
            None
        } else {
            Some(format!("\"{q}\" not found"))
        };
    }

    fn replace_all(&mut self) {
        if self.find_query.is_empty() {
            return self.open_find(true);
        }
        let (q, r) = (self.find_query.clone(), self.replace_text.clone());
        let n = self.editor.replace_all(&q, &r, self.match_case);
        self.message = Some(match n {
            0 => format!("\"{q}\" not found"),
            1 => "Replaced 1 occurrence".into(),
            n => format!("Replaced {n} occurrences"),
        });
    }

    fn zoom(&mut self, delta: f32) {
        self.editor.font_size = if delta == 0.0 {
            DEFAULT_FONT_SIZE
        } else {
            (self.editor.font_size + delta).clamp(6.0, 72.0)
        };
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let pressed = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        if pressed(sc(CMD_SHIFT, Key::S)) {
            self.save_as();
        }
        if pressed(sc(CMD, Key::S)) {
            self.save();
        }
        if pressed(sc(CMD, Key::O)) {
            self.open_dialog();
        }
        if pressed(sc(CMD, Key::N)) {
            self.new_window(None);
        }
        if pressed(sc(CMD, Key::W)) || pressed(sc(CMD, Key::Q)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        #[cfg(target_os = "macos")]
        if pressed(sc(CMD, Key::M)) {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        if pressed(replace_shortcut()) {
            self.open_find(true);
        }
        if pressed(sc(CMD, Key::F)) {
            self.open_find(false);
        }
        if pressed(sc(CMD_SHIFT, Key::G)) {
            self.find(true);
        }
        if pressed(sc(CMD, Key::G)) || pressed(sc(Modifiers::NONE, Key::F3)) {
            self.find(false);
        }
        if pressed(sc(CMD, Key::L)) {
            self.goto_open = true;
        }
        if pressed(sc(CMD, Key::Equals)) || pressed(sc(CMD, Key::Plus)) {
            self.zoom(1.0);
        }
        if pressed(sc(CMD, Key::Minus)) {
            self.zoom(-1.0);
        }
        if pressed(sc(CMD, Key::Num0)) {
            self.zoom(0.0);
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            if let Some(l) = &self.loading {
                l.cancel.store(true, Ordering::Relaxed);
            }
            if self.editor.is_modified() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                if self.confirm_discard() {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    // ---- UI ------------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let short = |s: KeyboardShortcut| ctx.format_shortcut(&s);
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New Window").shortcut_text(short(sc(CMD, Key::N)))).clicked() {
                    self.new_window(None);
                }
                if ui.add(egui::Button::new("Open…").shortcut_text(short(sc(CMD, Key::O)))).clicked() {
                    self.open_dialog();
                }
                ui.separator();
                if ui.add(egui::Button::new("Save").shortcut_text(short(sc(CMD, Key::S)))).clicked() {
                    self.save();
                }
                if ui.add(egui::Button::new("Save As…").shortcut_text(short(sc(CMD_SHIFT, Key::S)))).clicked() {
                    self.save_as();
                }
                ui.separator();
                if ui.add(egui::Button::new("Close Window").shortcut_text(short(sc(CMD, Key::W)))).clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                let now = ui.input(|i| i.time);
                if ui.add_enabled(self.editor.can_undo(), egui::Button::new("Undo").shortcut_text(short(sc(CMD, Key::Z)))).clicked() {
                    self.editor.undo();
                }
                if ui.add_enabled(self.editor.can_redo(), egui::Button::new("Redo").shortcut_text(short(sc(CMD_SHIFT, Key::Z)))).clicked() {
                    self.editor.redo();
                }
                ui.separator();
                let has_sel = self.editor.has_selection();
                if ui.add_enabled(has_sel, egui::Button::new("Cut").shortcut_text(short(sc(CMD, Key::X)))).clicked() {
                    self.editor.cut(&ctx, now);
                }
                if ui.add_enabled(has_sel, egui::Button::new("Copy").shortcut_text(short(sc(CMD, Key::C)))).clicked() {
                    self.editor.copy(&ctx);
                }
                if ui.add(egui::Button::new("Paste").shortcut_text(short(sc(CMD, Key::V)))).clicked() {
                    self.editor.request_focus(&ctx);
                    ctx.send_viewport_cmd(ViewportCommand::RequestPaste);
                }
                if ui.add(egui::Button::new("Select All").shortcut_text(short(sc(CMD, Key::A)))).clicked() {
                    self.editor.select_all();
                }
                ui.separator();
                if ui.add(egui::Button::new("Find…").shortcut_text(short(sc(CMD, Key::F)))).clicked() {
                    self.open_find(false);
                }
                if ui.add(egui::Button::new("Replace…").shortcut_text(short(replace_shortcut()))).clicked() {
                    self.open_find(true);
                }
                if ui.add(egui::Button::new("Find Next").shortcut_text(short(sc(CMD, Key::G)))).clicked() {
                    self.find(false);
                }
                if ui.add(egui::Button::new("Find Previous").shortcut_text(short(sc(CMD_SHIFT, Key::G)))).clicked() {
                    self.find(true);
                }
                if ui.add(egui::Button::new("Go to Line…").shortcut_text(short(sc(CMD, Key::L)))).clicked() {
                    self.goto_open = true;
                }
                ui.separator();
                if ui.button("Clear (new document)").clicked() {
                    self.new_document();
                    self.editor.request_focus(&ctx);
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.editor.wrap, "Word Wrap");
                ui.checkbox(&mut self.editor.line_numbers, "Line Numbers");
                ui.separator();
                if ui.add(egui::Button::new("Zoom In").shortcut_text(short(sc(CMD, Key::Equals)))).clicked() {
                    self.zoom(1.0);
                }
                if ui.add(egui::Button::new("Zoom Out").shortcut_text(short(sc(CMD, Key::Minus)))).clicked() {
                    self.zoom(-1.0);
                }
                if ui.add(egui::Button::new("Actual Size").shortcut_text(short(sc(CMD, Key::Num0)))).clicked() {
                    self.zoom(0.0);
                }
                ui.separator();
                ui.label("Line endings for new lines:");
                ui.radio_value(&mut self.editor.crlf, false, "LF (macOS / Linux)");
                ui.radio_value(&mut self.editor.crlf, true, "CRLF (Windows)");
                ui.separator();
                ui.label("Theme:");
                for (t, name) in [
                    (ThemePreference::System, "System"),
                    (ThemePreference::Light, "Light"),
                    (ThemePreference::Dark, "Dark"),
                ] {
                    if ui.radio(self.theme == t, name).clicked() && self.theme != t {
                        self.theme = t;
                        ctx.set_theme(t);
                        settings::save_theme(t);
                    }
                }
            });
            ui.menu_button("Language", |ui| {
                egui::ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                    if ui.radio(self.editor.language.is_none(), "Plain Text").clicked() {
                        self.editor.language = None;
                    }
                    ui.separator();
                    let langs = self.languages.get_or_insert_with(highlight::language_names);
                    for name in langs.iter() {
                        let on = self.editor.language.as_deref() == Some(name.as_str());
                        if ui.radio(on, name).clicked() {
                            self.editor.language = Some(name.clone());
                        }
                    }
                });
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About Notepad").clicked() {
                    self.about_open = true;
                }
            });
        });
    }

    fn find_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let now = ui.input(|i| i.time);
        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        let escape = ui.input(|i| i.key_pressed(Key::Escape));
        let label_w = 64.0;
        let mut close = false;
        ui.horizontal(|ui| {
            ui.add_sized([label_w, 18.0], egui::Label::new("Find:"));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.find_query)
                    .desired_width(260.0)
                    .id(egui::Id::new("find-field")),
            );
            if std::mem::take(&mut self.find_focus) {
                resp.request_focus();
            }
            if resp.lost_focus() && enter {
                let back = ui.input(|i| i.modifiers.shift);
                self.find(back);
                resp.request_focus();
            }
            close |= resp.has_focus() && escape;
            if ui.button("Next").clicked() {
                self.find(false);
            }
            if ui.button("Previous").clicked() {
                self.find(true);
            }
            ui.checkbox(&mut self.match_case, "Match case");
            if !self.replace_open && ui.button("Replace…").clicked() {
                self.replace_open = true;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close |= ui.button("✕").clicked();
            });
        });
        if self.replace_open {
            ui.horizontal(|ui| {
                ui.add_sized([label_w, 18.0], egui::Label::new("Replace:"));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.replace_text)
                        .desired_width(260.0)
                        .id(egui::Id::new("replace-field")),
                );
                if resp.lost_focus() && enter {
                    self.replace_one(now);
                    resp.request_focus();
                }
                close |= resp.has_focus() && escape;
                if ui.button("Replace").clicked() {
                    self.replace_one(now);
                }
                if ui.button("Replace All").clicked() {
                    self.replace_all();
                }
            });
        }
        if close {
            self.find_open = false;
            self.editor.request_focus(&ctx);
        }
    }

    fn goto_window(&mut self, ctx: &egui::Context) {
        let mut open = self.goto_open;
        let mut done = false;
        egui::Window::new("Go to Line")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .show(ctx, |ui| {
                let lines = self.editor.doc.line_count();
                ui.label(format!("Line number (1 – {lines}):"));
                let resp = ui.add(egui::TextEdit::singleline(&mut self.goto_text).desired_width(200.0));
                resp.request_focus();
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                if ui.button("Go").clicked() || enter {
                    if let Ok(n) = self.goto_text.trim().replace([' ', '_', ','], "").parse::<u64>() {
                        self.editor.goto_line(n);
                    }
                    done = true;
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = true;
                }
            });
        if done || !open {
            self.goto_open = false;
            self.editor.request_focus(ctx);
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        let icon = self
            .about_icon
            .get_or_insert_with(|| {
                let data = crate::icon();
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [data.width as usize, data.height as usize],
                    &data.rgba,
                );
                ctx.load_texture("about-icon", img, egui::TextureOptions::LINEAR)
            })
            .clone();
        let mut open = self.about_open;
        let mut done = false;
        egui::Window::new("About Notepad")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(280.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.add(egui::Image::new(&icon).fit_to_exact_size(egui::vec2(96.0, 96.0)));
                    ui.add_space(4.0);
                    ui.heading("Notepad");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(4.0);
                    ui.label("A minimal plain-text editor.");
                    ui.hyperlink_to("github.com/MarketKernel/notepad", REPO_URL);
                    ui.add_space(8.0);
                    let ok = ui.button("  OK  ");
                    // Takes focus from the editor, and Enter / Space press it.
                    ok.request_focus();
                    done = ok.clicked();
                    ui.add_space(4.0);
                });
            });
        if done || !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.about_open = false;
            self.editor.request_focus(ctx);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(n) = self.editor.notice.take() {
                self.message = Some(n);
            }
            let (ln, col) = self.editor.cursor_line_col();
            ui.label(format!("Ln {ln}, Col {col}"));
            ui.separator();
            let sel = self.editor.selection();
            if !sel.is_empty() {
                ui.label(format!("{} selected", human_size(sel.end - sel.start)));
                ui.separator();
            }
            ui.label(format!("{} lines", self.editor.doc.line_count()));
            ui.separator();
            ui.label(human_size(self.editor.doc.len()));
            if let Some(m) = &self.message {
                ui.separator();
                ui.label(RichText::new(m).weak());
            }
            if self.editor.doc.original().is_some_and(|o| o.read_error.load(Ordering::Relaxed)) {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, "File was changed on disk by another program!");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(self.editor.language.as_deref().unwrap_or("Plain Text"));
                ui.separator();
                ui.label(if self.editor.crlf { "CRLF" } else { "LF" });
                ui.separator();
                ui.label("UTF-8");
            });
        });
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let dirty = if self.editor.is_modified() { "• " } else { "" };
        let title = format!("{dirty}{} — Notepad", self.file_name());
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

impl eframe::App for NotepadApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_loading(&ctx);
        self.handle_close(&ctx);
        if self.loading.is_none() {
            self.handle_shortcuts(&ctx);
        }

        // Files dropped onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.open_path(p);
        }
        #[cfg(target_os = "macos")]
        for p in crate::mac_open::take(&ctx) {
            self.open_from_finder(p);
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        if self.find_open {
            egui::Panel::top("find").show(ui, |ui| self.find_bar(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::no_frame().show(ui, |ui| {
            if let Some(l) = &self.loading {
                let done = l.progress.load(Ordering::Relaxed);
                let frac = if l.total > 0 { done as f32 / l.total as f32 } else { 0.0 };
                let mut cancel = false;
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.label(format!("Opening {} ({})…", l.path.display(), human_size(l.total)));
                    ui.add(egui::ProgressBar::new(frac).desired_width(400.0).show_percentage());
                    cancel = ui.button("Cancel").clicked();
                });
                if cancel {
                    l.cancel.store(true, Ordering::Relaxed);
                }
            } else {
                self.editor.show(ui);
            }
        });

        if self.goto_open {
            self.goto_window(&ctx);
        }
        if self.about_open {
            self.about_window(&ctx);
        }
        self.update_title(&ctx);
    }
}

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn executable_bit() {
        let p = std::env::temp_dir().join(format!("notepad-exec-{}", std::process::id()));
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&p));
        set_executable(&p, true).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(is_executable(&p));
        set_executable(&p, false).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::remove_file(&p).unwrap();
    }
}

#[cfg(test)]
mod screenshots {
    use super::*;

    /// Renders the real app offscreen into PNG files:
    /// `NOTEPAD_SHOTS=/some/dir cargo test --release screenshots -- --ignored`
    #[test]
    #[ignore]
    fn render_screens() {
        let dir = PathBuf::from(std::env::var("NOTEPAD_SHOTS").expect("NOTEPAD_SHOTS"));
        let file = PathBuf::from(file!()).canonicalize().unwrap();
        let mut h = egui_kittest::Harness::builder()
            .with_size([900.0, 560.0])
            .wgpu()
            .build_eframe(|cc| NotepadApp::new(cc, Some(file)));
        for _ in 0..200 {
            h.step();
            if h.state().loading.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        h.state_mut().editor.goto_line(30);
        h.run_steps(3);
        h.event(egui::Event::Key {
            key: Key::ArrowDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        });
        h.run_steps(3);
        h.render().unwrap().save(dir.join("app-rust.png")).unwrap();

        // Fresh window with pasted text, word wrap and a search bar.
        let mut h = egui_kittest::Harness::builder()
            .with_size([700.0, 420.0])
            .wgpu()
            .build_eframe(|cc| NotepadApp::new(cc, None));
        h.run_steps(2);
        let text = "Привет, мир! Hello world. 你好世界 — こんにちは\n\n".to_string()
            + &"A long pasted paragraph that should wrap nicely at the window edge. ".repeat(8)
            + "\n\tIndented line\nend";
        h.event(egui::Event::Paste(text));
        h.run_steps(2);
        h.ctx.set_theme(egui::Theme::Light);
        h.state_mut().open_find(true);
        h.state_mut().find_query = "paragraph".into();
        h.state_mut().replace_text = "text block".into();
        h.run_steps(2);
        h.state_mut().find(false);
        h.run_steps(3);
        h.render().unwrap().save(dir.join("app-paste.png")).unwrap();

        // Dark theme with the About box.
        h.ctx.set_theme(egui::Theme::Dark);
        h.state_mut().find_open = false;
        h.state_mut().about_open = true;
        h.run_steps(3);
        h.render().unwrap().save(dir.join("app-about.png")).unwrap();
    }
}
