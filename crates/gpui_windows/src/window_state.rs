//! Native zoom changes show state; the OS retains the ordinary restore rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WindowOpenState {
    Maximized,
    Fullscreen,
    Windowed,
}

impl WindowOpenState {
    pub(super) fn toggle_zoom(self) -> Self {
        match self {
            Self::Windowed => Self::Maximized,
            Self::Maximized => Self::Windowed,
            Self::Fullscreen => Self::Fullscreen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WindowOpenState;

    #[test]
    fn repeated_zoom_restores_the_native_windowed_state() {
        let maximized = WindowOpenState::Windowed.toggle_zoom();
        assert_eq!(maximized, WindowOpenState::Maximized);
        assert_eq!(maximized.toggle_zoom(), WindowOpenState::Windowed);
    }

    #[test]
    fn pending_fullscreen_keeps_its_show_intent() {
        assert_eq!(
            WindowOpenState::Fullscreen.toggle_zoom(),
            WindowOpenState::Fullscreen
        );
    }
}
