//! Physical hit testing and work-area clipping for opt-in client decorations.
use windows::Win32::{
    Foundation::{POINT, RECT},
    UI::WindowsAndMessaging::{
        HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTNOWHERE, HTRIGHT, HTTOP,
        HTTOPLEFT, HTTOPRIGHT,
    },
};

pub(crate) fn maximized_client(proposed: RECT, work_area: RECT) -> Option<RECT> {
    let rect = RECT {
        left: proposed.left.max(work_area.left),
        top: proposed.top.max(work_area.top),
        right: proposed.right.min(work_area.right),
        bottom: proposed.bottom.min(work_area.bottom),
    };
    (rect.left < rect.right && rect.top < rect.bottom).then_some(rect)
}

pub(crate) fn hit_test(bounds: RECT, cursor: POINT, thickness: [i32; 2]) -> u32 {
    if cursor.x < bounds.left
        || cursor.x >= bounds.right
        || cursor.y < bounds.top
        || cursor.y >= bounds.bottom
    {
        return HTNOWHERE;
    }
    // Native RECT coordinates may span negative monitor origins. Use wider
    // arithmetic and disjoint bands even for a window smaller than two borders.
    let width = i64::from(bounds.right) - i64::from(bounds.left);
    let height = i64::from(bounds.bottom) - i64::from(bounds.top);
    let x = i64::from(cursor.x) - i64::from(bounds.left);
    let y = i64::from(cursor.y) - i64::from(bounds.top);
    let horizontal = i64::from(thickness[0].max(0)).min(width / 2);
    let vertical = i64::from(thickness[1].max(0)).min(height / 2);
    let left = x < horizontal;
    let right = x >= width - horizontal;
    let top = y < vertical;
    let bottom = y >= height - vertical;
    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    }
}

#[cfg(test)]
mod tests {
    use super::{hit_test, maximized_client};
    use windows::Win32::{
        Foundation::{POINT, RECT},
        UI::WindowsAndMessaging::{
            HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTNOWHERE, HTRIGHT, HTTOP,
            HTTOPLEFT, HTTOPRIGHT,
        },
    };

    #[test]
    fn all_edges_use_physical_dpi_bands_and_negative_monitor_origins() {
        let bounds = RECT {
            left: -1600,
            top: -900,
            right: -400,
            bottom: -100,
        };
        for dpi in [96, 120, 144, 192, 240, 288] {
            let bands = [8 * dpi / 96, 9 * dpi / 96];
            for (x, y, expected) in [
                (-1600, -900, HTTOPLEFT),
                (-1000, -900, HTTOP),
                (-401, -900, HTTOPRIGHT),
                (-1600, -500, HTLEFT),
                (-401, -500, HTRIGHT),
                (-1600, -101, HTBOTTOMLEFT),
                (-1000, -101, HTBOTTOM),
                (-401, -101, HTBOTTOMRIGHT),
                (-1600 + bands[0], -900 + bands[1], HTCLIENT),
                (-1600 - 1, -500, HTNOWHERE),
                (-400, -500, HTNOWHERE),
            ] {
                assert_eq!(hit_test(bounds, POINT { x, y }, bands), expected);
            }
        }
    }

    #[test]
    fn tiny_and_extreme_bounds_do_not_overflow_or_overlap_edge_bands() {
        let tiny = RECT {
            left: -1,
            top: -1,
            right: 2,
            bottom: 2,
        };
        assert_eq!(hit_test(tiny, POINT { x: 0, y: 0 }, [100; 2]), HTCLIENT);
        assert_eq!(
            hit_test(tiny, POINT { x: 1, y: 1 }, [100; 2]),
            HTBOTTOMRIGHT
        );
        let extreme = RECT {
            left: i32::MIN,
            top: i32::MIN,
            right: i32::MAX,
            bottom: i32::MAX,
        };
        assert_eq!(hit_test(extreme, POINT { x: 0, y: 0 }, [8; 2]), HTCLIENT);
    }

    #[test]
    fn maximized_client_excludes_taskbar_on_any_monitor_edge() {
        let outer = RECT {
            left: -1616,
            top: -916,
            right: 16,
            bottom: 16,
        };
        for work in [
            RECT {
                left: -1600,
                top: -900,
                right: 0,
                bottom: -80,
            },
            RECT {
                left: -1600,
                top: -820,
                right: 0,
                bottom: 0,
            },
            RECT {
                left: -1520,
                top: -900,
                right: 0,
                bottom: 0,
            },
            RECT {
                left: -1600,
                top: -900,
                right: -80,
                bottom: 0,
            },
        ] {
            assert_eq!(maximized_client(outer, work), Some(work));
        }
        assert!(
            maximized_client(
                outer,
                RECT {
                    left: 100,
                    top: 100,
                    right: 200,
                    bottom: 200
                }
            )
            .is_none()
        );
    }
}
