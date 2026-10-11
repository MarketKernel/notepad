#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod buffer;
mod editor;
mod fonts;
mod highlight;
#[cfg(target_os = "macos")]
mod mac_save;

use std::path::PathBuf;

fn main() -> eframe::Result {
    // Every launch starts with a clean sheet; a file is opened only if one is
    // passed on the command line.
    let path = std::env::args_os().nth(1).map(PathBuf::from);

    let mut options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Untitled — Notepad")
            .with_inner_size([900.0, 650.0])
            .with_min_inner_size([320.0, 200.0])
            .with_drag_and_drop(true),
        persist_window: false,
        ..Default::default()
    };

    // On macOS winit's default app menu makes Cmd+Q terminate the process
    // immediately, skipping the "save changes?" question. Without that menu
    // Cmd+Q reaches the app as a normal shortcut.
    #[cfg(target_os = "macos")]
    {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            builder.with_default_menu(false);
        }));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = &mut options;

    eframe::run_native(
        "notepad",
        options,
        Box::new(move |cc| Ok(Box::new(app::NotepadApp::new(cc, path)))),
    )
}
