//! Native macOS save panel (NSSavePanel) shown as a sheet on the window, with
//! the Tags field, a "Make file executable" checkbox and — when closing a new,
//! never saved document — a "Delete" button, like document-based Mac apps.

use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSModalResponseOK, NSSavePanel, NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLTagNamesKey};

pub struct Request<'a> {
    /// Text shown at the top of the panel.
    pub message: Option<String>,
    pub file_name: &'a str,
    pub directory: Option<&'a Path>,
    /// Show "Delete" (discard the document) in addition to Cancel / Save.
    pub delete_button: bool,
    /// Initial state of "Make file executable".
    pub executable: bool,
}

pub enum Outcome {
    Save {
        path: PathBuf,
        executable: bool,
        tags: Vec<String>,
    },
    Delete,
    Cancel,
}

pub fn run(req: Request) -> Outcome {
    let Some(mtm) = MainThreadMarker::new() else {
        return Outcome::Cancel;
    };
    let panel = NSSavePanel::savePanel(mtm);
    panel.setCanCreateDirectories(true);
    panel.setShowsTagField(true);
    panel.setNameFieldStringValue(&NSString::from_str(req.file_name));
    if let Some(m) = &req.message {
        panel.setMessage(Some(&NSString::from_str(m)));
    }
    if let Some(dir) = req.directory {
        let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&dir.to_string_lossy()), true);
        panel.setDirectoryURL(Some(&url));
    }

    // Accessory row: [Delete]   [x] Make file executable
    let checkbox = unsafe {
        NSButton::checkboxWithTitle_target_action(&NSString::from_str("Make file executable"), None, None, mtm)
    };
    checkbox.setState(if req.executable { NSControlStateValueOn } else { NSControlStateValueOff });
    checkbox.sizeToFit();
    // "Delete" closes the panel through its `cancel:` action; being an on/off
    // button, its state tells afterwards that it (and not Cancel) was clicked.
    let delete: Option<Retained<NSButton>> = req.delete_button.then(|| {
        let b = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("Delete"), Some(&panel), Some(sel!(cancel:)), mtm)
        };
        b.setButtonType(NSButtonType::PushOnPushOff);
        b.setHasDestructiveAction(true);
        b.setContentTintColor(Some(&NSColor::systemRedColor()));
        b.sizeToFit();
        b
    });

    let (width, height) = (480.0, 48.0);
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
    );
    let cb = checkbox.frame().size;
    let mut cb_x = ((width - cb.width) / 2.0).round();
    if let Some(d) = &delete {
        let ds = d.frame().size;
        d.setFrameOrigin(NSPoint::new(20.0, ((height - ds.height) / 2.0).round()));
        d.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMaxXMargin);
        view.addSubview(d);
        cb_x = cb_x.max(20.0 + ds.width + 24.0);
    }
    checkbox.setFrameOrigin(NSPoint::new(cb_x, ((height - cb.height) / 2.0).round()));
    checkbox.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin,
    );
    view.addSubview(&checkbox);
    panel.setAccessoryView(Some(&view));

    let app = NSApplication::sharedApplication(mtm);
    let parent = app.keyWindow().or_else(|| app.mainWindow());
    if let Some(w) = &parent {
        let done = block2::StackBlock::new(|_: isize| {});
        panel.beginSheetModalForWindow_completionHandler(w, &done);
    }
    let response = panel.runModal();
    if let Some(w) = &parent {
        w.makeKeyAndOrderFront(None);
    }

    if delete.is_some_and(|d| d.state() == NSControlStateValueOn) {
        return Outcome::Delete;
    }
    if response != NSModalResponseOK {
        return Outcome::Cancel;
    }
    let Some(path) = panel.URL().and_then(|u| u.path()) else {
        return Outcome::Cancel;
    };
    let tags = panel
        .tagNames()
        .map(|a| a.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    Outcome::Save {
        path: PathBuf::from(path.to_string()),
        executable: checkbox.state() == NSControlStateValueOn,
        tags,
    }
}

/// Applies Finder tags chosen in the panel.
pub fn set_tags(path: &Path, tags: &[String]) {
    if tags.is_empty() {
        return;
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let names: Vec<Retained<NSString>> = tags.iter().map(|t| NSString::from_str(t)).collect();
    let array = NSArray::from_retained_slice(&names);
    let _ = unsafe { url.setResourceValue_forKey_error(Some(&array), NSURLTagNamesKey) };
}
