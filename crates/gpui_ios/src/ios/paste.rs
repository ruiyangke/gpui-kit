//! Pastes the person starts. A paste from the keyboard (⌘V) or a menu reaches
//! the app as UIKit's `paste:`, and reading the pasteboard while UIKit runs it
//! needs no permission. A read at any other time, of what another app copied,
//! asks first ("Paste from Other Apps"; its default is Ask).
//!
//! GPUI's views take `paste:`, save what the pasteboard holds while they may
//! read it, post [`PASTE_NOTIFICATION`] so other code can save what it needs
//! (a remote app reads the pasteboard only when it asks for the paste), and
//! press ⌘V in GPUI as a key would. Later reads of the same copy use what was
//! saved.

use objc2_foundation::{NSNotificationCenter, NSString};
use objc2_ui_kit::UIPasteboard;
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// Posted on the main thread while UIKit runs a paste the person started; an
/// observer may read the general pasteboard then without asking.
pub const PASTE_NOTIFICATION: &str = "GPUIPasteStartedNotification";

/// How long a pasted copy is kept: its reads follow the paste at once, and
/// the copy may be a password.
const KEPT_FOR: Duration = Duration::from_secs(30);

/// The pasteboard's text when the person last pasted: its change count, the
/// text, and when.
type Saved = (isize, Option<String>, Instant);

thread_local! {
    static SAVED: RefCell<Option<Saved>> = const { RefCell::new(None) };
}

/// Whether there is something to paste. Counting items reads no contents,
/// so it never asks.
pub(super) fn available() -> bool {
    // SAFETY: UIPasteboard on the main thread.
    unsafe { UIPasteboard::generalPasteboard().numberOfItems() > 0 }
}

/// Saves what the pasteboard holds and tells observers, from `paste:`.
pub(super) fn started() {
    let pasteboard = UIPasteboard::generalPasteboard();
    // SAFETY: UIPasteboard on the main thread, inside UIKit's paste action.
    let (count, text) = unsafe {
        (
            pasteboard.changeCount(),
            pasteboard.string().map(|text| text.to_string()),
        )
    };
    SAVED.with(|saved| *saved.borrow_mut() = Some((count, text, Instant::now())));
    let name = NSString::from_str(PASTE_NOTIFICATION);
    // SAFETY: posting a notification with no object, on the main thread.
    unsafe {
        NSNotificationCenter::defaultCenter().postNotificationName_object(&name, None);
    }
}

/// The text saved when the person pasted the pasteboard's current copy:
/// `Some` once saved (its text, if any), `None` for a copy never pasted.
pub(super) fn saved_text(pasteboard: &UIPasteboard) -> Option<Option<String>> {
    // SAFETY: the change count is read without asking.
    let count = unsafe { pasteboard.changeCount() };
    SAVED.with(|saved| {
        let mut saved = saved.borrow_mut();
        if saved.as_ref().is_some_and(|(_, _, at)| at.elapsed() > KEPT_FOR) {
            *saved = None;
        }
        saved
            .as_ref()
            .filter(|(saved, _, _)| *saved == count)
            .map(|(_, text, _)| text.clone())
    })
}
