//! Bounded conversion between logical client sizes and native sizing rectangles.
use gpui::{NativeResizeConstraint, Pixels, Size, px, size};

const MAX_EXTENT: i32 = 1 << 20;

#[derive(Clone, Copy)]
pub(crate) struct Geometry {
    pub(crate) current: [i32; 4],
    pub(crate) chrome: [i32; 2],
    pub(crate) dpi: u32,
}

fn extent(rect: [i32; 4]) -> Option<[i32; 2]> {
    let width = rect[2].checked_sub(rect[0])?;
    let height = rect[3].checked_sub(rect[1])?;
    ((1..=MAX_EXTENT).contains(&width) && (1..=MAX_EXTENT).contains(&height))
        .then_some([width, height])
}

fn logical(value: Size<Pixels>) -> Option<[f64; 2]> {
    let value = [
        f32::from(value.width) as f64,
        f32::from(value.height) as f64,
    ];
    value
        .iter()
        .all(|n| n.is_finite() && *n > 0.)
        .then_some(value)
}

impl Geometry {
    /// The limits describe the restored window. A client-decorated window has
    /// no native chrome then; only while maximized is its client clipped
    /// inside the outer frame.
    pub(crate) fn measure(
        outer: [i32; 4],
        client: [i32; 2],
        client_decorated: bool,
        dpi: u32,
    ) -> Option<Self> {
        let chrome = if client_decorated {
            [0, 0]
        } else {
            let [width, height] = extent(outer)?;
            [
                width.checked_sub(client[0])?,
                height.checked_sub(client[1])?,
            ]
        };
        Some(Self {
            current: outer,
            chrome,
            dpi,
        })
    }

    fn scale(self) -> Option<f64> {
        (self.dpi > 0 && self.chrome.iter().all(|n| (0..MAX_EXTENT).contains(n)))
            .then_some(self.dpi as f64 / 96.)
    }

    fn outer(self, client: Size<Pixels>) -> Option<[i32; 2]> {
        let scale = self.scale()?;
        let client = logical(client)?;
        let mut outer = [0; 2];
        for axis in 0..2 {
            let physical = (client[axis] * scale).round();
            if !physical.is_finite() || physical < 1. || physical > MAX_EXTENT as f64 {
                return None;
            }
            outer[axis] = (physical as i32).checked_add(self.chrome[axis])?;
            if outer[axis] > MAX_EXTENT {
                return None;
            }
        }
        Some(outer)
    }

    /// Native maximize bounds remain untouched; these are only tracking limits.
    pub(crate) fn limits(self, policy: &NativeResizeConstraint) -> Option<([i32; 2], [i32; 2])> {
        match policy {
            NativeResizeConstraint::Fixed => extent(self.current).map(|size| (size, size)),
            NativeResizeConstraint::Constrained {
                minimum, maximum, ..
            } => {
                let min = logical(*minimum)?;
                let max = logical(*maximum)?;
                if (0..2).any(|axis| min[axis] > max[axis]) {
                    return None;
                }
                Some((self.outer(*minimum)?, self.outer(*maximum)?))
            }
        }
    }

    pub(crate) fn constrain(
        self,
        proposed: [i32; 4],
        edge: u32,
        policy: &NativeResizeConstraint,
    ) -> Option<[i32; 4]> {
        if matches!(policy, NativeResizeConstraint::Fixed) {
            return Some(self.current);
        }
        let NativeResizeConstraint::Constrained {
            minimum,
            maximum,
            select,
        } = policy
        else {
            unreachable!()
        };
        let scale = self.scale()?;
        self.limits(policy)?;
        let outer = extent(proposed)?;
        let requested = [
            outer[0].checked_sub(self.chrome[0])?,
            outer[1].checked_sub(self.chrome[1])?,
        ];
        if requested.iter().any(|n| *n <= 0) {
            return None;
        }
        let selected = select(size(
            px((requested[0] as f64 / scale) as f32),
            px((requested[1] as f64 / scale) as f32),
        ))?;
        let value = logical(selected)?;
        let min = logical(*minimum)?;
        let max = logical(*maximum)?;
        if (0..2).any(|axis| value[axis] < min[axis] || value[axis] > max[axis]) {
            return None;
        }
        anchor(proposed, edge, self.outer(selected)?)
    }
}

/// WM_SIZING edges are stable Win32 constants; preserve each opposite edge.
fn anchor(mut rect: [i32; 4], edge: u32, size: [i32; 2]) -> Option<[i32; 4]> {
    if !(1..=8).contains(&edge) {
        return None;
    }
    if matches!(edge, 1 | 4 | 7) {
        rect[0] = rect[2].checked_sub(size[0])?;
    } else {
        rect[2] = rect[0].checked_add(size[0])?;
    }
    if matches!(edge, 3 | 4 | 5) {
        rect[1] = rect[3].checked_sub(size[1])?;
    } else {
        rect[3] = rect[1].checked_add(size[1])?;
    }
    Some(rect)
}

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
    use std::rc::Rc;

    fn grid() -> NativeResizeConstraint {
        NativeResizeConstraint::Constrained {
            minimum: size(px(100.), px(80.)),
            maximum: size(px(500.), px(400.)),
            select: Rc::new(|requested| {
                Some(size(
                    px((f32::from(requested.width) / 20.).round().clamp(5., 25.) * 20.),
                    px((f32::from(requested.height) / 20.).round().clamp(4., 20.) * 20.),
                ))
            }),
        }
    }

    #[test]
    fn increments_and_chrome_use_current_dpi_at_negative_monitor_origins() {
        for dpi in [96, 120, 144, 192] {
            let geometry = Geometry {
                current: [-2000, -700, -1500, -300],
                chrome: [18, 40],
                dpi,
            };
            let requested = [-1800, -600, -1391, -279];
            let selected = geometry.constrain(requested, 8, &grid()).unwrap();
            let logical = [
                (selected[2] - selected[0] - 18) as f64 * 96. / dpi as f64,
                (selected[3] - selected[1] - 40) as f64 * 96. / dpi as f64,
            ];
            assert_eq!(selected[..2], requested[..2]);
            assert_eq!(logical[0] % 20., 0.);
            assert_eq!(logical[1] % 20., 0.);
        }
    }

    #[test]
    fn a_maximized_client_decorated_window_restores_to_its_minimum() {
        let minimum = size(px(360.), px(616.));
        let policy = NativeResizeConstraint::Constrained {
            minimum,
            maximum: size(px(4000.), px(4000.)),
            select: Rc::new(Some),
        };
        // At 200 %, maximized: the outer frame overhangs the client by 13 px.
        let maximized = [-13, -13, 3469, 2085];
        let client = [3456, 2072];
        let restored = Geometry::measure(maximized, client, true, 192).unwrap();
        assert_eq!(restored.limits(&policy).unwrap().0, [720, 1232]);
        let framed = Geometry::measure(maximized, client, false, 192).unwrap();
        assert_eq!(framed.limits(&policy).unwrap().0, [746, 1258]);
    }

    #[test]
    fn every_edge_preserves_its_opposite_anchor() {
        let original = [-1000, -600, -500, -200];
        for edge in 1..=8 {
            let adjusted = anchor(original, edge, [300, 240]).unwrap();
            assert_eq!(adjusted[2] - adjusted[0], 300);
            assert_eq!(adjusted[3] - adjusted[1], 240);
            let x = if matches!(edge, 1 | 4 | 7) { 2 } else { 0 };
            let y = if matches!(edge, 3 | 4 | 5) { 3 } else { 1 };
            assert_eq!(adjusted[x], original[x]);
            assert_eq!(adjusted[y], original[y]);
        }
        assert!(anchor(original, 0, [300, 240]).is_none());
        assert!(anchor([i32::MAX - 2, 0, i32::MAX, 20], 2, [30, 20]).is_none());
    }

    #[test]
    fn invalid_or_conflicting_policy_refuses_resize_and_fixed_keeps_current_bounds() {
        let geometry = Geometry {
            current: [10, 20, 210, 180],
            chrome: [0, 0],
            dpi: 96,
        };
        assert_eq!(
            geometry.constrain([0, 0, 800, 600], 8, &NativeResizeConstraint::Fixed),
            Some(geometry.current)
        );
        assert_eq!(
            geometry.limits(&NativeResizeConstraint::Fixed),
            Some(([200, 160], [200, 160]))
        );
        for (min, max, selected) in [
            (500., 100., 200.),
            (100., 500., f32::NAN),
            (100., 500., 600.),
        ] {
            let policy = NativeResizeConstraint::Constrained {
                minimum: size(px(min), px(min)),
                maximum: size(px(max), px(max)),
                select: Rc::new(move |_| Some(size(px(selected), px(selected)))),
            };
            assert!(geometry.constrain([0, 0, 300, 300], 8, &policy).is_none());
        }
        assert!(
            Geometry { dpi: 0, ..geometry }
                .constrain([0, 0, 300, 300], 8, &grid())
                .is_none()
        );
    }

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
