//! Native Windows drag authority, separate from GPUI's rendered drag payload.

/// A single-use native left-button gesture captured by its source window.
///
/// It remains pending while an application prepares a drag. Native release,
/// capture loss, deactivation, a new press, or source retirement invalidates it.
/// Constructed GPUI mouse events cannot create this authority.
pub struct NativeDragGesture {
    claim: Box<dyn FnOnce() -> Option<NativeDragTracking>>,
}

impl NativeDragGesture {
    /// Claim this press after its input dispatch has returned. The platform
    /// rechecks the live native owner, foreground, capture and held button.
    ///
    /// This does not start OLE, release capture, or enter a native modal loop.
    /// The caller must enter such a loop outside GPUI App/window updates.
    pub fn claim(self) -> Option<NativeDragTracking> {
        (self.claim)()
    }

    /// Construct authority supplied by the native platform implementation.
    ///
    /// # Safety
    /// The closure must consume a single, actual native press; reject expired
    /// or still-dispatching presses; and recheck native ownership at claim.
    #[doc(hidden)]
    pub unsafe fn from_platform(
        claim: impl FnOnce() -> Option<NativeDragTracking> + 'static,
    ) -> Self {
        Self { claim: Box::new(claim) }
    }
}

/// Claimed native drag authority owned by the caller's tracking operation.
///
/// Capture transfer, button release and target activation are normal during
/// OLE tracking. They do not revoke this authority; source retirement does.
/// Remote-session cancellation and native drop-result policy remain the caller's
/// responsibility. This handle does not keep an HWND alive or delay destruction.
pub struct NativeDragTracking {
    current: Box<dyn Fn() -> bool>,
    finish: Option<Box<dyn FnOnce()>>,
}

impl NativeDragTracking {
    /// Whether the native source still permits this tracking operation.
    pub fn is_current(&self) -> bool {
        (self.current)()
    }

    /// Construct a tracking lifetime supplied by the native platform.
    ///
    /// # Safety
    /// `current` must reject source retirement and never revive an expired
    /// operation. `finish` must release only this operation's ownership.
    #[doc(hidden)]
    pub unsafe fn from_platform(
        current: impl Fn() -> bool + 'static,
        finish: impl FnOnce() + 'static,
    ) -> Self {
        Self { current: Box::new(current), finish: Some(Box::new(finish)) }
    }
}

impl Drop for NativeDragTracking {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish();
        }
    }
}
