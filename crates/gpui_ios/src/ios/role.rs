//! What a window is to the person using it: which app it belongs to, and
//! whether it is a dialog over another window. The code that opens a window
//! says so just before opening it.
use raw_window_handle::RawWindowHandle;
use std::cell::RefCell;

/// How a window shows on iPad. A window gets an iPad window (a UIKit scene)
/// of its own, titled with its title and subtitled `app_name`; `group` names
/// its app. A sheet shows over its parent, in the parent's iPad window.
#[derive(Clone, Debug)]
pub struct WindowRole {
    pub group: String,
    pub app_name: String,
    pub parent: Option<RawWindowHandle>,
    pub sheet: bool,
    /// A modal sheet blocks its parent until it closes.
    pub modal: bool,
    /// The size range, in points, the person may resize its iPad window to.
    pub min_size: Option<(f64, f64)>,
    pub max_size: Option<(f64, f64)>,
}

thread_local! {
    static NEXT: RefCell<Option<WindowRole>> = const { RefCell::new(None) };
}

/// Give the next window opened on this thread `role`. Windows opened with no
/// role each get an iPad window of their own.
pub fn set_next_window_role(role: WindowRole) {
    NEXT.with(|next| *next.borrow_mut() = Some(role));
}

pub(super) fn take_next() -> Option<WindowRole> {
    NEXT.with(|next| next.borrow_mut().take())
}
