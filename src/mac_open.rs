//! Files opened from Finder ("Open With", double-click, a drop onto the Dock
//! icon, `open -a Notepad file`). macOS hands them over as an "open documents"
//! Apple Event rather than on the command line, and winit ignores that event,
//! so we install our own handler and queue the paths for the app.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::NSApplicationWillFinishLaunchingNotification;
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter};

const fn four_cc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const CORE_EVENT_CLASS: u32 = four_cc(b"aevt");
const OPEN_DOCUMENTS: u32 = four_cc(b"odoc");
const DIRECT_OBJECT: u32 = four_cc(b"----");

static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// Set on the first frame; wakes the app up when files arrive later.
static CTX: OnceLock<egui::Context> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and there is no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NotepadOpenHandler"]
    struct OpenHandler;

    impl OpenHandler {
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _notification: &NSNotification) {
            // AppKit installs its own handler before this point (and would
            // replace ours if we were earlier); the files of the launch arrive
            // right after it.
            let manager = NSAppleEventManager::sharedAppleEventManager();
            unsafe {
                let _: () = msg_send![
                    &manager,
                    setEventHandler: self,
                    andSelector: sel!(handleOpenDocuments:withReplyEvent:),
                    forEventClass: CORE_EVENT_CLASS,
                    andEventID: OPEN_DOCUMENTS
                ];
            }
        }

        #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
        fn handle_open_documents(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            let list: Option<Retained<NSAppleEventDescriptor>> =
                unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
            let Some(list) = list else { return };
            // Apple Event lists are 1-based.
            let paths = (1..=list.numberOfItems())
                .filter_map(|i| list.descriptorAtIndex(i)?.fileURLValue()?.to_file_path());
            PENDING.lock().unwrap().extend(paths);
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
    }
);

/// Call before the event loop starts.
pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let handler: Retained<OpenHandler> = unsafe { msg_send![OpenHandler::alloc(mtm), init] };
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &handler,
            sel!(applicationWillFinishLaunching:),
            Some(NSApplicationWillFinishLaunchingNotification),
            None,
        );
    }
    // Neither the notification center nor the Apple Event manager retains it.
    std::mem::forget(handler);
}

/// Files that Finder asked to open since the last call.
pub fn take(ctx: &egui::Context) -> Vec<PathBuf> {
    CTX.get_or_init(|| ctx.clone());
    std::mem::take(&mut *PENDING.lock().unwrap())
}
