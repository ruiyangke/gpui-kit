use ::windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, HWND, LPARAM, POINT, RECT, SetLastError, WPARAM},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromWindow,
        },
    },
    UI::{
        HiDpi::{
            AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            GetThreadDpiAwarenessContext, SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::IsWindowEnabled,
        WindowsAndMessaging::*,
    },
};
use anyhow::{Context as _, Result, anyhow, ensure};
use gpui::{
    App, AppContext, Application, AsyncApp, Bounds, Context, Decorations, IntoElement,
    NativeResizeConstraint, NativeWindowActivation, QuitMode, Render, Styled, TitlebarOptions,
    Window, WindowBounds, WindowDecorations, WindowHandle, WindowKind, WindowOptions, div, point,
    px, size,
};
use gpui_windows::WindowsPlatform;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

const SETTLE_LIMIT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const HELP: &str = "window_lifecycle --isolated-profile NEW_TEMP_DIRECTORY\nRequires GPUI_WINDOW_LIFECYCLE_FIXTURE=1 and an isolated interactive desktop.\nCreates only fixture windows and a fresh temporary guard directory; emits JSON.";

#[derive(Default)]
struct Report {
    observations: Vec<Value>,
    failures: Vec<String>,
    completed: bool,
    created: usize,
    destroyed: usize,
}

struct ProfileGuard(PathBuf);

impl ProfileGuard {
    fn create(path: PathBuf) -> Result<Self> {
        ensure!(
            std::env::var("GPUI_WINDOW_LIFECYCLE_FIXTURE").as_deref() == Ok("1"),
            "The isolated fixture environment guard is missing"
        );
        ensure!(
            path.is_absolute(),
            "The fixture profile must be an absolute path"
        );
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("The fixture profile needs a parent"))?
            .canonicalize()
            .context("Cannot resolve fixture profile parent")?;
        let temporary = std::env::temp_dir()
            .canonicalize()
            .context("Cannot resolve temporary directory")?;
        ensure!(
            parent.starts_with(&temporary),
            "The fixture profile must be inside the temporary directory"
        );
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("The fixture profile needs a name"))?;
        let path = parent.join(name);
        fs::create_dir(&path).context("Fixture profile must be a fresh directory")?;
        Ok(Self(path))
    }

    fn remove(self) -> Result<()> {
        // Never recurse: unexpected files must fail cleanup, not be deleted.
        fs::remove_dir(&self.0).context("Cannot remove owned fixture profile")
    }
}

struct DpiContext(DPI_AWARENESS_CONTEXT);

impl DpiContext {
    fn enter() -> Result<Self> {
        // SAFETY: this fixture owns the current UI thread and restores its context.
        let old =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        ensure!(!old.0.is_null(), "Cannot enter per-monitor-v2 DPI context");
        Ok(Self(old))
    }

    fn restore(self) -> Result<()> {
        // SAFETY: the saved context belongs to this thread.
        let previous = unsafe { SetThreadDpiAwarenessContext(self.0) };
        ensure!(
            !previous.0.is_null()
                && unsafe {
                    AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), self.0).as_bool()
                },
            "Cannot restore original DPI context"
        );
        Ok(())
    }
}

struct Probe;

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

struct OwnedWindow {
    name: &'static str,
    window: WindowHandle<Probe>,
    hwnd: HWND,
}

fn arguments(args: Vec<OsString>) -> Result<Option<PathBuf>> {
    if args.len() == 1 && args[0] == "--help" {
        return Ok(None);
    }
    ensure!(args.len() == 2 && args[0] == "--isolated-profile", "{HELP}");
    Ok(Some(PathBuf::from(&args[1])))
}

fn hwnd(window: &Window) -> Result<HWND> {
    match HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow!("Cannot get native window handle: {error}"))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as _)),
        _ => Err(anyhow!("Expected an owned Win32 window")),
    }
}

fn rect(rect: RECT) -> [i32; 4] {
    [rect.left, rect.top, rect.right, rect.bottom]
}

fn client_origin(window: HWND) -> Result<POINT> {
    let mut origin = POINT::default();
    // SAFETY: callers retain ownership of the live fixture HWND.
    ensure!(
        unsafe { ClientToScreen(window, &mut origin).as_bool() },
        "Cannot measure fixture client origin"
    );
    Ok(origin)
}

fn placement(window: HWND) -> Result<WINDOWPLACEMENT> {
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: callers retain ownership of the live fixture HWND.
    unsafe { GetWindowPlacement(window, &mut placement) }?;
    Ok(placement)
}

fn native_identity(window: HWND) -> Value {
    let mut pid = 0;
    let mut owner_pid = 0;
    // Read-only diagnostics, not authorization: a foreign HWND may disappear
    // between these queries. Zero IDs and the live flag preserve that ambiguity.
    let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    let owner = unsafe { GetWindow(window, GW_OWNER) }.unwrap_or_default();
    let owner_thread = unsafe { GetWindowThreadProcessId(owner, Some(&mut owner_pid)) };
    json!({
        "hwnd": window.0 as usize, "pid": pid, "thread": thread,
        "live": unsafe { IsWindow(Some(window)).as_bool() },
        "fixtureProcess": pid == std::process::id(),
        "owner": { "hwnd": owner.0 as usize, "pid": owner_pid, "thread": owner_thread },
    })
}

fn observe_fullscreen_focus(
    phase: &str,
    expected: HWND,
    owned: &[OwnedWindow],
    events: &RefCell<Vec<Value>>,
    report: &RefCell<Report>,
) -> HWND {
    let foreground = unsafe { GetForegroundWindow() };
    let identity = native_identity(foreground);
    let expected_identity = native_identity(expected);
    let observation = json!({
        "phase": phase, "foregroundIdentity": identity, "expectedIdentity": expected_identity,
        "foregroundStableDuringObservation": unsafe { GetForegroundWindow() } == foreground,
        "ownedWindows": owned.iter().map(|window| json!({
            "name": window.name, "hwnd": window.hwnd.0 as usize,
        })).collect::<Vec<_>>(),
        "activationEvents": *events.borrow(),
    });
    report.borrow_mut().observations.push(observation);
    foreground
}

fn snapshot(window: HWND, name: &str, phase: &str) -> Result<Value> {
    let mut outer = RECT::default();
    let mut client = RECT::default();
    let mut visible = RECT::default();
    let placement = placement(window)?;
    let origin = client_origin(window)?;
    // SAFETY: the fixture retains ownership of this live HWND on its UI thread.
    unsafe {
        GetWindowRect(window, &mut outer)?;
        GetClientRect(window, &mut client)?;
        let dwm = DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut visible as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        );
        Ok(json!({
            "window": name, "phase": phase, "hwnd": window.0 as usize,
            "shown": IsWindowVisible(window).as_bool(), "dpi": GetDpiForWindow(window),
            "style": GetWindowLongPtrW(window, GWL_STYLE) as u32,
            "extendedStyle": GetWindowLongPtrW(window, GWL_EXSTYLE) as u32,
            "foreground": GetForegroundWindow() == window,
            "outer": rect(outer), "client": rect(client), "dwmFrame": rect(visible),
            "clientOrigin": [origin.x, origin.y],
            "clientInsets": [origin.x - outer.left, origin.y - outer.top,
                outer.right - origin.x - client.right, outer.bottom - origin.y - client.bottom],
            "normalPlacement": rect(placement.rcNormalPosition),
            "dwmAvailable": dwm.is_ok(), "showCommand": placement.showCmd,
        }))
    }
}

fn open(
    cx: &mut App,
    name: &'static str,
    options: WindowOptions,
    report: &Rc<RefCell<Report>>,
) -> Result<OwnedWindow> {
    let expected_shown = options.show;
    let full_client = options.window_decorations == Some(WindowDecorations::Client);
    let native_popup_size = matches!(options.kind, WindowKind::PopUp | WindowKind::Dialog)
        .then(|| options.window_bounds.as_ref().unwrap().get_bounds().size);
    let expected_caption = !full_client
        && options
            .titlebar
            .as_ref()
            .is_some_and(|titlebar| !titlebar.appears_transparent);
    let report_at_creation = report.clone();
    let window = cx.open_window(options, move |window, cx| {
        let observation = (|| -> Result<()> {
            let handle = hwnd(window)?;
            let observation = snapshot(handle, name, "root-construction")?;
            report_at_creation
                .borrow_mut()
                .observations
                .push(observation);
            ensure!(
                unsafe { IsWindowVisible(handle).as_bool() } == expected_shown,
                "{name}: native show state differs during root construction"
            );
            // WS_CAPTION may remain set on a client-decorated overlapped
            // window. WM_NCCALCSIZE owns its actual nonclient top boundary.
            let mut outer = RECT::default();
            unsafe { GetWindowRect(handle, &mut outer) }?;
            if let Some(expected) = native_popup_size {
                let mut client = RECT::default();
                unsafe { GetClientRect(handle, &mut client) }?;
                let scale = unsafe { GetDpiForWindow(handle) } as f32 / 96.;
                ensure!(
                    (client.right as f32 / scale - f32::from(expected.width)).abs() <= 1.
                        && (client.bottom as f32 / scale - f32::from(expected.height)).abs() <= 1.,
                    "{name}: first show did not preserve the requested client extent"
                );
                ensure!(
                    unsafe { GetWindowLongPtrW(handle, GWL_STYLE) } as u32 & WS_POPUP.0 != 0,
                    "{name}: expected a native WS_POPUP window"
                );
            }
            let top_inset = client_origin(handle)?.y - outer.top;
            ensure!(
                if expected_caption { top_inset > 0 } else { top_inset == 0 },
                "{name}: native caption geometry differs during root construction (top inset {top_inset})"
            );
            if full_client {
                assert_full_client(handle)?;
            }
            Ok(())
        })();
        if let Err(error) = observation {
            report_at_creation
                .borrow_mut()
                .failures
                .push(error.to_string());
        }
        cx.new(|_| Probe)
    })?;
    let hwnd = window.update(cx, |_, window, _| hwnd(window))??;
    report.borrow_mut().created += 1;
    Ok(OwnedWindow { name, window, hwnd })
}

async fn until(cx: &AsyncApp, description: &str, mut ready: impl FnMut() -> bool) -> Result<()> {
    let start = Instant::now();
    while !ready() {
        ensure!(
            start.elapsed() < SETTLE_LIMIT,
            "Timed out waiting for {description}"
        );
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Ok(())
}

fn options(kind: WindowKind, caption: bool, show: bool, focus: bool) -> WindowOptions {
    WindowOptions {
        kind,
        show,
        focus,
        window_decorations: (!caption).then_some(WindowDecorations::Client),
        titlebar: caption.then(TitlebarOptions::default),
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(120.), px(100.)),
            size(px(420.), px(280.)),
        ))),
        ..Default::default()
    }
}

fn set_long(window: HWND, index: WINDOW_LONG_PTR_INDEX, value: isize) -> Result<()> {
    // SAFETY: only owned live fixture HWNDs reach this helper.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let old = SetWindowLongPtrW(window, index, value);
        if old == 0 && GetLastError() != ERROR_SUCCESS {
            return Err(::windows::core::Error::from_thread().into());
        }
    }
    Ok(())
}

fn outer_rect(window: HWND) -> Result<RECT> {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(window, &mut bounds) }?;
    Ok(bounds)
}

fn client_screen_rect(window: HWND) -> Result<RECT> {
    let origin = client_origin(window)?;
    let mut client = RECT::default();
    unsafe { GetClientRect(window, &mut client) }?;
    Ok(RECT {
        left: origin.x,
        top: origin.y,
        right: origin.x + client.right,
        bottom: origin.y + client.bottom,
    })
}

fn assert_full_client(window: HWND) -> Result<()> {
    ensure!(
        client_screen_rect(window)? == outer_rect(window)?,
        "Client decorations retained native frame insets"
    );
    Ok(())
}

fn assert_resize_edges(window: HWND, enabled: bool) -> Result<()> {
    let r = outer_rect(window)?;
    let x = r.left + (r.right - r.left) / 2;
    let y = r.top + (r.bottom - r.top) / 2;
    for (x, y, expected) in [
        (r.left, r.top, HTTOPLEFT),
        (x, r.top, HTTOP),
        (r.right - 1, r.top, HTTOPRIGHT),
        (r.left, y, HTLEFT),
        (r.right - 1, y, HTRIGHT),
        (r.left, r.bottom - 1, HTBOTTOMLEFT),
        (x, r.bottom - 1, HTBOTTOM),
        (r.right - 1, r.bottom - 1, HTBOTTOMRIGHT),
        (x, y, HTCLIENT),
    ] {
        ensure!(
            i16::try_from(x).is_ok() && i16::try_from(y).is_ok(),
            "Fixture hit point exceeds message coordinates"
        );
        let hit = unsafe {
            SendMessageW(
                window,
                WM_NCHITTEST,
                None,
                Some(LPARAM(((y as u16 as u32) << 16 | x as u16 as u32) as isize)),
            )
        };
        ensure!(
            hit.0
                == if enabled {
                    expected as isize
                } else {
                    HTCLIENT as isize
                },
            "Client resize hit differs at ({x},{y}): {}",
            hit.0
        );
    }
    Ok(())
}

fn assert_maximized_client(window: HWND) -> Result<()> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    unsafe { GetMonitorInfoW(monitor, &mut info) }.ok()?;
    let client = client_screen_rect(window)?;
    let outer = outer_rect(window)?;
    ensure!(
        client == info.rcWork,
        "Maximized Client content overlaps or misses work area: client={:?}, work={:?}, outer={:?}, dpi={}",
        rect(client),
        rect(info.rcWork),
        rect(outer),
        unsafe { GetDpiForWindow(window) },
    );
    assert_resize_edges(window, false)
}

async fn exercise_decoration_changes(
    cx: &mut AsyncApp,
    target: &OwnedWindow,
    hidden: &OwnedWindow,
    anchor: HWND,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    let before = outer_rect(target.hwnd)?;
    let style = unsafe { GetWindowLongPtrW(target.hwnd, GWL_STYLE) };
    target.window.update(cx, |_, window, _| {
        window.request_decorations(WindowDecorations::Server)
    })?;
    until(cx, "server frame restored on explicit request", || {
        client_screen_rect(target.hwnd)
            .ok()
            .zip(outer_rect(target.hwnd).ok())
            .is_some_and(|(client, outer)| client != outer)
    })
    .await?;
    target.window.update(cx, |_, window, _| {
        window.request_decorations(WindowDecorations::Client)
    })?;
    until(cx, "client frame restored on explicit request", || {
        assert_full_client(target.hwnd).is_ok()
    })
    .await?;
    ensure!(
        outer_rect(target.hwnd)? == before
            && unsafe { GetWindowLongPtrW(target.hwnd, GWL_STYLE) } == style,
        "Runtime decoration change altered outer bounds or native action styles"
    );
    assert_resize_edges(target.hwnd, true)?;

    let normal = target
        .window
        .update(cx, |_, window, _| window.window_bounds().get_bounds())?;
    target
        .window
        .update(cx, |_, window, _| window.set_maximized(true))?;
    until(cx, "client maximize before fullscreen", || unsafe {
        IsZoomed(target.hwnd).as_bool()
    })
    .await?;
    assert_maximized_client(target.hwnd)?;
    ensure!(
        target
            .window
            .update(cx, |_, window, _| window.window_bounds().get_bounds())?
            == normal,
        "Maximized client changed logical normal restore bounds"
    );
    target
        .window
        .update(cx, |_, window, _| window.toggle_fullscreen())?;
    until(cx, "client fullscreen entry", || {
        cx.update(|cx| {
            target
                .window
                .update(cx, |_, window, _| window.is_fullscreen())
                .unwrap_or(false)
        })
    })
    .await?;
    assert_full_client(target.hwnd)?;
    assert_resize_edges(target.hwnd, false)?;
    ensure!(
        target
            .window
            .update(cx, |_, window, _| window.window_bounds().get_bounds())?
            == normal,
        "Fullscreen from maximized Client stored incorrect logical restore bounds"
    );
    for decorations in [WindowDecorations::Server, WindowDecorations::Client] {
        target
            .window
            .update(cx, |_, window, _| window.request_decorations(decorations))?;
        until(cx, "fullscreen decoration policy update", || {
            cx.update(|cx| {
                target
                    .window
                    .update(cx, |_, window, _| {
                        matches!(
                            (window.window_decorations(), decorations),
                            (Decorations::Server, WindowDecorations::Server)
                                | (Decorations::Client { .. }, WindowDecorations::Client)
                        )
                    })
                    .unwrap_or(false)
            })
        })
        .await?;
        assert_full_client(target.hwnd)?;
    }
    target
        .window
        .update(cx, |_, window, _| window.toggle_fullscreen())?;
    until(cx, "client fullscreen restore", || {
        cx.update(|cx| {
            target
                .window
                .update(cx, |_, window, _| !window.is_fullscreen())
                .unwrap_or(false)
        })
    })
    .await?;
    ensure!(
        unsafe { IsZoomed(target.hwnd).as_bool() },
        "Fullscreen lost native maximized restore state"
    );
    ensure!(
        target
            .window
            .update(cx, |_, window, _| window.window_bounds().get_bounds())?
            == normal,
        "Fullscreen exit changed logical normal restore bounds"
    );
    target
        .window
        .update(cx, |_, window, _| window.set_maximized(false))?;
    until(cx, "client normal restore after fullscreen", || !unsafe {
        IsZoomed(target.hwnd).as_bool()
    })
    .await?;
    assert_full_client(target.hwnd)?;
    ensure!(
        outer_rect(target.hwnd)? == before,
        "Client fullscreen changed restore bounds"
    );
    report.borrow_mut().observations.push(snapshot(
        target.hwnd,
        target.name,
        "client-runtime-fullscreen-restored",
    )?);

    // The earlier hidden-zoom test has finished. This now exercises the same
    // pending first placement after a deferred frame-policy change.
    hidden.window.update(cx, |_, window, _| {
        window.request_decorations(WindowDecorations::Client)
    })?;
    until(cx, "hidden client frame calculation", || {
        assert_full_client(hidden.hwnd).is_ok()
    })
    .await?;
    ensure!(
        !unsafe { IsWindowVisible(hidden.hwnd).as_bool() },
        "Decoration request showed hidden window"
    );
    hidden
        .window
        .update(cx, |_, window, _| window.activate_window())?;
    until(cx, "hidden client first show", || unsafe {
        IsWindowVisible(hidden.hwnd).as_bool()
    })
    .await?;
    assert_full_client(hidden.hwnd)?;
    let client = client_screen_rect(hidden.hwnd)?;
    let scale = unsafe { GetDpiForWindow(hidden.hwnd) } as f64 / 96.;
    ensure!(
        ((client.right - client.left) as f64 / scale - 420.).abs() <= 1.
            && ((client.bottom - client.top) as f64 / scale - 280.).abs() <= 1.,
        "Hidden decoration change lost pending client extent"
    );
    report.borrow_mut().observations.push(snapshot(
        hidden.hwnd,
        hidden.name,
        "hidden-client-first-show",
    )?);
    ensure!(
        unsafe { SetForegroundWindow(anchor).as_bool() },
        "Cannot restore owned anchor after client-frame checks"
    );
    until(
        cx,
        "anchor after client-frame checks",
        || unsafe { GetForegroundWindow() } == anchor,
    )
    .await?;
    Ok(())
}

fn sizing_message(window: HWND, edge: u32, mut bounds: RECT) -> Result<RECT> {
    let result = unsafe {
        SendMessageW(
            window,
            WM_SIZING,
            Some(WPARAM(edge as usize)),
            Some(LPARAM((&mut bounds as *mut RECT) as isize)),
        )
    };
    ensure!(
        result.0 == 1,
        "Native sizing policy did not handle its owned rectangle"
    );
    Ok(bounds)
}

fn tracking_limits(window: HWND) -> Result<MINMAXINFO> {
    let mut limits = MINMAXINFO {
        ptMaxSize: POINT { x: 3333, y: 2222 },
        ptMaxPosition: POINT { x: 101, y: -202 },
        ..Default::default()
    };
    let result = unsafe {
        SendMessageW(
            window,
            WM_GETMINMAXINFO,
            None,
            Some(LPARAM((&mut limits as *mut MINMAXINFO) as isize)),
        )
    };
    ensure!(
        result.0 == 0,
        "Native tracking policy returned an invalid result"
    );
    ensure!(
        limits.ptMaxSize == POINT { x: 3333, y: 2222 }
            && limits.ptMaxPosition == POINT { x: 101, y: -202 },
        "Tracking policy overwrote OS-owned maximize/Snap bounds"
    );
    Ok(limits)
}

fn grid_policy() -> NativeResizeConstraint {
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

async fn exercise_fixed_content_size(
    cx: &mut AsyncApp,
    target: &OwnedWindow,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    let foreground = unsafe { GetForegroundWindow() };
    let small = if target.name == "popup" {
        [96., 48.]
    } else {
        [240., 160.]
    };
    for [width, height] in [small, [280., 190.], [420., 280.]] {
        target.window.update(cx, |_, window, _| {
            window.resize(size(px(width), px(height)))
        })?;
        let settled = until(cx, "fixed-window programmatic content resize", || {
            let mut client = RECT::default();
            if unsafe { GetClientRect(target.hwnd, &mut client) }.is_err() {
                return false;
            }
            let scale = unsafe { GetDpiForWindow(target.hwnd) } as f32 / 96.;
            (client.right as f32 / scale - width).abs() <= 1.
                && (client.bottom as f32 / scale - height).abs() <= 1.
        })
        .await;
        let native = snapshot(target.hwnd, target.name, "fixed-content-size-observation")
            .unwrap_or_else(|error| json!({ "measurementError": error.to_string() }));
        let logical = target
            .window
            .update(cx, |_, window, _| {
                let viewport = window.viewport_size();
                json!({
                    "viewport": [f32::from(viewport.width), f32::from(viewport.height)],
                    "scaleFactor": window.scale_factor(),
                })
            })
            .unwrap_or_else(|error| json!({ "measurementError": error.to_string() }));
        report.borrow_mut().observations.push(json!({
            "window": target.name, "phase": "fixed-content-size-result",
            "requestedLogical": [width, height], "settled": settled.is_ok(),
            "native": native, "gpui": logical,
        }));
        settled.with_context(|| format!(
            "{} requested client size {width}×{height}; measured native {native}; GPUI {logical}",
            target.name,
        ))?;
        let current = outer_rect(target.hwnd)?;
        let mut proposed = current;
        proposed.right += 100;
        proposed.bottom += 50;
        ensure!(
            rect(sizing_message(target.hwnd, WMSZ_BOTTOMRIGHT, proposed)?) == rect(current),
            "Fixed window allowed a user sizing request after programmatic resize"
        );
        ensure!(
            unsafe { GetWindowLongPtrW(target.hwnd, GWL_STYLE) } as u32 & WS_THICKFRAME.0 == 0,
            "Programmatic resize enabled fixed-window sizing handles"
        );
        ensure!(
            unsafe { GetForegroundWindow() } == foreground,
            "Programmatic resize changed foreground ownership"
        );
    }
    report.borrow_mut().observations.push(snapshot(
        target.hwnd,
        target.name,
        "fixed-user-size-programmatic-content-updates",
    )?);
    Ok(())
}

async fn exercise_constraints(
    cx: &mut AsyncApp,
    target: &OwnedWindow,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    let original = outer_rect(target.hwnd)?;
    let mut client = RECT::default();
    unsafe { GetClientRect(target.hwnd, &mut client) }?;
    let chrome = [
        original.right - original.left - client.right,
        original.bottom - original.top - client.bottom,
    ];
    let scale = unsafe { GetDpiForWindow(target.hwnd) } as f64 / 96.;
    let physical = |logical: f64, axis: usize| (logical * scale).round() as i32 + chrome[axis];
    let proposed = RECT {
        left: -900,
        top: -700,
        right: -900 + physical(333., 0),
        bottom: -700 + physical(243., 1),
    };
    target.window.update(cx, |_, window, _| {
        window.set_native_resize_constraint(Some(grid_policy()))
    })?;
    let limits = tracking_limits(target.hwnd)?;
    ensure!(
        limits.ptMinTrackSize
            == POINT {
                x: physical(100., 0),
                y: physical(80., 1)
            }
            && limits.ptMaxTrackSize
                == POINT {
                    x: physical(500., 0),
                    y: physical(400., 1)
                },
        "Native track limits lost current DPI or client chrome"
    );
    for edge in 1..=8 {
        let adjusted = sizing_message(target.hwnd, edge, proposed)?;
        ensure!(
            adjusted.right - adjusted.left == physical(340., 0)
                && adjusted.bottom - adjusted.top == physical(240., 1),
            "Native sizing did not use the logical increment grid"
        );
        ensure!(
            if matches!(edge, 1 | 4 | 7) {
                adjusted.right == proposed.right
            } else {
                adjusted.left == proposed.left
            },
            "Native sizing moved the opposite horizontal edge"
        );
        ensure!(
            if matches!(edge, 3 | 4 | 5) {
                adjusted.bottom == proposed.bottom
            } else {
                adjusted.top == proposed.top
            },
            "Native sizing moved the opposite vertical edge"
        );
    }

    target.window.update(cx, |_, window, _| {
        window.set_native_resize_constraint(Some(NativeResizeConstraint::Constrained {
            minimum: size(px(240.), px(160.)),
            maximum: size(px(240.), px(160.)),
            select: Rc::new(|_| Some(size(px(240.), px(160.)))),
        }))
    })?;
    let limits = tracking_limits(target.hwnd)?;
    ensure!(
        limits.ptMinTrackSize == limits.ptMaxTrackSize,
        "Fixed app dimensions did not produce fixed tracking bounds"
    );
    let adjusted = sizing_message(target.hwnd, WMSZ_BOTTOMRIGHT, proposed)?;
    ensure!(
        adjusted.right - adjusted.left == physical(240., 0)
            && adjusted.bottom - adjusted.top == physical(160., 1),
        "Fixed app dimensions were ignored"
    );

    let called = Rc::new(Cell::new(false));
    let called_by_selector = called.clone();
    target.window.update(cx, |_, window, _| {
        window.set_native_resize_constraint(Some(NativeResizeConstraint::Constrained {
            minimum: size(px(500.), px(300.)),
            maximum: size(px(100.), px(100.)),
            select: Rc::new(move |_| {
                called_by_selector.set(true);
                Some(size(px(200.), px(200.)))
            }),
        }))
    })?;
    let limits = tracking_limits(target.hwnd)?;
    ensure!(
        limits.ptMinTrackSize == limits.ptMaxTrackSize,
        "Conflicting hints did not freeze tracking"
    );
    ensure!(
        rect(sizing_message(target.hwnd, WMSZ_TOPLEFT, proposed)?) == rect(original)
            && !called.get(),
        "Conflicting hints permitted native resizing"
    );

    target.window.update(cx, |_, window, _| {
        window.set_native_resize_constraint(Some(grid_policy()));
        window.set_resizable(false);
    })?;
    until(cx, "disabled native resize capability", || unsafe {
        GetWindowLongPtrW(target.hwnd, GWL_STYLE) as u32 & WS_THICKFRAME.0 == 0
    })
    .await?;
    let disabled = outer_rect(target.hwnd)?;
    ensure!(
        rect(sizing_message(target.hwnd, WMSZ_RIGHT, proposed)?) == rect(disabled),
        "Disabled resize capability still accepted a sizing request"
    );
    target
        .window
        .update(cx, |_, window, _| window.set_resizable(true))?;
    until(cx, "restored native resize capability", || unsafe {
        GetWindowLongPtrW(target.hwnd, GWL_STYLE) as u32 & WS_THICKFRAME.0 != 0
    })
    .await?;
    let guard = Rc::new(());
    let released = Rc::downgrade(&guard);
    target.window.update(cx, |_, window, _| {
        window.set_native_resize_constraint(Some(NativeResizeConstraint::Constrained {
            minimum: size(px(100.), px(80.)),
            maximum: size(px(500.), px(400.)),
            select: Rc::new(move |value| {
                let _ = &guard;
                Some(value)
            }),
        }))
    })?;
    ensure!(
        released.upgrade().is_some(),
        "Native policy did not retain its immutable snapshot"
    );
    target.window.update(cx, |_, window, _| {
        window.set_native_resize_constraint(Some(grid_policy()))
    })?;
    ensure!(
        released.upgrade().is_none(),
        "Replacing native policy retained the previous selector"
    );
    ensure!(
        rect(outer_rect(target.hwnd)?) == rect(original),
        "Policy updates moved or resized the native window"
    );
    report.borrow_mut().observations.push(snapshot(
        target.hwnd,
        target.name,
        "native-resize-constraints",
    )?);
    // Later lifecycle checks maximize to the monitor work area. They must not
    // inherit this scenario's 500×400 logical tracking limit.
    target
        .window
        .update(cx, |_, window, _| window.set_native_resize_constraint(None))?;
    Ok(())
}

async fn exercise_activation(
    cx: &mut AsyncApp,
    source: &OwnedWindow,
    destination: &OwnedWindow,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    let admitted = Rc::new(Cell::new(true));
    let order = Rc::new(RefCell::new(Vec::new()));
    let related = Rc::new(Cell::new(None));
    let retired = Rc::new(Cell::new(0));
    let observer = source.window.update(cx, |_, window, cx| {
        let admitted = admitted.clone();
        let native_order = order.clone();
        let related = related.clone();
        let retired = retired.clone();
        window.set_native_activation_handler(Some(Rc::new(move |event| match event {
            NativeWindowActivation::Deactivated { next_window } => {
                admitted.set(false);
                related.set(next_window);
                native_order.borrow_mut().push("native-blur");
            }
            NativeWindowActivation::Retired => retired.set(retired.get() + 1),
            NativeWindowActivation::Activated => (),
        })));
        let deferred_order = order.clone();
        cx.observe_window_activation(window, move |_: &mut Probe, window, _| {
            if !window.is_window_active() {
                deferred_order.borrow_mut().push("deferred-blur");
            }
        })
    })?;
    // Both HWNDs belong to this fixture and share the UI thread. Native blur
    // must revoke admission before its deferred App-borrowing observer runs.
    ensure!(
        unsafe { SetForegroundWindow(destination.hwnd).as_bool() },
        "Cannot activate owned focus destination; possible desktop collision"
    );
    ensure!(
        !admitted.get(),
        "Native blur left publication admission open"
    );
    ensure!(
        related.get() == Some(destination.hwnd.0 as isize),
        "Native focus fence lost the related owned HWND"
    );
    until(cx, "deferred activation observer", || {
        order.borrow().contains(&"deferred-blur")
    })
    .await?;
    ensure!(
        *order.borrow() == ["native-blur", "deferred-blur"],
        "Native focus fence did not precede the deferred observer"
    );
    source.window.update(cx, |_, window, _| {
        window.set_native_activation_handler(None)
    })?;
    ensure!(
        retired.get() == 1,
        "Replaced native focus registration was not retired once"
    );
    drop(observer);
    ensure!(
        unsafe { SetForegroundWindow(source.hwnd).as_bool() },
        "Cannot restore owned focus source; possible desktop collision"
    );
    until(cx, "focus source restoration", || unsafe {
        GetForegroundWindow() == source.hwnd
    })
    .await?;
    report.borrow_mut().observations.push(json!({
        "phase": "native-activation-fence", "order": *order.borrow(),
        "admissionRevokedBeforeDeferredObserver": !admitted.get(),
        "relatedOwnedWindow": true, "replacedRegistrationRetired": retired.get(),
    }));
    Ok(())
}

async fn exercise(
    cx: &mut AsyncApp,
    owned: &mut Vec<OwnedWindow>,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    let anchor = cx.update(|cx| {
        open(
            cx,
            "caption",
            options(WindowKind::Normal, true, true, true),
            report,
        )
    })?;
    let anchor_hwnd = anchor.hwnd;
    owned.push(anchor);
    until(cx, "requested initial activation", || unsafe {
        GetForegroundWindow() == anchor_hwnd
    })
    .await?;

    for (name, caption, shown) in [("hidden", true, false), ("client-decorated", false, true)] {
        let window = cx.update(|cx| {
            open(
                cx,
                name,
                options(WindowKind::Normal, caption, shown, false),
                report,
            )
        })?;
        owned.push(window);
        cx.background_executor().timer(POLL_INTERVAL).await;
        ensure!(
            unsafe { GetForegroundWindow() } == anchor_hwnd,
            "{name}: nonactivating construction changed foreground"
        );
    }

    exercise_activation(cx, &owned[0], &owned[2], report).await?;

    let dialog = cx.update(|cx| {
        open(
            cx,
            "dialog",
            WindowOptions {
                is_resizable: false,
                ..options(WindowKind::Dialog, true, true, true)
            },
            report,
        )
    })?;
    let dialog_hwnd = dialog.hwnd;
    owned.push(dialog);
    until(cx, "dialog activation", || unsafe {
        GetForegroundWindow() == dialog_hwnd
    })
    .await?;
    ensure!(
        unsafe { GetWindow(dialog_hwnd, GW_OWNER) }? == anchor_hwnd,
        "Dialog owner differs from active fixture window"
    );
    ensure!(
        !unsafe { IsWindowEnabled(anchor_hwnd).as_bool() },
        "Dialog did not disable its fixture owner"
    );
    exercise_fixed_content_size(cx, owned.last().unwrap(), report).await?;
    owned
        .last()
        .unwrap()
        .window
        .update(cx, |_, window, _| window.remove_window())?;
    until(cx, "dialog destruction", || !unsafe {
        IsWindow(Some(dialog_hwnd)).as_bool()
    })
    .await?;
    ensure!(
        unsafe { IsWindowEnabled(anchor_hwnd).as_bool() },
        "Dialog owner remained disabled after close"
    );
    // No synthetic input or foreign-window activation is used.
    ensure!(
        unsafe { SetForegroundWindow(anchor_hwnd).as_bool() },
        "Cannot reactivate fixture anchor; possible desktop collision"
    );
    until(cx, "fixture anchor activation", || unsafe {
        GetForegroundWindow() == anchor_hwnd
    })
    .await?;

    let popup = cx.update(|cx| {
        let display = cx
            .primary_display()
            .context("Missing popup fixture display")?;
        let area = display.visible_bounds();
        open(
            cx,
            "popup",
            WindowOptions {
                is_resizable: false,
                display_id: Some(display.id()),
                // Deliberately outside the selected monitor. First-show
                // placement may move this origin but must retain 136x64.
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(area.right() + px(400.), area.bottom() + px(300.)),
                    size(px(136.), px(64.)),
                ))),
                ..options(WindowKind::PopUp, false, true, false)
            },
            report,
        )
    })?;
    let popup_hwnd = popup.hwnd;
    owned.push(popup);
    ensure!(
        unsafe { GetForegroundWindow() } == anchor_hwnd,
        "Popup constructor changed foreground"
    );
    // Reproduce the remote presenter's owned-popup policy after the real GPUI constructor.
    set_long(popup_hwnd, GWLP_HWNDPARENT, anchor_hwnd.0 as isize)?;
    let style = unsafe { GetWindowLongPtrW(popup_hwnd, GWL_EXSTYLE) } as u32;
    set_long(
        popup_hwnd,
        GWL_EXSTYLE,
        ((style | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0) as isize,
    )?;
    unsafe {
        SetWindowPos(
            popup_hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
        )
    }?;
    let activation = unsafe {
        SendMessageW(
            popup_hwnd,
            WM_MOUSEACTIVATE,
            Some(WPARAM(anchor_hwnd.0 as usize)),
            Some(LPARAM((HTCLIENT | (WM_LBUTTONDOWN << 16)) as isize)),
        )
    };
    ensure!(
        activation.0 == MA_NOACTIVATE as isize,
        "Owned popup would activate on a client click"
    );
    ensure!(
        unsafe { GetForegroundWindow() } == anchor_hwnd,
        "Popup policy changed foreground"
    );
    ensure!(
        unsafe { GetWindowLongPtrW(popup_hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 == 0,
        "Owned popup remained topmost"
    );
    report.borrow_mut().observations.push(snapshot(
        popup_hwnd,
        "popup",
        "owned-nonactivating-policy",
    )?);
    exercise_fixed_content_size(cx, owned.last().unwrap(), report).await?;

    for index in [0, 2] {
        let target = &owned[index];
        for (resize, minimize) in [(false, false), (true, false), (false, true), (true, true)] {
            target.window.update(cx, |_, window, _| {
                window.set_resizable(resize);
                window.set_minimizable(minimize);
                ensure!(
                    window.is_resizable() == resize && window.is_minimizable() == minimize,
                    "GPUI capability caches differ"
                );
                Ok::<_, anyhow::Error>(())
            })??;
            until(cx, "deferred native capabilities", || {
                let style = unsafe { GetWindowLongPtrW(target.hwnd, GWL_STYLE) } as u32;
                (style & WS_THICKFRAME.0 != 0) == resize
                    && (style & WS_MAXIMIZEBOX.0 != 0) == resize
                    && (style & WS_MINIMIZEBOX.0 != 0) == minimize
            })
            .await?;
            report.borrow_mut().observations.push(snapshot(
                target.hwnd,
                target.name,
                "dynamic-capabilities",
            )?);
            if index == 2 {
                assert_full_client(target.hwnd)?;
                assert_resize_edges(target.hwnd, resize)?;
            }
            ensure!(
                unsafe { GetForegroundWindow() } == anchor_hwnd,
                "Capability changes moved foreground"
            );
        }
    }

    for index in [0, 2] {
        let target = &owned[index];
        for (resize, maximize) in [(true, false), (false, true), (true, true)] {
            target.window.update(cx, |_, window, _| {
                window.set_resizable(resize);
                window.set_maximizable(maximize);
                ensure!(
                    window.is_maximizable() == maximize,
                    "Independent maximize cache differs"
                );
                Ok::<_, anyhow::Error>(())
            })??;
            until(cx, "independent maximize style", || {
                let style = unsafe { GetWindowLongPtrW(target.hwnd, GWL_STYLE) } as u32;
                (style & WS_THICKFRAME.0 != 0) == resize
                    && (style & WS_MAXIMIZEBOX.0 != 0) == maximize
            })
            .await?;
            if !maximize {
                unsafe {
                    SendMessageW(
                        target.hwnd,
                        WM_SYSCOMMAND,
                        Some(WPARAM(SC_MAXIMIZE as usize)),
                        None,
                    )
                };
                ensure!(
                    !unsafe { IsZoomed(target.hwnd).as_bool() },
                    "Disabled maximize accepted a native command"
                );
            }
        }
        report.borrow_mut().observations.push(snapshot(
            target.hwnd,
            target.name,
            "independent-maximize-permission",
        )?);
        exercise_constraints(cx, target, report).await?;
        let original = placement(target.hwnd)?;
        let logical_normal = target
            .window
            .update(cx, |_, window, _| window.window_bounds().get_bounds())?;
        target.window.update(cx, |_, window, _| {
            window.set_maximized(true);
            window.set_maximized(false);
        })?;
        cx.background_executor().timer(POLL_INTERVAL).await;
        ensure!(
            !unsafe { IsZoomed(target.hwnd).as_bool() },
            "Superseded absolute maximize still applied"
        );
        ensure!(
            rect(placement(target.hwnd)?.rcNormalPosition) == rect(original.rcNormalPosition),
            "Superseded maximize changed normal placement"
        );
        target.window.update(cx, |_, window, _| {
            window.set_maximized(true);
            window.set_maximized(true);
        })?;
        until(cx, "native maximize", || unsafe {
            IsZoomed(target.hwnd).as_bool()
        })
        .await?;
        until(cx, "zoom activation", || unsafe {
            GetForegroundWindow() == target.hwnd
        })
        .await?;
        if index == 2 {
            assert_maximized_client(target.hwnd)?;
            ensure!(
                target
                    .window
                    .update(cx, |_, window, _| window.window_bounds().get_bounds())?
                    == logical_normal,
                "Maximized Client geometry used current frame insets for normal bounds"
            );
        }
        report.borrow_mut().observations.push(snapshot(
            target.hwnd,
            target.name,
            "native-maximized",
        )?);
        let other = if index == 0 {
            owned[2].hwnd
        } else {
            anchor_hwnd
        };
        ensure!(
            unsafe { SetForegroundWindow(other).as_bool() },
            "Cannot focus owned restore observer"
        );
        until(cx, "owned restore observer activation", || unsafe {
            GetForegroundWindow() == other
        })
        .await?;
        target.window.update(cx, |_, window, _| {
            window.set_maximized(false);
            window.set_maximized(false);
        })?;
        until(cx, "native zoom restore", || unsafe {
            !IsZoomed(target.hwnd).as_bool() && !IsIconic(target.hwnd).as_bool()
        })
        .await?;
        ensure!(
            unsafe { GetForegroundWindow() } == other,
            "Absolute restore activated its window"
        );
        // WINDOWPLACEMENT uses workspace coordinates. Compare its native
        // restore rectangle directly, never apply it as screen coordinates.
        ensure!(
            rect(placement(target.hwnd)?.rcNormalPosition) == rect(original.rcNormalPosition),
            "{}: zoom changed the native restore rectangle",
            target.name
        );
        if index == 2 {
            assert_full_client(target.hwnd)?;
            assert_resize_edges(target.hwnd, true)?;
        }
        report.borrow_mut().observations.push(snapshot(
            target.hwnd,
            target.name,
            "native-zoom-restored",
        )?);
        target
            .window
            .update(cx, |_, window, _| window.minimize_window())?;
        until(cx, "owned minimize", || unsafe {
            IsIconic(target.hwnd).as_bool()
        })
        .await?;
        target.window.update(cx, |_, window, _| {
            window.set_maximized(true);
            window.set_maximized(false);
        })?;
        cx.background_executor().timer(POLL_INTERVAL).await;
        ensure!(
            unsafe { IsIconic(target.hwnd).as_bool() },
            "Absolute maximize changed minimized visibility"
        );
        unsafe { ShowWindowAsync(target.hwnd, SW_SHOWNOACTIVATE) }.ok()?;
        until(cx, "owned deiconify", || unsafe {
            !IsIconic(target.hwnd).as_bool()
        })
        .await?;
        ensure!(
            rect(placement(target.hwnd)?.rcNormalPosition) == rect(original.rcNormalPosition),
            "Minimized requests changed restore geometry"
        );
        report.borrow_mut().observations.push(snapshot(
            target.hwnd,
            target.name,
            "minimized-state-defers-maximize",
        )?);
        target
            .window
            .update(cx, |_, window, _| window.set_native_resize_constraint(None))?;
    }
    ensure!(
        unsafe { SetForegroundWindow(anchor_hwnd).as_bool() },
        "Cannot reactivate fixture anchor after zoom; possible desktop collision"
    );
    until(cx, "anchor activation after zoom", || unsafe {
        GetForegroundWindow() == anchor_hwnd
    })
    .await?;

    let hidden = &owned[1];
    hidden.window.update(cx, |_, window, _| {
        window.zoom_window();
        window.zoom_window();
        window.set_maximized(true);
        window.set_maximized(false);
    })?;
    ensure!(
        !unsafe { IsWindowVisible(hidden.hwnd).as_bool() },
        "Queued hidden zoom unexpectedly showed its window"
    );
    report.borrow_mut().observations.push(snapshot(
        hidden.hwnd,
        hidden.name,
        "hidden-zoom-remains-hidden",
    )?);

    let anchor = &owned[0];
    let focus_phase = Rc::new(Cell::new("fullscreen-entry"));
    let focus_events = Rc::new(RefCell::new(Vec::new()));
    let started = Instant::now();
    anchor.window.update(cx, |_, window, _| {
        let phase = focus_phase.clone();
        let events = focus_events.clone();
        window.set_native_activation_handler(Some(Rc::new(move |event| {
            if events.borrow().len() > 64 {
                return;
            }
            if events.borrow().len() == 64 {
                events.borrow_mut().push(json!({ "truncated": true }));
                return;
            }
            let (kind, next) = match event {
                NativeWindowActivation::Activated => ("activated", None),
                NativeWindowActivation::Deactivated { next_window } => ("deactivated", next_window),
                NativeWindowActivation::Retired => ("retired", None),
            };
            let foreground = unsafe { GetForegroundWindow() };
            let observation = json!({
                "event": kind, "phase": phase.get(), "elapsedMs": started.elapsed().as_millis(),
                "foreground": native_identity(foreground),
                "nextWindow": next.map(|raw| native_identity(HWND(raw as _))),
            });
            events.borrow_mut().push(observation);
        })));
    })?;
    ensure!(
        observe_fullscreen_focus(
            "before-fullscreen-entry",
            anchor_hwnd,
            owned,
            &focus_events,
            report
        ) == anchor_hwnd,
        "Foreground changed before fullscreen entry; inspect focus identities"
    );
    anchor
        .window
        .update(cx, |_, window, _| window.toggle_fullscreen())?;
    let entered = until(cx, "fullscreen entry", || {
        cx.update(|cx| {
            anchor
                .window
                .update(cx, |_, window, _| window.is_fullscreen())
                .unwrap_or(false)
        })
    })
    .await;
    let foreground = observe_fullscreen_focus(
        "after-fullscreen-entry",
        anchor_hwnd,
        owned,
        &focus_events,
        report,
    );
    entered?;
    ensure!(
        foreground == anchor_hwnd,
        "Foreground changed during fullscreen entry; inspect focus identities"
    );
    focus_phase.set("fullscreen-zoom-requests");
    anchor.window.update(cx, |_, window, _| {
        window.zoom_window();
        window.set_maximized(true);
        window.set_maximized(false);
    })?;
    cx.background_executor().timer(POLL_INTERVAL).await;
    let foreground = observe_fullscreen_focus(
        "after-fullscreen-zoom-requests",
        anchor_hwnd,
        owned,
        &focus_events,
        report,
    );
    ensure!(
        anchor
            .window
            .update(cx, |_, window, _| window.is_fullscreen())?,
        "Zoom changed an active fullscreen window"
    );
    ensure!(
        foreground == anchor_hwnd,
        "Foreground changed during fullscreen zoom requests; inspect focus identities"
    );
    focus_phase.set("fullscreen-capability-restore");
    observe_fullscreen_focus(
        "before-fullscreen-capability-restore",
        anchor_hwnd,
        owned,
        &focus_events,
        report,
    );
    anchor.window.update(cx, |_, window, _| {
        window.set_resizable(false);
        window.set_minimizable(false);
        window.set_maximizable(false);
        window.toggle_fullscreen();
    })?;
    let restored = until(
        cx,
        "fullscreen exit with updated native capabilities",
        || {
            let fullscreen = cx.update(|cx| {
                anchor
                    .window
                    .update(cx, |_, window, _| window.is_fullscreen())
                    .unwrap_or(true)
            });
            let style = unsafe { GetWindowLongPtrW(anchor.hwnd, GWL_STYLE) } as u32;
            !fullscreen
                && style & (WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 == 0
                && style & WS_CAPTION.0 == WS_CAPTION.0
        },
    )
    .await;
    let foreground = observe_fullscreen_focus(
        "after-fullscreen-capability-restore",
        anchor_hwnd,
        owned,
        &focus_events,
        report,
    );
    restored?;
    ensure!(
        foreground == anchor_hwnd,
        "Fullscreen capability update moved foreground"
    );
    anchor.window.update(cx, |_, window, _| {
        window.set_native_activation_handler(None)
    })?;
    report.borrow_mut().observations.push(snapshot(
        anchor.hwnd,
        anchor.name,
        "fullscreen-restored-capabilities",
    )?);

    exercise_decoration_changes(cx, &owned[2], &owned[1], anchor_hwnd, report).await?;

    let closing = cx.update(|cx| {
        open(
            cx,
            "closing",
            options(WindowKind::Normal, true, false, false),
            report,
        )
    })?;
    let closing_guard = Rc::new(());
    let closing_released = Rc::downgrade(&closing_guard);
    let focus_guard = Rc::new(());
    let focus_released = Rc::downgrade(&focus_guard);
    let focus_retired = Rc::new(Cell::new(0));
    let retired_by_close = focus_retired.clone();
    closing.window.update(cx, |_, window, _| {
        window.set_native_activation_handler(Some(Rc::new(move |event| {
            let _ = &focus_guard;
            if event == NativeWindowActivation::Retired {
                retired_by_close.set(retired_by_close.get() + 1);
            }
        })));
        window.set_native_resize_constraint(Some(NativeResizeConstraint::Constrained {
            minimum: size(px(100.), px(80.)),
            maximum: size(px(500.), px(400.)),
            select: Rc::new(move |value| {
                let _ = &closing_guard;
                Some(value)
            }),
        }));
        window.set_resizable(false);
        window.set_minimizable(false);
        window.remove_window();
    })?;
    owned.push(closing);
    until(cx, "closed window sizing policy retirement", || {
        closing_released.upgrade().is_none()
    })
    .await?;
    ensure!(
        focus_retired.get() == 1 && focus_released.upgrade().is_none(),
        "Closing native window did not retire and release its focus fence once"
    );
    let closing_hwnd = owned.last().unwrap().hwnd;
    until(cx, "closed focus window destruction", || !unsafe {
        IsWindow(Some(closing_hwnd)).as_bool()
    })
    .await?;
    ensure!(
        focus_retired.get() == 1,
        "Native destruction retired the focus fence twice"
    );
    Ok(())
}

fn run_app(report: Rc<RefCell<Report>>) -> Result<()> {
    let platform = Rc::new(WindowsPlatform::new(false)?);
    Application::with_platform(platform)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            cx.spawn(async move |cx| {
                let mut owned = Vec::new();
                if let Err(error) = exercise(cx, &mut owned, &report).await {
                    report.borrow_mut().failures.push(error.to_string());
                }
                cx.update(|cx| {
                    // Every app window belongs to this fixture, including partial setup.
                    for window in cx.windows() {
                        if let Err(error) = window.update(cx, |_, window, _| window.remove_window())
                        {
                            report
                                .borrow_mut()
                                .failures
                                .push(format!("Cannot close fixture window: {error}"));
                        }
                    }
                });
                if let Err(error) = until(cx, "all owned HWNDs to be destroyed", || {
                    owned
                        .iter()
                        .all(|window| !unsafe { IsWindow(Some(window.hwnd)).as_bool() })
                })
                .await
                {
                    report.borrow_mut().failures.push(error.to_string());
                }
                let mut report = report.borrow_mut();
                report.destroyed = owned
                    .iter()
                    .filter(|window| !unsafe { IsWindow(Some(window.hwnd)).as_bool() })
                    .count();
                if report.destroyed != report.created {
                    report
                        .failures
                        .push("Owned window creation and destruction counts differ".into());
                }
                report.completed = true;
                drop(report);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    Ok(())
}

pub(super) fn run() -> i32 {
    let report = Rc::new(RefCell::new(Report::default()));
    let setup = (|| -> Result<_> {
        let Some(path) = arguments(std::env::args_os().skip(1).collect())? else {
            println!("{HELP}");
            return Ok(None);
        };
        Ok(Some(ProfileGuard::create(path)?))
    })();
    let guard = match setup {
        Ok(None) => return 0,
        Ok(Some(guard)) => guard,
        Err(error) => {
            println!(
                "{}",
                json!({"schemaVersion": 1, "ok": false, "setupFailure": error.to_string()})
            );
            return 2;
        }
    };
    let initial_foreground = unsafe { GetForegroundWindow() };
    let initial_foreground_identity = native_identity(initial_foreground);
    match DpiContext::enter() {
        Ok(dpi) => {
            if let Err(error) = run_app(report.clone()) {
                report.borrow_mut().failures.push(error.to_string());
            }
            if let Err(error) = dpi.restore() {
                report.borrow_mut().failures.push(error.to_string());
            }
        }
        Err(error) => report.borrow_mut().failures.push(error.to_string()),
    }
    if let Err(error) = guard.remove() {
        report.borrow_mut().failures.push(error.to_string());
    }
    let report = report.borrow();
    let ok = report.completed && report.failures.is_empty();
    println!(
        "{}",
        json!({
            "schemaVersion": 1, "ok": ok, "completed": report.completed,
            "failures": report.failures, "observations": report.observations,
            "created": report.created, "destroyed": report.destroyed,
            "initialForeground": initial_foreground.0 as usize,
            "initialForegroundIdentity": initial_foreground_identity,
            "finalForeground": unsafe { GetForegroundWindow() }.0 as usize,
            "finalForegroundIdentity": native_identity(unsafe { GetForegroundWindow() }),
            "scope": "GPUI constructor/window lifecycle; not remote rendering, mixed-DPI hardware or full product acceptance",
        })
    );
    if ok { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_arguments_require_one_explicit_profile() {
        assert!(arguments(vec!["--help".into()]).unwrap().is_none());
        assert_eq!(
            arguments(vec!["--isolated-profile".into(), "fixture profile".into()]).unwrap(),
            Some(PathBuf::from("fixture profile"))
        );
        for args in [
            vec![],
            vec!["--isolated-profile"],
            vec!["--help", "extra"],
            vec!["--other", "profile"],
            vec!["--isolated-profile", "one", "two"],
        ] {
            assert!(arguments(args.into_iter().map(OsString::from).collect()).is_err());
        }
    }
}
