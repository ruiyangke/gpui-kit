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
}
