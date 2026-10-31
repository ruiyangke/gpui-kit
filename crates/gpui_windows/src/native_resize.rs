//! Bounded conversion between logical client sizes and native sizing rectangles.

const MAX_EXTENT: i32 = 1 << 20;

/// The outer size that keeps a client's logical size at a new DPI: the
/// client scaled by the DPI ratio, plus `chrome` measured at the new DPI.
pub(crate) fn dpi_scaled_size(
    client: [i32; 2],
    from: u32,
    to: u32,
    chrome: [i32; 2],
) -> Option<[i32; 2]> {
    if from == 0 || to == 0 || !chrome.iter().all(|n| (0..MAX_EXTENT).contains(n)) {
        return None;
    }
    let ratio = to as f64 / from as f64;
    let mut size = [0; 2];
    for axis in 0..2 {
        if !(1..=MAX_EXTENT).contains(&client[axis]) {
            return None;
        }
        let scaled = (client[axis] as f64 * ratio).round() as i32;
        size[axis] = scaled.checked_add(chrome[axis])?;
        if !(1..=MAX_EXTENT).contains(&size[axis]) {
            return None;
        }
    }
    Some(size)
}

/// The native frame around a client at one DPI, from the window rectangle
/// `AdjustWindowRectExForDpi` gives an empty client. A window that hides its
/// title bar keeps the frame's top as client area (see WM_NCCALCSIZE).
pub(crate) fn frame_chrome(frame: [i32; 4], hide_title_bar: bool) -> Option<[i32; 2]> {
    let [left, top, right, bottom] = frame;
    let top = if hide_title_bar { 0 } else { top };
    let chrome = [right.checked_sub(left)?, bottom.checked_sub(top)?];
    chrome
        .iter()
        .all(|n| (0..MAX_EXTENT).contains(n))
        .then_some(chrome)
}

/// The outer size answered for a DPI change, and the size Windows scales the
/// old outer size to on its own. After the display change, Windows may place
/// the window again at its own size; that placement keeps the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DpiResize {
    dpi: u32,
    answer: [i32; 2],
    system: [i32; 2],
}

/// What a proposed resize does to a [`DpiResize`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DpiPlacement {
    /// The answered size; keep watching.
    Keep,
    /// Windows' own size: use the answer instead.
    Replace([i32; 2]),
    /// Any other resize is newer intent; stop watching.
    End,
}

impl DpiResize {
    pub(crate) fn new(outer: [i32; 2], from: u32, to: u32, answer: [i32; 2]) -> Option<Self> {
        if from == 0 || to == 0 || !outer.iter().all(|n| (1..=MAX_EXTENT).contains(n)) {
            return None;
        }
        // Windows rounds the scaled size to the nearest pixel.
        let system = outer.map(|n| {
            ((i64::from(n) * i64::from(to) + i64::from(from) / 2) / i64::from(from)) as i32
        });
        (system != answer).then_some(Self {
            dpi: to,
            answer,
            system,
        })
    }

    pub(crate) fn place(self, dpi: u32, proposed: [i32; 2]) -> DpiPlacement {
        if dpi != self.dpi {
            DpiPlacement::End
        } else if proposed == self.answer {
            DpiPlacement::Keep
        } else if (0..2).all(|axis| (proposed[axis] - self.system[axis]).abs() <= 1) {
            DpiPlacement::Replace(self.answer)
        } else {
            DpiPlacement::End
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dpi_change_keeps_the_client_size_and_measures_the_frame_anew() {
        // 200 % to 150 %: an 1800x1200 client becomes 1350x900, plus the
        // native frame at 144 DPI, not three quarters of the 192 DPI frame.
        assert_eq!(dpi_scaled_size([1800, 1200], 192, 144, [22, 53]), Some([1372, 953]));
        // A client-decorated window has no native frame.
        assert_eq!(dpi_scaled_size([1201, 801], 96, 144, [0, 0]), Some([1802, 1202]));
        assert_eq!(dpi_scaled_size([800, 600], 144, 144, [16, 39]), Some([816, 639]));
        assert!(dpi_scaled_size([800, 600], 0, 144, [0, 0]).is_none());
        assert!(dpi_scaled_size([0, 600], 96, 144, [0, 0]).is_none());
        assert!(dpi_scaled_size([800, 600], 96, 144, [-1, 0]).is_none());
        assert!(dpi_scaled_size([MAX_EXTENT, 600], 96, 192, [0, 0]).is_none());
    }

    #[test]
    fn a_hidden_title_bar_keeps_only_the_side_and_bottom_frame() {
        // WS_OVERLAPPEDWINDOW at 192 DPI: 13-pixel borders and a 45-pixel caption.
        let frame = [-13, -58, 13, 13];
        assert_eq!(frame_chrome(frame, false), Some([26, 71]));
        assert_eq!(frame_chrome(frame, true), Some([26, 13]));
        assert!(frame_chrome([13, 0, -13, 13], false).is_none());
    }

    #[test]
    fn windows_placing_a_window_at_its_own_dpi_size_keeps_the_answer() {
        // QTerminal at 200 % to 150 %: the answer keeps a 1580x630 client,
        // Windows' own size is three quarters of 2132x911.
        let resize = DpiResize::new([2132, 911], 192, 144, [1602, 686]).unwrap();
        assert_eq!(resize.place(144, [1602, 686]), DpiPlacement::Keep);
        assert_eq!(
            resize.place(144, [1599, 683]),
            DpiPlacement::Replace([1602, 686])
        );
        // Back to 200 %: 1602x686 by four thirds is 2136x914.67.
        let resize = DpiResize::new([1602, 686], 144, 192, [2133, 911]).unwrap();
        assert_eq!(
            resize.place(192, [2136, 915]),
            DpiPlacement::Replace([2133, 911])
        );
        // Windows' rounding may differ by a pixel (1409 by four thirds is 1878.67).
        let resize = DpiResize::new([1571, 1409], 144, 192, [2094, 1879]).unwrap();
        assert_eq!(
            resize.place(192, [2095, 1878]),
            DpiPlacement::Replace([2094, 1879])
        );
    }

    #[test]
    fn any_other_resize_or_dpi_ends_the_dpi_resize() {
        let resize = DpiResize::new([2132, 911], 192, 144, [1602, 686]).unwrap();
        assert_eq!(resize.place(144, [1700, 686]), DpiPlacement::End);
        assert_eq!(resize.place(144, [1597, 683]), DpiPlacement::End);
        assert_eq!(resize.place(192, [1599, 683]), DpiPlacement::End);
        // Nothing to keep when Windows' own size is the answer.
        assert!(DpiResize::new([2132, 912], 192, 144, [1599, 684]).is_none());
        assert!(DpiResize::new([2132, 911], 0, 144, [1602, 686]).is_none());
        assert!(DpiResize::new([0, 911], 192, 144, [1602, 686]).is_none());
    }
}
