#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    cell::{Cell, RefCell},
    num::NonZeroIsize,
    path::PathBuf,
    rc::{Rc, Weak},
    str::FromStr,
    sync::{Arc, Once, atomic::AtomicBool},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use futures::channel::oneshot::{self, Receiver};
use gpui_util::ResultExt;
use raw_window_handle as rwh;
use smallvec::SmallVec;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Dwm::*,
        Graphics::Gdi::*,
        System::{
            Com::*, Diagnostics::Debug::MessageBeep, LibraryLoader::*, Ole::*,
            SystemInformation::OSVERSIONINFOW, SystemServices::*,
        },
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

use crate::direct_manipulation::DirectManipulationHandler;
use crate::*;
use gpui::*;

#[path = "window_state.rs"]
mod window_state;
use window_state::WindowOpenState;
#[path = "client_frame.rs"]
pub(crate) mod client_frame;

pub(crate) struct WindowsWindow(pub Rc<WindowsWindowInner>);

fn window_control_style(
    style: WINDOW_STYLE,
    resizable: bool,
    minimizable: bool,
    maximizable: bool,
) -> WINDOW_STYLE {
    let mut style = style & !(WS_THICKFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX);
    if resizable {
        style |= WS_THICKFRAME;
    }
    if maximizable {
        style |= WS_MAXIMIZEBOX;
    }
    if minimizable {
        style |= WS_MINIMIZEBOX;
    }
    style
}

fn initial_show_command(focus: bool) -> SHOW_WINDOW_CMD {
    if focus {
        SW_SHOWNORMAL
    } else {
        SW_SHOWNOACTIVATE
    }
}

fn initial_window_styles(
    kind: WindowKind,
    hide_title_bar: bool,
    resizable: bool,
    minimizable: bool,
) -> (WINDOW_EX_STYLE, WINDOW_STYLE) {
    if kind == WindowKind::PopUp {
        return (WS_EX_TOOLWINDOW | WS_EX_TOPMOST, WS_POPUP);
    }
    let mut style = window_control_style(WS_SYSMENU, resizable, minimizable, resizable);
    let extended = if kind == WindowKind::Dialog {
        style |= WS_POPUP | WS_CAPTION;
        WS_EX_DLGMODALFRAME
    } else {
        if !hide_title_bar {
            style |= WS_CAPTION;
        }
        WS_EX_APPWINDOW
    };
    (extended, style)
}

fn resize_hit_test(edge: ResizeEdge) -> u32 {
    match edge {
        ResizeEdge::Top => HTTOP,
        ResizeEdge::TopRight => HTTOPRIGHT,
        ResizeEdge::Right => HTRIGHT,
        ResizeEdge::BottomRight => HTBOTTOMRIGHT,
        ResizeEdge::Bottom => HTBOTTOM,
        ResizeEdge::BottomLeft => HTBOTTOMLEFT,
        ResizeEdge::Left => HTLEFT,
        ResizeEdge::TopLeft => HTTOPLEFT,
    }
}

impl std::ops::Deref for WindowsWindow {
    type Target = WindowsWindowInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct WindowsWindowState {
    pub origin: Cell<Point<Pixels>>,
    pub logical_size: Cell<Size<Pixels>>,
    pub min_size: Option<Size<Pixels>>,
    pub(crate) resize_constraint: RefCell<Option<NativeResizeConstraint>>,
    pub fullscreen_restore_bounds: Cell<Bounds<Pixels>>,
    pub border_offset: WindowBorderOffset,
    pub appearance: Cell<WindowAppearance>,
    pub background_appearance: Cell<WindowBackgroundAppearance>,
    pub(crate) decorations: Cell<WindowDecorations>,
    pub scale_factor: Cell<f32>,
    pub restore_from_minimized: Cell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,

    pub callbacks: Callbacks,
    pub frame_signal: Arc<PlatformFrameSignal>,
    pub input_handler: Cell<Option<PlatformInputHandler>>,
    pub ime_enabled: Cell<bool>,
    pub pending_surrogate: Cell<Option<(u16, (u64, u64))>>,
    pub(crate) native_character_authority: crate::native_characters::Authority,
    pub last_reported_modifiers: Cell<Option<Modifiers>>,
    pub last_reported_capslock: Cell<Option<Capslock>>,
    pub hovered: Cell<bool>,
    /// The pointer's last position in the window and the button it moved
    /// with, for the exit when it leaves.
    pub last_pointer: Cell<(Point<Pixels>, Option<MouseButton>)>,
    pub(crate) modal_loops: Cell<NativeModalLoops>,
    pub(crate) closing: Cell<bool>,
    pub last_visibility: Cell<Option<WindowVisibility>>,
    pub direct_manipulation: DirectManipulationHandler,

    pub renderer: RefCell<DirectXRenderer>,
    /// Set when the next `draw_window` call must be treated as a forced
    /// render. Used after a GPU device-lost recovery, where the next frame
    /// must both re-enable drawing (via `mark_drawable`) and bypass the GPUI
    /// view cache (which would otherwise replay stale atlas tile references
    /// from the previous frame and panic in `DirectXAtlasTextures::texture`),
    /// and when a forced render was requested while another draw was in
    /// progress and had to be deferred.
    pub force_render_pending: Cell<bool>,

    pub click_state: ClickState,
    pub current_cursor: Cell<Option<HCURSOR>>,
    /// Shared with [`WindowsPlatformState::cursor_visible`].
    pub cursor_visible: Arc<AtomicBool>,
    pub nc_button_pressed: Cell<Option<u32>>,

    pub display: Cell<WindowsDisplay>,
    /// Flag to instruct the `VSyncProvider` thread to invalidate the directx devices
    /// as resizing them has failed, causing us to have lost at least the render target.
    pub invalidate_devices: Arc<AtomicBool>,
    /// Shared with [`WindowsPlatformState::draw_coordinator`] and every other window.
    pub(crate) draw_coordinator: Rc<DrawCoordinator>,
    fullscreen: Cell<Option<StyleAndBounds>>,
    initial_placement: Cell<Option<WindowOpenStatus>>,
    hwnd: HWND,
    pub(crate) a11y: RefCell<Option<A11yState>>,
}

pub(crate) struct WindowsWindowInner {
    pub(crate) local_file_drop: crate::file_drop::LocalFileDrop,
    pub(crate) native_drag: Rc<crate::native_drag::Authority>,
    is_popup: bool,
    pub(crate) mouse_passthrough: Cell<bool>,
    ole_drop_registered: Cell<bool>,
    hwnd: HWND,
    pub(crate) dialog_owner: Rc<crate::dialog::DialogOwner>,
    drop_target_helper: IDropTargetHelper,
    pub(crate) state: WindowsWindowState,
    system_settings: WindowsSystemSettings,
    pub(crate) handle: AnyWindowHandle,
    pub(crate) hide_title_bar: bool,
    pending_decorations: Cell<Option<WindowDecorations>>,
    pub(crate) is_movable: bool,
    pub(crate) is_resizable: Cell<bool>,
    pub(crate) is_minimizable: Cell<bool>,
    maximizable: Cell<Option<bool>>,
    pending_maximize: Cell<Option<bool>>,
    pub(crate) dpi_resize: Cell<Option<crate::native_resize::DpiResize>>,
    pub(crate) executor: ForegroundExecutor,
    pub(crate) validation_number: usize,
    pub(crate) main_receiver: PriorityQueueReceiver<RunnableVariant>,
    pub(crate) platform_window_handle: HWND,
    pub(crate) parent_hwnd: Option<HWND>,
    /// The AppUserModelID set on the window, removed when it is destroyed.
    pub(crate) app_id: RefCell<Option<String>>,
}

impl WindowsWindowState {
    fn new(
        hwnd: HWND,
        directx_devices: &DirectXDevices,
        window_params: &CREATESTRUCTW,
        decorations: WindowDecorations,
        current_cursor: Option<HCURSOR>,
        cursor_visible: Arc<AtomicBool>,
        display: WindowsDisplay,
        min_size: Option<Size<Pixels>>,
        appearance: WindowAppearance,
        disable_direct_composition: bool,
        invalidate_devices: Arc<AtomicBool>,
        draw_coordinator: Rc<DrawCoordinator>,
    ) -> Result<Self> {
        let scale_factor = {
            let monitor_dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
            monitor_dpi / USER_DEFAULT_SCREEN_DPI as f32
        };
        let origin = logical_point(window_params.x as f32, window_params.y as f32, scale_factor);
        let logical_size = {
            let physical_size = size(
                DevicePixels(window_params.cx),
                DevicePixels(window_params.cy),
            );
            physical_size.to_pixels(scale_factor)
        };
        let fullscreen_restore_bounds = Bounds {
            origin,
            size: logical_size,
        };
        let border_offset = WindowBorderOffset::default();
        let restore_from_minimized = None;
        let renderer = DirectXRenderer::new(hwnd, directx_devices, disable_direct_composition)
            .context("Creating DirectX renderer")?;
        let callbacks = Callbacks::default();
        let input_handler = None;
        let pending_surrogate = None;
        let last_reported_modifiers = None;
        let last_reported_capslock = None;
        let hovered = false;
        let click_state = ClickState::new();
        let nc_button_pressed = None;
        let fullscreen = None;
        let initial_placement = None;

        let direct_manipulation = DirectManipulationHandler::new(hwnd, scale_factor)
            .context("initializing Direct Manipulation")?;

        Ok(Self {
            origin: Cell::new(origin),
            logical_size: Cell::new(logical_size),
            fullscreen_restore_bounds: Cell::new(fullscreen_restore_bounds),
            border_offset,
            appearance: Cell::new(appearance),
            background_appearance: Cell::new(WindowBackgroundAppearance::Opaque),
            decorations: Cell::new(decorations),
            scale_factor: Cell::new(scale_factor),
            restore_from_minimized: Cell::new(restore_from_minimized),
            min_size,
            resize_constraint: RefCell::new(None),
            callbacks,
            frame_signal: Arc::new(PlatformFrameSignal::new()),
            input_handler: Cell::new(input_handler),
            ime_enabled: Cell::new(true),
            pending_surrogate: Cell::new(pending_surrogate),
            native_character_authority: crate::native_characters::Authority::new(hwnd),
            last_reported_modifiers: Cell::new(last_reported_modifiers),
            last_reported_capslock: Cell::new(last_reported_capslock),
            hovered: Cell::new(hovered),
            last_pointer: Cell::new((Point::default(), None)),
            modal_loops: Cell::new(NativeModalLoops::default()),
            closing: Cell::new(false),
            last_visibility: Cell::new(None),
            renderer: RefCell::new(renderer),
            force_render_pending: Cell::new(false),
            click_state,
            current_cursor: Cell::new(current_cursor),
            cursor_visible,
            nc_button_pressed: Cell::new(nc_button_pressed),
            display: Cell::new(display),
            fullscreen: Cell::new(fullscreen),
            initial_placement: Cell::new(initial_placement),
            hwnd,
            invalidate_devices,
            draw_coordinator,
            direct_manipulation,
            a11y: RefCell::new(None),
        })
    }

    #[inline]
    pub(crate) fn is_fullscreen(&self) -> bool {
        self.fullscreen.get().is_some()
    }

    pub(crate) fn is_maximized(&self) -> bool {
        !self.is_fullscreen() && unsafe { IsZoomed(self.hwnd) }.as_bool()
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: self.origin.get(),
            size: self.logical_size.get(),
        }
    }

    // Calculate the bounds used for saving and whether the window is maximized.
    fn calculate_window_bounds(&self) -> (Bounds<Pixels>, bool) {
        let placement = unsafe {
            let mut placement = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            GetWindowPlacement(self.hwnd, &mut placement)
                .context("failed to get window placement")
                .log_err();
            placement
        };
        // rcNormalPosition describes the restored window, not the current
        // maximized client clipped to the work area. Full-client normal
        // windows have no native insets regardless of their current state.
        let client_border = WindowBorderOffset::default();
        let normal_border = if self.decorations.get() == WindowDecorations::Client {
            &client_border
        } else {
            &self.border_offset
        };
        (
            calculate_client_rect(
                placement.rcNormalPosition,
                normal_border,
                self.scale_factor.get(),
            ),
            placement.showCmd == SW_SHOWMAXIMIZED.0 as u32,
        )
    }

    fn window_bounds(&self) -> WindowBounds {
        let (bounds, maximized) = self.calculate_window_bounds();

        if self.is_fullscreen() {
            WindowBounds::Fullscreen(self.fullscreen_restore_bounds.get())
        } else if maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        }
    }

    /// get the logical size of the app's drawable area.
    ///
    /// Currently, GPUI uses the logical size of the app to handle mouse interactions (such as
    /// whether the mouse collides with other elements of GPUI).
    fn content_size(&self) -> Size<Pixels> {
        self.logical_size.get()
    }
}

impl WindowsWindowInner {
    pub(crate) fn native_handle(&self) -> HWND {
        self.hwnd
    }

    pub(crate) fn is_maximizable(&self) -> bool {
        self.maximizable.get().unwrap_or(self.is_resizable.get())
    }

    fn update_window_controls(self: &Rc<Self>) {
        let this = self.clone();
        self.executor
            .spawn(async move {
                if this.state.closing.get() {
                    return;
                }
                if let Some(mut restore) = this.state.fullscreen.get() {
                    restore.style = window_control_style(
                        restore.style,
                        this.is_resizable.get(),
                        this.is_minimizable.get(),
                        this.is_maximizable(),
                    );
                    this.state.fullscreen.set(Some(restore));
                    return;
                }
                unsafe {
                    let previous = WINDOW_STYLE(get_window_long(this.hwnd, GWL_STYLE) as u32);
                    let style = window_control_style(
                        previous,
                        this.is_resizable.get(),
                        this.is_minimizable.get(),
                        this.is_maximizable(),
                    );
                    if style != previous {
                        set_window_long(this.hwnd, GWL_STYLE, style.0 as isize);
                        SetWindowPos(
                            this.hwnd,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED
                                | SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_NOACTIVATE
                                | SWP_NOZORDER,
                        )
                        .log_err();
                    }
                }
            })
            .detach();
    }

    fn start_native_drag(self: &Rc<Self>, hit_test: u32) {
        let this = self.clone();
        self.executor
            .spawn(async move {
                if this.state.closing.get()
                    || this.state.is_fullscreen()
                    || (hit_test == HTCAPTION && !this.is_movable)
                    || (hit_test != HTCAPTION
                        && (!this.is_resizable.get() || this.state.is_maximized()))
                {
                    return;
                }
                unsafe {
                    let mut position = POINT::default();
                    if GetCursorPos(&mut position).is_err() {
                        return;
                    }
                    ReleaseCapture().log_err();
                    DefWindowProcW(
                        this.hwnd,
                        WM_NCLBUTTONDOWN,
                        WPARAM(hit_test as usize),
                        LPARAM(
                            ((position.y as u16 as u32) << 16 | position.x as u16 as u32) as isize,
                        ),
                    );
                }
            })
            .detach();
    }

    /// Whether the window is being presented: shown and not minimized. Windows
    /// has no notification for a window fully covered by other windows, so
    /// that case reports `Visible`.
    pub(crate) fn visibility(&self) -> WindowVisibility {
        let is_visible =
            unsafe { IsWindowVisible(self.hwnd).as_bool() && !IsIconic(self.hwnd).as_bool() };
        if is_visible {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        }
    }

    fn new(context: &mut WindowCreateContext, hwnd: HWND, cs: &CREATESTRUCTW) -> Result<Rc<Self>> {
        let state = WindowsWindowState::new(
            hwnd,
            &context.directx_devices,
            cs,
            context.decorations,
            context.current_cursor,
            context.cursor_visible.clone(),
            context.display,
            context.min_size,
            context.appearance,
            context.disable_direct_composition,
            context.invalidate_devices.clone(),
            context.draw_coordinator.clone(),
        )?;

        Ok(Rc::new(Self {
            local_file_drop: crate::file_drop::LocalFileDrop::new(context.local_file_drop),
            native_drag: crate::native_drag::Authority::new(hwnd),
            is_popup: context.is_popup,
            mouse_passthrough: Cell::new(context.mouse_passthrough),
            ole_drop_registered: Cell::new(false),
            hwnd,
            dialog_owner: crate::dialog::DialogOwner::new(hwnd),
            drop_target_helper: context.drop_target_helper.clone(),
            state,
            handle: context.handle,
            hide_title_bar: context.hide_title_bar,
            pending_decorations: Cell::new(None),
            is_movable: context.is_movable,
            is_resizable: Cell::new(context.is_resizable),
            is_minimizable: Cell::new(context.is_minimizable),
            maximizable: Cell::new(None),
            pending_maximize: Cell::new(None),
            dpi_resize: Cell::new(None),
            executor: context.executor.clone(),
            validation_number: context.validation_number,
            main_receiver: context.main_receiver.clone(),
            platform_window_handle: context.platform_window_handle,
            system_settings: WindowsSystemSettings::new(),
            parent_hwnd: context.parent_hwnd,
            app_id: RefCell::new(None),
        }))
    }

    fn set_app_id(&self, app_id: &str) {
        if self.app_id.borrow().as_deref() == Some(app_id) {
            return;
        }
        match crate::app_user_model::set_window_app_id(self.hwnd, Some(app_id)) {
            Ok(()) => *self.app_id.borrow_mut() = Some(app_id.to_owned()),
            Err(error) => log::warn!("failed to set the window's AppUserModelID: {error}"),
        }
    }

    fn toggle_fullscreen(self: &Rc<Self>) {
        let this = self.clone();
        self.executor
            .spawn(async move {
                let StyleAndBounds {
                    style,
                    x,
                    y,
                    cx,
                    cy,
                } = match this.state.fullscreen.take() {
                    Some(state) => state,
                    None => {
                        let (window_bounds, _) = this.state.calculate_window_bounds();
                        this.state.fullscreen_restore_bounds.set(window_bounds);

                        let style =
                            WINDOW_STYLE(unsafe { get_window_long(this.hwnd, GWL_STYLE) } as _);
                        let mut rc = RECT::default();
                        unsafe { GetWindowRect(this.hwnd, &mut rc) }
                            .context("failed to get window rect")
                            .log_err();
                        let _ = this.state.fullscreen.set(Some(StyleAndBounds {
                            style,
                            x: rc.left,
                            y: rc.top,
                            cx: rc.right - rc.left,
                            cy: rc.bottom - rc.top,
                        }));
                        let style = style
                            & !(WS_THICKFRAME
                                | WS_SYSMENU
                                | WS_MAXIMIZEBOX
                                | WS_MINIMIZEBOX
                                | WS_CAPTION);
                        let physical_bounds = this.state.display.get().physical_bounds();
                        StyleAndBounds {
                            style,
                            x: physical_bounds.left().0,
                            y: physical_bounds.top().0,
                            cx: physical_bounds.size.width.0,
                            cy: physical_bounds.size.height.0,
                        }
                    }
                };
                set_non_rude_hwnd(this.hwnd, !this.state.is_fullscreen());
                unsafe { set_window_long(this.hwnd, GWL_STYLE, style.0 as isize) };
                unsafe {
                    SetWindowPos(
                        this.hwnd,
                        None,
                        x,
                        y,
                        cx,
                        cy,
                        SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOZORDER,
                    )
                }
                .log_err();
            })
            .detach();
    }

    fn can_activate(&self) -> bool {
        !self.state.closing.get() && !self.mouse_passthrough.get()
    }

    fn set_window_placement(self: &Rc<Self>) -> Result<()> {
        let Some(open_status) = self.state.initial_placement.take() else {
            return Ok(());
        };
        match open_status.state {
            WindowOpenState::Maximized => unsafe {
                SetWindowPlacement(self.hwnd, &open_status.placement)
                    .context("failed to set window placement")?;
                ShowWindowAsync(self.hwnd, SW_MAXIMIZE).ok()?;
            },
            WindowOpenState::Fullscreen => {
                unsafe {
                    SetWindowPlacement(self.hwnd, &open_status.placement)
                        .context("failed to set window placement")?
                };
                self.toggle_fullscreen();
            }
            WindowOpenState::Windowed => unsafe {
                SetWindowPlacement(self.hwnd, &open_status.placement)
                    .context("failed to set window placement")?;
            },
        }
        Ok(())
    }

    pub(crate) fn system_settings(&self) -> &WindowsSystemSettings {
        &self.system_settings
    }
}

#[derive(Default)]
pub(crate) struct Callbacks {
    pub(crate) request_frame: Cell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    pub(crate) input: Cell<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    pub(crate) prepare_key_input: Cell<Option<Box<dyn FnMut(bool) -> bool>>>,
    pub(crate) active_status_change: Cell<Option<Box<dyn FnMut(bool)>>>,
    pub(crate) native_activation: RefCell<Option<Rc<dyn Fn(NativeWindowActivation)>>>,
    pub(crate) visibility_change: Cell<Option<Box<dyn FnMut(WindowVisibility)>>>,
    pub(crate) hovered_status_change: Cell<Option<Box<dyn FnMut(bool)>>>,
    pub(crate) resize: Cell<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    pub(crate) moved: Cell<Option<Box<dyn FnMut()>>>,
    pub(crate) display_changed: Cell<Option<Box<dyn FnMut()>>>,
    pub(crate) should_close: Cell<Option<Box<dyn FnMut() -> bool>>>,
    pub(crate) close: Cell<Option<Box<dyn FnOnce()>>>,
    pub(crate) hit_test_window_control: Cell<Option<Box<dyn FnMut() -> Option<WindowControlArea>>>>,
    pub(crate) appearance_changed: Cell<Option<Box<dyn FnMut()>>>,
}

struct WindowCreateContext {
    local_file_drop: bool,
    is_popup: bool,
    mouse_passthrough: bool,
    inner: Option<Result<Rc<WindowsWindowInner>>>,
    handle: AnyWindowHandle,
    hide_title_bar: bool,
    decorations: WindowDecorations,
    display: WindowsDisplay,
    is_movable: bool,
    is_resizable: bool,
    is_minimizable: bool,
    min_size: Option<Size<Pixels>>,
    executor: ForegroundExecutor,
    current_cursor: Option<HCURSOR>,
    cursor_visible: Arc<AtomicBool>,
    drop_target_helper: IDropTargetHelper,
    validation_number: usize,
    main_receiver: PriorityQueueReceiver<RunnableVariant>,
    platform_window_handle: HWND,
    appearance: WindowAppearance,
    disable_direct_composition: bool,
    directx_devices: DirectXDevices,
    invalidate_devices: Arc<AtomicBool>,
    draw_coordinator: Rc<DrawCoordinator>,
    parent_hwnd: Option<HWND>,
}

impl WindowsWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        creation_info: WindowCreationInfo,
    ) -> Result<Self> {
        // Native popups are not implemented on Windows yet. Rejecting lets callers fall back to
        // gpui's in-window popovers.
        if let WindowKind::AnchoredPopup(_) = params.kind {
            return Err(popup::PopupNotSupportedError.into());
        }

        let WindowCreationInfo {
            icon,
            executor,
            current_cursor,
            cursor_visible,
            drop_target_helper,
            validation_number,
            main_receiver,
            platform_window_handle,
            disable_direct_composition,
            directx_devices,
            invalidate_devices,
            draw_coordinator,
        } = creation_info;
        register_window_class(icon);
        let parent_hwnd = if params.kind == WindowKind::Dialog {
            let parent_window = unsafe { GetActiveWindow() };
            if parent_window.is_invalid() {
                None
            } else {
                // Disable the parent window to make this dialog modal
                unsafe {
                    EnableWindow(parent_window, false).as_bool();
                };
                Some(parent_window)
            }
        } else {
            None
        };
        let hide_title_bar = params
            .titlebar
            .as_ref()
            .map(|titlebar| titlebar.appears_transparent)
            .unwrap_or(true);
        let window_name = HSTRING::from(
            params
                .titlebar
                .as_ref()
                .and_then(|titlebar| titlebar.title.as_ref())
                .map(|title| title.as_ref())
                .unwrap_or(""),
        );

        let popup = params.kind == WindowKind::PopUp;
        if params.mouse_passthrough && !popup {
            anyhow::bail!("Mouse passthrough requires a visual-only popup");
        }
        let (mut dwexstyle, dwstyle) = initial_window_styles(
            params.kind,
            hide_title_bar,
            params.is_resizable,
            params.is_minimizable,
        );
        if !disable_direct_composition {
            dwexstyle |= WS_EX_NOREDIRECTIONBITMAP;
        }
        if params.mouse_passthrough {
            dwexstyle |= WS_EX_NOACTIVATE;
        }

        let hinstance = get_module_handle();
        let display = if let Some(display_id) = params.display_id {
            WindowsDisplay::new(display_id)
        } else {
            None
        }
        .or_else(WindowsDisplay::primary_monitor)
        .context("failed to find any monitor")?;
        let appearance = system_appearance().unwrap_or_default();
        // CW_USEDEFAULT is an overlapped-window policy. Native WS_POPUP windows
        // (including captioned dialogs) otherwise start at zero size, preventing
        // the first chrome measurement from observing their actual borders.
        let initial_bounds = if popup {
            popup_placement(params.bounds, display.visible_bounds())?
        } else if display.check_given_bounds(params.bounds) {
            params.bounds
        } else {
            display.default_bounds()
        };
        let creation_bounds = dwstyle
            .contains(WS_POPUP)
            .then(|| initial_bounds.to_device_pixels(display.scale_factor()));
        let mut context = WindowCreateContext {
            local_file_drop: params.local_file_drop,
            is_popup: popup,
            mouse_passthrough: params.mouse_passthrough,
            inner: None,
            handle,
            hide_title_bar,
            decorations: params.window_decorations,
            display,
            is_movable: params.is_movable,
            is_resizable: params.is_resizable,
            is_minimizable: params.is_minimizable,
            min_size: params.window_min_size,
            executor,
            current_cursor,
            cursor_visible,
            drop_target_helper,
            validation_number,
            main_receiver,
            platform_window_handle,
            appearance,
            disable_direct_composition,
            directx_devices,
            invalidate_devices,
            draw_coordinator,
            parent_hwnd,
        };
        let creation_result = unsafe {
            CreateWindowExW(
                dwexstyle,
                WINDOW_CLASS_NAME,
                &window_name,
                dwstyle,
                creation_bounds.map_or(CW_USEDEFAULT, |bounds| bounds.origin.x.0),
                creation_bounds.map_or(CW_USEDEFAULT, |bounds| bounds.origin.y.0),
                creation_bounds.map_or(CW_USEDEFAULT, |bounds| bounds.size.width.0),
                creation_bounds.map_or(CW_USEDEFAULT, |bounds| bounds.size.height.0),
                parent_hwnd,
                None,
                Some(hinstance.into()),
                Some(&context as *const _ as *const _),
            )
        };

        // Failure to create a `WindowsWindowState` can cause window creation to fail,
        // so check the inner result first.
        let this = context.inner.take().transpose()?;
        let hwnd = creation_result?;
        let this = this.unwrap();

        if !params.local_file_drop {
            register_drag_drop(&this)?;
            this.ole_drop_registered.set(true);
        }
        // Before the window is shown, so the taskbar never lists it under
        // another app first.
        if let Some(app_id) = params.app_id.as_deref() {
            this.set_app_id(app_id);
        }
        set_non_rude_hwnd(hwnd, true);
        configure_dwm_dark_mode(hwnd, appearance);
        this.state.border_offset.update(hwnd)?;
        let mut placement =
            retrieve_window_placement(hwnd, display, initial_bounds, &this.state.border_offset)?;
        // A newly created HWND reports SW_HIDE. The requested show policy, not
        // that initial native state, determines its first presentation.
        placement.showCmd =
            initial_show_command(params.focus && !params.mouse_passthrough).0 as u32;
        if params.show {
            unsafe { SetWindowPlacement(hwnd, &placement)? };
        } else {
            this.state.initial_placement.set(Some(WindowOpenStatus {
                placement,
                state: WindowOpenState::Windowed,
            }));
        }

        Ok(Self(this))
    }
}

impl rwh::HasWindowHandle for WindowsWindow {
    fn window_handle(&self) -> std::result::Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let raw = rwh::Win32WindowHandle::new(unsafe {
            NonZeroIsize::new_unchecked(self.0.hwnd.0 as isize)
        })
        .into();
        Ok(unsafe { rwh::WindowHandle::borrow_raw(raw) })
    }
}

impl rwh::HasDisplayHandle for WindowsWindow {
    fn display_handle(&self) -> std::result::Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(rwh::DisplayHandle::windows())
    }
}

impl WindowsWindowInner {
    pub(crate) fn notify_native_activation(&self, event: NativeWindowActivation) {
        let handler = self.state.callbacks.native_activation.borrow().clone();
        if let Some(handler) = handler {
            handler(event);
        }
    }

    pub(crate) fn retire_native_activation(&self) {
        let handler = self.state.callbacks.native_activation.take();
        if let Some(handler) = handler {
            handler(NativeWindowActivation::Retired);
        }
    }
}

impl Drop for WindowsWindow {
    fn drop(&mut self) {
        self.0.native_drag.retire();
        self.0.state.native_character_authority.retire();
        self.0.state.closing.set(true);
        self.0.local_file_drop.retire(self.0.hwnd);
        self.0.retire_native_activation();
        self.0.state.resize_constraint.borrow_mut().take();
        self.0.dialog_owner.close();
        unsafe { ShowWindowAsync(self.0.hwnd, SW_HIDE).ok().log_err() };
        // `DestroyWindow` below sends `WM_SHOWWINDOW`; without a callback the
        // resulting visibility report has nothing to notify.
        self.0.state.callbacks.visibility_change.take();
        // Hiding or destroying an active window deactivates it after GPUI has
        // already removed it, and the report would find no window.
        self.0.state.callbacks.active_status_change.take();
        // Hiding a window under the cursor sends `WM_MOUSELEAVE`, which would
        // report on a window GPUI has already removed.
        self.0.state.callbacks.hovered_status_change.take();
        // clone this `Rc` to prevent early release of the pointer
        let this = self.0.clone();
        self.0
            .executor
            .spawn(async move {
                this.dialog_owner.when_idle().await;
                let handle = this.hwnd;
                unsafe {
                    if this.ole_drop_registered.replace(false) {
                        RevokeDragDrop(handle).log_err();
                    }
                    DestroyWindow(handle).log_err();
                }
            })
            .detach();
    }
}

impl PlatformWindow for WindowsWindow {
    fn window_decorations(&self) -> Decorations {
        match self.state.decorations.get() {
            WindowDecorations::Server => Decorations::Server,
            WindowDecorations::Client => Decorations::Client {
                tiling: if self.state.is_maximized() || self.state.is_fullscreen() {
                    Tiling::tiled()
                } else {
                    Tiling::default()
                },
            },
        }
    }

    fn request_decorations(&self, decorations: WindowDecorations) {
        if self.state.closing.get()
            || (self.state.decorations.get() == decorations
                && self.pending_decorations.get().is_none())
        {
            return;
        }
        if self
            .pending_decorations
            .replace(Some(decorations))
            .is_some()
        {
            return;
        }
        let weak = Rc::downgrade(&self.0);
        self.executor
            .spawn(async move {
                let Some(this) = weak.upgrade().filter(|this| !this.state.closing.get()) else {
                    return;
                };
                let Some(decorations) = this.pending_decorations.take() else {
                    return;
                };
                let previous = this.state.decorations.replace(decorations);
                if previous == decorations {
                    return;
                }
                // A hidden first placement stores an outer rectangle. Preserve its
                // requested client extent when the frame policy changes before show.
                let pending = this.state.initial_placement.take().map(|status| {
                    let client = calculate_client_rect(
                        status.placement.rcNormalPosition,
                        &this.state.border_offset,
                        this.state.scale_factor.get(),
                    )
                    .to_device_pixels(this.state.scale_factor.get());
                    (status, client)
                });
                let changed = unsafe {
                    SetWindowPos(
                        this.hwnd,
                        None,
                        0,
                        0,
                        0,
                        0,
                        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                    )
                }
                .context("changing window decorations")
                .and_then(|_| this.state.border_offset.update(this.hwnd));
                if let Err(error) = changed {
                    this.state.decorations.set(previous);
                    if let Some((status, _)) = pending {
                        this.state.initial_placement.set(Some(status));
                    }
                    // A failed native call may have delivered some messages. Put
                    // the previous calculation policy back before retrying it.
                    unsafe {
                        SetWindowPos(
                            this.hwnd,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED
                                | SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_NOACTIVATE
                                | SWP_NOZORDER,
                        )
                    }
                    .context("restoring window decorations after failure")
                    .log_err();
                    this.state.border_offset.update(this.hwnd).log_err();
                    log::error!("Cannot change window decorations: {error:#}");
                    return;
                }
                if let Some((mut status, client)) = pending {
                    status.placement.rcNormalPosition =
                        calculate_window_rect(client, &this.state.border_offset);
                    this.state.initial_placement.set(Some(status));
                }
            })
            .detach();
    }

    fn set_popup_mouse_passthrough(&self, enabled: bool) {
        if self.is_popup && !self.state.closing.get() {
            // Synchronous policy only: metadata updates can hold a GPUI borrow.
            // No native style mutation or callback may run from this setter.
            self.mouse_passthrough.set(enabled);
        }
    }

    fn on_native_file_drop(
        &self,
        context: Rc<dyn Fn() -> Option<u64>>,
        callback: Box<dyn FnMut(NativeFileDrop)>,
    ) {
        self.0.local_file_drop.install(context, callback);
    }

    fn set_native_file_drop_enabled(&self, enabled: bool) {
        self.0
            .local_file_drop
            .enable(self.0.hwnd, enabled && !self.0.state.closing.get());
    }
    fn capture_native_drag_gesture(&self) -> Option<NativeDragGesture> {
        self.0.native_drag.capture()
    }
    fn bounds(&self) -> Bounds<Pixels> {
        self.state.bounds()
    }

    fn is_maximized(&self) -> bool {
        self.state.is_maximized()
    }

    fn window_bounds(&self) -> WindowBounds {
        self.state.window_bounds()
    }

    /// get the logical size of the app's drawable area.
    ///
    /// Currently, GPUI uses the logical size of the app to handle mouse interactions (such as
    /// whether the mouse collides with other elements of GPUI).
    fn content_size(&self) -> Size<Pixels> {
        self.state.content_size()
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let this = self.0.clone();
        self.0
            .executor
            .spawn(async move {
                if this.state.closing.get() {
                    return;
                }
                // First show, a style change, or a DPI transition may have
                // changed the nonclient extent since the cached measurement.
                // Measure on the UI thread immediately before applying the
                // requested client size, after earlier queued style updates.
                if this
                    .state
                    .border_offset
                    .update(this.hwnd)
                    .log_err()
                    .is_none()
                {
                    return;
                }
                let bounds = gpui::bounds(this.state.bounds().origin, size)
                    .to_device_pixels(this.state.scale_factor.get());
                let rect = calculate_window_rect(bounds, &this.state.border_offset);
                unsafe {
                    SetWindowPos(
                        this.hwnd,
                        None,
                        bounds.origin.x.0,
                        bounds.origin.y.0,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
                    )
                    .context("unable to set window content size")
                    .log_err();
                }
            })
            .detach();
    }

    fn scale_factor(&self) -> f32 {
        self.state.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        self.state.appearance.get()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(self.state.display.get()))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        let scale_factor = self.scale_factor();
        let point = unsafe {
            let mut point: POINT = std::mem::zeroed();
            GetCursorPos(&mut point)
                .context("unable to get cursor position")
                .log_err();
            ScreenToClient(self.0.hwnd, &mut point).ok().log_err();
            point
        };
        logical_point(point.x as f32, point.y as f32, scale_factor)
    }

    fn modifiers(&self) -> Modifiers {
        current_modifiers()
    }

    fn capslock(&self) -> Capslock {
        current_capslock()
    }

    fn numlock(&self) -> Option<bool> {
        Some(unsafe { GetKeyState(VK_NUMLOCK.0 as i32) & 1 != 0 })
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.state.input_handler.set(Some(input_handler));
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.state.input_handler.take()
    }

    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<Receiver<usize>> {
        let (mut done_tx, done_rx) = oneshot::channel();
        let msg = msg.to_string();
        let detail_string = detail.map(|detail| detail.to_string());
        let answers = answers.to_vec();
        let dialog = crate::dialog::show_dialog(
            Some(self.0.dialog_owner.clone()),
            &self.0.executor,
            move |handle| {
                unsafe {
                    let mut config = TASKDIALOGCONFIG::default();
                    config.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as _;
                    config.hwndParent = handle;
                    config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION;
                    let title;
                    let main_icon;
                    match level {
                        PromptLevel::Info => {
                            title = windows::core::w!("Info");
                            main_icon = TD_INFORMATION_ICON;
                        }
                        PromptLevel::Warning => {
                            title = windows::core::w!("Warning");
                            main_icon = TD_WARNING_ICON;
                        }
                        PromptLevel::Critical => {
                            title = windows::core::w!("Critical");
                            main_icon = TD_ERROR_ICON;
                        }
                    };
                    config.pszWindowTitle = title;
                    config.Anonymous1.pszMainIcon = main_icon;
                    let instruction = HSTRING::from(msg);
                    config.pszMainInstruction = PCWSTR::from_raw(instruction.as_ptr());
                    let hints_encoded;
                    if let Some(ref hints) = detail_string {
                        hints_encoded = HSTRING::from(hints);
                        config.pszContent = PCWSTR::from_raw(hints_encoded.as_ptr());
                    };
                    let mut button_id_map = Vec::with_capacity(answers.len());
                    let mut buttons = Vec::new();
                    let mut btn_encoded = Vec::new();
                    for (index, btn) in answers.iter().enumerate() {
                        let encoded = HSTRING::from(btn.label().as_ref());
                        let button_id = match btn {
                            PromptButton::Ok(_) => IDOK.0,
                            PromptButton::Cancel(_) => IDCANCEL.0,
                            // the first few low integer values are reserved for known buttons
                            // so for simplicity we just go backwards from -1
                            PromptButton::Other(_) => -(index as i32) - 1,
                        };
                        button_id_map.push(button_id);
                        buttons.push(TASKDIALOG_BUTTON {
                            nButtonID: button_id,
                            pszButtonText: PCWSTR::from_raw(encoded.as_ptr()),
                        });
                        btn_encoded.push(encoded);
                    }
                    config.cButtons = buttons.len() as _;
                    config.pButtons = buttons.as_ptr();

                    config.pfCallback = Some(crate::dialog::task_dialog_callback);
                    config.lpCallbackData = button_id_map.contains(&IDCANCEL.0) as isize;
                    let mut res = std::mem::zeroed();
                    TaskDialogIndirect(&config, Some(&mut res), None, None)
                        .context("unable to create task dialog")?;
                    Ok(button_id_map.iter().position(|&button_id| button_id == res))
                }
            },
        );
        self.0
            .executor
            .spawn(async move {
                if let futures::future::Either::Left((result, _)) =
                    futures::future::select(dialog, done_tx.cancellation()).await
                    && let Some(Some(clicked)) = result
                        .context("native dialog thread stopped")
                        .and_then(|result| result)
                        .log_err()
                {
                    done_tx.send(clicked).ok();
                }
            })
            .detach();

        Some(done_rx)
    }

    fn activate(&self) {
        if !self.can_activate() {
            return;
        }
        let hwnd = self.0.hwnd;
        let this = self.0.clone();
        self.0
            .executor
            .spawn(async move {
                // Metadata can revoke activation after this task was queued.
                if !this.can_activate() {
                    return;
                }
                this.set_window_placement().log_err();
                if !this.can_activate() {
                    return;
                }

                unsafe {
                    // If the window is minimized, restore it.
                    if IsIconic(hwnd).as_bool() {
                        // Execute on the owning UI thread so a later policy
                        // change cannot be overtaken by an async restore.
                        let _was_visible = ShowWindow(hwnd, SW_RESTORE);
                    }
                    if !this.can_activate() {
                        return;
                    }
                    SetActiveWindow(hwnd).ok();
                    if !this.can_activate() {
                        return;
                    }
                    SetFocus(Some(hwnd)).ok();
                }
                if !this.can_activate() {
                    return;
                }

                // premium ragebait by windows, this is needed because the window
                // must have received an input event to be able to set itself to foreground
                // so let's just simulate user input as that seems to be the most reliable way
                // some more info: https://gist.github.com/Aetopia/1581b40f00cc0cadc93a0e8ccb65dc8c
                // bonus: this bug also doesn't manifest if you have vs attached to the process
                let inputs = [
                    INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VK_MENU,
                                dwFlags: KEYBD_EVENT_FLAGS(0),
                                ..Default::default()
                            },
                        },
                    },
                    INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VK_MENU,
                                dwFlags: KEYEVENTF_KEYUP,
                                ..Default::default()
                            },
                        },
                    },
                ];
                unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
                if !this.can_activate() {
                    return;
                }

                // todo(windows)
                // crate `windows 0.56` reports true as Err
                unsafe { SetForegroundWindow(hwnd).as_bool() };
            })
            .detach();
    }

    fn request_attention(&self) {
        if self.is_active() {
            return;
        }

        let hwnd = self.0.hwnd;
        self.0
            .executor
            .spawn(async move {
                let info = FLASHWINFO {
                    cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                    hwnd,
                    dwFlags: FLASHW_ALL,
                    uCount: 1,
                    dwTimeout: 0,
                };
                unsafe { FlashWindowEx(&info).ok().log_err() };
            })
            .detach();
    }

    fn is_active(&self) -> bool {
        self.0.hwnd == unsafe { GetActiveWindow() }
    }

    fn visibility(&self) -> WindowVisibility {
        self.0.visibility()
    }

    fn is_hovered(&self) -> bool {
        self.state.hovered.get()
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.state.background_appearance.get()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        true
    }

    fn set_title(&mut self, title: &str) {
        unsafe { SetWindowTextW(self.0.hwnd, &HSTRING::from(title)) }
            .inspect_err(|e| log::error!("Set title failed: {e}"))
            .ok();
    }

    fn set_app_id(&mut self, app_id: &str) {
        self.0.set_app_id(app_id);
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        self.state.background_appearance.set(background_appearance);
        let hwnd = self.0.hwnd;

        // using Dwm APIs for Mica and MicaAlt backdrops.
        // others follow the set_window_composition_attribute approach
        let result = match background_appearance {
            WindowBackgroundAppearance::Opaque => set_window_composition_attribute(hwnd, None, 0),
            WindowBackgroundAppearance::Transparent => {
                set_window_composition_attribute(hwnd, None, 2)
            }
            WindowBackgroundAppearance::Blurred => {
                set_window_composition_attribute(hwnd, Some((0, 0, 0, 0)), 4)
            }
            WindowBackgroundAppearance::MicaBackdrop => {
                // DWMSBT_MAINWINDOW => MicaBase
                dwm_set_window_composition_attribute(hwnd, 2)
            }
            WindowBackgroundAppearance::MicaAltBackdrop => {
                // DWMSBT_TABBEDWINDOW => MicaAlt
                dwm_set_window_composition_attribute(hwnd, 4)
            }
        };
        result
            .context("Setting native window background appearance")
            .log_err();
    }

    fn minimize(&self) {
        unsafe { ShowWindowAsync(self.0.hwnd, SW_MINIMIZE).ok().log_err() };
    }

    fn set_resizable(&self, resizable: bool) {
        if self.is_resizable.replace(resizable) != resizable {
            self.0.update_window_controls();
        }
    }

    fn set_maximizable(&self, maximizable: bool) {
        if self.maximizable.replace(Some(maximizable)) != Some(maximizable) {
            self.0.update_window_controls();
        }
    }

    fn set_maximized(&self, maximized: bool) {
        if self.state.closing.get()
            || self.state.is_fullscreen()
            || self.state.modal_loops.get().in_size_move()
            || unsafe { IsIconic(self.hwnd).as_bool() }
        {
            self.pending_maximize.set(None);
            return;
        }
        if !unsafe { IsWindowVisible(self.hwnd).as_bool() }
            && let Some(mut status) = self.state.initial_placement.take()
        {
            if status.state != WindowOpenState::Fullscreen {
                status.state = if maximized {
                    WindowOpenState::Maximized
                } else {
                    WindowOpenState::Windowed
                };
            }
            self.state.initial_placement.set(Some(status));
            return;
        }
        if self.pending_maximize.replace(Some(maximized)).is_some() {
            return;
        }
        let this = Rc::downgrade(&self.0);
        self.executor
            .spawn(async move {
                let Some(this) = this.upgrade() else {
                    return;
                };
                let Some(maximized) = this.pending_maximize.take() else {
                    return;
                };
                if this.state.closing.get()
                    || this.state.is_fullscreen()
                    || this.state.modal_loops.get().in_size_move()
                    || unsafe {
                        IsIconic(this.hwnd).as_bool() || !IsWindowVisible(this.hwnd).as_bool()
                    }
                {
                    return;
                }
                if this.state.is_maximized() != maximized {
                    // Synchronous on its UI thread, outside GPUI borrows. A later
                    // opposite request cannot overtake an unprocessed async show.
                    let _was_visible = unsafe {
                        ShowWindow(
                            this.hwnd,
                            if maximized {
                                SW_MAXIMIZE
                            } else {
                                SW_SHOWNOACTIVATE
                            },
                        )
                    };
                }
            })
            .detach();
    }

    fn set_minimizable(&self, minimizable: bool) {
        if self.is_minimizable.replace(minimizable) != minimizable {
            self.0.update_window_controls();
        }
    }

    fn is_in_live_resize(&self) -> bool {
        self.state.modal_loops.get().in_size_move()
    }

    fn start_window_move(&self) {
        if self.is_movable && !self.state.is_fullscreen() {
            self.0.start_native_drag(HTCAPTION);
        }
    }

    fn start_window_resize(&self, edge: ResizeEdge) {
        if self.is_resizable.get() && !self.state.is_fullscreen() && !self.state.is_maximized() {
            self.0.start_native_drag(resize_hit_test(edge));
        }
    }

    fn show_window_menu(&self, position: Point<Pixels>) {
        let this = self.0.clone();
        // Native menus and move/resize run modal loops. Enter them after GPUI
        // releases its window update so their callbacks may safely re-enter.
        self.executor
            .spawn(async move {
                if this.state.closing.get() {
                    return;
                }
                let scale = this.state.scale_factor.get();
                let mut position = POINT {
                    x: (position.x.as_f32() * scale).round() as i32,
                    y: (position.y.as_f32() * scale).round() as i32,
                };
                unsafe {
                    if !ClientToScreen(this.hwnd, &mut position).as_bool() {
                        return;
                    }
                    let menu = GetSystemMenu(this.hwnd, false);
                    if menu.is_invalid() {
                        return;
                    }
                    let maximized = this.state.is_maximized();
                    let fullscreen = this.state.is_fullscreen();
                    for (command, enabled) in [
                        (SC_MOVE, this.is_movable && !maximized && !fullscreen),
                        (
                            SC_SIZE,
                            this.is_resizable.get() && !maximized && !fullscreen,
                        ),
                        (SC_MINIMIZE, this.is_minimizable.get()),
                        (
                            SC_MAXIMIZE,
                            this.is_maximizable() && !maximized && !fullscreen,
                        ),
                        (SC_RESTORE, maximized || IsIconic(this.hwnd).as_bool()),
                    ] {
                        let _ = EnableMenuItem(
                            menu,
                            command,
                            MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
                        );
                    }
                    let command = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                        position.x,
                        position.y,
                        None,
                        this.hwnd,
                        None,
                    );
                    if command.0 != 0 && !this.state.closing.get() {
                        PostMessageW(
                            Some(this.hwnd),
                            WM_SYSCOMMAND,
                            WPARAM(command.0 as usize),
                            LPARAM(0),
                        )
                        .log_err();
                    }
                }
            })
            .detach();
    }

    fn set_native_activation_handler(&self, handler: Option<Rc<dyn Fn(NativeWindowActivation)>>) {
        let old = self.state.callbacks.native_activation.replace(handler);
        if let Some(old) = old {
            old(NativeWindowActivation::Retired);
        }
        if self.state.closing.get() {
            self.retire_native_activation();
        }
    }

    fn set_native_resize_constraint(&self, constraint: Option<NativeResizeConstraint>) {
        if !self.state.closing.get() {
            *self.state.resize_constraint.borrow_mut() = constraint;
        }
    }

    fn zoom(&self) {
        if self.state.is_fullscreen() {
            return;
        }
        unsafe {
            if IsWindowVisible(self.0.hwnd).as_bool() {
                // The OS owns its restore rectangle, including Snap and the
                // workspace origin. Do not restore it with SetWindowPos.
                let command = if self.state.is_maximized() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                ShowWindowAsync(self.0.hwnd, command).ok().log_err();
            } else if let Some(mut status) = self.state.initial_placement.take() {
                status.state = status.state.toggle_zoom();
                self.state.initial_placement.set(Some(status));
            }
        }
    }

    fn toggle_fullscreen(&self) {
        if unsafe { IsWindowVisible(self.0.hwnd).as_bool() } {
            self.0.toggle_fullscreen();
        } else if let Some(mut status) = self.state.initial_placement.take() {
            status.state = WindowOpenState::Fullscreen;
            self.state.initial_placement.set(Some(status));
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.state.is_fullscreen()
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.state.callbacks.request_frame.set(Some(callback));
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.state.callbacks.input.set(Some(callback));
    }

    fn on_prepare_key_input(&self, callback: Box<dyn FnMut(bool) -> bool>) {
        self.state.callbacks.prepare_key_input.set(Some(callback));
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0
            .state
            .callbacks
            .active_status_change
            .set(Some(callback));
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.0.state.last_visibility.set(Some(self.0.visibility()));
        self.0.state.callbacks.visibility_change.set(Some(callback));
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0
            .state
            .callbacks
            .hovered_status_change
            .set(Some(callback));
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.state.callbacks.resize.set(Some(callback));
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.display_changed.set(Some(callback));
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.moved.set(Some(callback));
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.state.callbacks.should_close.set(Some(callback));
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.state.callbacks.close.set(Some(callback));
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.0
            .state
            .callbacks
            .hit_test_window_control
            .set(Some(callback));
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0
            .state
            .callbacks
            .appearance_changed
            .set(Some(callback));
    }

    fn draw(&self, scene: &Scene) {
        self.state
            .renderer
            .borrow_mut()
            .draw(scene, self.state.background_appearance.get())
            .log_err();
    }

    #[cfg(any(test, feature = "test-support"))]
    fn render_to_image(&self, scene: &Scene) -> anyhow::Result<image::RgbaImage> {
        self.state
            .renderer
            .borrow_mut()
            .render_to_image(scene, self.state.background_appearance.get())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.state.renderer.borrow().sprite_atlas()
    }

    fn get_raw_handle(&self) -> HWND {
        self.0.hwnd
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.state.renderer.borrow().gpu_specs().log_err()
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        let scale_factor = self.state.scale_factor.get();
        let caret_position = POINT {
            x: (bounds.origin.x.as_f32() * scale_factor) as i32,
            y: (bounds.origin.y.as_f32() * scale_factor) as i32
                + ((bounds.size.height.as_f32() * scale_factor) as i32 / 2),
        };

        self.0.update_ime_position(self.0.hwnd, caret_position);
    }

    fn play_system_bell(&self) {
        // MB_OK: The sound specified as the Windows Default Beep sound.
        let _ = unsafe { MessageBeep(MB_OK) };
    }

    fn a11y_init(&self, callbacks: gpui::A11yCallbacks) {
        let action_handler = A11yActionHandler(callbacks.action);
        let is_focused = unsafe { GetForegroundWindow() } == self.0.hwnd;

        let adapter = accesskit_windows::Adapter::new(
            accesskit_windows::HWND(self.0.hwnd.0),
            is_focused,
            action_handler,
        );

        let activation_handler = A11yActivationHandler {
            callback: callbacks.activation,
        };

        *self.state.a11y.borrow_mut() = Some(A11yState {
            adapter,
            activation_handler,
        });
    }

    fn a11y_tree_update(&self, tree_update: accesskit::TreeUpdate) {
        let events = {
            let mut a11y = self.state.a11y.borrow_mut();
            a11y.as_mut()
                .and_then(|a11y| a11y.adapter.update_if_active(|| tree_update))
        };
        // The borrow must be dropped before raising events, because
        // `events.raise()` calls `UiaRaiseAutomationPropertyChangedEvent`
        // which may send a nested `WM_GETOBJECT` back into this window
        // procedure, re-entering `handle_wm_getobject` which also borrows
        // `self.state.a11y`.
        if let Some(events) = events {
            events.raise();
        }
    }

    fn a11y_update_window_bounds(&self) {
        // Windows UIA handles window bounds tracking automatically.
    }
}

pub(crate) struct A11yState {
    pub(crate) adapter: accesskit_windows::Adapter,
    pub(crate) activation_handler: A11yActivationHandler,
}

pub(crate) struct A11yActivationHandler {
    callback: Box<dyn Fn() -> Option<accesskit::TreeUpdate> + Send + 'static>,
}

impl accesskit::ActivationHandler for A11yActivationHandler {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        (self.callback)()
    }
}

struct A11yActionHandler(Box<dyn Fn(accesskit::ActionRequest) + Send + 'static>);

impl accesskit::ActionHandler for A11yActionHandler {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        (self.0)(request);
    }
}

#[implement(IDropTarget)]
struct WindowsDragDropHandler(pub Rc<WindowsWindowInner>);

impl WindowsDragDropHandler {
    fn handle_drag_drop(&self, input: PlatformInput) {
        if let Some(mut func) = self.0.state.callbacks.input.take() {
            func(input);
            self.0.state.callbacks.input.set(Some(func));
        }
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for WindowsDragDropHandler_Impl {
    fn DragEnter(
        &self,
        pdataobj: windows::core::Ref<IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            let idata_obj = pdataobj.ok()?;
            let config = FORMATETC {
                cfFormat: CF_HDROP.0,
                ptd: std::ptr::null_mut() as _,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as _,
            };
            let cursor_position = POINT { x: pt.x, y: pt.y };
            if idata_obj.QueryGetData(&config as _) == S_OK {
                *pdweffect = DROPEFFECT_COPY;
                let Some(mut idata) = idata_obj.GetData(&config as _).log_err() else {
                    return Ok(());
                };
                if idata.u.hGlobal.is_invalid() {
                    return Ok(());
                }
                let hdrop = HDROP(idata.u.hGlobal.0);
                let mut paths = SmallVec::<[PathBuf; 2]>::new();
                with_file_names(hdrop, |file_name| {
                    if let Some(path) = PathBuf::from_str(&file_name).log_err() {
                        paths.push(path);
                    }
                });
                ReleaseStgMedium(&mut idata);
                let mut cursor_position = cursor_position;
                ScreenToClient(self.0.hwnd, &mut cursor_position)
                    .ok()
                    .log_err();
                let scale_factor = self.0.state.scale_factor.get();
                let input = PlatformInput::FileDrop(FileDropEvent::Entered {
                    position: logical_point(
                        cursor_position.x as f32,
                        cursor_position.y as f32,
                        scale_factor,
                    ),
                    paths: ExternalPaths(paths),
                });
                self.handle_drag_drop(input);
            } else {
                *pdweffect = DROPEFFECT_NONE;
            }
            self.0
                .drop_target_helper
                .DragEnter(self.0.hwnd, idata_obj, &cursor_position, *pdweffect)
                .log_err();
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let mut cursor_position = POINT { x: pt.x, y: pt.y };
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
            self.0
                .drop_target_helper
                .DragOver(&cursor_position, *pdweffect)
                .log_err();
            ScreenToClient(self.0.hwnd, &mut cursor_position)
                .ok()
                .log_err();
        }
        let scale_factor = self.0.state.scale_factor.get();
        let input = PlatformInput::FileDrop(FileDropEvent::Pending {
            position: logical_point(
                cursor_position.x as f32,
                cursor_position.y as f32,
                scale_factor,
            ),
        });
        self.handle_drag_drop(input);

        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        unsafe {
            self.0.drop_target_helper.DragLeave().log_err();
        }
        let input = PlatformInput::FileDrop(FileDropEvent::Exited);
        self.handle_drag_drop(input);

        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: windows::core::Ref<IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let idata_obj = pdataobj.ok()?;
        let mut cursor_position = POINT { x: pt.x, y: pt.y };
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
            self.0
                .drop_target_helper
                .Drop(idata_obj, &cursor_position, *pdweffect)
                .log_err();
            ScreenToClient(self.0.hwnd, &mut cursor_position)
                .ok()
                .log_err();
        }
        let scale_factor = self.0.state.scale_factor.get();
        let input = PlatformInput::FileDrop(FileDropEvent::Submit {
            position: logical_point(
                cursor_position.x as f32,
                cursor_position.y as f32,
                scale_factor,
            ),
        });
        self.handle_drag_drop(input);

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClickState {
    button: Cell<MouseButton>,
    last_click: Cell<Instant>,
    last_position: Cell<Point<DevicePixels>>,
    double_click_spatial_tolerance_width: Cell<i32>,
    double_click_spatial_tolerance_height: Cell<i32>,
    double_click_interval: Cell<Duration>,
    pub(crate) current_count: Cell<usize>,
}

impl ClickState {
    pub fn new() -> Self {
        let double_click_spatial_tolerance_width = unsafe { GetSystemMetrics(SM_CXDOUBLECLK) };
        let double_click_spatial_tolerance_height = unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
        let double_click_interval = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64);

        ClickState {
            button: Cell::new(MouseButton::Left),
            last_click: Cell::new(Instant::now()),
            last_position: Cell::new(Point::default()),
            double_click_spatial_tolerance_width: Cell::new(double_click_spatial_tolerance_width),
            double_click_spatial_tolerance_height: Cell::new(double_click_spatial_tolerance_height),
            double_click_interval: Cell::new(double_click_interval),
            current_count: Cell::new(0),
        }
    }

    /// update self and return the needed click count
    pub fn update(&self, button: MouseButton, new_position: Point<DevicePixels>) -> usize {
        if self.button.get() == button && self.is_double_click(new_position) {
            self.current_count.update(|it| it + 1);
        } else {
            self.current_count.set(1);
        }
        self.last_click.set(Instant::now());
        self.last_position.set(new_position);
        self.button.set(button);

        self.current_count.get()
    }

    pub fn system_update(&self, wparam: usize) {
        match wparam {
            // SPI_SETDOUBLECLKWIDTH
            29 => self
                .double_click_spatial_tolerance_width
                .set(unsafe { GetSystemMetrics(SM_CXDOUBLECLK) }),
            // SPI_SETDOUBLECLKHEIGHT
            30 => self
                .double_click_spatial_tolerance_height
                .set(unsafe { GetSystemMetrics(SM_CYDOUBLECLK) }),
            // SPI_SETDOUBLECLICKTIME
            32 => self
                .double_click_interval
                .set(Duration::from_millis(unsafe { GetDoubleClickTime() } as u64)),
            _ => {}
        }
    }

    #[inline]
    fn is_double_click(&self, new_position: Point<DevicePixels>) -> bool {
        let diff = self.last_position.get() - new_position;

        self.last_click.get().elapsed() < self.double_click_interval.get()
            && diff.x.0.abs() <= self.double_click_spatial_tolerance_width.get()
            && diff.y.0.abs() <= self.double_click_spatial_tolerance_height.get()
    }
}

#[derive(Copy, Clone)]
struct StyleAndBounds {
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
}

#[repr(C)]
struct WINDOWCOMPOSITIONATTRIBDATA {
    attrib: u32,
    pv_data: *mut std::ffi::c_void,
    cb_data: usize,
}

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

type Color = (u8, u8, u8, u8);

#[derive(Debug, Default, Clone)]
pub(crate) struct WindowBorderOffset {
    pub(crate) width_offset: Cell<i32>,
    pub(crate) height_offset: Cell<i32>,
}

impl WindowBorderOffset {
    pub(crate) fn update(&self, hwnd: HWND) -> anyhow::Result<()> {
        let window_rect = unsafe {
            let mut rect = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rect)?;
            rect
        };
        let client_rect = unsafe {
            let mut rect = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect)?;
            rect
        };
        self.width_offset
            .set((window_rect.right - window_rect.left) - (client_rect.right - client_rect.left));
        self.height_offset
            .set((window_rect.bottom - window_rect.top) - (client_rect.bottom - client_rect.top));
        Ok(())
    }
}

#[derive(Clone)]
struct WindowOpenStatus {
    placement: WINDOWPLACEMENT,
    state: WindowOpenState,
}

const WINDOW_CLASS_NAME: PCWSTR = w!("Zed::Window");

fn register_window_class(icon_handle: HICON) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_procedure),
            hIcon: icon_handle,
            lpszClassName: PCWSTR(WINDOW_CLASS_NAME.as_ptr()),
            style: CS_HREDRAW | CS_VREDRAW,
            hInstance: get_module_handle().into(),
            hbrBackground: unsafe { CreateSolidBrush(COLORREF(0x00000000)) },
            ..Default::default()
        };
        unsafe { RegisterClassW(&wc) };
    });
}

unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let window_params = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        let window_creation_context = window_params.lpCreateParams as *mut WindowCreateContext;
        let window_creation_context = unsafe { &mut *window_creation_context };
        return match WindowsWindowInner::new(window_creation_context, hwnd, window_params) {
            Ok(window_state) => {
                let weak = Box::new(Rc::downgrade(&window_state));
                unsafe { set_window_long(hwnd, GWLP_USERDATA, Box::into_raw(weak) as isize) };
                window_creation_context.inner = Some(Ok(window_state));
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            Err(error) => {
                window_creation_context.inner = Some(Err(error));
                LRESULT(0)
            }
        };
    }

    let ptr = unsafe { get_window_long(hwnd, GWLP_USERDATA) } as *mut Weak<WindowsWindowInner>;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let inner = unsafe { &*ptr };
    let result = if let Some(inner) = inner.upgrade() {
        if msg == WM_NCDESTROY {
            inner.local_file_drop.retire(hwnd);
            inner.dialog_owner.close();
        }
        inner.handle_msg(hwnd, msg, wparam, lparam)
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    };

    if msg == WM_NCDESTROY {
        unsafe { set_window_long(hwnd, GWLP_USERDATA, 0) };
        unsafe { drop(Box::from_raw(ptr)) };
    }

    result
}

pub(crate) fn window_from_hwnd(hwnd: HWND) -> Option<Rc<WindowsWindowInner>> {
    if hwnd.is_invalid() {
        return None;
    }

    let ptr = unsafe { get_window_long(hwnd, GWLP_USERDATA) } as *mut Weak<WindowsWindowInner>;
    if !ptr.is_null() {
        let inner = unsafe { &*ptr };
        inner.upgrade()
    } else {
        None
    }
}

fn get_module_handle() -> HMODULE {
    unsafe {
        let mut h_module = std::mem::zeroed();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::w!("ZedModule"),
            &mut h_module,
        )
        .expect("Unable to get module handle"); // this should never fail

        h_module
    }
}

fn register_drag_drop(window: &Rc<WindowsWindowInner>) -> Result<()> {
    let window_handle = window.hwnd;
    let handler = WindowsDragDropHandler(window.clone());
    // The lifetime of `IDropTarget` is handled by Windows, it won't release until
    // we call `RevokeDragDrop`.
    // So, it's safe to drop it here.
    let drag_drop_handler: IDropTarget = handler.into();
    unsafe {
        RegisterDragDrop(window_handle, &drag_drop_handler)
            .context("unable to register drag-drop event")?;
    }
    Ok(())
}

fn calculate_window_rect(bounds: Bounds<DevicePixels>, border_offset: &WindowBorderOffset) -> RECT {
    // NOTE:
    // The reason we're not using `AdjustWindowRectEx()` here is
    // that the size reported by this function is incorrect.
    // You can test it, and there are similar discussions online.
    // See: https://stackoverflow.com/questions/12423584/how-to-set-exact-client-size-for-overlapped-window-winapi
    //
    // So we manually calculate these values here.
    let mut rect = RECT {
        left: bounds.left().0,
        top: bounds.top().0,
        right: bounds.right().0,
        bottom: bounds.bottom().0,
    };
    let left_offset = border_offset.width_offset.get() / 2;
    let top_offset = border_offset.height_offset.get() / 2;
    let right_offset = border_offset.width_offset.get() - left_offset;
    let bottom_offset = border_offset.height_offset.get() - top_offset;
    rect.left -= left_offset;
    rect.top -= top_offset;
    rect.right += right_offset;
    rect.bottom += bottom_offset;
    rect
}

fn calculate_client_rect(
    rect: RECT,
    border_offset: &WindowBorderOffset,
    scale_factor: f32,
) -> Bounds<Pixels> {
    let left_offset = border_offset.width_offset.get() / 2;
    let top_offset = border_offset.height_offset.get() / 2;
    let right_offset = border_offset.width_offset.get() - left_offset;
    let bottom_offset = border_offset.height_offset.get() - top_offset;
    let left = rect.left + left_offset;
    let top = rect.top + top_offset;
    let right = rect.right - right_offset;
    let bottom = rect.bottom - bottom_offset;
    let physical_size = size(DevicePixels(right - left), DevicePixels(bottom - top));
    Bounds {
        origin: logical_point(left as f32, top as f32, scale_factor),
        size: physical_size.to_pixels(scale_factor),
    }
}

/// Move a popup into its selected work area without replacing its content size.
/// An oversized popup is anchored at the area's origin; its owner decides how
/// to present or constrain that content, not the generic top-level fallback.
fn popup_placement(requested: Bounds<Pixels>, work_area: Bounds<Pixels>) -> Result<Bounds<Pixels>> {
    anyhow::ensure!(
        [
            requested.origin.x,
            requested.origin.y,
            requested.size.width,
            requested.size.height,
            work_area.left(),
            work_area.top(),
            work_area.right(),
            work_area.bottom()
        ]
        .into_iter()
        .all(|value| f32::from(value).is_finite())
            && requested.size.width > px(0.)
            && requested.size.height > px(0.)
            && work_area.size.width > px(0.)
            && work_area.size.height > px(0.),
        "Invalid popup placement geometry"
    );
    let clamp = |origin: Pixels, start: Pixels, end: Pixels| {
        px(f32::from(origin).clamp(f32::from(start), f32::from(end).max(f32::from(start))))
    };
    Ok(Bounds::new(
        point(
            clamp(
                requested.origin.x,
                work_area.left(),
                work_area.right() - requested.size.width,
            ),
            clamp(
                requested.origin.y,
                work_area.top(),
                work_area.bottom() - requested.size.height,
            ),
        ),
        requested.size,
    ))
}

fn retrieve_window_placement(
    hwnd: HWND,
    display: WindowsDisplay,
    bounds: Bounds<Pixels>,
    border_offset: &WindowBorderOffset,
) -> Result<WINDOWPLACEMENT> {
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    unsafe { GetWindowPlacement(hwnd, &mut placement)? };
    // Both construction and first show use the same resolved bounds. In
    // particular, a popup must never re-enter the top-level default-size policy.
    // `bounds` is expressed in logical pixels for `display`, so it must be converted
    // to device pixels using that display's own scale factor. The window's current
    // scale factor can't be used here: an overlapped window is created with
    // `CW_USEDEFAULT`, so it may still be sitting on whichever monitor Windows
    // picked by default, which can have a different DPI than `display`
    // and would otherwise throw off the physical position (e.g. leaving the window
    // partially off-screen when moved to a monitor with a different scale factor).
    let bounds = bounds.to_device_pixels(display.scale_factor());
    placement.rcNormalPosition = calculate_window_rect(bounds, border_offset);
    Ok(placement)
}

fn dwm_set_window_composition_attribute(hwnd: HWND, backdrop_type: u32) -> Result<bool> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut version) }
        .ok()
        .context("Querying Windows version for DWM backdrop")?;

    // DWMWA_SYSTEMBACKDROP_TYPE is available only on version 22621 or later
    if version.dwBuildNumber < 22621 {
        return Ok(false);
    }

    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop_type as *const _ as *const _,
            std::mem::size_of_val(&backdrop_type) as u32,
        )
        .context("Setting DWM system backdrop")?;
    }
    Ok(true)
}

fn set_window_composition_attribute(hwnd: HWND, color: Option<Color>, state: u32) -> Result<bool> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut version) }
        .ok()
        .context("Querying Windows version for composition attribute")?;

    if version.dwBuildNumber < 17763 {
        return Ok(false);
    }

    unsafe {
        type SetWindowCompositionAttributeType =
            unsafe extern "system" fn(HWND, *mut WINDOWCOMPOSITIONATTRIBDATA) -> BOOL;
        let module_name = PCSTR::from_raw(c"user32.dll".as_ptr() as *const u8);
        let user32 = GetModuleHandleA(module_name).context("Unable to get user32.dll handle")?;
        let func_name = PCSTR::from_raw(c"SetWindowCompositionAttribute".as_ptr() as *const u8);
        let Some(raw_set_window_composition_attribute) = GetProcAddress(user32, func_name) else {
            return Ok(false);
        };
        let set_window_composition_attribute: SetWindowCompositionAttributeType =
            std::mem::transmute(raw_set_window_composition_attribute);
        let mut color = color.unwrap_or_default();
        let is_acrylic = state == 4;
        if is_acrylic && color.3 == 0 {
            color.3 = 1;
        }
        let accent = AccentPolicy {
            accent_state: state,
            accent_flags: if is_acrylic { 0 } else { 2 },
            gradient_color: (color.0 as u32)
                | ((color.1 as u32) << 8)
                | ((color.2 as u32) << 16)
                | ((color.3 as u32) << 24),
            animation_id: 0,
        };
        let mut data = WINDOWCOMPOSITIONATTRIBDATA {
            attrib: 0x13,
            pv_data: &accent as *const _ as *mut _,
            cb_data: std::mem::size_of::<AccentPolicy>(),
        };
        anyhow::ensure!(
            set_window_composition_attribute(hwnd, &mut data as *mut _ as _).as_bool(),
            "SetWindowCompositionAttribute rejected accent state {state}",
        );
    }
    Ok(true)
}

#[cfg(test)]
#[path = "window_appearance_tests.rs"]
mod window_appearance_tests;

// When the platform title bar is hidden, Windows may think that our application is meant to appear 'fullscreen'
// and will stop the taskbar from appearing on top of our window. Prevent this.
// https://devblogs.microsoft.com/oldnewthing/20250522-00/?p=111211
fn set_non_rude_hwnd(hwnd: HWND, non_rude: bool) {
    if non_rude {
        unsafe { SetPropW(hwnd, w!("NonRudeHWND"), Some(HANDLE(1 as _))) }.log_err();
    } else {
        unsafe { RemovePropW(hwnd, w!("NonRudeHWND")) }.log_err();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClickState, initial_show_command, initial_window_styles, resize_hit_test,
        window_control_style,
    };
    use gpui::{DevicePixels, MouseButton, ResizeEdge, WindowKind, point};
    use std::time::Duration;
    use windows::Win32::UI::WindowsAndMessaging::*;

    #[test]
    fn first_show_uses_requested_activation_instead_of_hidden_placement() {
        assert_eq!(initial_show_command(true), SW_SHOWNORMAL);
        assert_eq!(initial_show_command(false), SW_SHOWNOACTIVATE);
    }

    #[test]
    fn popup_fallback_moves_only_the_origin_even_beyond_monitor_edges() {
        use gpui::{Bounds, point, px, size};
        let work = Bounds::new(point(px(-1920.), px(-1080.)), size(px(1920.), px(1040.)));
        for (requested, expected) in [
            ([-2100., -1200.], [-1920., -1080.]),
            ([-10., -10.], [-140., -88.]),
            ([5000., 5000.], [-140., -88.]),
            ([-1000., -500.], [-1000., -500.]),
        ] {
            let bounds = Bounds::new(
                point(px(requested[0]), px(requested[1])),
                size(px(140.), px(48.)),
            );
            let placed = super::popup_placement(bounds, work).unwrap();
            assert_eq!(placed.size, bounds.size);
            assert_eq!(placed.origin, point(px(expected[0]), px(expected[1])));
        }
        let oversized = Bounds::new(point(px(0.), px(0.)), size(px(2200.), px(1200.)));
        let placed = super::popup_placement(oversized, work).unwrap();
        assert_eq!(placed.size, oversized.size);
        assert_eq!(placed.origin, work.origin);
        let invalid = Bounds::new(point(px(f32::NAN), px(0.)), size(px(140.), px(48.)));
        assert!(super::popup_placement(invalid, work).is_err());
    }

    #[test]
    fn normal_windows_get_only_the_title_bar_they_requested() {
        let controls = WS_SYSMENU | WS_THICKFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX;
        assert_eq!(
            initial_window_styles(WindowKind::Normal, false, true, true),
            (WS_EX_APPWINDOW, controls | WS_CAPTION),
        );
        assert_eq!(
            initial_window_styles(WindowKind::Normal, true, true, true),
            (WS_EX_APPWINDOW, controls),
        );
        assert_eq!(
            initial_window_styles(WindowKind::PopUp, false, true, true),
            (WS_EX_TOOLWINDOW | WS_EX_TOPMOST, WS_POPUP),
        );
    }

    #[test]
    fn runtime_controls_preserve_unrelated_window_styles() {
        let base = WS_CAPTION | WS_SYSMENU | WS_POPUP | WS_VISIBLE;
        let enabled = window_control_style(base, true, true, true);
        assert_eq!(
            enabled,
            base | WS_THICKFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX
        );
        assert_eq!(
            window_control_style(enabled, false, true, false),
            base | WS_MINIMIZEBOX
        );
        assert_eq!(
            window_control_style(enabled, true, false, true),
            base | WS_THICKFRAME | WS_MAXIMIZEBOX
        );
        assert_eq!(window_control_style(enabled, false, false, false), base);
    }

    #[test]
    fn maximize_permission_is_independent_of_resize() {
        let base = WS_CAPTION | WS_SYSMENU | WS_VISIBLE;
        assert_eq!(
            window_control_style(base, true, true, false),
            base | WS_THICKFRAME | WS_MINIMIZEBOX
        );
        assert_eq!(
            window_control_style(base, false, false, true),
            base | WS_MAXIMIZEBOX
        );
    }

    #[test]
    fn resize_edges_use_native_screen_directions() {
        for (edge, hit) in [
            (ResizeEdge::Top, HTTOP),
            (ResizeEdge::TopRight, HTTOPRIGHT),
            (ResizeEdge::Right, HTRIGHT),
            (ResizeEdge::BottomRight, HTBOTTOMRIGHT),
            (ResizeEdge::Bottom, HTBOTTOM),
            (ResizeEdge::BottomLeft, HTBOTTOMLEFT),
            (ResizeEdge::Left, HTLEFT),
            (ResizeEdge::TopLeft, HTTOPLEFT),
        ] {
            assert_eq!(resize_hit_test(edge), hit);
        }
    }

    #[test]
    fn test_double_click_interval() {
        let state = ClickState::new();
        assert_eq!(
            state.update(MouseButton::Left, point(DevicePixels(0), DevicePixels(0))),
            1
        );
        assert_eq!(
            state.update(MouseButton::Right, point(DevicePixels(0), DevicePixels(0))),
            1
        );
        assert_eq!(
            state.update(MouseButton::Left, point(DevicePixels(0), DevicePixels(0))),
            1
        );
        assert_eq!(
            state.update(MouseButton::Left, point(DevicePixels(0), DevicePixels(0))),
            2
        );
        state
            .last_click
            .update(|it| it - Duration::from_millis(700));
        assert_eq!(
            state.update(MouseButton::Left, point(DevicePixels(0), DevicePixels(0))),
            1
        );
    }

    #[test]
    fn test_double_click_spatial_tolerance() {
        let state = ClickState::new();
        assert_eq!(
            state.update(MouseButton::Left, point(DevicePixels(-3), DevicePixels(0))),
            1
        );
        assert_eq!(
            state.update(MouseButton::Left, point(DevicePixels(0), DevicePixels(3))),
            2
        );
        assert_eq!(
            state.update(MouseButton::Right, point(DevicePixels(3), DevicePixels(2))),
            1
        );
        assert_eq!(
            state.update(MouseButton::Right, point(DevicePixels(10), DevicePixels(0))),
            1
        );
    }
}
