use std::{cell::Cell, rc::Rc, sync::atomic::Ordering};

use anyhow::Context as _;
use gpui_util::ResultExt;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::SystemServices::*,
        UI::{
            Controls::*,
            HiDpi::*,
            Input::{Ime::*, KeyboardAndMouse::*},
            WindowsAndMessaging::*,
        },
    },
    core::PCWSTR,
};

use crate::*;
use gpui::*;

pub(crate) const WM_GPUI_CURSOR_STYLE_CHANGED: u32 = WM_USER + 1;
pub(crate) const WM_GPUI_CLOSE_ONE_WINDOW: u32 = WM_USER + 2;
pub(crate) const WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD: u32 = WM_USER + 3;
pub(crate) const WM_GPUI_DOCK_MENU_ACTION: u32 = WM_USER + 4;
pub(crate) const WM_GPUI_FORCE_UPDATE_WINDOW: u32 = WM_USER + 5;
pub(crate) const WM_GPUI_KEYBOARD_LAYOUT_CHANGED: u32 = WM_USER + 6;
pub(crate) const WM_GPUI_GPU_DEVICE_LOST: u32 = WM_USER + 7;
pub(crate) const WM_GPUI_DISPLAYS_CHANGED: u32 = WM_USER + 10;

const SIZE_MOVE_LOOP_TIMER_ID: usize = 1;

thread_local! {
    // TranslateMessage owns the keyboard layout's dead-key state on this
    // thread. The next text key must reach it even when it produces ASCII.
    static NATIVE_CHARACTERS: Cell<NativeCharacterState> = const {
        Cell::new(NativeCharacterState {
            dead_key_pending: false,
            ordinary_dead_keys: crate::native_characters::OrdinaryDeadKeys::new(),
        })
    };
}

#[derive(Clone, Copy, Default)]
struct NativeCharacterState {
    dead_key_pending: bool,
    ordinary_dead_keys: crate::native_characters::OrdinaryDeadKeys,
}

impl NativeCharacterState {
    fn prefers_text(self, has_text: bool, prefer_character_input: bool) -> bool {
        prefer_character_input || (self.dead_key_pending && has_text)
    }

    fn key_dispatched(&mut self, has_text: bool, dead_key: bool, handled: bool) {
        if !handled && has_text {
            self.dead_key_pending = dead_key;
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct NativeModalLoops {
    size_move: bool,
    menus: u32,
}

impl NativeModalLoops {
    pub(crate) fn in_size_move(self) -> bool {
        self.size_move
    }

    fn active(self) -> bool {
        self.size_move || self.menus > 0
    }

    fn update(&mut self, message: u32) {
        match message {
            WM_ENTERSIZEMOVE => self.size_move = true,
            WM_EXITSIZEMOVE => self.size_move = false,
            WM_ENTERMENULOOP => self.menus = self.menus.saturating_add(1),
            WM_EXITMENULOOP => self.menus = self.menus.saturating_sub(1),
            _ => {}
        }
    }
}

fn cancel_marked_text(
    marked: Option<std::ops::Range<usize>>,
    replace: impl FnOnce(std::ops::Range<usize>),
) {
    if let Some(range) = marked.filter(|range| !range.is_empty()) {
        replace(range);
    }
}

fn decode_character(
    pending: &mut Option<(u16, (u64, u64))>,
    code: u16,
    generation: Option<(u64, u64)>,
) -> Option<String> {
    let previous = pending.take();
    let generation = generation?;
    match code {
        0xD800..=0xDBFF => {
            *pending = Some((code, generation));
            None
        }
        0xDC00..=0xDFFF => {
            let (high, previous_generation) = previous?;
            (generation == previous_generation)
                .then(|| String::from_utf16(&[high, code]).ok())
                .flatten()
        }
        _ => char::from_u32(code as u32)
            .filter(|character| !character.is_control())
            .map(|character| character.to_string()),
    }
}

fn mouse_activate_result(style: u32) -> isize {
    if style & WS_EX_NOACTIVATE.0 != 0 {
        MA_NOACTIVATE as isize
    } else {
        MA_ACTIVATE as isize
    }
}

/// Coordinates window draws on the UI thread. Owned by the platform and
/// shared with every window (like `WindowsPlatformState::cursor_visible`),
/// because the coordination is inherently cross-window: while window A is
/// drawing, a re-entrant paint request for window B must be deferred.
pub(crate) struct DrawCoordinator {
    /// Whether some window is currently inside `draw_window`. Win32 can
    /// re-enter the window procedure while a draw is in progress (e.g.
    /// cross-thread `SendMessage` dispatch during message pumping, or modal
    /// message loops entered by COM calls), and drawing re-entrantly would
    /// nest GPUI draws. Nested draws are wasted work whose output is
    /// immediately redrawn, so we defer them instead.
    drawing: Cell<bool>,
}

impl DrawCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            drawing: Cell::new(false),
        }
    }

    fn try_begin_draw(&self) -> Option<DrawWindowGuard<'_>> {
        // This only covers the `draw_window` span (which extends past the GPUI
        // draw, through presentation and IME updates). Requests that arrive
        // re-entrantly during GPUI-initiated draws (e.g. key dispatch or
        // opening a window draws synchronously) are deferred by GPUI's
        // `on_request_frame` callback itself, which no-ops in that case.
        if self.drawing.get() {
            None
        } else {
            self.drawing.set(true);
            Some(DrawWindowGuard { coordinator: self })
        }
    }
}

struct DrawWindowGuard<'a> {
    coordinator: &'a DrawCoordinator,
}

impl Drop for DrawWindowGuard<'_> {
    fn drop(&mut self) {
        self.coordinator.drawing.set(false);
    }
}

impl WindowsWindowInner {
    pub(crate) fn handle_msg(
        self: &Rc<Self>,
        handle: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        self.native_drag.observe(msg, wparam, lparam);
        // Native focus may leave and return before GPUI's deferred observers run.
        self.state
            .native_character_authority
            .observe(msg, wparam, lparam);
        // A delayed drop must not target a later geometry or activation, even
        // if a subsequent message restores the same numerical bounds.
        if matches!(
            msg,
            WM_MOVE
                | WM_SIZE
                | WM_DPICHANGED
                | WM_DISPLAYCHANGE
                | WM_ACTIVATE
                | WM_KILLFOCUS
                | WM_SHOWWINDOW
                | WM_DESTROY
                | WM_CLOSE
        ) {
            self.local_file_drop.invalidate();
        }
        let handled = match msg {
            WM_DROPFILES => {
                self.handle_local_file_drop(windows::Win32::UI::Shell::HDROP(wparam.0 as *mut _));
                Some(0)
            }
            // `DefWindowProc` answers `MA_NOACTIVATE` for a left click on `HTCAPTION`.
            // The activation is only triggered when `DefWindowProc` handles the following `WM_NCLBUTTONDOWN`.
            // The GPUI event is dispatched in between, so a click handler runs while `active_window` is still
            // whichever window was active before the click. If that handler consumes the
            // press, `DefWindowProc` never sees it, so the window is never activated at all.
            // So, let's eagerly activate the window.
            WM_MOUSEACTIVATE if self.mouse_passthrough.get() => Some(MA_NOACTIVATE as isize),
            WM_MOUSEACTIVATE => {
                let style = unsafe { get_window_long(handle, GWL_EXSTYLE) } as u32;
                Some(mouse_activate_result(style))
            }
            WM_NCHITTEST if self.mouse_passthrough.get() => Some(HTTRANSPARENT as isize),
            WM_SYSCOMMAND if wparam.0 as u32 & 0xfff0 == SC_MAXIMIZE && !self.is_maximizable() => {
                Some(0)
            }
            WM_ACTIVATE => self.handle_activate_msg(wparam, lparam),
            WM_KILLFOCUS => {
                self.state.pending_surrogate.set(None);
                None
            }
            WM_CREATE => self.handle_create_msg(handle),
            WM_MOVE => self.handle_move_msg(handle, lparam),
            WM_SIZE => self.handle_size_msg(wparam, lparam),
            WM_GETMINMAXINFO => self.handle_get_min_max_info_msg(lparam),
            WM_SIZING => self.handle_sizing_msg(wparam, lparam),
            WM_ENTERSIZEMOVE | WM_EXITSIZEMOVE | WM_ENTERMENULOOP | WM_EXITMENULOOP => {
                let mut loops = self.state.modal_loops.get();
                let was_active = loops.active();
                loops.update(msg);
                self.state.modal_loops.set(loops);
                match (was_active, loops.active()) {
                    (false, true) => self.handle_size_move_loop(handle),
                    (true, false) => self.handle_size_move_loop_exit(handle),
                    _ => None,
                }
            }
            WM_TIMER => self.handle_timer_msg(handle, wparam),
            WM_NCCALCSIZE => self.handle_calc_client_size(handle, wparam, lparam),
            WM_DPICHANGED => self.handle_dpi_changed_msg(handle, wparam, lparam),
            WM_DISPLAYCHANGE => self.handle_display_change_msg(handle),
            WM_NCHITTEST => self.handle_hit_test_msg(handle, lparam),
            WM_PAINT => self.handle_paint_msg(handle),
            WM_CLOSE => self.handle_close_msg(),
            WM_DESTROY => self.handle_destroy_msg(handle),
            WM_MOUSEMOVE => self.handle_mouse_move_msg(handle, lparam, wparam),
            WM_MOUSELEAVE | WM_NCMOUSELEAVE => self.handle_mouse_leave_msg(),
            WM_NCMOUSEMOVE => self.handle_nc_mouse_move_msg(handle, lparam),
            // Treat double click as a second single click, since we track the double clicks ourselves.
            // If you don't interact with any elements, this will fall through to the windows default
            // behavior of toggling whether the window is maximized.
            WM_NCLBUTTONDBLCLK | WM_NCLBUTTONDOWN => {
                self.handle_nc_mouse_down_msg(handle, MouseButton::Left, wparam, lparam)
            }
            WM_NCRBUTTONDOWN => {
                self.handle_nc_mouse_down_msg(handle, MouseButton::Right, wparam, lparam)
            }
            WM_NCMBUTTONDOWN => {
                self.handle_nc_mouse_down_msg(handle, MouseButton::Middle, wparam, lparam)
            }
            WM_NCLBUTTONUP => {
                self.handle_nc_mouse_up_msg(handle, MouseButton::Left, wparam, lparam)
            }
            WM_NCRBUTTONUP => {
                self.handle_nc_mouse_up_msg(handle, MouseButton::Right, wparam, lparam)
            }
            WM_NCMBUTTONUP => {
                self.handle_nc_mouse_up_msg(handle, MouseButton::Middle, wparam, lparam)
            }
            WM_LBUTTONDOWN => self.handle_mouse_down_msg(handle, MouseButton::Left, lparam),
            WM_RBUTTONDOWN => self.handle_mouse_down_msg(handle, MouseButton::Right, lparam),
            WM_MBUTTONDOWN => self.handle_mouse_down_msg(handle, MouseButton::Middle, lparam),
            WM_XBUTTONDOWN => {
                self.handle_xbutton_msg(handle, wparam, lparam, Self::handle_mouse_down_msg)
            }
            WM_LBUTTONUP => self.handle_mouse_up_msg(handle, MouseButton::Left, lparam),
            WM_RBUTTONUP => self.handle_mouse_up_msg(handle, MouseButton::Right, lparam),
            WM_MBUTTONUP => self.handle_mouse_up_msg(handle, MouseButton::Middle, lparam),
            WM_XBUTTONUP => {
                self.handle_xbutton_msg(handle, wparam, lparam, Self::handle_mouse_up_msg)
            }
            WM_MOUSEWHEEL => self.handle_mouse_wheel_msg(handle, wparam, lparam),
            WM_MOUSEHWHEEL => self.handle_mouse_horizontal_wheel_msg(handle, wparam, lparam),
            WM_SYSKEYUP => self.handle_syskeyup_msg(wparam, lparam),
            WM_KEYUP => self.handle_keyup_msg(wparam, lparam),
            WM_CHAR => self.handle_char_msg(wparam),
            WM_IME_STARTCOMPOSITION => self.handle_ime_position(handle),
            WM_IME_COMPOSITION => self.handle_ime_composition(handle, lparam),
            WM_IME_ENDCOMPOSITION => {
                self.cancel_ime_composition();
                None
            }
            WM_SETCURSOR => self.handle_set_cursor(handle, lparam),
            WM_SETTINGCHANGE => self.handle_system_settings_changed(handle, wparam, lparam),
            WM_INPUTLANGCHANGE => self.handle_input_language_changed(),
            WM_SHOWWINDOW => self.handle_window_visibility_changed(handle, wparam),
            DM_POINTERHITTEST => self.handle_dm_pointer_hit_test(wparam),
            WM_GETOBJECT => self.handle_wm_getobject(wparam, lparam),
            WM_GPUI_CURSOR_STYLE_CHANGED
            | WM_GPUI_FORCE_UPDATE_WINDOW
            | WM_GPUI_GPU_DEVICE_LOST => self.handle_gpui_events(handle, msg, wparam, lparam),
            _ => None,
        };
        if let Some(n) = handled {
            LRESULT(n)
        } else {
            unsafe { DefWindowProcW(handle, msg, wparam, lparam) }
        }
    }

    fn handle_move_msg(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        let origin = logical_point(
            lparam.signed_loword() as f32,
            lparam.signed_hiword() as f32,
            self.state.scale_factor.get(),
        );
        self.state.origin.set(origin);
        let size = self.state.logical_size.get();
        let center_x = origin.x.as_f32() + size.width.as_f32() / 2.;
        let center_y = origin.y.as_f32() + size.height.as_f32() / 2.;
        let monitor_bounds = self.state.display.get().bounds();
        if center_x < monitor_bounds.left().as_f32()
            || center_x > monitor_bounds.right().as_f32()
            || center_y < monitor_bounds.top().as_f32()
            || center_y > monitor_bounds.bottom().as_f32()
        {
            // center of the window may have moved to another monitor
            let monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONULL) };
            // minimize the window can trigger this event too, in this case,
            // monitor is invalid, we do nothing.
            if !monitor.is_invalid() && self.state.display.get().handle != monitor {
                // we will get the same monitor if we only have one
                self.set_display(WindowsDisplay::new(
                    WindowsDisplay::display_id_for_monitor(monitor),
                )?);
            }
        }
        if let Some(mut callback) = self.state.callbacks.moved.take() {
            callback();
            self.state.callbacks.moved.set(Some(callback));
        }
        Some(0)
    }

    fn native_resize_geometry(&self) -> Option<crate::native_resize::Geometry> {
        let mut outer = RECT::default();
        let mut client = RECT::default();
        // SAFETY: these messages are dispatched to this live HWND on its UI thread.
        unsafe {
            GetWindowRect(self.native_handle(), &mut outer).ok()?;
            GetClientRect(self.native_handle(), &mut client).ok()?;
        }
        crate::native_resize::Geometry::measure(
            [outer.left, outer.top, outer.right, outer.bottom],
            [client.right - client.left, client.bottom - client.top],
            self.state.decorations.get() == WindowDecorations::Client,
            unsafe { GetDpiForWindow(self.native_handle()) },
        )
    }

    fn native_resize_policy(&self) -> Option<NativeResizeConstraint> {
        if self.state.closing.get() || self.state.is_fullscreen() {
            return None;
        }
        if !self.is_resizable.get() {
            return Some(NativeResizeConstraint::Fixed);
        }
        // Release the borrow before calling the immutable policy. Native
        // message delivery can nest even though the selector itself is pure.
        self.state.resize_constraint.borrow().clone()
    }

    fn handle_sizing_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        if lparam.0 == 0 || self.state.is_maximized() {
            return None;
        }
        let policy = self.native_resize_policy()?;
        let geometry = self.native_resize_geometry()?;
        // SAFETY: WM_SIZING supplies a writable screen-coordinate RECT.
        unsafe {
            let proposed = &mut *(lparam.0 as *mut RECT);
            let rect = geometry
                .constrain(
                    [proposed.left, proposed.top, proposed.right, proposed.bottom],
                    u32::try_from(wparam.0).ok()?,
                    &policy,
                )
                .unwrap_or(geometry.current);
            *proposed = RECT {
                left: rect[0],
                top: rect[1],
                right: rect[2],
                bottom: rect[3],
            };
        }
        Some(1)
    }

    fn handle_get_min_max_info_msg(&self, lparam: LPARAM) -> Option<isize> {
        if lparam.0 == 0 {
            return None;
        }
        if let Some(policy) = self.native_resize_policy().filter(|policy| {
            // WM_GETMINMAXINFO also runs during SetWindowPlacement/SetWindowPos.
            // Non-resizable means the user cannot resize, not that a programmatic
            // content update must retain the old (or constructor-default) size.
            // WM_SIZING still applies Fixed, and native styles/hit testing reject
            // user resize even outside the size/move loop.
            !matches!(policy, NativeResizeConstraint::Fixed)
                || self.state.modal_loops.get().in_size_move()
        }) {
            let geometry = self.native_resize_geometry()?;
            let (min, max) = geometry
                .limits(&policy)
                .or_else(|| geometry.limits(&NativeResizeConstraint::Fixed))?;
            // SAFETY: WM_GETMINMAXINFO supplies writable tracking limits.
            // Keep ptMaxSize/ptMaxPosition native-owned for maximize and Snap.
            unsafe {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: min[0],
                    y: min[1],
                };
                info.ptMaxTrackSize = POINT {
                    x: max[0],
                    y: max[1],
                };
            }
            return Some(0);
        }
        let min_size = self.state.min_size?;
        let scale_factor = self.state.scale_factor.get();
        let boarder_offset = &self.state.border_offset;

        unsafe {
            let minmax_info = &mut *(lparam.0 as *mut MINMAXINFO);
            minmax_info.ptMinTrackSize.x = min_size.width.scale(scale_factor).as_f32() as i32
                + boarder_offset.width_offset.get();
            minmax_info.ptMinTrackSize.y = min_size.height.scale(scale_factor).as_f32() as i32
                + boarder_offset.height_offset.get();
        }
        Some(0)
    }

    fn handle_size_msg(self: &Rc<Self>, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        // Minimizing and restoring both arrive as `WM_SIZE`; the deferred report
        // reads `IsIconic` at delivery, so one call covers both directions.
        self.report_visibility();

        // Don't resize the renderer when the window is minimized, but record that it was minimized so
        // that on restore the swap chain can be recreated via `update_drawable_size_even_if_unchanged`.
        if wparam.0 == SIZE_MINIMIZED as usize {
            self.state
                .restore_from_minimized
                .set(self.state.callbacks.request_frame.take());
            return Some(0);
        }

        if self.state.decorations.get() == WindowDecorations::Client {
            self.state
                .border_offset
                .update(self.native_handle())
                .log_err();
        }

        let width = lparam.loword().max(1) as i32;
        let height = lparam.hiword().max(1) as i32;
        let new_size = size(DevicePixels(width), DevicePixels(height));

        let scale_factor = self.state.scale_factor.get();
        let mut should_resize_renderer = false;
        if let Some(restore_from_minimized) = self.state.restore_from_minimized.take() {
            self.state
                .callbacks
                .request_frame
                .set(Some(restore_from_minimized));
        } else {
            should_resize_renderer = true;
        }

        self.handle_size_change(new_size, scale_factor, should_resize_renderer);
        Some(0)
    }

    fn handle_size_change(
        &self,
        device_size: Size<DevicePixels>,
        scale_factor: f32,
        should_resize_renderer: bool,
    ) {
        let new_logical_size = device_size.to_pixels(scale_factor);

        self.state.logical_size.set(new_logical_size);
        if should_resize_renderer
            && let Err(e) = self.state.renderer.borrow_mut().resize(device_size)
        {
            log::error!("Failed to resize renderer, invalidating devices: {}", e);
            self.state
                .invalidate_devices
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(mut callback) = self.state.callbacks.resize.take() {
            callback(new_logical_size, scale_factor);
            self.state.callbacks.resize.set(Some(callback));
        }
    }

    fn handle_size_move_loop(&self, handle: HWND) -> Option<isize> {
        unsafe {
            let ret = SetTimer(
                Some(handle),
                SIZE_MOVE_LOOP_TIMER_ID,
                USER_TIMER_MINIMUM,
                None,
            );
            if ret == 0 {
                log::error!(
                    "unable to create timer: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        None
    }

    fn handle_size_move_loop_exit(&self, handle: HWND) -> Option<isize> {
        unsafe {
            KillTimer(Some(handle), SIZE_MOVE_LOOP_TIMER_ID).log_err();
        }
        None
    }

    fn handle_timer_msg(&self, handle: HWND, wparam: WPARAM) -> Option<isize> {
        if wparam.0 == SIZE_MOVE_LOOP_TIMER_ID {
            let mut runnables = self.main_receiver.clone().try_iter();
            while let Some(Ok(runnable)) = runnables.next() {
                WindowsDispatcher::execute_runnable(runnable);
            }
            self.handle_paint_msg(handle)
        } else {
            None
        }
    }

    fn handle_paint_msg(&self, handle: HWND) -> Option<isize> {
        self.draw_window(handle, false)
    }

    fn handle_close_msg(&self) -> Option<isize> {
        let mut callback = self.state.callbacks.should_close.take()?;
        let should_close = callback();
        self.state.callbacks.should_close.set(Some(callback));
        if should_close { None } else { Some(0) }
    }

    fn handle_destroy_msg(&self, handle: HWND) -> Option<isize> {
        self.state.closing.set(true);
        self.retire_native_activation();
        let callback = { self.state.callbacks.close.take() };
        // Re-enable parent window if this was a modal dialog
        if let Some(parent_hwnd) = self.parent_hwnd {
            unsafe {
                let _ = EnableWindow(parent_hwnd, true);
                let _ = SetForegroundWindow(parent_hwnd);
            }
        }

        if let Some(callback) = callback {
            callback();
        }
        unsafe {
            PostMessageW(
                Some(self.platform_window_handle),
                WM_GPUI_CLOSE_ONE_WINDOW,
                WPARAM(self.validation_number),
                LPARAM(handle.0 as isize),
            )
            .log_err();
        }
        Some(0)
    }

    fn handle_mouse_move_msg(&self, handle: HWND, lparam: LPARAM, wparam: WPARAM) -> Option<isize> {
        self.start_tracking_mouse(handle, TME_LEAVE);
        self.restore_cursor_after_hide();

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let scale_factor = self.state.scale_factor.get();

        let pressed_button = match MODIFIERKEYS_FLAGS(wparam.loword() as u32) {
            flags if flags.contains(MK_LBUTTON) => Some(MouseButton::Left),
            flags if flags.contains(MK_RBUTTON) => Some(MouseButton::Right),
            flags if flags.contains(MK_MBUTTON) => Some(MouseButton::Middle),
            flags if flags.contains(MK_XBUTTON1) => {
                Some(MouseButton::Navigate(NavigationDirection::Back))
            }
            flags if flags.contains(MK_XBUTTON2) => {
                Some(MouseButton::Navigate(NavigationDirection::Forward))
            }
            _ => None,
        };
        let x = lparam.signed_loword() as f32;
        let y = lparam.signed_hiword() as f32;
        let input = PlatformInput::MouseMove(MouseMoveEvent {
            position: logical_point(x, y, scale_factor),
            pressed_button,
            modifiers: current_modifiers(),
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_mouse_leave_msg(&self) -> Option<isize> {
        self.state.hovered.set(false);
        // The next window's `WM_SETCURSOR` picks its own cursor, so we just clear
        // the flag for tight `is_cursor_visible()` semantics.
        self.state.cursor_visible.store(true, Ordering::Relaxed);
        if let Some(mut callback) = self.state.callbacks.hovered_status_change.take() {
            callback(false);
            self.state
                .callbacks
                .hovered_status_change
                .set(Some(callback));
        }

        Some(0)
    }

    fn handle_syskeyup_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let (input, _) =
            handle_key_event(wparam, lparam, &self.state, |keystroke, _, metadata| {
                PlatformInput::KeyUp(KeyUpEvent {
                    physical_key: physical_key_from_lparam(lparam),
                    key_metadata: Some(metadata),
                    keystroke,
                })
            })?;
        let mut func = self.state.callbacks.input.take()?;

        func(input);
        self.state.callbacks.input.set(Some(func));

        // Always return 0 to indicate that the message was handled, so we could properly handle `ModifiersChanged` event.
        Some(0)
    }

    // It's a known bug that you can't trigger `ctrl-shift-0`. See:
    // https://superuser.com/questions/1455762/ctrl-shift-number-key-combination-has-stopped-working-for-a-few-numbers
    pub(crate) fn handle_keydown_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        if self.state.closing.get() {
            return Some(0);
        }
        let Some((mut input, dead_key)) = handle_key_event(
            wparam,
            lparam,
            &self.state,
            |keystroke, prefer_character_input, metadata| {
                PlatformInput::KeyDown(KeyDownEvent {
                    physical_key: physical_key_from_lparam(lparam),
                    key_metadata: Some(metadata),
                    keystroke,
                    is_held: lparam.0 & (0x1 << 30) > 0,
                    prefer_character_input,
                })
            },
        ) else {
            // Some layouts have a native character but no shortcut key name.
            // Keep that case on the same opt-in path; the probe is nonmutating.
            let key = VIRTUAL_KEY(wparam.loword());
            if !matches!(key, VK_PACKET | VK_PROCESSKEY) {
                let (text, prefer_characters, dead_key) = process_key(key, lparam.hiword());
                let modifiers = current_modifiers();
                let native_text =
                    prefer_characters || (text.is_some() && !modifiers.control && !modifiers.alt);
                if native_text && !modifiers.platform {
                    if let Some(origin) = self.prepare_native_character_target(true) {
                        return self.handle_native_character_msg(origin, wparam, lparam);
                    }
                    if self.state.closing.get() {
                        return Some(0);
                    }
                }
                NATIVE_CHARACTERS.with(|state| {
                    let mut next = state.get();
                    next.ordinary_dead_keys.observe(
                        unsafe { GetKeyboardLayout(0) }.0 as isize,
                        text.is_some(),
                        dead_key,
                        false,
                    );
                    state.set(next);
                });
            }
            return Some(1);
        };

        let native_target =
            self.prepare_native_character_target(matches!(&input, PlatformInput::KeyDown(_)));
        if self.state.closing.get() {
            return Some(0);
        }
        let has_text =
            matches!(&input, PlatformInput::KeyDown(event) if event.keystroke.key_char.is_some());
        if native_target.is_some()
            && let PlatformInput::KeyDown(event) = &mut input
        {
            event.prefer_character_input |= NATIVE_CHARACTERS
                .get()
                .ordinary_dead_keys
                .requires_refusal(event);
        }
        let native_preference = if let PlatformInput::KeyDown(event) = &input {
            event.prefer_character_input && !event.keystroke.modifiers.platform
        } else {
            false
        };
        let handled = if let Some(mut func) = self.state.callbacks.input.take() {
            let handled = !func(input).propagate;
            self.state.callbacks.input.set(Some(func));
            handled
        } else {
            false
        };
        if !handled
            && native_preference
            && let Some(origin) = native_target
        {
            return self.handle_native_character_msg(origin, wparam, lparam);
        }
        NATIVE_CHARACTERS.with(|state| {
            let mut next = state.get();
            next.ordinary_dead_keys.observe(
                unsafe { GetKeyboardLayout(0) }.0 as isize,
                has_text,
                dead_key,
                handled,
            );
            next.key_dispatched(has_text, dead_key, handled);
            state.set(next);
        });

        if handled { Some(0) } else { Some(1) }
    }

    fn prepare_native_character_target(
        &self,
        key_down: bool,
    ) -> Option<crate::native_characters::Origin> {
        if self.state.closing.get() {
            return None;
        }
        // Paint before borrowing an input handler: painting may replace it.
        let mut callback = self.state.callbacks.prepare_key_input.take()?;
        let prepared = callback(key_down);
        if self.state.closing.get() {
            return None;
        }
        let replacement = self.state.callbacks.prepare_key_input.take();
        self.state
            .callbacks
            .prepare_key_input
            .set(replacement.or(Some(callback)));
        if !prepared {
            return None;
        }
        self.with_input_handler(|handler| handler.native_character_target())
            .flatten()
            .filter(|_| unsafe { GetFocus() } == self.native_handle())
            .and_then(|target| {
                crate::native_characters::Origin::capture(
                    self.native_handle(),
                    target,
                    &self.state.native_character_authority,
                )
            })
    }

    fn handle_native_character_msg(
        &self,
        origin: crate::native_characters::Origin,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let current = self
            .with_input_handler(|handler| handler.native_character_target())
            .flatten();
        if current != Some(origin.target)
            || !origin.is_current(&self.state.native_character_authority)
        {
            return Some(0);
        }
        let translated = crate::native_characters::translate(
            origin,
            &self.state.native_character_authority,
            VIRTUAL_KEY(wparam.loword()),
            u32::from(lparam.hiword() & 0xff),
            NATIVE_CHARACTERS.get().ordinary_dead_keys,
        );
        if !origin.is_current(&self.state.native_character_authority) {
            return Some(0);
        }
        match translated {
            Ok(text) => {
                self.with_input_handler(|handler| {
                    handler.handle_native_characters_if(origin.target, Ok(&text), || {
                        origin.is_current(&self.state.native_character_authority)
                    })
                });
            }
            Err(error) => {
                self.with_input_handler(|handler| {
                    handler.handle_native_characters_if(origin.target, Err(error), || {
                        origin.is_current(&self.state.native_character_authority)
                    })
                });
            }
        }
        // Both pumps call WM_GPUI_KEYDOWN before TranslateMessage. The selected
        // target consumes this key even on failure; a second translation could
        // create dead-key state or insert text twice.
        Some(0)
    }

    fn handle_keyup_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let Some((input, _)) =
            handle_key_event(wparam, lparam, &self.state, |keystroke, _, metadata| {
                PlatformInput::KeyUp(KeyUpEvent {
                    physical_key: physical_key_from_lparam(lparam),
                    key_metadata: Some(metadata),
                    keystroke,
                })
            })
        else {
            return Some(1);
        };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };

        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_char_msg(&self, wparam: WPARAM) -> Option<isize> {
        let generation = self
            .with_input_handler(|input_handler| input_handler.text_input_generation())
            .flatten();
        let mut pending = self.state.pending_surrogate.take();
        let input = decode_character(&mut pending, wparam.loword(), generation);
        self.state.pending_surrogate.set(pending);
        let input = input?;
        self.with_input_handler(|input_handler| {
            input_handler.replace_text_in_range(None, &input);
        });

        Some(0)
    }

    fn handle_mouse_down_msg(
        &self,
        handle: HWND,
        button: MouseButton,
        lparam: LPARAM,
    ) -> Option<isize> {
        unsafe { SetCapture(handle) };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let x = lparam.signed_loword();
        let y = lparam.signed_hiword();
        let physical_point = point(DevicePixels(x as i32), DevicePixels(y as i32));
        let click_count = self.state.click_state.update(button, physical_point);
        let scale_factor = self.state.scale_factor.get();

        let input = PlatformInput::MouseDown(MouseDownEvent {
            button,
            position: logical_point(x as f32, y as f32, scale_factor),
            modifiers: current_modifiers(),
            click_count,
            first_mouse: false,
        });
        let _native_drag_dispatch = (button == MouseButton::Left)
            .then(|| self.native_drag.begin_left_down());
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_mouse_up_msg(
        &self,
        _handle: HWND,
        button: MouseButton,
        lparam: LPARAM,
    ) -> Option<isize> {
        unsafe { ReleaseCapture().log_err() };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let x = lparam.signed_loword() as f32;
        let y = lparam.signed_hiword() as f32;
        let click_count = self.state.click_state.current_count.get();
        let scale_factor = self.state.scale_factor.get();

        let input = PlatformInput::MouseUp(MouseUpEvent {
            button,
            position: logical_point(x, y, scale_factor),
            modifiers: current_modifiers(),
            click_count,
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_xbutton_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
        handler: impl Fn(&Self, HWND, MouseButton, LPARAM) -> Option<isize>,
    ) -> Option<isize> {
        let nav_dir = match wparam.hiword() {
            XBUTTON1 => NavigationDirection::Back,
            XBUTTON2 => NavigationDirection::Forward,
            _ => return Some(1),
        };
        handler(self, handle, MouseButton::Navigate(nav_dir), lparam)
    }

    fn handle_mouse_wheel_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let modifiers = current_modifiers();

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let scale_factor = self.state.scale_factor.get();
        let wheel_scroll_amount = match modifiers.shift {
            true => self
                .system_settings()
                .mouse_wheel_settings
                .wheel_scroll_chars
                .get(),
            false => self
                .system_settings()
                .mouse_wheel_settings
                .wheel_scroll_lines
                .get(),
        };

        let wheel_distance =
            (wparam.signed_hiword() as f32 / WHEEL_DELTA as f32) * wheel_scroll_amount as f32;
        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };
        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        let input = PlatformInput::Scroll(ScrollEvent {
            position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
            delta: ScrollDelta::Lines(match modifiers.shift {
                true => Point {
                    x: wheel_distance,
                    y: 0.0,
                },
                false => Point {
                    y: wheel_distance,
                    x: 0.0,
                },
            }),
            modifiers,
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_mouse_horizontal_wheel_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let scale_factor = self.state.scale_factor.get();
        let wheel_scroll_chars = self
            .system_settings()
            .mouse_wheel_settings
            .wheel_scroll_chars
            .get();

        let wheel_distance =
            (-wparam.signed_hiword() as f32 / WHEEL_DELTA as f32) * wheel_scroll_chars as f32;
        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };
        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        let event = PlatformInput::Scroll(ScrollEvent {
            position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
            delta: ScrollDelta::Lines(Point {
                x: wheel_distance,
                y: 0.0,
            }),
            modifiers: current_modifiers(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
        let handled = !func(event).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn retrieve_caret_position(&self) -> Option<POINT> {
        self.with_input_handler_and_scale_factor(|input_handler, scale_factor| {
            let caret_range = input_handler.selected_text_range(false)?;
            let caret_position = input_handler.bounds_for_range(caret_range.range)?;
            Some(POINT {
                // logical to physical
                x: (caret_position.origin.x.as_f32() * scale_factor) as i32,
                y: (caret_position.origin.y.as_f32() * scale_factor) as i32
                    + ((caret_position.size.height.as_f32() * scale_factor) as i32 / 2),
            })
        })
    }

    fn handle_ime_position(&self, handle: HWND) -> Option<isize> {
        if let Some(caret_position) = self.retrieve_caret_position() {
            self.update_ime_position(handle, caret_position);
        }
        Some(0)
    }

    pub(crate) fn update_ime_position(&self, handle: HWND, caret_position: POINT) {
        let Some(ctx) = ImeContext::get(handle) else {
            return;
        };
        unsafe {
            ImmSetCompositionWindow(
                *ctx,
                &COMPOSITIONFORM {
                    dwStyle: CFS_POINT,
                    ptCurrentPos: caret_position,
                    ..Default::default()
                },
            )
            .ok()
            .log_err();

            ImmSetCandidateWindow(
                *ctx,
                &CANDIDATEFORM {
                    dwStyle: CFS_CANDIDATEPOS,
                    ptCurrentPos: caret_position,
                    ..Default::default()
                },
            )
            .ok()
            .log_err();
        }
    }

    fn update_ime_enabled(&self, handle: HWND) {
        let ime_enabled = self
            .with_input_handler(|input_handler| input_handler.query_accepts_text_input())
            .unwrap_or(false);
        if !ime_enabled {
            self.state.pending_surrogate.set(None);
        }
        if ime_enabled == self.state.ime_enabled.get() {
            return;
        }
        self.state.ime_enabled.set(ime_enabled);
        unsafe {
            if ime_enabled {
                ImmAssociateContextEx(handle, HIMC::default(), IACE_DEFAULT)
                    .ok()
                    .log_err();
            } else {
                // The IME context is per-thread, so without this check a change in this
                // window's text input state could commit an IME composition happening in another window.
                if GetFocus() == handle
                    && let Some(ctx) = ImeContext::get(handle)
                {
                    ImmNotifyIME(*ctx, NI_COMPOSITIONSTR, CPS_COMPLETE, 0)
                        .ok()
                        .log_err();
                }
                ImmAssociateContextEx(handle, HIMC::default(), 0)
                    .ok()
                    .log_err();
            }
        }
    }

    fn handle_ime_composition(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        let ctx = ImeContext::get(handle)?;
        self.handle_ime_composition_inner(*ctx, lparam)
    }

    fn handle_ime_composition_inner(&self, ctx: HIMC, lparam: LPARAM) -> Option<isize> {
        let lparam = lparam.0 as u32;
        if lparam == 0 {
            self.cancel_ime_composition();
            Some(0)
        } else {
            if lparam & GCS_RESULTSTR.0 > 0 {
                let comp_result = parse_ime_composition_string(ctx, GCS_RESULTSTR)?;
                self.with_input_handler(|input_handler| {
                    input_handler
                        .replace_text_in_range(None, &String::from_utf16_lossy(&comp_result));
                })?;
            }
            if lparam & GCS_COMPSTR.0 > 0 {
                let comp_string = parse_ime_composition_string(ctx, GCS_COMPSTR)?;
                let caret_pos =
                    (!comp_string.is_empty() && lparam & GCS_CURSORPOS.0 > 0).then(|| {
                        let cursor_pos = retrieve_composition_cursor_position(ctx);
                        let pos = if should_use_ime_cursor_position(ctx, cursor_pos) {
                            cursor_pos
                        } else {
                            comp_string.len()
                        };
                        pos..pos
                    });
                self.with_input_handler(|input_handler| {
                    input_handler.replace_and_mark_text_in_range(
                        None,
                        &String::from_utf16_lossy(&comp_string),
                        caret_pos,
                    );
                })?;
            }
            if lparam & (GCS_RESULTSTR.0 | GCS_COMPSTR.0) > 0 {
                return Some(0);
            }

            // currently, we don't care other stuff
            None
        }
    }

    fn cancel_ime_composition(&self) {
        self.with_input_handler(|input_handler| {
            // After a result was committed there may be a selected document
            // range. Cancellation may remove only the remaining marked text.
            let marked = input_handler.marked_text_range();
            cancel_marked_text(marked, |range| {
                input_handler.replace_text_in_range(Some(range), "");
            });
        });
    }

    fn handle_calc_client_size(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if self.state.decorations.get() == WindowDecorations::Client && lparam.0 != 0 {
            // Both message forms begin with the proposed window RECT. Returning
            // zero makes it drawable; preserve the OS-owned maximized outer
            // frame while clipping its client area away from the taskbar.
            let proposed = unsafe { &mut *(lparam.0 as *mut RECT) };
            if self.state.is_maximized() {
                let monitor = unsafe { MonitorFromRect(proposed, MONITOR_DEFAULTTONEAREST) };
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                unsafe { GetMonitorInfoW(monitor, &mut info) }
                    .ok()
                    .log_err()?;
                *proposed = crate::window::client_frame::maximized_client(*proposed, info.rcWork)?;
            }
            return Some(0);
        }
        if !self.hide_title_bar || self.state.is_fullscreen() || wparam.0 == 0 {
            return None;
        }

        unsafe {
            let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
            let saved_top = (*params).rgrc[0].top;
            let result = DefWindowProcW(handle, WM_NCCALCSIZE, wparam, lparam);
            (*params).rgrc[0].top = saved_top;
            if self.state.is_maximized() {
                let dpi = GetDpiForWindow(handle);
                (*params).rgrc[0].top += get_frame_thicknessx(dpi);
            }
            Some(result.0 as isize)
        }
    }

    fn handle_activate_msg(self: &Rc<Self>, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let activated = wparam.loword() > 0;
        self.notify_native_activation(if activated {
            NativeWindowActivation::Activated
        } else {
            NativeWindowActivation::Deactivated {
                next_window: (lparam.0 != 0).then_some(lparam.0),
            }
        });

        let events = self
            .state
            .a11y
            .try_borrow_mut()
            .ok()
            .and_then(|mut a11y| a11y.as_mut()?.adapter.update_window_focus_state(activated));
        if let Some(events) = events {
            events.raise();
        }

        let this = self.clone();

        if !activated {
            this.state.cursor_visible.store(true, Ordering::Relaxed);
            this.state.pending_surrogate.set(None);
        }

        // When the window is activated (gains focus), reset the modifier tracking state.
        // This fixes the issue where Alt-Tab away and back leaves stale modifier state
        // (especially the Alt key) because Windows doesn't always send key-up events to
        // windows that have lost focus.
        if activated {
            this.state.last_reported_modifiers.set(None);
            this.state.last_reported_capslock.set(None);

            if let Some(mut func) = this.state.callbacks.input.take() {
                let input = PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: current_modifiers(),
                    capslock: current_capslock(),
                });
                func(input);
                this.state.callbacks.input.set(Some(func));
            }
        }

        self.executor
            .spawn(async move {
                if let Some(mut func) = this.state.callbacks.active_status_change.take() {
                    func(activated);
                    this.state.callbacks.active_status_change.set(Some(func));
                }
            })
            .detach();

        None
    }

    fn handle_wm_getobject(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let result = {
            let mut a11y = self.state.a11y.borrow_mut();
            let a11y = a11y.as_mut()?;
            a11y.adapter.handle_wm_getobject(
                accesskit_windows::WPARAM(wparam.0),
                accesskit_windows::LPARAM(lparam.0),
                &mut a11y.activation_handler,
            )?
        };
        // The borrow above must be dropped before calling `.into()`, because
        // it calls `UiaReturnRawElementProvider` which may send a nested
        // `WM_GETOBJECT` back into this window procedure.
        let lresult: accesskit_windows::LRESULT = result.into();
        Some(lresult.0)
    }

    fn handle_create_msg(&self, handle: HWND) -> Option<isize> {
        if self.hide_title_bar || self.state.decorations.get() == WindowDecorations::Client {
            notify_frame_changed(handle);
            Some(0)
        } else {
            None
        }
    }

    fn handle_dpi_changed_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let new_dpi = wparam.loword() as f32;

        let is_maximized = self.state.is_maximized();
        let new_scale_factor = new_dpi / USER_DEFAULT_SCREEN_DPI as f32;
        self.state.scale_factor.set(new_scale_factor);
        self.state.border_offset.update(handle).log_err();

        self.state
            .direct_manipulation
            .set_scale_factor(new_scale_factor);

        if is_maximized {
            // Get the monitor and its work area at the new DPI
            let monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONEAREST) };
            let mut monitor_info: MONITORINFO = unsafe { std::mem::zeroed() };
            monitor_info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
                let work_area = monitor_info.rcWork;
                let width = work_area.right - work_area.left;
                let height = work_area.bottom - work_area.top;

                // Update the window size to match the new monitor work area
                // This will trigger WM_SIZE which will handle the size change
                unsafe {
                    SetWindowPos(
                        handle,
                        None,
                        work_area.left,
                        work_area.top,
                        width,
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    )
                    .context("unable to set maximized window position after dpi has changed")
                    .log_err();
                }

                // SetWindowPos may not send WM_SIZE for maximized windows in some cases,
                // so we manually update the size to ensure proper rendering
                let device_size = size(DevicePixels(width), DevicePixels(height));
                self.handle_size_change(device_size, new_scale_factor, true);
            }
        } else {
            // For non-maximized windows, use the suggested RECT from the system
            let rect = unsafe { &*(lparam.0 as *const RECT) };
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            // this will emit `WM_SIZE` and `WM_MOVE` right here
            // even before this function returns
            // the new size is handled in `WM_SIZE`
            unsafe {
                SetWindowPos(
                    handle,
                    None,
                    rect.left,
                    rect.top,
                    width,
                    height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .context("unable to set window position after dpi has changed")
                .log_err();
            }
        }

        Some(0)
    }

    fn handle_display_change_msg(&self, handle: HWND) -> Option<isize> {
        let new_monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONULL) };
        if new_monitor.is_invalid() {
            log::error!("No monitor detected!");
            return None;
        }
        let new_display = WindowsDisplay::new(WindowsDisplay::display_id_for_monitor(new_monitor))?;
        self.set_display(new_display);
        Some(0)
    }

    fn set_display(&self, display: WindowsDisplay) {
        self.state.display.set(display);
        if let Some(mut callback) = self.state.callbacks.display_changed.take() {
            callback();
            self.state.callbacks.display_changed.set(Some(callback));
        }
    }

    fn handle_hit_test_msg(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        if self.state.is_fullscreen() {
            return (self.state.decorations.get() == WindowDecorations::Client)
                .then_some(HTCLIENT as isize);
        }

        let client_decorated = self.state.decorations.get() == WindowDecorations::Client;
        if client_decorated && self.is_resizable.get() && !self.state.is_maximized() {
            let mut bounds = RECT::default();
            unsafe { GetWindowRect(handle, &mut bounds) }.log_err()?;
            let dpi = unsafe { GetDpiForWindow(handle) };
            let hit = crate::window::client_frame::hit_test(
                bounds,
                POINT {
                    x: lparam.signed_loword().into(),
                    y: lparam.signed_hiword().into(),
                },
                [get_frame_thicknessx(dpi), get_frame_thicknessy(dpi)],
            );
            if hit != HTCLIENT {
                return Some(hit as isize);
            }
        }

        let callback = self.state.callbacks.hit_test_window_control.take();
        let drag_area = if let Some(mut callback) = callback {
            let area = callback();
            self.state
                .callbacks
                .hit_test_window_control
                .set(Some(callback));
            area.and_then(|area| match area {
                WindowControlArea::Drag if self.is_movable => Some(HTCAPTION as _),
                WindowControlArea::Drag => None,
                WindowControlArea::Close => Some(HTCLOSE as _),
                WindowControlArea::Max if self.is_maximizable() => Some(HTMAXBUTTON as _),
                WindowControlArea::Max if self.is_movable => Some(HTCAPTION as _),
                WindowControlArea::Max => Some(HTNOWHERE as _),
                WindowControlArea::Min if self.is_minimizable.get() => Some(HTMINBUTTON as _),
                WindowControlArea::Min if self.is_movable => Some(HTCAPTION as _),
                WindowControlArea::Min => Some(HTNOWHERE as _),
            })
        } else {
            None
        };

        if client_decorated {
            return Some(drag_area.unwrap_or(HTCLIENT as isize));
        }

        if !self.hide_title_bar {
            // If the OS draws the title bar, we don't need to handle hit test messages.
            return drag_area;
        }

        let dpi = unsafe { GetDpiForWindow(handle) };
        // We do not use the OS title bar, so the default `DefWindowProcW` will only register a 1px edge for resizes
        // We need to calculate the frame thickness ourselves and do the hit test manually.
        let frame_y = get_frame_thicknessx(dpi);
        let frame_x = get_frame_thicknessy(dpi);
        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };

        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        if self.is_resizable.get()
            && !self.state.is_maximized()
            && 0 <= cursor_point.y
            && cursor_point.y <= frame_y
        {
            // x-axis actually goes from -frame_x to 0
            return Some(if cursor_point.x <= 0 {
                HTTOPLEFT
            } else {
                let mut rect = Default::default();
                unsafe { GetWindowRect(handle, &mut rect) }.log_err();
                // right and bottom bounds of RECT are exclusive, thus `-1`
                let right = rect.right - rect.left - 1;
                // the bounds include the padding frames, so accommodate for both of them
                if right - 2 * frame_x <= cursor_point.x {
                    HTTOPRIGHT
                } else {
                    HTTOP
                }
            } as _);
        }

        drag_area
    }

    fn handle_nc_mouse_move_msg(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        self.start_tracking_mouse(handle, TME_LEAVE | TME_NONCLIENT);
        self.restore_cursor_after_hide();

        let mut func = self.state.callbacks.input.take()?;
        let scale_factor = self.state.scale_factor.get();

        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };
        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        let input = PlatformInput::MouseMove(MouseMoveEvent {
            position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
            pressed_button: None,
            modifiers: current_modifiers(),
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { None }
    }

    fn handle_nc_mouse_down_msg(
        &self,
        handle: HWND,
        button: MouseButton,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if let Some(mut func) = self.state.callbacks.input.take() {
            let scale_factor = self.state.scale_factor.get();
            let mut cursor_point = POINT {
                x: lparam.signed_loword().into(),
                y: lparam.signed_hiword().into(),
            };
            unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
            let physical_point = point(DevicePixels(cursor_point.x), DevicePixels(cursor_point.y));
            let click_count = self.state.click_state.update(button, physical_point);

            let input = PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
                modifiers: current_modifiers(),
                click_count,
                first_mouse: false,
            });
            let handled = !func(input).propagate;
            self.state.callbacks.input.set(Some(func));

            if handled {
                return Some(0);
            }
        } else {
        };

        // Since these are handled in handle_nc_mouse_up_msg we must prevent the default window proc
        if button == MouseButton::Left {
            match wparam.0 as u32 {
                HTMINBUTTON => self.state.nc_button_pressed.set(Some(HTMINBUTTON)),
                HTMAXBUTTON => self.state.nc_button_pressed.set(Some(HTMAXBUTTON)),
                HTCLOSE => self.state.nc_button_pressed.set(Some(HTCLOSE)),
                _ => return None,
            };
            Some(0)
        } else {
            None
        }
    }

    fn handle_nc_mouse_up_msg(
        &self,
        handle: HWND,
        button: MouseButton,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if let Some(mut func) = self.state.callbacks.input.take() {
            let scale_factor = self.state.scale_factor.get();

            let mut cursor_point = POINT {
                x: lparam.signed_loword().into(),
                y: lparam.signed_hiword().into(),
            };
            unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
            let input = PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
                modifiers: current_modifiers(),
                click_count: 1,
            });
            let handled = !func(input).propagate;
            self.state.callbacks.input.set(Some(func));

            if handled {
                return Some(0);
            }
        } else {
        }

        let last_pressed = self.state.nc_button_pressed.take();
        if button == MouseButton::Left
            && let Some(last_pressed) = last_pressed
        {
            let handled = match (wparam.0 as u32, last_pressed) {
                (HTMINBUTTON, HTMINBUTTON) if self.is_minimizable.get() => {
                    unsafe { ShowWindowAsync(handle, SW_MINIMIZE).ok().log_err() };
                    true
                }
                (HTMINBUTTON, HTMINBUTTON) => true,
                (HTMAXBUTTON, HTMAXBUTTON) if self.is_maximizable() => {
                    if self.state.is_maximized() {
                        unsafe { ShowWindowAsync(handle, SW_NORMAL).ok().log_err() };
                    } else {
                        unsafe { ShowWindowAsync(handle, SW_MAXIMIZE).ok().log_err() };
                    }
                    true
                }
                (HTMAXBUTTON, HTMAXBUTTON) => true,
                (HTCLOSE, HTCLOSE) => {
                    unsafe {
                        PostMessageW(Some(handle), WM_CLOSE, WPARAM::default(), LPARAM::default())
                            .log_err()
                    };
                    true
                }
                _ => false,
            };
            if handled {
                return Some(0);
            }
        }

        None
    }

    fn handle_cursor_changed(&self, lparam: LPARAM) -> Option<isize> {
        let had_cursor = self.state.current_cursor.get().is_some();

        self.state.current_cursor.set(if lparam.0 == 0 {
            None
        } else {
            Some(HCURSOR(lparam.0 as _))
        });

        if had_cursor != self.state.current_cursor.get().is_some() {
            unsafe { SetCursor(self.state.current_cursor.get()) };
        }

        Some(0)
    }

    fn handle_set_cursor(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        if unsafe { !IsWindowEnabled(handle).as_bool() }
            || matches!(
                lparam.loword() as u32,
                HTLEFT
                    | HTRIGHT
                    | HTTOP
                    | HTTOPLEFT
                    | HTTOPRIGHT
                    | HTBOTTOM
                    | HTBOTTOMLEFT
                    | HTBOTTOMRIGHT
            )
        {
            return None;
        }
        let cursor = if self.state.cursor_visible.load(Ordering::Relaxed) {
            self.state.current_cursor.get()
        } else {
            None
        };
        unsafe {
            SetCursor(cursor);
        };
        Some(0)
    }

    fn handle_system_settings_changed(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if wparam.0 != 0 {
            self.state.click_state.system_update(wparam.0);
            self.state.border_offset.update(handle).log_err();
            // system settings may emit a window message which wants to take the refcell self.state, so drop it

            self.system_settings().update(wparam.0);
        } else {
            self.handle_system_theme_changed(handle, lparam)?;
        };

        Some(0)
    }

    fn handle_system_theme_changed(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        // lParam is a pointer to a string that indicates the area containing the system parameter
        // that was changed.
        let parameter = PCWSTR::from_raw(lparam.0 as _);
        if unsafe { !parameter.is_null() && !parameter.is_empty() }
            && let Some(parameter_string) = unsafe { parameter.to_string() }.log_err()
        {
            log::info!("System settings changed: {}", parameter_string);
            if parameter_string.as_str() == "ImmersiveColorSet" {
                let new_appearance = system_appearance()
                    .context("unable to get system appearance when handling ImmersiveColorSet")
                    .log_err()?;

                if new_appearance != self.state.appearance.get() {
                    self.state.appearance.set(new_appearance);
                    let mut callback = self.state.callbacks.appearance_changed.take()?;

                    callback();
                    self.state.callbacks.appearance_changed.set(Some(callback));
                    configure_dwm_dark_mode(handle, new_appearance);
                }
            }
        }
        Some(0)
    }

    fn handle_input_language_changed(&self) -> Option<isize> {
        NATIVE_CHARACTERS.with(|state| {
            let mut next = state.get();
            next.dead_key_pending = false;
            state.set(next);
        });
        self.state.pending_surrogate.set(None);
        unsafe {
            PostMessageW(
                Some(self.platform_window_handle),
                WM_GPUI_KEYBOARD_LAYOUT_CHANGED,
                WPARAM(self.validation_number),
                LPARAM(0),
            )
            .log_err();
        }
        Some(0)
    }

    fn handle_window_visibility_changed(
        self: &Rc<Self>,
        handle: HWND,
        wparam: WPARAM,
    ) -> Option<isize> {
        self.report_visibility();
        if wparam.0 == 1 {
            self.draw_window(handle, false);
        }
        None
    }

    // The window procedure can run while GPUI is updating this window (e.g.
    // `ShowWindow` from an action handler), so deliver observers after that
    // update completes, as activation does. The state is read at delivery so
    // a burst of messages collapses to the final value.
    fn report_visibility(self: &Rc<Self>) {
        if self.state.last_visibility.get().is_none() {
            return;
        }
        let this = self.clone();
        self.executor
            .spawn(async move {
                let visibility = this.visibility();
                if this.state.last_visibility.get() == Some(visibility) {
                    return;
                }
                this.state.last_visibility.set(Some(visibility));
                if let Some(mut callback) = this.state.callbacks.visibility_change.take() {
                    callback(visibility);
                    this.state.callbacks.visibility_change.set(Some(callback));
                }
            })
            .detach();
    }

    fn handle_gpui_events(
        &self,
        handle: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if wparam.0 != self.validation_number {
            return None;
        }
        match message {
            WM_GPUI_CURSOR_STYLE_CHANGED => self.handle_cursor_changed(lparam),
            WM_GPUI_FORCE_UPDATE_WINDOW => self.draw_window(handle, true),
            WM_GPUI_GPU_DEVICE_LOST => self.handle_device_lost(lparam),
            _ => unreachable!(),
        }
    }

    fn handle_device_lost(&self, lparam: LPARAM) -> Option<isize> {
        if lparam.0 == 0 {
            return None;
        }
        let devices = lparam.0 as *const DirectXDevices;
        let devices = unsafe { &*devices };
        if let Err(err) = self
            .state
            .renderer
            .borrow_mut()
            .handle_device_lost(&devices)
        {
            panic!("Device lost: {err}");
        }
        // Make sure the first `draw_window` after recovery (whether it comes
        // from the forced WM_GPUI_FORCE_UPDATE_WINDOW or a stray WM_PAINT in
        // between) is treated as a forced render so it both clears
        // `skip_draws` and bypasses the view cache.
        self.state.force_render_pending.set(true);
        Some(0)
    }

    fn handle_dm_pointer_hit_test(&self, wparam: WPARAM) -> Option<isize> {
        self.state.direct_manipulation.on_pointer_hit_test(wparam);
        None
    }

    #[inline]
    fn draw_window(&self, handle: HWND, force_render: bool) -> Option<isize> {
        let Some(_guard) = self.state.draw_coordinator.try_begin_draw() else {
            log::debug!("deferring re-entrant draw of window {handle:?}");
            if force_render {
                self.state.force_render_pending.set(true);
            }
            // Validate the region so a nested message pump doesn't keep
            // re-dispatching WM_PAINT for the still-invalid region in a busy
            // loop until the in-progress draw unwinds. The vsync thread
            // re-invalidates every window on each vsync (see
            // `begin_vsync_thread`), so the deferred frame still gets drawn,
            // at most one vsync late.
            unsafe { ValidateRect(Some(handle), None).ok().log_err() };
            return Some(0);
        };
        let mut request_frame = self.state.callbacks.request_frame.take()?;

        self.state.direct_manipulation.update();

        let events = self.state.direct_manipulation.drain_events();
        if !events.is_empty() {
            if let Some(mut func) = self.state.callbacks.input.take() {
                for event in events {
                    func(event);
                }
                self.state.callbacks.input.set(Some(func));
            }
        }

        let force_render = force_render || self.state.force_render_pending.take();
        if force_render {
            // Re-enable drawing after a device loss recovery. The forced render
            // will rebuild the scene with fresh atlas textures.
            self.state.renderer.borrow_mut().mark_drawable();
        }
        let (signal_at, signal_source) = self.state.frame_signal.take().map_or(
            (None, FrameRequestSource::NativeCallback),
            |(at, source)| (Some(at), source),
        );
        request_frame(RequestFrameOptions {
            require_presentation: false,
            force_render,
            signal_at,
            signal_source,
        });

        self.state.callbacks.request_frame.set(Some(request_frame));
        self.update_ime_enabled(handle);
        unsafe { ValidateRect(Some(handle), None).ok().log_err() };

        Some(0)
    }

    /// Clear the hidden flag and restore the cursor immediately
    fn restore_cursor_after_hide(&self) {
        if !self.state.cursor_visible.swap(true, Ordering::Relaxed) {
            unsafe {
                SetCursor(self.state.current_cursor.get());
            }
        }
    }

    fn start_tracking_mouse(&self, handle: HWND, flags: TRACKMOUSEEVENT_FLAGS) {
        if !self.state.hovered.get() {
            self.state.hovered.set(true);
            unsafe {
                TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: flags,
                    hwndTrack: handle,
                    dwHoverTime: HOVER_DEFAULT,
                })
                .log_err()
            };
            if let Some(mut callback) = self.state.callbacks.hovered_status_change.take() {
                callback(true);
                self.state
                    .callbacks
                    .hovered_status_change
                    .set(Some(callback));
            }
        }
    }

    fn with_input_handler<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut PlatformInputHandler) -> R,
    {
        let mut input_handler = self.state.input_handler.take()?;
        let result = f(&mut input_handler);
        self.state.input_handler.set(Some(input_handler));
        Some(result)
    }

    fn with_input_handler_and_scale_factor<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut PlatformInputHandler, f32) -> Option<R>,
    {
        let mut input_handler = self.state.input_handler.take()?;
        let scale_factor = self.state.scale_factor.get();

        let result = f(&mut input_handler, scale_factor);
        self.state.input_handler.set(Some(input_handler));
        result
    }
}

struct ImeContext {
    hwnd: HWND,
    himc: HIMC,
}

impl ImeContext {
    fn get(hwnd: HWND) -> Option<Self> {
        let himc = unsafe { ImmGetContext(hwnd) };
        if himc.is_invalid() {
            return None;
        }
        Some(Self { hwnd, himc })
    }
}

impl std::ops::Deref for ImeContext {
    type Target = HIMC;
    fn deref(&self) -> &HIMC {
        &self.himc
    }
}

impl Drop for ImeContext {
    fn drop(&mut self) {
        unsafe {
            ImmReleaseContext(self.hwnd, self.himc).ok().log_err();
        }
    }
}

fn handle_key_event<F>(
    wparam: WPARAM,
    lparam: LPARAM,
    state: &WindowsWindowState,
    f: F,
) -> Option<(PlatformInput, bool)>
where
    F: FnOnce(Keystroke, bool, KeyEventMetadata) -> PlatformInput,
{
    let virtual_key = VIRTUAL_KEY(wparam.loword());
    let modifiers = current_modifiers();

    match virtual_key {
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN => {
            if state
                .last_reported_modifiers
                .get()
                .is_some_and(|prev_modifiers| prev_modifiers == modifiers)
            {
                return None;
            }
            state.last_reported_modifiers.set(Some(modifiers));
            Some((
                PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers,
                    capslock: current_capslock(),
                }),
                false,
            ))
        }
        VK_PACKET | VK_PROCESSKEY => None,
        VK_CAPITAL => {
            let capslock = current_capslock();
            if state
                .last_reported_capslock
                .get()
                .is_some_and(|prev_capslock| prev_capslock == capslock)
            {
                return None;
            }
            state.last_reported_capslock.set(Some(capslock));
            Some((
                PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers,
                    capslock,
                }),
                false,
            ))
        }
        vkey => {
            // Keep native layout facts separate from shortcut normalization.
            // ToUnicode's no-state-change flag in generate_key_char preserves
            // the dead-key state that Windows uses for subsequent WM_CHAR/IME.
            let metadata = KeyEventMetadata {
                unmodified_text: generate_key_char(
                    vkey,
                    u32::from(lparam.hiword() & 0xFF),
                    false,
                    false,
                    false,
                )
                .map(Into::into),
                modifiers,
                caps_lock: current_capslock().on,
                num_lock: unsafe { GetKeyState(VK_NUMLOCK.0 as i32) & 1 } != 0,
            };
            let (keystroke, prefer_character_input, dead_key) =
                parse_normal_key(vkey, lparam, modifiers)?;
            let prefer_character_input = NATIVE_CHARACTERS
                .get()
                .prefers_text(keystroke.key_char.is_some(), prefer_character_input);
            Some((f(keystroke, prefer_character_input, metadata), dead_key))
        }
    }
}

fn parse_immutable(vkey: VIRTUAL_KEY) -> Option<String> {
    Some(
        match vkey {
            VK_SPACE => "space",
            VK_BACK => "backspace",
            VK_RETURN => "enter",
            VK_TAB => "tab",
            VK_UP => "up",
            VK_DOWN => "down",
            VK_RIGHT => "right",
            VK_LEFT => "left",
            VK_HOME => "home",
            VK_END => "end",
            VK_PRIOR => "pageup",
            VK_NEXT => "pagedown",
            VK_BROWSER_BACK => "back",
            VK_BROWSER_FORWARD => "forward",
            VK_ESCAPE => "escape",
            VK_INSERT => "insert",
            VK_DELETE => "delete",
            VK_APPS => "menu",
            VK_F1 => "f1",
            VK_F2 => "f2",
            VK_F3 => "f3",
            VK_F4 => "f4",
            VK_F5 => "f5",
            VK_F6 => "f6",
            VK_F7 => "f7",
            VK_F8 => "f8",
            VK_F9 => "f9",
            VK_F10 => "f10",
            VK_F11 => "f11",
            VK_F12 => "f12",
            VK_F13 => "f13",
            VK_F14 => "f14",
            VK_F15 => "f15",
            VK_F16 => "f16",
            VK_F17 => "f17",
            VK_F18 => "f18",
            VK_F19 => "f19",
            VK_F20 => "f20",
            VK_F21 => "f21",
            VK_F22 => "f22",
            VK_F23 => "f23",
            VK_F24 => "f24",
            _ => return None,
        }
        .to_string(),
    )
}

fn parse_normal_key(
    vkey: VIRTUAL_KEY,
    lparam: LPARAM,
    mut modifiers: Modifiers,
) -> Option<(Keystroke, bool, bool)> {
    let (key_char, prefer_character_input, dead_key) = process_key(vkey, lparam.hiword());

    let key = parse_immutable(vkey).or_else(|| {
        let scan_code = lparam.hiword() & 0xFF;
        get_keystroke_key(vkey, scan_code as u32, &mut modifiers)
    })?;

    Some((
        Keystroke {
            modifiers,
            key,
            key_char,
        },
        prefer_character_input,
        dead_key,
    ))
}

fn process_key(vkey: VIRTUAL_KEY, scan_code: u16) -> (Option<String>, bool, bool) {
    let mut keyboard_state = [0u8; 256];
    unsafe {
        if GetKeyboardState(&mut keyboard_state).is_err() {
            return (None, false, false);
        }
    }

    let mut buffer_c = [0u16; 8];
    let result_c = unsafe {
        ToUnicode(
            vkey.0 as u32,
            scan_code as u32,
            Some(&keyboard_state),
            &mut buffer_c,
            0x4,
        )
    };

    if result_c == 0 {
        return (None, false, false);
    }

    let c = &buffer_c[..result_c.unsigned_abs() as usize];
    let key_char = String::from_utf16(c)
        .ok()
        .filter(|s| !s.is_empty() && !s.chars().next().unwrap().is_control());

    if result_c < 0 {
        return (key_char, true, true);
    }

    if key_char.is_none() {
        return (None, false, false);
    }

    // Workaround for some bug that makes the compiler think keyboard_state is still zeroed out
    let keyboard_state = std::hint::black_box(keyboard_state);
    let ctrl_down = (keyboard_state[VK_CONTROL.0 as usize] & 0x80) != 0;
    let alt_down = (keyboard_state[VK_MENU.0 as usize] & 0x80) != 0;
    let win_down = (keyboard_state[VK_LWIN.0 as usize] & 0x80) != 0
        || (keyboard_state[VK_RWIN.0 as usize] & 0x80) != 0;

    let has_modifiers = ctrl_down || alt_down || win_down;
    if !has_modifiers {
        return (key_char, false, false);
    }

    let mut state_no_modifiers = keyboard_state;
    state_no_modifiers[VK_CONTROL.0 as usize] = 0;
    state_no_modifiers[VK_LCONTROL.0 as usize] = 0;
    state_no_modifiers[VK_RCONTROL.0 as usize] = 0;
    state_no_modifiers[VK_MENU.0 as usize] = 0;
    state_no_modifiers[VK_LMENU.0 as usize] = 0;
    state_no_modifiers[VK_RMENU.0 as usize] = 0;
    state_no_modifiers[VK_LWIN.0 as usize] = 0;
    state_no_modifiers[VK_RWIN.0 as usize] = 0;

    let mut buffer_c_no_modifiers = [0u16; 8];
    let result_c_no_modifiers = unsafe {
        ToUnicode(
            vkey.0 as u32,
            scan_code as u32,
            Some(&state_no_modifiers),
            &mut buffer_c_no_modifiers,
            0x4,
        )
    };

    let c_no_modifiers = &buffer_c_no_modifiers[..result_c_no_modifiers.unsigned_abs() as usize];
    (
        key_char,
        result_c != result_c_no_modifiers || c != c_no_modifiers,
        false,
    )
}

fn parse_ime_composition_string(ctx: HIMC, comp_type: IME_COMPOSITION_STRING) -> Option<Vec<u16>> {
    unsafe {
        let string_len = ImmGetCompositionStringW(ctx, comp_type, None, 0);
        if string_len >= 0 {
            let mut buffer = vec![0u8; string_len as usize + 2];
            ImmGetCompositionStringW(
                ctx,
                comp_type,
                Some(buffer.as_mut_ptr() as _),
                string_len as _,
            );
            let wstring = std::slice::from_raw_parts::<u16>(
                buffer.as_mut_ptr().cast::<u16>(),
                string_len as usize / 2,
            );
            Some(wstring.to_vec())
        } else {
            None
        }
    }
}

#[inline]
fn retrieve_composition_cursor_position(ctx: HIMC) -> usize {
    unsafe { ImmGetCompositionStringW(ctx, GCS_CURSORPOS, None, 0) as usize }
}

fn should_use_ime_cursor_position(ctx: HIMC, cursor_pos: usize) -> bool {
    let attrs_size = unsafe { ImmGetCompositionStringW(ctx, GCS_COMPATTR, None, 0) } as usize;
    if attrs_size == 0 {
        return false;
    }

    let mut attrs = vec![0u8; attrs_size];
    let result = unsafe {
        ImmGetCompositionStringW(
            ctx,
            GCS_COMPATTR,
            Some(attrs.as_mut_ptr() as *mut _),
            attrs_size as u32,
        )
    };
    if result <= 0 {
        return false;
    }

    // Keep the cursor adjacent to the inserted text by only using the suggested position
    // if it's adjacent to unconverted text.
    let at_cursor_is_input = cursor_pos < attrs.len() && attrs[cursor_pos] == (ATTR_INPUT as u8);
    let before_cursor_is_input = cursor_pos > 0
        && (cursor_pos - 1) < attrs.len()
        && attrs[cursor_pos - 1] == (ATTR_INPUT as u8);

    at_cursor_is_input || before_cursor_is_input
}

#[inline]
fn is_virtual_key_pressed(vkey: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vkey.0 as i32) < 0 }
}

#[inline]
pub(crate) fn current_modifiers() -> Modifiers {
    Modifiers {
        control: is_virtual_key_pressed(VK_CONTROL),
        alt: is_virtual_key_pressed(VK_MENU),
        shift: is_virtual_key_pressed(VK_SHIFT),
        platform: is_virtual_key_pressed(VK_LWIN) || is_virtual_key_pressed(VK_RWIN),
        function: false,
    }
}

#[inline]
pub(crate) fn current_capslock() -> Capslock {
    let on = unsafe { GetKeyState(VK_CAPITAL.0 as i32) & 1 } > 0;
    Capslock { on }
}

// there is some additional non-visible space when talking about window
// borders on Windows:
// - SM_CXSIZEFRAME: The resize handle.
// - SM_CXPADDEDBORDER: Additional border space that isn't part of the resize handle.
fn get_frame_thicknessx(dpi: u32) -> i32 {
    let resize_frame_thickness = unsafe { GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) };
    let padding_thickness = unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    resize_frame_thickness + padding_thickness
}

fn get_frame_thicknessy(dpi: u32) -> i32 {
    let resize_frame_thickness = unsafe { GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) };
    let padding_thickness = unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    resize_frame_thickness + padding_thickness
}

fn notify_frame_changed(handle: HWND) {
    unsafe {
        SetWindowPos(
            handle,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED
                | SWP_NOACTIVATE
                | SWP_NOCOPYBITS
                | SWP_NOMOVE
                | SWP_NOOWNERZORDER
                | SWP_NOREPOSITION
                | SWP_NOSENDCHANGING
                | SWP_NOSIZE
                | SWP_NOZORDER,
        )
        .log_err();
    }
}

// Key-up and repeat flags occupy higher bits and must not affect identity.
fn physical_key_from_lparam(lparam: LPARAM) -> Option<gpui::PhysicalKey> {
    let code = ((lparam.0 as usize >> 16) & 0x1ff) as u32;
    (code & 0xff != 0).then_some(gpui::PhysicalKey::Native(code))
}

#[cfg(test)]
mod physical_key_tests {
    use super::physical_key_from_lparam;
    use windows::Win32::Foundation::LPARAM;
    #[test]
    fn physical_key_keeps_extended_bit_but_ignores_press_repeat_release_flags() {
        let normal = physical_key_from_lparam(LPARAM(0x001c0001));
        assert_eq!(normal, Some(gpui::PhysicalKey::Native(0x1c)));
        assert_eq!(
            normal,
            physical_key_from_lparam(LPARAM(0xc01c0001u32 as isize))
        );
        assert_eq!(normal, physical_key_from_lparam(LPARAM(0x401c0001)));
        assert_ne!(normal, physical_key_from_lparam(LPARAM(0x011c0001)));
        assert_eq!(physical_key_from_lparam(LPARAM(0)), None);
    }
}

#[cfg(test)]
mod native_window_input_tests {
    use super::{
        NativeCharacterState, NativeModalLoops, cancel_marked_text, decode_character,
        mouse_activate_result,
    };

    #[test]
    fn utf16_pairs_cannot_cross_focus_or_text_acceptance_boundaries() {
        let mut pending = None;
        assert_eq!(decode_character(&mut pending, 0xd83d, Some((4, 0))), None);
        assert_eq!(
            decode_character(&mut pending, 0xdc4b, Some((4, 0))),
            Some("👋".into())
        );
        assert_eq!(pending, None);
        assert_eq!(decode_character(&mut pending, 0xd83d, Some((4, 0))), None);
        assert_eq!(decode_character(&mut pending, 0xdc4b, Some((5, 0))), None);
        assert_eq!(decode_character(&mut pending, 0xd83d, Some((5, 0))), None);
        assert_eq!(decode_character(&mut pending, 0xdc4b, Some((5, 1))), None);
        assert_eq!(decode_character(&mut pending, 0xd83d, Some((5, 1))), None);
        assert_eq!(decode_character(&mut pending, 0xdc4b, None), None);
        assert_eq!(decode_character(&mut pending, 0xdc4b, Some((5, 1))), None);
        assert_eq!(decode_character(&mut pending, 0xd83d, Some((5, 1))), None);
        assert_eq!(
            decode_character(&mut pending, b'a' as u16, Some((5, 1))),
            Some("a".into())
        );
        assert_eq!(decode_character(&mut pending, 0xdc4b, Some((5, 1))), None);
    }
    use windows::Win32::UI::WindowsAndMessaging::{
        MA_ACTIVATE, MA_NOACTIVATE, WM_ENTERMENULOOP, WM_ENTERSIZEMOVE, WM_EXITMENULOOP,
        WM_EXITSIZEMOVE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    #[test]
    fn nonactivating_popups_keep_the_owner_active() {
        assert_eq!(
            mouse_activate_result(WS_EX_TOOLWINDOW.0),
            MA_ACTIVATE as isize
        );
        assert_eq!(
            mouse_activate_result((WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0),
            MA_NOACTIVATE as isize,
        );
    }

    #[test]
    fn nested_menus_do_not_end_an_interactive_resize() {
        let mut loops = NativeModalLoops::default();
        loops.update(WM_ENTERMENULOOP);
        assert!(loops.active());
        assert!(!loops.in_size_move());
        loops.update(WM_ENTERSIZEMOVE);
        loops.update(WM_ENTERMENULOOP);
        loops.update(WM_EXITMENULOOP);
        loops.update(WM_EXITMENULOOP);
        assert!(loops.active());
        assert!(loops.in_size_move());
        loops.update(WM_EXITSIZEMOVE);
        assert!(!loops.active());
        loops.update(WM_EXITMENULOOP);
        assert!(!loops.active());
    }

    #[test]
    fn cancelled_composition_cannot_delete_a_committed_document_selection() {
        let mut document = "before marked after".to_owned();
        cancel_marked_text(Some(7..13), |range| {
            document.replace_range(range, "");
        });
        assert_eq!(document, "before  after");
        // A result commit has already removed the marked range. The following
        // end-composition message must leave the document's selection alone.
        cancel_marked_text(None, |_| panic!("cancelled committed text"));
        cancel_marked_text(Some(0..0), |_| panic!("cancelled an empty composition"));
    }

    #[test]
    fn a_dead_key_is_drained_by_ascii_text_but_not_by_remote_shortcuts() {
        let mut state = NativeCharacterState::default();
        assert!(!state.prefers_text(true, false));
        state.key_dispatched(true, true, true);
        assert!(!state.prefers_text(true, false));
        state.key_dispatched(true, true, false);
        assert!(state.prefers_text(true, false));
        assert!(!state.prefers_text(false, false));
        state.key_dispatched(false, false, true);
        assert!(state.prefers_text(true, false));
        state.key_dispatched(true, false, false);
        assert!(!state.prefers_text(true, false));
        assert!(state.prefers_text(true, true));
    }
}
