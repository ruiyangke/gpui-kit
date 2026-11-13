//! iOS Window implementation using UIWindow and UIViewController.
//!
//! iOS windows are fundamentally different from desktop windows:
//! - Always fullscreen (or split-screen on iPad)
//! - No title bar or window chrome
//! - Touch-based input
//! - Safe area insets for notch/home indicator
//!
//! The window is backed by a UIWindow containing a UIViewController
//! whose view hosts a CAMetalLayer.

use super::events::*;
use super::text_input::TextInputView;
use super::{CallbackSlot, IosDisplay, platform::IosPlatformState};
use gpui::{
    AnyWindowHandle, BackGestureEvent, BackGesturePhase, Bounds, Capslock, Corners, DevicePixels,
    DispatchEventResult, DisplayId, Edges, GpuSpecs, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels, PlatformAtlas,
    PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton,
    PromptLevel, RequestFrameOptions, Scene, Size, TextInputConfiguration, TextInputStateChange,
    TouchEvent, TouchId, TouchPhase, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowInsets, WindowKind, WindowParams, WindowVisibility, px, size,
};
use gpui_apple::{
    main_thread_marker,
    metal_renderer::{Context as MetalContext, MetalRenderer},
};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject, Sel};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSNotification, NSNotificationCenter, NSObjectProtocol, NSSet, NSString, NSValue,
};
use objc2_quartz_core::CAMetalLayer;
use objc2_ui_kit::{
    NSValueUIGeometryExtensions, UIKeyboardFrameEndUserInfoKey,
    UIKeyboardWillChangeFrameNotification, UIKeyboardWillHideNotification, UIStatusBarStyle,
};
use objc2_ui_kit::{
    UICornerConfiguration, UICornerRadius, UIEvent, UIGestureRecognizer,
    UIGestureRecognizerDelegate, UIKey, UIPanGestureRecognizer, UIPress, UIPressesEvent, UIRectCorner, UIRectEdge, UIScreen,
    UIScreenEdgePanGestureRecognizer, UIScrollTypeMask, UITouch, UITraitCollection,
    UITraitEnvironment, UIUserInterfaceLayoutDirection, UIUserInterfaceStyle, UIView,
    UICoordinateSpace, UIViewAutoresizing, UIViewController, UIWindow, UIWindowLevelNormal,
    UIWindowScene,
};
use parking_lot::Mutex;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, UiKitDisplayHandle, UiKitWindowHandle};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::NonNull,
    rc::{Rc, Weak},
    sync::Arc,
};

const KEYBOARD_DISMISS_DISTANCE: Pixels = px(24.);

#[derive(Clone, Copy)]
struct KeyboardDismissTouch {
    id: TouchId,
    start_position: Point<Pixels>,
}

/// The status bar style the app set: 0 = none (follow the appearance),
/// 1 = light content, 2 = dark content. Accessed from the main thread only.
static STATUS_BAR_STYLE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[derive(Default)]
pub(super) struct WindowReference(pub(super) RefCell<Weak<IosWindowState>>);

impl WindowReference {
    pub(super) fn with_window<R>(&self, callback: impl FnOnce(&IosWindowState) -> R) -> Option<R> {
        let window = self.0.borrow().upgrade()?;
        // Keep callback storage and native objects alive even if the callback
        // synchronously closes the platform window.
        Some(callback(&window))
    }
}

define_class!(
    #[unsafe(super = UIViewController)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIViewController"]
    #[ivars = WindowReference]
    struct ViewController;

    impl ViewController {
        #[unsafe(method(preferredStatusBarStyle))]
        fn preferred_status_bar_style(&self) -> UIStatusBarStyle {
            // A window with a UIKit bar under the status bar follows the
            // system's appearance, as that bar does; others, the app's style.
            let bar = self.ivars().with_window(|window| window.header.is_some());
            match STATUS_BAR_STYLE.load(std::sync::atomic::Ordering::Relaxed) {
                _ if bar == Some(true) => UIStatusBarStyle::Default,
                1 => UIStatusBarStyle::LightContent,
                2 => UIStatusBarStyle::DarkContent,
                _ => UIStatusBarStyle::Default,
            }
        }

        // The window menu's Paste looks for `paste:` from its button up,
        // which passes here rather than the GPUI view beside the button.
        #[unsafe(method(canPerformAction:withSender:))]
        fn can_perform_action(&self, action: Sel, sender: Option<&AnyObject>) -> bool {
            if action == sel!(paste:) {
                super::paste::available()
            } else {
                unsafe { msg_send![super(self), canPerformAction: action, withSender: sender] }
            }
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            super::paste::started();
            self.ivars()
                .with_window(|window| window.press_key_with("v", gpui::Modifiers::command()));
        }

        // The system's window controls move the layout margins and safe
        // area after the first layout: lay out again when they do.
        #[unsafe(method(viewLayoutMarginsDidChange))]
        fn view_layout_margins_did_change(&self) {
            unsafe {
                let _: () = msg_send![super(self), viewLayoutMarginsDidChange];
            }
            self.ivars().with_window(|window| window.fit_to_safe_area());
        }

        #[unsafe(method(viewSafeAreaInsetsDidChange))]
        fn view_safe_area_insets_did_change(&self) {
            unsafe {
                let _: () = msg_send![super(self), viewSafeAreaInsetsDidChange];
            }
            self.ivars().with_window(|window| window.fit_to_safe_area());
        }

        #[unsafe(method(viewDidLayoutSubviews))]
        fn view_did_layout_subviews(&self) {
            unsafe {
                let _: () = msg_send![super(self), viewDidLayoutSubviews];
            }
            self.ivars().with_window(|window| {
                window.fit_to_safe_area();
                window.handle_layout_change();
            });
        }
    }
);

impl ViewController {
    fn new(main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(WindowReference::default());
        unsafe { msg_send![super(this), init] }
    }
}

/// Set the iOS status bar content style (light or dark text/icons).
///
/// This updates the stored style and asks the root view controller
/// to re-query `preferredStatusBarStyle`.
pub fn set_status_bar_style(style: crate::StatusBarContentStyle) {
    use crate::StatusBarContentStyle;

    let value = match style {
        StatusBarContentStyle::Light => 1,
        StatusBarContentStyle::Dark => 2,
    };
    STATUS_BAR_STYLE.store(value, std::sync::atomic::Ordering::Relaxed);

    // This public free function predates explicit platform ownership.
    // Keep its single-application behavior without making runtime events global.
    if let Some(platform) = super::application::current_platform() {
        platform.application.with_windows(|window| {
            window.view_controller.setNeedsStatusBarAppearanceUpdate();
        });
    }
}

define_class!(
    #[unsafe(super = UIView)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIMetalView"]
    #[ivars = WindowReference]
    struct MetalView;

    unsafe impl NSObjectProtocol for MetalView {}

    impl MetalView {
        #[unsafe(method(layerClass))]
        fn layer_class() -> &'static AnyClass {
            CAMetalLayer::class()
        }

        // This callback also covers iOS 15/16, before UIKit's trait-registration API.
        #[unsafe(method(traitCollectionDidChange:))]
        fn trait_collection_did_change(&self, previous: Option<&UITraitCollection>) {
            unsafe {
                let _: () = msg_send![super(self), traitCollectionDidChange: previous];
                if self
                    .traitCollection()
                    .hasDifferentColorAppearanceComparedToTraitCollection(previous)
                {
                    self.ivars()
                        .with_window(IosWindowState::notify_appearance_changed);
                }
            }
        }


        // A paste the person starts (⌘V, a menu) comes as UIKit's `paste:`:
        // the pasteboard may be read without asking while it runs (`paste`).
        #[unsafe(method(canPerformAction:withSender:))]
        fn can_perform_action(&self, action: Sel, sender: Option<&AnyObject>) -> bool {
            if action == sel!(paste:) {
                super::paste::available()
            } else {
                unsafe { msg_send![super(self), canPerformAction: action, withSender: sender] }
            }
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            super::paste::started();
            self.ivars().with_window(|window| window.press_key_with("v", gpui::Modifiers::command()));
        }

        // Takes hardware keys while no text field has the keyboard.
        #[unsafe(method(canBecomeFirstResponder))]
        fn can_become_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(pressesBegan:withEvent:))]
        fn presses_began(&self, presses: &NSSet<UIPress>, _event: Option<&UIPressesEvent>) {
            self.ivars().with_window(|window| window.presses(presses, true));
        }

        #[unsafe(method(pressesEnded:withEvent:))]
        fn presses_ended(&self, presses: &NSSet<UIPress>, _event: Option<&UIPressesEvent>) {
            self.ivars().with_window(|window| window.presses(presses, false));
        }

        #[unsafe(method(touchesBegan:withEvent:))]
        fn touches_began(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(touchesMoved:withEvent:))]
        fn touches_moved(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(touchesEnded:withEvent:))]
        fn touches_ended(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(touchesCancelled:withEvent:))]
        fn touches_cancelled(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(handleScroll:))]
        fn handle_scroll(&self, recognizer: &UIPanGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_scroll(recognizer));
        }

        #[unsafe(method(handleBackGesture:))]
        fn handle_back_gesture(&self, recognizer: &UIScreenEdgePanGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_back_gesture(recognizer));
        }

        #[unsafe(method(handleLongPress:))]
        fn handle_long_press(&self, recognizer: &UIGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_long_press(recognizer));
        }

        #[unsafe(method(handleHover:))]
        fn handle_hover(&self, recognizer: &UIGestureRecognizer) {
            self.ivars().with_window(|window| window.handle_hover(recognizer));
        }

        #[unsafe(method(handleTwoFingerPan:))]
        fn handle_two_finger_pan(&self, recognizer: &UIPanGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_two_finger_pan(recognizer));
        }

        #[unsafe(method(dragSheet:))]
        fn drag_sheet(&self, recognizer: &UIPanGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.drag_sheet(recognizer));
        }

        #[unsafe(method(handlePinch:))]
        fn handle_pinch(&self, recognizer: &UIGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_pinch(recognizer));
        }

        #[unsafe(method(closeWindow:))]
        fn close_window(&self, _sender: &AnyObject) {
            // Not inside `with_window`: closing may drop the window state.
            let window = self.ivars().0.borrow().upgrade();
            if let Some(window) = window {
                window.request_close();
            }
        }
    }

    unsafe impl UIGestureRecognizerDelegate for MetalView {
        // Only the back-gesture recognizer uses this view as its delegate, but
        // UIKit also asks the view itself (UIView's method of this name) about
        // every recognizer attached to it: the others, such as pinch, begin.
        // The pinch and the two-finger pan run together.
        #[unsafe(method(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:))]
        fn should_recognize_simultaneously(
            &self,
            recognizer: &UIGestureRecognizer,
            other: &UIGestureRecognizer,
        ) -> bool {
            !recognizer.isKindOfClass(UIScreenEdgePanGestureRecognizer::class())
                && !other.isKindOfClass(UIScreenEdgePanGestureRecognizer::class())
        }

        #[unsafe(method(gestureRecognizerShouldBegin:))]
        fn gesture_recognizer_should_begin(&self, recognizer: &UIGestureRecognizer) -> bool {
            !recognizer.isKindOfClass(UIScreenEdgePanGestureRecognizer::class())
                || self
                    .ivars()
                    .with_window(|window| window.begin_back_gesture(recognizer))
                    .unwrap_or(false)
        }
    }
);

impl MetalView {
    fn new(frame: CGRect, main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(WindowReference::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn handle_touches(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
        for touch in touches {
            self.ivars()
                .with_window(|window| window.handle_touch(&touch, event));
        }
    }
}

pub(crate) struct IosWindow {
    state: Rc<IosWindowState>,
}

impl std::ops::Deref for IosWindow {
    type Target = IosWindowState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

#[allow(clippy::type_complexity)]
pub(crate) struct IosWindowState {
    platform: Weak<IosPlatformState>,
    keyboard_observers:
        RefCell<Vec<Retained<objc2::runtime::ProtocolObject<dyn NSObjectProtocol>>>>,
    /// The UIWindow object
    window: Retained<UIWindow>,
    /// The UIViewController
    view_controller: Retained<ViewController>,
    /// The Metal-backed UIView
    view: Retained<MetalView>,
    /// The hidden text input view for keyboard input
    text_input_view: Retained<TextInputView>,
    /// Current bounds in pixels
    bounds: Cell<Bounds<Pixels>>,
    /// Scale factor
    scale_factor: Cell<f32>,
    /// Input handler for text input
    input_handler: CallbackSlot<PlatformInputHandler>,
    request_frame_callback: CallbackSlot<Box<dyn FnMut(RequestFrameOptions)>>,
    force_next_frame: Cell<bool>,
    /// Callback for input events
    input_callback: CallbackSlot<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    /// Callback for active status changes
    active_status_callback: CallbackSlot<Box<dyn FnMut(bool)>>,
    visibility: Cell<WindowVisibility>,
    visibility_callback: CallbackSlot<Box<dyn FnMut(WindowVisibility)>>,
    /// Callback for the pointer coming over the window or leaving it.
    hover_status_callback: CallbackSlot<Box<dyn FnMut(bool)>>,
    /// Whether a trackpad's or mouse's pointer is over the window.
    hovered: Cell<bool>,
    /// Callback for resize events
    resize_callback: CallbackSlot<Box<dyn FnMut(Size<Pixels>, f32)>>,
    display_id: Cell<DisplayId>,
    display_changed_callback: CallbackSlot<Box<dyn FnMut()>>,
    /// Callback for move events (not applicable on iOS)
    moved_callback: CallbackSlot<Box<dyn FnMut()>>,
    /// Callback for should close
    should_close_callback: CallbackSlot<Box<dyn FnMut() -> bool>>,
    /// Callback for hit test
    hit_test_callback: CallbackSlot<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    /// Callback for close
    close_callback: CallbackSlot<Box<dyn FnOnce()>>,
    /// Callback for appearance changes
    appearance_changed_callback: CallbackSlot<Box<dyn FnMut()>>,
    insets_changed_callback: CallbackSlot<Box<dyn FnMut(WindowInsets)>>,
    keyboard_dismiss_touch: Cell<Option<KeyboardDismissTouch>>,
    /// Where the back gesture an element claimed started, while it runs.
    back_gesture_start: Cell<Option<Point<Pixels>>>,
    keyboard_height: Cell<f32>,
    /// Current mouse position (from touch)
    mouse_position: Cell<Point<Pixels>>,
    /// The scale of the pinch in progress at its last step.
    pinch_scale: Cell<f64>,
    /// Current modifiers
    modifiers: Cell<Modifiers>,
    renderer: Mutex<MetalRenderer>,
    /// A normal window gets a UIKit scene of its own; pop-ups float in the
    /// scene the user is working in.
    normal: bool,
    title: RefCell<Option<String>>,
    /// The app a window belongs to, and its name: its iPad window's subtitle.
    group: Option<String>,
    app_name: Option<String>,
    /// The size range its iPad window may take.
    size_range: (Option<(f64, f64)>, Option<(f64, f64)>),
    /// A window bar (title, subtitle, beside the system's window controls),
    /// or a sheet's title and Close.
    header: Option<Header>,
    /// For a sheet: the window it shows over, and its content size.
    sheet_parent: Option<Weak<IosWindowState>>,
    sheet_size: Cell<CGSize>,
    /// How far the person moved a sheet from its centered place.
    sheet_offset: Cell<CGPoint>,
    modal: bool,
    /// The sheets over this window.
    sheets: RefCell<Vec<Weak<IosWindowState>>>,
    /// Covers this window's content while a modal sheet is over it.
    dim: RefCell<Option<Retained<UIView>>>,
}

impl IosWindow {
    #[allow(deprecated)] // Window construction can precede scene connection.
    pub fn new(
        _handle: AnyWindowHandle,
        params: WindowParams,
        platform: &Rc<IosPlatformState>,
    ) -> anyhow::Result<Self> {
        // Create the window on the main screen
        let screen = IosDisplay::main();
        let screen_bounds = screen.bounds();
        let scale_factor = screen.scale();

        let main_thread = main_thread_marker().expect("UIKit requires the main thread");
        let role = super::role::take_next();
        // A sheet shows over its parent; one with none, over its app's shown window.
        let sheet_parent = role.as_ref().filter(|role| role.sheet).and_then(|role| {
            role.parent
                .and_then(|parent| platform.application.window_for_handle(parent))
                .or_else(|| platform.application.shown_in_group(&role.group))
                .or_else(|| platform.application.any_in_group(&role.group))
        });
        let sheet = sheet_parent.is_some();
        let normal = matches!(params.kind, WindowKind::Normal) && !sheet;
        let window_scene = if let Some(parent) = &sheet_parent {
            parent.scene()
        } else if normal && role.is_none() {
            platform.application.free_scene()
        } else {
            platform.application.window_scene()
        };
        let window_scene = window_scene.as_deref();
        let (window, screen_bounds_cg, scale) = if let Some(scene) = window_scene {
            let screen = scene.screen();
            let bounds = if normal {
                #[allow(deprecated)]
                scene.coordinateSpace().bounds()
            } else if let Some(parent) = &sheet_parent {
                parent.sheet_frame(CGSize::new(
                    f32::from(params.bounds.size.width) as f64,
                    f32::from(params.bounds.size.height) as f64,
                ))
            } else {
                // A pop-up sits where GPUI places it, over its parent's scene.
                CGRect::new(
                    CGPoint::new(
                        f32::from(params.bounds.origin.x) as f64,
                        f32::from(params.bounds.origin.y) as f64,
                    ),
                    CGSize::new(
                        f32::from(params.bounds.size.width) as f64,
                        f32::from(params.bounds.size.height) as f64,
                    ),
                )
            };
            let window = UIWindow::initWithWindowScene(UIWindow::alloc(main_thread), scene);
            window.setFrame(bounds);
            if sheet {
                window.setWindowLevel(unsafe { UIWindowLevelNormal } + 0.5);
                let layer = window.layer();
                layer.setCornerRadius(12.0);
                layer.setMasksToBounds(true);
            } else if !normal {
                window.setWindowLevel(unsafe { UIWindowLevelNormal } + 1.0);
            }
            (window, bounds, screen.scale())
        } else {
            let screen = UIScreen::mainScreen(main_thread);
            let bounds = screen.bounds();
            let window = UIWindow::initWithFrame(UIWindow::alloc(main_thread), bounds);
            (window, bounds, screen.scale())
        };

        let metal_frame = CGRect::new(CGPoint::ZERO, screen_bounds_cg.size);
        let view = MetalView::new(metal_frame, main_thread);

        let layer = view.layer();
        layer.setContentsScale(scale);

        // Auto-resize the Metal view when the parent view changes size
        // (e.g. rotation). UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight
        view.setAutoresizingMask(
            UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
        );

        // Enable user interaction on the Metal view for touch handling
        view.setUserInteractionEnabled(true);
        view.setMultipleTouchEnabled(true);

        // UIKit has no public API for the display's corner radius, but since
        // iOS 26 a view filling the window with a container-concentric corner
        // configuration resolves its radii to the display's. This only sets the
        // layer's corner radius; without `masksToBounds` the Metal drawable is
        // not clipped.
        if reports_corner_radii(&view) {
            view.setCornerConfiguration(&UICornerConfiguration::configurationWithRadius(
                &UICornerRadius::containerConcentricRadius(),
            ));
        }

        // Trackpad and mouse-wheel scrolling reaches UIKit apps only through a
        // pan recognizer that accepts scroll types. With no allowed touch types
        // it never claims finger touches, so those keep reaching `touchesBegan`
        // and GPUI's own recognizer.
        let scroll_target: &AnyObject = &view;
        // SAFETY: `MetalView` implements `handleScroll:` and owns the recognizer.
        let scroll_recognizer = unsafe {
            UIPanGestureRecognizer::initWithTarget_action(
                UIPanGestureRecognizer::alloc(main_thread),
                Some(scroll_target),
                Some(sel!(handleScroll:)),
            )
        };
        scroll_recognizer.setAllowedScrollTypesMask(UIScrollTypeMask::All);
        scroll_recognizer.setAllowedTouchTypes(&NSArray::new());
        scroll_recognizer.setCancelsTouchesInView(false);
        view.addGestureRecognizer(&scroll_recognizer);

        // The leading-edge swipe is iOS's back gesture. The delegate lets it
        // begin only when an element claims it; it then cancels the touches it
        // took over, so GPUI's own recognizer abandons any pan it had started.
        let back_target: &AnyObject = &view;
        // SAFETY: `MetalView` implements `handleBackGesture:` and owns the recognizer.
        let back_recognizer = unsafe {
            UIScreenEdgePanGestureRecognizer::initWithTarget_action(
                UIScreenEdgePanGestureRecognizer::alloc(main_thread),
                Some(back_target),
                Some(sel!(handleBackGesture:)),
            )
        };
        back_recognizer.setEdges(
            if view.effectiveUserInterfaceLayoutDirection()
                == UIUserInterfaceLayoutDirection::RightToLeft
            {
                UIRectEdge::Right
            } else {
                UIRectEdge::Left
            },
        );
        back_recognizer.setDelegate(Some(ProtocolObject::from_ref(&*view)));
        view.addGestureRecognizer(&back_recognizer);

        // Touch and hold is the secondary click: it opens context menus. It
        // cancels the touch it recognizes, so that touch doesn't also click.
        // SAFETY: `MetalView` implements `handleLongPress:` and owns the recognizer.
        unsafe {
            let long_press: *mut AnyObject =
                msg_send![objc2::class!(UILongPressGestureRecognizer), alloc];
            let long_press: Retained<UIGestureRecognizer> = Retained::from_raw(msg_send![
                long_press,
                initWithTarget: &*view,
                action: sel!(handleLongPress:)
            ])
            .expect("UIKit returns a long-press recognizer");
            let _: () = msg_send![&long_press, setMinimumPressDuration: 0.5f64];
            view.addGestureRecognizer(&long_press);

            // A trackpad's or mouse's pointer moving over the window, with
            // no button down, hovers what it is over.
            let hover: *mut AnyObject = msg_send![objc2::class!(UIHoverGestureRecognizer), alloc];
            let hover: Retained<UIGestureRecognizer> = Retained::from_raw(msg_send![
                hover,
                initWithTarget: &*view,
                action: sel!(handleHover:)
            ])
            .expect("UIKit returns a hover recognizer");
            view.addGestureRecognizer(&hover);

            // Two fingers pinching zoom what shows a zoomable view.
            let pinch: *mut AnyObject =
                msg_send![objc2::class!(UIPinchGestureRecognizer), alloc];
            let pinch: Retained<UIGestureRecognizer> = Retained::from_raw(msg_send![
                pinch,
                initWithTarget: &*view,
                action: sel!(handlePinch:)
            ])
            .expect("UIKit returns a pinch recognizer");
            pinch.setDelegate(Some(ProtocolObject::from_ref(&*view)));
            view.addGestureRecognizer(&pinch);

            // Two fingers moving together pan what a pinch zoomed; with the
            // pinch, they track one gesture (`shouldRecognizeSimultaneously`).
            let two_finger_pan = UIPanGestureRecognizer::initWithTarget_action(
                UIPanGestureRecognizer::alloc(main_thread),
                Some(&*view),
                Some(sel!(handleTwoFingerPan:)),
            );
            two_finger_pan.setMinimumNumberOfTouches(2);
            two_finger_pan.setMaximumNumberOfTouches(2);
            two_finger_pan.setDelegate(Some(ProtocolObject::from_ref(&*view)));
            view.addGestureRecognizer(&two_finger_pan);
        }

        let view_controller = ViewController::new(main_thread);
        let mut header = None;
        if normal || sheet {
            // GPUI draws inside the safe area: never under the status bar,
            // the home indicator or the window controls. The container
            // fills the window; the strips around GPUI show its background.
            let container = UIView::initWithFrame(UIView::alloc(main_thread), screen_bounds_cg);
            container.setAutoresizingMask(
                UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
            );
            unsafe {
                let color: *mut AnyObject =
                    msg_send![objc2::class!(UIColor), systemBackgroundColor];
                let _: () = msg_send![&container, setBackgroundColor: color];
            }
            view.setAutoresizingMask(UIViewAutoresizing::empty());
            container.addSubview(&view);
            let wants_title_bar = params
                .titlebar
                .as_ref()
                .is_some_and(|titlebar| !titlebar.appears_transparent);
            let bar = if sheet {
                Some(Header::sheet(&view, main_thread))
            } else if role.is_some() || wants_title_bar {
                let bar = Header::window(&view, main_thread);
                if let Some(role) = &role {
                    bar.set_subtitle(&role.app_name);
                }
                Some(bar)
            } else {
                None
            };
            if let Some(bar) = bar {
                if let Some(title) = params.titlebar.as_ref().and_then(|t| t.title.as_ref()) {
                    bar.set_title(title);
                }
                container.addSubview(&bar.bar);
                // A sheet that doesn't block its window moves by its bar.
                if sheet && !role.as_ref().is_some_and(|role| role.modal) {
                    let target: &AnyObject = &view;
                    // SAFETY: `MetalView` implements `dragSheet:` and outlives its bar.
                    let drag = unsafe {
                        UIPanGestureRecognizer::initWithTarget_action(
                            UIPanGestureRecognizer::alloc(main_thread),
                            Some(target),
                            Some(sel!(dragSheet:)),
                        )
                    };
                    bar.bar.addGestureRecognizer(&drag);
                }
                header = Some(bar);
            }
            view_controller.setView(Some(&container));
        } else {
            view_controller.setView(Some(&view));
        }
        window.setRootViewController(Some(&view_controller));

        // A normal window with a role is built in the scene in use and shows
        // once it has an iPad window of its own (`attach_to_scene`); so does
        // a sheet whose parent is still waiting for one.
        let waits = (normal && role.is_some())
            || sheet_parent
                .as_ref()
                .is_some_and(|parent| !platform.application.has_scene(parent));
        if !waits {
            window.makeKeyAndVisible();
            view.becomeFirstResponder();
        }

        // Create a hidden text input view for keyboard handling.
        // Uses our custom GPUITextInputView which implements UIKeyInput
        // so iOS actually routes keyboard text to us.
        let text_input_frame = CGRect::new(CGPoint::ZERO, CGSize::new(1.0, 1.0));
        let text_input_view = TextInputView::new(text_input_frame, main_thread);
        text_input_view.setAlpha(0.01);
        text_input_view.setUserInteractionEnabled(true);
        view.addSubview(&text_input_view);

        let pixel_w = (screen_bounds_cg.size.width * scale) as i32;
        let pixel_h = (screen_bounds_cg.size.height * scale) as i32;
        let mut renderer = MetalRenderer::from_layer(
            MetalContext::default(),
            layer
                .downcast_ref::<CAMetalLayer>()
                .ok_or_else(|| anyhow::anyhow!("UIKit view must have a CAMetalLayer"))?,
            false,
        );
        renderer.update_drawable_size(size(DevicePixels(pixel_w), DevicePixels(pixel_h)));

        let state = IosWindowState {
            platform: Rc::downgrade(platform),
            keyboard_observers: RefCell::default(),
            display_id: Cell::new(IosDisplay::from_screen(window.screen()).id()),
            display_changed_callback: CallbackSlot::default(),
            window,
            view_controller,
            view,
            text_input_view,
            bounds: Cell::new(screen_bounds),
            scale_factor: Cell::new(scale_factor),
            input_handler: CallbackSlot::default(),
            request_frame_callback: CallbackSlot::default(),
            force_next_frame: Cell::new(true),
            input_callback: CallbackSlot::default(),
            active_status_callback: CallbackSlot::default(),
            visibility: Cell::new(WindowVisibility::Visible),
            visibility_callback: CallbackSlot::default(),
            hover_status_callback: CallbackSlot::default(),
            hovered: Cell::new(false),
            resize_callback: CallbackSlot::default(),
            moved_callback: CallbackSlot::default(),
            should_close_callback: CallbackSlot::default(),
            hit_test_callback: CallbackSlot::default(),
            close_callback: CallbackSlot::default(),
            appearance_changed_callback: CallbackSlot::default(),
            insets_changed_callback: CallbackSlot::default(),
            keyboard_dismiss_touch: Cell::new(None),
            back_gesture_start: Cell::new(None),
            keyboard_height: Cell::new(0.),
            mouse_position: Cell::new(Point::default()),
            pinch_scale: Cell::new(1.0),
            modifiers: Cell::new(Modifiers::default()),
            renderer: Mutex::new(renderer),
            normal,
            title: RefCell::default(),
            group: role.as_ref().map(|role| role.group.clone()),
            app_name: role.as_ref().map(|role| role.app_name.clone()),
            // A window with no role keeps the minimum its app opened it with.
            size_range: role.as_ref().map_or(
                (
                    params.window_min_size.map(|size| {
                        (f32::from(size.width) as f64, f32::from(size.height) as f64)
                    }),
                    None,
                ),
                |role| (role.min_size, role.max_size),
            ),
            header,
            sheet_parent: sheet_parent.as_ref().map(Rc::downgrade),
            sheet_size: Cell::new(CGSize::new(
                f32::from(params.bounds.size.width) as f64,
                f32::from(params.bounds.size.height) as f64,
            )),
            sheet_offset: Cell::new(CGPoint::ZERO),
            modal: role.as_ref().is_some_and(|role| role.modal),
            sheets: RefCell::default(),
            dim: RefCell::default(),
        };

        Ok(Self {
            state: Rc::new(state),
        })
    }

    pub(crate) fn register(&self) {
        if let Some(platform) = self.platform.upgrade() {
            platform.application.register_window(&self.state);
        }

        let window = Rc::downgrade(&self.state);
        *self.view_controller.ivars().0.borrow_mut() = window.clone();
        *self.view.ivars().0.borrow_mut() = window.clone();
        self.text_input_view.set_window(window);

        self.state.register_keyboard_observers();
        if self.state.normal
            && let Some(platform) = self.platform.upgrade()
        {
            platform.application.place_window(&self.state);
        }
        if let Some(parent) = self.state.sheet_parent.as_ref().and_then(Weak::upgrade) {
            parent.sheets.borrow_mut().push(Rc::downgrade(&self.state));
            parent.update_dim();
        }
    }
}

impl IosWindowState {
    /// The room the window controls pill takes at a window's leading edge.
    const CONTROLS_WIDTH: f64 = 78.0;

    /// The sizes the person may resize the window's iPad window to, as the
    /// app allows.
    pub(super) fn restrict_size(&self, scene: &UIWindowScene) {
        // SAFETY: UISceneSizeRestrictions on the main thread.
        unsafe {
            let restrictions: *mut AnyObject = msg_send![scene, sizeRestrictions];
            let (min, max) = self.size_range;
            // Above the app's own size: the window's header, or for a window
            // without one, the system's window controls, measured once laid out.
            let laid_out = self.window.frame().size.height - self.view.bounds().size.height;
            let bar = if self.header.is_none() && self.view.bounds().size.height > 0. {
                laid_out.max(0.)
            } else {
                Header::HEIGHT
            };
            if !restrictions.is_null() {
                if let Some((width, height)) = min {
                    let _: () = msg_send![
                        restrictions,
                        setMinimumSize: CGSize::new(width, height + bar)
                    ];
                }
                if let Some((width, height)) = max {
                    let _: () = msg_send![
                        restrictions,
                        setMaximumSize: CGSize::new(width, height + bar)
                    ];
                }
            }
        }
    }

    pub(super) fn attach_to_scene(&self, scene: &UIWindowScene) {
        self.window.setWindowScene(Some(scene));
        #[allow(deprecated)]
        self.window.setFrame(scene.coordinateSpace().bounds());
        // The App Switcher, Exposé and the app menu's window list name a
        // window by its title, and its app by the subtitle.
        if let Some(title) = self.title.borrow().as_deref() {
            scene.setTitle(Some(&NSString::from_str(title)));
        }
        if let Some(app) = &self.app_name {
            scene.setSubtitle(&NSString::from_str(app));
        }
        self.restrict_size(scene);
        self.window.makeKeyAndVisible();
        self.take_keys();
        // Its sheets come along, over it.
        let sheets: Vec<_> = self.sheets.borrow().iter().filter_map(Weak::upgrade).collect();
        for sheet in &sheets {
            sheet.window.setWindowScene(Some(scene));
            sheet.window.makeKeyAndVisible();
        }
        self.handle_layout_change();
        // Its layout placed them; each now draws at its new size.
        for sheet in &sheets {
            sheet.handle_layout_change();
        }
        // iPadOS places the window controls after the scene connects and
        // reports no layout change for it: lay out again once they're up.
        if let Some(container) = self.view.superview() {
            for delay in [0.3f64, 1.0] {
                // SAFETY: a UIView method with no argument, on the main thread.
                unsafe {
                    let _: () = msg_send![
                        &container,
                        performSelector: sel!(setNeedsLayout),
                        withObject: std::ptr::null_mut::<AnyObject>(),
                        afterDelay: delay
                    ];
                }
            }
        }
    }

    /// Lays this window out again (its scene's geometry changed).
    pub(super) fn relayout(&self) {
        // A window keeps the frame it was given: one whose scene changed size
        // (a size restriction applied after it opened) follows it here.
        if self.normal
            && let Some(scene) = self.scene()
        {
            #[allow(deprecated)]
            let bounds = scene.coordinateSpace().bounds();
            if self.window.frame() != bounds {
                self.window.setFrame(bounds);
            }
        }
        if let Some(container) = self.view.superview() {
            container.setNeedsLayout();
        }
    }

    /// The container's insets that clear the system's window controls
    /// (iPadOS 26): beside them with `axis` 1 (horizontal), below them with 2.
    fn controls_insets(container: &UIView, axis: usize) -> Option<objc2_ui_kit::UIEdgeInsets> {
        // iPadOS 26 and later.
        let region = AnyClass::get(c"UIViewLayoutRegion")?;
        // SAFETY: UIKit calls on the main thread, on a class that exists.
        unsafe {
            let region: *mut AnyObject = if axis == 1 {
                msg_send![region, marginsLayoutRegionWithCornerAdaptation: axis]
            } else {
                msg_send![region, safeAreaLayoutRegionWithCornerAdaptation: axis]
            };
            let insets: objc2_ui_kit::UIEdgeInsets =
                msg_send![container, edgeInsetsForLayoutRegion: region];
            Some(insets)
        }
    }

    /// Lays out a normal window edge to edge, like iPadOS's own apps: GPUI
    /// draws under the status bar and the home indicator and reads them as
    /// its safe area insets. A window's bar reaches up under the status bar,
    /// its content starting below it.
    fn fit_to_safe_area(&self) {
        let sheet = self.sheet_parent.is_some();
        if !self.normal && !sheet {
            return;
        }
        let Some(container) = self.view.superview() else {
            return;
        };
        let bounds = container.bounds();
        let insets = if sheet {
            objc2_ui_kit::UIEdgeInsets { top: 0.0, left: 0.0, bottom: 0.0, right: 0.0 }
        } else {
            container.safeAreaInsets()
        };
        let width = (bounds.size.width - insets.left - insets.right).max(1.0);
        let bar = self.header.as_ref().map_or(0.0, |bar| {
            let mut leading = Self::controls_insets(&container, 1)
                .map_or(0.0, |beside| beside.left - insets.left);
            // A window smaller than its screen shows the controls pill, but
            // iPadOS reports it only after the window first moves or resizes:
            // until then keep clear of the pill's width.
            if AnyClass::get(c"UIViewLayoutRegion").is_some() {
                let screen = self.window.screen().bounds().size;
                let frame = self.window.frame().size;
                if frame.width < screen.width || frame.height < screen.height {
                    leading = leading.max(Self::CONTROLS_WIDTH);
                }
            }
            bar.layout(
                CGRect::new(
                    CGPoint::new(insets.left, 0.0),
                    CGSize::new(width, insets.top + Header::HEIGHT),
                ),
                insets.top,
                leading,
            );
            insets.top + Header::HEIGHT
        });
        let frame = if self.header.is_some() || sheet {
            CGRect::new(
                CGPoint::new(insets.left, bar),
                CGSize::new(width, (bounds.size.height - bar).max(1.0)),
            )
        } else {
            bounds
        };
        if self.view.frame() != frame {
            self.view.setFrame(frame);
        }
        if let Some(dim) = self.dim.borrow().as_ref() {
            dim.setFrame(self.view.frame());
        }
        for sheet in self.sheets.borrow().iter().filter_map(Weak::upgrade) {
            sheet.window.setFrame(self.moved_sheet_frame(&sheet, sheet.sheet_offset.get()));
        }
    }

    pub(super) fn scene(&self) -> Option<Retained<UIWindowScene>> {
        self.window.windowScene()
    }

    pub(super) fn is_normal(&self) -> bool {
        self.normal
    }

    /// The user closed this window's scene: GPUI may keep it open (unsaved
    /// work), else it closes.
    pub(super) fn request_close(&self) {
        let close = self.should_close_callback.with(|callback| callback()).unwrap_or(true);
        if close && let Some(callback) = self.close_callback.take() {
            callback();
        }
    }

    fn register_keyboard_observers(self: &Rc<Self>) {
        unsafe {
            let notification_center = NSNotificationCenter::defaultCenter();

            let window = Rc::downgrade(self);
            let frame_change_block =
                block2::RcBlock::new(move |notification: NonNull<NSNotification>| {
                    let Some(user_info) = notification.as_ref().userInfo() else {
                        return;
                    };
                    let Some(frame_value) = user_info.objectForKey(UIKeyboardFrameEndUserInfoKey)
                    else {
                        return;
                    };
                    let Some(frame_value) = frame_value.downcast_ref::<NSValue>() else {
                        return;
                    };
                    let frame = frame_value.CGRectValue();
                    if let Some(window) = window.upgrade() {
                        window.set_keyboard_height(window.keyboard_overlap(frame));
                    }
                });

            let window = Rc::downgrade(self);
            let hide_block = block2::RcBlock::new(move |_notification: NonNull<NSNotification>| {
                if let Some(window) = window.upgrade() {
                    window.set_keyboard_height(0.);
                }
            });

            let frame_observer = notification_center.addObserverForName_object_queue_usingBlock(
                Some(UIKeyboardWillChangeFrameNotification),
                None,
                None,
                &frame_change_block,
            );
            let hide_observer = notification_center.addObserverForName_object_queue_usingBlock(
                Some(UIKeyboardWillHideNotification),
                None,
                None,
                &hide_block,
            );
            self.keyboard_observers
                .borrow_mut()
                .extend([frame_observer, hide_observer]);
        }
    }

    /// Delivers a UIKit touch through GPUI's platform-neutral touch API.
    pub fn handle_touch(&self, touch: &UITouch, _event: Option<&UIEvent>) {
        // GPUI timestamps delivery itself. Replaying UIKit's historical coalesced
        // samples here would make them appear simultaneous and distort fling velocity.
        let id = touch_id(touch);
        self.handle_touch_sample(touch, id);
    }

    fn handle_touch_sample(&self, touch: &UITouch, id: TouchId) {
        let position = touch_location_in_view(touch, &self.view);
        self.mouse_position.set(position);
        self.text_input_view.touched();
        if matches!(touch_phase(touch), TouchPhase::Started) {
            self.text_input_view.touch_began();
        }

        let event = TouchEvent {
            id,
            phase: touch_phase(touch),
            position,
            predicted_position: None,
            force: touch_force(touch),
        };
        self.handle_keyboard_dismiss_touch(&event);
        self.input_callback
            .with(|callback| callback(PlatformInput::Touch(event)));
    }

    /// Delivers one step of a trackpad or mouse-wheel scroll gesture.
    fn handle_scroll(&self, recognizer: &UIPanGestureRecognizer) {
        let Some(event) = scroll_event(recognizer, &self.view) else {
            return;
        };
        self.mouse_position.set(event.position);
        self.modifiers.set(event.modifiers);
        self.input_callback
            .with(|callback| callback(PlatformInput::Scroll(event)));
    }

    /// Offers a starting back gesture to GPUI, returning whether an element
    /// claimed it.
    fn begin_back_gesture(&self, recognizer: &UIGestureRecognizer) -> bool {
        let location = recognizer.locationInView(Some(&self.view));
        let start_position = Point::new(px(location.x as f32), px(location.y as f32));
        let claimed = self
            .input_callback
            .with(|callback| {
                callback(PlatformInput::BackGesture(BackGestureEvent {
                    phase: BackGesturePhase::Started,
                    start_position,
                    progress: 0.,
                }))
            })
            .is_some_and(|result| result.default_prevented);
        self.back_gesture_start
            .set(claimed.then_some(start_position));
        claimed
    }

    /// Delivers a step of a claimed back gesture.
    fn handle_back_gesture(&self, recognizer: &UIScreenEdgePanGestureRecognizer) {
        let Some(start_position) = self.back_gesture_start.get() else {
            return;
        };
        let Some(event) = back_event(recognizer, &self.view, start_position) else {
            return;
        };
        if matches!(
            event.phase,
            BackGesturePhase::Committed | BackGesturePhase::Cancelled
        ) {
            self.back_gesture_start.set(None);
        }
        self.input_callback
            .with(|callback| callback(PlatformInput::BackGesture(event)));
    }

    /// One step of a pinch: GPUI gets the change in scale since the last step.
    fn handle_pinch(&self, recognizer: &UIGestureRecognizer) {
        // SAFETY: a UIPinchGestureRecognizer's scale, on the main thread.
        let scale: f64 = unsafe { msg_send![recognizer, scale] };
        let phase = match recognizer.state().0 {
            1 => {
                self.pinch_scale.set(1.0);
                TouchPhase::Started
            }
            2 => TouchPhase::Moved,
            3 => TouchPhase::Ended,
            _ => TouchPhase::Cancelled,
        };
        let previous = self.pinch_scale.replace(scale);
        let delta = if previous > 0.0 { scale / previous - 1.0 } else { 0.0 };
        let location = recognizer.locationInView(Some(&self.view));
        let position = Point::new(px(location.x as f32), px(location.y as f32));
        let modifiers = self.modifiers.get();
        self.input_callback.with(|callback| {
            callback(PlatformInput::Pinch(PinchEvent {
                position,
                delta: delta as f32,
                modifiers,
                phase,
            }))
        });
    }

    /// Hardware keys come to GPUI's view while no text field has them.
    fn take_keys(&self) {
        if !self.text_input_view.isFirstResponder() {
            self.view.becomeFirstResponder();
        }
    }

    /// Hardware keys going down or up, as GPUI key events.
    pub(super) fn presses(&self, presses: &NSSet<UIPress>, down: bool) {
        let Some(main_thread) = main_thread_marker() else {
            return;
        };
        for press in presses.iter() {
            if let Some(key) = press.key(main_thread) {
                self.key_event(&key, down);
            }
        }
    }

    /// One hardware key going down or up, as a GPUI key event.
    pub(super) fn key_event(&self, key: &UIKey, down: bool) {
        let Some(mut keystroke) = super::events::keystroke(key) else {
            return;
        };
        // The shortcuts bar's latched Control applies to the next key.
        if down && self.text_input_view.take_control_latch() {
            keystroke.modifiers.control = true;
            keystroke.key_char = None;
        }
        self.modifiers.set(keystroke.modifiers);
        let typed = keystroke.key_char.clone().filter(|_| down);
        let input = if down {
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                physical_key: None,
                key_metadata: None,
                is_held: false,
                prefer_character_input: false,
            })
        } else {
            PlatformInput::KeyUp(KeyUpEvent {
                keystroke,
                physical_key: None,
                key_metadata: None,
            })
        };
        let handled = self
            .input_callback
            .with(|callback| !callback(input).propagate)
            .unwrap_or(false);
        // As on a Mac: a character no binding took is typed into the focused
        // field (a key pressed before the field took the keyboard).
        if !handled && let Some(text) = typed {
            self.text_input_view.type_text(&text);
        }
    }

    /// Presses and releases `key` with no modifiers: a key the on-screen
    /// keyboard sends that no text edit covers.
    pub(super) fn press_key(&self, key: &str) {
        self.press_key_with(key, Modifiers::default());
    }

    /// Presses and releases `key` with `modifiers`.
    pub(super) fn press_key_with(&self, key: &str, modifiers: Modifiers) {
        let keystroke = Keystroke {
            modifiers,
            key: key.to_owned(),
            key_char: None,
        };
        self.input_callback.with(|callback| {
            callback(PlatformInput::KeyDown(KeyDownEvent {
                keystroke: keystroke.clone(),
                physical_key: None,
                key_metadata: None,
                is_held: false,
                prefer_character_input: false,
            }));
            callback(PlatformInput::KeyUp(KeyUpEvent {
                keystroke,
                physical_key: None,
                key_metadata: None,
            }));
        });
    }

    /// Two fingers moving together: a pinch step with no change of scale,
    /// at the fingers' center, so GPUI pans what the pinch zoomed.
    fn handle_two_finger_pan(&self, recognizer: &UIPanGestureRecognizer) {
        let phase = match recognizer.state().0 {
            1 => TouchPhase::Started,
            2 => TouchPhase::Moved,
            3 => TouchPhase::Ended,
            _ => TouchPhase::Cancelled,
        };
        let location = recognizer.locationInView(Some(&self.view));
        let position = Point::new(px(location.x as f32), px(location.y as f32));
        let modifiers = self.modifiers.get();
        self.input_callback.with(|callback| {
            callback(PlatformInput::Pinch(PinchEvent {
                position,
                delta: 0.0,
                modifiers,
                phase,
            }))
        });
    }

    /// A recognized touch and hold presses and releases the secondary button
    /// where it began.
    fn handle_long_press(&self, recognizer: &UIGestureRecognizer) {
        // UIGestureRecognizerStateBegan
        if recognizer.state().0 != 1 {
            return;
        }
        let location = recognizer.locationInView(Some(&self.view));
        let position = Point::new(px(location.x as f32), px(location.y as f32));
        self.mouse_position.set(position);
        let modifiers = self.modifiers.get();
        self.input_callback.with(|callback| {
            callback(PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Right,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }))
        });
        self.input_callback.with(|callback| {
            callback(PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Right,
                position,
                modifiers,
                click_count: 1,
            }))
        });
    }

    /// The pointer moving over the window with no button down, or leaving it.
    fn handle_hover(&self, recognizer: &UIGestureRecognizer) {
        let location = recognizer.locationInView(Some(&self.view));
        let position = Point::new(px(location.x as f32), px(location.y as f32));
        let modifiers = self.modifiers.get();
        // UIGestureRecognizerStateBegan, Changed; Ended, Cancelled, Failed.
        let input = match recognizer.state().0 {
            1 | 2 => {
                if !self.hovered.replace(true) {
                    self.hover_status_callback.with(|callback| callback(true));
                }
                self.mouse_position.set(position);
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                })
            }
            3..=5 => {
                if self.hovered.replace(false) {
                    self.hover_status_callback.with(|callback| callback(false));
                }
                PlatformInput::MouseExited(MouseExitEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                })
            }
            _ => return,
        };
        self.input_callback.with(|callback| {
            callback(input);
        });
    }

    fn handle_keyboard_dismiss_touch(&self, event: &TouchEvent) {
        if self.keyboard_height.get() <= 0. {
            self.keyboard_dismiss_touch.set(None);
            return;
        }

        match event.phase {
            TouchPhase::Started if self.keyboard_dismiss_touch.get().is_none() => {
                self.keyboard_dismiss_touch.set(Some(KeyboardDismissTouch {
                    id: event.id,
                    start_position: event.position,
                }));
            }
            TouchPhase::Moved => {
                let Some(touch) = self
                    .keyboard_dismiss_touch
                    .get()
                    .filter(|touch| touch.id == event.id)
                else {
                    return;
                };
                let delta = event.position - touch.start_position;
                if delta.y >= KEYBOARD_DISMISS_DISTANCE && delta.y.abs() > delta.x.abs() * 1.25 {
                    self.keyboard_dismiss_touch.set(None);
                    self.hide_keyboard();
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if self
                    .keyboard_dismiss_touch
                    .get()
                    .is_some_and(|touch| touch.id == event.id)
                {
                    self.keyboard_dismiss_touch.set(None);
                }
            }
            TouchPhase::Started => {}
        }
    }

    pub(super) fn request_frame(&self) {
        self.request_frame_callback.with(|callback| {
            let force_render = self.force_next_frame.replace(false);
            callback(RequestFrameOptions {
                force_render,
                ..Default::default()
            });
        });
    }

    /// Query the safe area insets from the UIView.
    ///
    /// Returns `(top, bottom, left, right)` in logical points.
    /// These represent the areas occupied by system UI (status bar,
    /// home indicator, camera notch) that content should avoid.
    fn safe_area_insets(&self) -> (f32, f32, f32, f32) {
        let insets = self.view.safeAreaInsets();
        (
            insets.top as f32,
            insets.bottom as f32,
            insets.left as f32,
            insets.right as f32,
        )
    }

    fn current_insets(&self) -> WindowInsets {
        let (top, bottom, left, right) = self.safe_area_insets();
        WindowInsets {
            safe_area: Edges {
                top: px(top),
                right: px(right),
                bottom: px(bottom),
                left: px(left),
            },
            ime: Edges {
                bottom: px(self.keyboard_height.get()),
                ..Default::default()
            },
            corner_radii: self.display_corner_radii(),
            window_controls: self.window_controls(top),
        }
    }

    /// The window controls over a window without a bar of its own, which
    /// draws its title bar beside them (as UIKit's bars do).
    fn window_controls(&self, safe_top: f32) -> Edges<Pixels> {
        if !self.normal || self.header.is_some() {
            return Edges::default();
        }
        let Some(below) = Self::controls_insets(&self.view, 2) else {
            return Edges::default();
        };
        // No controls drawn over the content (full screen: in the status bar).
        if below.top as f32 <= safe_top + 1. {
            return Edges::default();
        }
        let mut leading = Self::controls_insets(&self.view, 1).map_or(0., |beside| beside.left);
        // iPadOS reports the pill's width only after the window first moves
        // or resizes; until then keep clear of it.
        let screen = self.window.screen().bounds().size;
        let frame = self.window.frame().size;
        if frame.width < screen.width || frame.height < screen.height {
            leading = leading.max(Self::CONTROLS_WIDTH);
        }
        Edges {
            top: px(below.top as f32),
            left: px(leading as f32),
            ..Default::default()
        }
    }

    fn display_corner_radii(&self) -> Corners<Pixels> {
        if !reports_corner_radii(&self.view) {
            return Corners::default();
        }
        let radius = |corner| px(self.view.effectiveRadiusForCorner(corner) as f32);
        Corners {
            top_left: radius(UIRectCorner::TopLeft),
            top_right: radius(UIRectCorner::TopRight),
            bottom_right: radius(UIRectCorner::BottomRight),
            bottom_left: radius(UIRectCorner::BottomLeft),
        }
    }

    fn notify_insets_changed(&self) {
        self.insets_changed_callback
            .with(|callback| callback(self.current_insets()));
    }

    fn notify_appearance_changed(&self) {
        self.appearance_changed_callback.with(|callback| callback());
    }

    /// How far the keyboard, at `frame` in screen coordinates, covers this
    /// window from its bottom edge: none for a keyboard this window's text
    /// view doesn't own (every scene has a key window, so another scene's
    /// keyboard reaches it too) or one that floats clear of it.
    fn keyboard_overlap(&self, frame: CGRect) -> f32 {
        if !self.window.isKeyWindow() || !self.text_input_view.isFirstResponder() {
            return 0.;
        }
        let Some(screen) = self.window.windowScene().map(|scene| scene.screen()) else {
            return frame.size.height as f32;
        };
        let local = self
            .view
            .convertRect_fromCoordinateSpace(frame, &screen.coordinateSpace());
        let bounds = self.view.bounds();
        let bottom = bounds.origin.y + bounds.size.height;
        let covered_left = local.origin.x.max(bounds.origin.x);
        let covered_right = (local.origin.x + local.size.width).min(bounds.origin.x + bounds.size.width);
        if covered_right <= covered_left || local.origin.y >= bottom {
            return 0.;
        }
        (bottom - local.origin.y.max(bounds.origin.y)) as f32
    }

    fn set_keyboard_height(&self, height: f32) {
        let height = height.max(0.);
        if (self.keyboard_height.get() - height).abs() <= 0.5 {
            return;
        }
        self.keyboard_height.set(height);
        if height <= 0. {
            self.keyboard_dismiss_touch.set(None);
        }
        self.notify_insets_changed();
    }

    pub fn show_keyboard(&self) {
        self.text_input_view.set_keyboard_visible(true);
    }

    pub fn hide_keyboard(&self) {
        self.text_input_view.set_keyboard_visible(false);
    }

    pub(super) fn with_input_handler<R>(
        &self,
        callback: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        self.input_handler.with(callback)
    }

    /// Notify the window when its UIKit scene becomes active or inactive.
    pub fn notify_active_status_change(&self, is_active: bool) {
        log::info!("GPUI iOS: Window active status changed to: {}", is_active);
        // A keyboard that left with another scene sends this window no hide.
        if is_active && !self.text_input_view.isFirstResponder() {
            self.set_keyboard_height(0.);
        }

        self.active_status_callback
            .with(|callback| callback(is_active));
    }

    pub(crate) fn notify_visibility_change(&self, visibility: WindowVisibility) {
        if self.visibility.replace(visibility) != visibility {
            self.visibility_callback
                .with(|callback| callback(visibility));
        }
    }

    pub fn handle_layout_change(&self) {
        let view_bounds = self.view.bounds();
        let display = IosDisplay::from_screen(self.window.screen());
        if self.display_id.replace(display.id()) != display.id() {
            self.display_changed_callback.with(|callback| callback());
        }
        let scale = display.scale() as f64;

        let width = view_bounds.size.width as f32;
        let height = view_bounds.size.height as f32;
        let new_scale = scale as f32;

        let old_bounds = self.bounds.get();
        let old_scale = self.scale_factor.get();

        let new_size = size(px(width), px(height));
        self.notify_insets_changed();

        if old_bounds.size == new_size && (old_scale - new_scale).abs() < 0.01 {
            return;
        }
        if self.normal
            && self.header.is_none()
            && let Some(scene) = self.scene()
        {
            self.restrict_size(&scene);
        }

        log::info!(
            "GPUI iOS: Layout changed — {:?} @{:.1}x → {:?} @{:.1}x",
            old_bounds.size,
            old_scale,
            new_size,
            new_scale,
        );

        // Update stored bounds (in logical pixels, matching GPUI convention).
        let new_bounds = Bounds {
            origin: Default::default(),
            size: new_size,
        };
        self.bounds.set(new_bounds);
        self.scale_factor.set(new_scale);
        // Update the Metal layer's contentsScale so the drawable has the
        // correct pixel dimensions.
        self.view.layer().setContentsScale(scale);

        let pixel_width = (width * new_scale) as i32;
        let pixel_height = (height * new_scale) as i32;
        self.renderer
            .lock()
            .update_drawable_size(size(DevicePixels(pixel_width), DevicePixels(pixel_height)));

        self.resize_callback
            .with(|callback| callback(new_size, new_scale));
    }
}

/// The bar at the top of a window: its title and its app, set beside the
/// system's window controls, and a menu with Close for when iPadOS shows no
/// controls (Full Screen Apps). A sheet's bar has its
/// title and Close.
struct Header {
    bar: Retained<UIView>,
    label: Retained<AnyObject>,
    subtitle: Option<Retained<AnyObject>>,
    trailing: Retained<AnyObject>,
    sheet: bool,
}

impl Header {
    const HEIGHT: f64 = 48.0;

    fn bar(main_thread: MainThreadMarker) -> Retained<UIView> {
        let bar = UIView::initWithFrame(UIView::alloc(main_thread), CGRect::ZERO);
        // SAFETY: plain UIKit calls on new views, on the main thread.
        unsafe {
            let color: *mut AnyObject =
                msg_send![objc2::class!(UIColor), secondarySystemBackgroundColor];
            let _: () = msg_send![&bar, setBackgroundColor: color];
            let separator: Retained<AnyObject> =
                Retained::from_raw(msg_send![objc2::class!(UIView), new])
                    .expect("UIKit returns a view");
            let color: *mut AnyObject = msg_send![objc2::class!(UIColor), separatorColor];
            let _: () = msg_send![&separator, setBackgroundColor: color];
            let _: () = msg_send![&separator, setTag: 1isize];
            let _: () = msg_send![&bar, addSubview: &*separator];
        }
        bar
    }

    fn label(bold: bool, size: f64, centered: bool, secondary: bool) -> Retained<AnyObject> {
        // SAFETY: a new UILabel on the main thread.
        unsafe {
            let label: Retained<AnyObject> =
                Retained::from_raw(msg_send![objc2::class!(UILabel), new])
                    .expect("UIKit returns a label");
            let font: *mut AnyObject = if bold {
                msg_send![objc2::class!(UIFont), boldSystemFontOfSize: size]
            } else {
                msg_send![objc2::class!(UIFont), systemFontOfSize: size]
            };
            let _: () = msg_send![&label, setFont: font];
            let color: *mut AnyObject = if secondary {
                msg_send![objc2::class!(UIColor), secondaryLabelColor]
            } else {
                msg_send![objc2::class!(UIColor), labelColor]
            };
            let _: () = msg_send![&label, setTextColor: color];
            // NSLineBreakByTruncatingTail
            let _: () = msg_send![&label, setLineBreakMode: 4isize];
            if centered {
                // NSTextAlignmentCenter
                let _: () = msg_send![&label, setTextAlignment: 1isize];
            }
            label
        }
    }

    /// A window's bar.
    fn window(view: &MetalView, main_thread: MainThreadMarker) -> Self {
        let bar = Self::bar(main_thread);
        let label = Self::label(true, 15.0, false, false);
        let subtitle = Self::label(false, 12.0, false, true);
        // SAFETY: the bar's new subviews, on the main thread.
        let trailing = unsafe {
            // UIButtonTypeSystem
            let button: *mut AnyObject =
                msg_send![objc2::class!(UIButton), buttonWithType: 1isize];
            let button = Retained::retain(button).expect("UIKit returns a button");
            let name = NSString::from_str("ellipsis.circle");
            let image: *mut AnyObject =
                msg_send![objc2::class!(UIImage), systemImageNamed: &*name];
            let _: () = msg_send![&button, setImage: image, forState: 0usize];
            let name = NSString::from_str("Window");
            let _: () = msg_send![&button, setAccessibilityLabel: &*name];
            // UIKit's own Paste, sent to the window's focused view: a paste
            // the person starts, so the app may read what another app copied.
            let paste: Retained<AnyObject> = {
                let title = NSString::from_str("Paste");
                let name = NSString::from_str("doc.on.clipboard");
                let image: *mut AnyObject =
                    msg_send![objc2::class!(UIImage), systemImageNamed: &*name];
                let command: *mut AnyObject = msg_send![
                    objc2::class!(UICommand),
                    commandWithTitle: &*title,
                    image: image,
                    action: sel!(paste:),
                    propertyList: std::ptr::null_mut::<AnyObject>()
                ];
                Retained::retain(command).expect("UIKit returns a command")
            };
            let close = Self::action("Close Window", true, view);
            let children = NSArray::from_retained_slice(&[paste, close]);
            let title = NSString::from_str("");
            let menu: *mut AnyObject =
                msg_send![objc2::class!(UIMenu), menuWithTitle: &*title, children: &*children];
            let _: () = msg_send![&button, setMenu: menu];
            let _: () = msg_send![&button, setShowsMenuAsPrimaryAction: true];
            let _: () = msg_send![&bar, addSubview: &*label];
            let _: () = msg_send![&bar, addSubview: &*subtitle];
            let _: () = msg_send![&bar, addSubview: &*button];
            button
        };
        let header = Self { bar, label, subtitle: Some(subtitle), trailing, sheet: false };
        header.follow_width();
        header
    }

    /// A sheet's bar: its title and Close.
    fn sheet(view: &MetalView, main_thread: MainThreadMarker) -> Self {
        let bar = Self::bar(main_thread);
        let label = Self::label(true, 14.0, true, false);
        // SAFETY: the bar's new subviews, on the main thread.
        let trailing = unsafe {
            let button: *mut AnyObject =
                msg_send![objc2::class!(UIButton), buttonWithType: 1isize];
            let button = Retained::retain(button).expect("UIKit returns a button");
            let title = NSString::from_str("Close");
            let _: () = msg_send![&button, setTitle: &*title, forState: 0usize];
            let button_label: *mut AnyObject = msg_send![&button, titleLabel];
            let font: *mut AnyObject =
                msg_send![objc2::class!(UIFont), boldSystemFontOfSize: 15.0f64];
            let _: () = msg_send![button_label, setFont: font];
            // UIControlEventTouchUpInside
            let _: () = msg_send![
                &button,
                addTarget: view,
                action: sel!(closeWindow:),
                forControlEvents: 1usize << 6
            ];
            let _: () = msg_send![&bar, addSubview: &*label];
            let _: () = msg_send![&bar, addSubview: &*button];
            button
        };
        let header = Self { bar, label, subtitle: None, trailing, sheet: true };
        header.follow_width();
        header
    }

    /// A menu action that closes the window `view` shows.
    fn action(title: &str, destructive: bool, view: &MetalView) -> Retained<AnyObject> {
        // Weak: the view owns the bar that owns this action.
        let target = objc2::rc::Weak::new(view);
        let handler = block2::RcBlock::new(move |_action: *mut AnyObject| {
            if let Some(target) = target.load() {
                // SAFETY: the window's own view handles closeWindow:.
                unsafe {
                    let _: () = msg_send![&target, closeWindow: std::ptr::null_mut::<AnyObject>()];
                }
            }
        });
        let title = NSString::from_str(title);
        // SAFETY: UIAction with a handler block, on the main thread.
        unsafe {
            let action: *mut AnyObject = msg_send![
                objc2::class!(UIAction),
                actionWithTitle: &*title,
                image: std::ptr::null_mut::<AnyObject>(),
                identifier: std::ptr::null_mut::<AnyObject>(),
                handler: &*handler
            ];
            let action = Retained::retain(action).expect("UIKit returns an action");
            if destructive {
                // UIMenuElementAttributesDestructive
                let _: () = msg_send![&action, setAttributes: 1usize << 1];
            }
            action
        }
    }

    fn set_title(&self, title: &str) {
        let title = NSString::from_str(title);
        // SAFETY: a UILabel on the main thread.
        unsafe {
            let _: () = msg_send![&self.label, setText: &*title];
        }
    }

    fn set_subtitle(&self, subtitle: &str) {
        if let Some(label) = &self.subtitle {
            let subtitle = NSString::from_str(subtitle);
            // SAFETY: a UILabel on the main thread.
            unsafe {
                let _: () = msg_send![label, setText: &*subtitle];
            }
        }
    }

    /// Lets the bar's parts follow its width between layouts: labels stretch,
    /// the trailing button keeps to the trailing edge.
    fn follow_width(&self) {
        // UIViewAutoresizingFlexibleWidth, FlexibleLeftMargin
        let (stretch, trailing) = (1usize << 1, 1usize << 0);
        // SAFETY: the bar's own views, on the main thread.
        unsafe {
            let _: () = msg_send![&self.bar, setAutoresizingMask: stretch];
            let _: () = msg_send![&self.label, setAutoresizingMask: stretch];
            if let Some(subtitle) = &self.subtitle {
                let _: () = msg_send![subtitle, setAutoresizingMask: stretch];
            }
            let _: () = msg_send![&self.trailing, setAutoresizingMask: trailing];
            let separator: *mut AnyObject = msg_send![&self.bar, viewWithTag: 1isize];
            if !separator.is_null() {
                let _: () = msg_send![separator, setAutoresizingMask: stretch];
            }
        }
    }

    /// Lays the bar out in `frame`, its content `top` points down (below the
    /// status bar) and `leading` points in (clear of the system's window
    /// controls).
    fn layout(&self, frame: CGRect, top: f64, leading: f64) {
        self.bar.setFrame(frame);
        let width = frame.size.width;
        let height = Self::HEIGHT;
        // SAFETY: the bar's own subviews, on the main thread.
        unsafe {
            let separator: *mut AnyObject = msg_send![&self.bar, viewWithTag: 1isize];
            if !separator.is_null() {
                let _: () = msg_send![separator, setFrame: CGRect::new(
                    CGPoint::new(0.0, top + height - 0.5),
                    CGSize::new(width, 0.5),
                )];
            }
            if self.sheet {
                let _: () = msg_send![&self.label, setFrame: CGRect::new(
                    CGPoint::new(72.0, top),
                    CGSize::new((width - 144.0).max(0.0), height),
                )];
                let _: () = msg_send![&self.trailing, setFrame: CGRect::new(
                    CGPoint::new(width - 72.0, top),
                    CGSize::new(72.0, height),
                )];
                return;
            }
            let start = leading.max(16.0);
            let text_width = (width - start - 56.0).max(0.0);
            let _: () = msg_send![&self.label, setFrame: CGRect::new(
                CGPoint::new(start, top + 6.0),
                CGSize::new(text_width, 20.0),
            )];
            if let Some(subtitle) = &self.subtitle {
                let _: () = msg_send![subtitle, setFrame: CGRect::new(
                    CGPoint::new(start, top + 26.0),
                    CGSize::new(text_width, 16.0),
                )];
            }
            let _: () = msg_send![&self.trailing, setFrame: CGRect::new(
                CGPoint::new(width - 48.0, top),
                CGSize::new(44.0, height),
            )];
        }
    }
}

impl IosWindowState {
    fn view_ptr(&self) -> *mut c_void {
        Retained::as_ptr(&self.view).cast_mut().cast()
    }

    pub(super) fn is_handle(&self, handle: raw_window_handle::RawWindowHandle) -> bool {
        matches!(handle, raw_window_handle::RawWindowHandle::UiKit(handle)
            if handle.ui_view.as_ptr() == self.view_ptr())
    }

    pub(super) fn group(&self) -> Option<String> {
        self.group.clone()
    }

    /// Whether this window draws a bar for the system's window controls to
    /// sit in.
    /// A sheet's frame: centered over this window's content, at `size` plus
    /// its header, inside the content.
    fn sheet_frame(&self, size: CGSize) -> CGRect {
        // SAFETY: UIKit geometry on the main thread.
        let content: CGRect = unsafe {
            msg_send![
                &self.view,
                convertRect: self.view.bounds(),
                toView: std::ptr::null_mut::<AnyObject>()
            ]
        };
        let origin = self.window.frame().origin;
        let width = size.width.min(content.size.width - 24.0).max(160.0);
        let height = (size.height + Header::HEIGHT)
            .min(content.size.height - 24.0)
            .max(Header::HEIGHT + 60.0);
        CGRect::new(
            CGPoint::new(
                origin.x + content.origin.x + (content.size.width - width) / 2.0,
                origin.y + content.origin.y + (content.size.height - height) / 2.0,
            ),
            CGSize::new(width, height),
        )
    }

    /// `sheet`'s frame moved `offset` from its centered place, kept inside
    /// this window's content.
    fn moved_sheet_frame(&self, sheet: &IosWindowState, offset: CGPoint) -> CGRect {
        let centered = self.sheet_frame(sheet.sheet_size.get());
        // SAFETY: UIKit geometry on the main thread.
        let content: CGRect = unsafe {
            msg_send![
                &self.view,
                convertRect: self.view.bounds(),
                toView: std::ptr::null_mut::<AnyObject>()
            ]
        };
        let origin = self.window.frame().origin;
        let (left, top) = (origin.x + content.origin.x, origin.y + content.origin.y);
        let right = (left + content.size.width - centered.size.width).max(left);
        let bottom = (top + content.size.height - centered.size.height).max(top);
        CGRect::new(
            CGPoint::new(
                (centered.origin.x + offset.x).clamp(left, right),
                (centered.origin.y + offset.y).clamp(top, bottom),
            ),
            centered.size,
        )
    }

    /// A drag on a non-modal sheet's bar moves the sheet.
    fn drag_sheet(&self, recognizer: &UIPanGestureRecognizer) {
        let Some(parent) = self.sheet_parent.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        // SAFETY: a UIPanGestureRecognizer on the main thread.
        let moved: CGPoint = unsafe {
            let moved: CGPoint =
                msg_send![recognizer, translationInView: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![
                recognizer,
                setTranslation: CGPoint::ZERO,
                inView: std::ptr::null_mut::<AnyObject>()
            ];
            moved
        };
        let offset = self.sheet_offset.get();
        let wanted = CGPoint::new(offset.x + moved.x, offset.y + moved.y);
        let frame = parent.moved_sheet_frame(self, wanted);
        // Keep what the clamp allowed, so a drag back starts from the edge.
        let centered = parent.sheet_frame(self.sheet_size.get());
        self.sheet_offset.set(CGPoint::new(
            frame.origin.x - centered.origin.x,
            frame.origin.y - centered.origin.y,
        ));
        self.window.setFrame(frame);
    }

    /// Dim and block this window's content while a modal sheet is over it.
    fn update_dim(&self) {
        let modal = self
            .sheets
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|sheet| sheet.modal);
        let mut dim = self.dim.borrow_mut();
        if modal && dim.is_none() {
            let Some(container) = self.view.superview() else {
                return;
            };
            let view = UIView::initWithFrame(UIView::alloc(self.view.mtm()), self.view.frame());
            // SAFETY: a new view on the main thread.
            unsafe {
                let color: *mut AnyObject =
                    msg_send![objc2::class!(UIColor), colorWithWhite: 0.0f64, alpha: 0.32f64];
                let _: () = msg_send![&view, setBackgroundColor: color];
            }
            container.addSubview(&view);
            *dim = Some(view);
        } else if !modal && let Some(view) = dim.take() {
            view.removeFromSuperview();
        }
    }
}

/// Whether UIKit can resolve corner configurations, which arrived in iOS 26.
fn reports_corner_radii(view: &UIView) -> bool {
    view.respondsToSelector(sel!(effectiveRadiusForCorner:))
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        if let Some(platform) = self.platform.upgrade() {
            platform.application.unregister_window(&self.state);
            platform.application.window_closed(&self.state);
        }
        if let Some(parent) = self.sheet_parent.as_ref().and_then(Weak::upgrade) {
            let this = Rc::downgrade(&self.state);
            parent.sheets.borrow_mut().retain(|sheet| !sheet.ptr_eq(&this));
            parent.update_dim();
        }
        self.window.setHidden(true);
        let observers = std::mem::take(&mut *self.keyboard_observers.borrow_mut());
        for observer in observers {
            unsafe { NSNotificationCenter::defaultCenter().removeObserver((&*observer).as_ref()) };
        }

        *self.view_controller.ivars().0.borrow_mut() = Weak::new();
        *self.view.ivars().0.borrow_mut() = Weak::new();
        self.text_input_view.set_window(Weak::new());
        self.text_input_view.removeFromSuperview();
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let view = NonNull::new(Retained::as_ptr(&self.view).cast_mut().cast::<c_void>())
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = UiKitWindowHandle::new(view);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let handle = UiKitDisplayHandle::new();
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(handle.into()) })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn is_maximized(&self) -> bool {
        true // iOS windows are always "maximized"
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds.get())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.get().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // iOS windows cannot be resized programmatically
    }

    fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        match unsafe { self.view.traitCollection().userInterfaceStyle() } {
            UIUserInterfaceStyle::Dark => WindowAppearance::Dark,
            _ => WindowAppearance::Light,
        }
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::from_screen(self.window.screen())))
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        self.display_changed_callback.set(callback);
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.modifiers.get()
    }

    fn capslock(&self) -> Capslock {
        // Would need to check UIKeyModifierFlags
        Capslock { on: false }
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.text_input_view
            .set_input_field(input_handler.focus_id());
        self.input_handler.set(input_handler);
        self.text_input_view.refresh_keyboard();
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        let handler = self.input_handler.take();
        self.text_input_view.refresh_keyboard();
        handler
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        if self.normal
            && let Some(platform) = self.platform.upgrade()
        {
            platform.application.activate_window(&self.state);
            if !platform.application.has_scene(&self.state) {
                // Waiting for its iPad window: `attach_to_scene` shows it.
                return;
            }
        }
        if let (Some(parent), Some(platform)) = (
            self.state.sheet_parent.as_ref().and_then(Weak::upgrade),
            self.platform.upgrade(),
        ) && !platform.application.has_scene(&parent)
        {
            // Its parent's `attach_to_scene` shows it.
            return;
        }
        self.window.makeKeyAndVisible();
        self.take_keys();
    }

    fn is_active(&self) -> bool {
        self.window.isKeyWindow()
    }

    fn is_hovered(&self) -> bool {
        self.state.hovered.get()
    }

    fn set_title(&mut self, title: &str) {
        // The scene's title names it in the App Switcher and window overviews.
        *self.title.borrow_mut() = Some(title.to_owned());
        if self.sheet_parent.is_some() {
            if let Some(bar) = &self.header {
                bar.set_title(title);
            }
            return;
        }
        if let Some(bar) = &self.header {
            bar.set_title(title);
        }
        // The App Switcher, Exposé and the app menu's window list name it so.
        // A window still waiting for its iPad window sits in another's scene:
        // `attach_to_scene` names its own.
        let shown = self
            .platform
            .upgrade()
            .is_some_and(|platform| platform.application.has_scene(&self.state));
        if shown && let Some(scene) = self.window.windowScene() {
            scene.setTitle(Some(&NSString::from_str(title)));
        }
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {
        // Could adjust view background color
    }

    fn minimize(&self) {
        // iOS apps cannot be minimized
    }

    fn zoom(&self) {
        // iOS apps cannot be zoomed
    }

    fn toggle_fullscreen(&self) {
        // iOS apps are always fullscreen
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.request_frame_callback.set(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.input_callback.set(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.active_status_callback.set(callback);
    }

    fn visibility(&self) -> WindowVisibility {
        self.visibility.get()
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.visibility_callback.set(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.hover_status_callback.set(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.resize_callback.set(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.moved_callback.set(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.should_close_callback.set(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.hit_test_callback.set(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.close_callback.set(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.appearance_changed_callback.set(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.renderer.lock().draw(scene);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.lock().sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // iOS handles IME positioning automatically
    }

    fn insets(&self) -> WindowInsets {
        self.current_insets()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.insets_changed_callback.set(callback);
    }

    fn show_soft_keyboard(&self) {
        self.show_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        self.hide_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        self.text_input_view.state_changed(change);
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        self.text_input_view.set_configuration(&configuration);
    }
}

#[cfg(test)]
mod tests {
    use super::CallbackSlot;

    #[test]
    fn callback_is_unavailable_during_dispatch_and_restored_afterward() {
        let slot = CallbackSlot::default();
        slot.set(1);
        assert_eq!(
            slot.with(|value| {
                *value += 1;
                assert!(
                    slot.with(|_| panic!("reentered the same handler"))
                        .is_none()
                );
                *value
            }),
            Some(2)
        );
        assert_eq!(slot.with(|value| *value), Some(2));
    }

    #[test]
    fn callback_replacement_during_dispatch_is_preserved() {
        let slot = CallbackSlot::default();
        slot.set(1);
        slot.with(|_| {
            slot.set(2);
            slot.with(|value| *value += 1);
        });
        assert_eq!(slot.take(), Some(3));
    }

    #[test]
    fn withdrawing_an_in_flight_handler_does_not_restore_it() {
        let slot = CallbackSlot::default();
        slot.set(1);
        slot.with(|_| assert!(slot.take().is_none()));
        assert!(
            slot.with(|_| panic!("restored a withdrawn handler"))
                .is_none()
        );
    }

    #[test]
    fn removing_a_replacement_does_not_resurrect_the_previous_handler() {
        let slot = CallbackSlot::default();
        slot.set(1);
        slot.with(|_| {
            slot.set(2);
            assert_eq!(slot.take(), Some(2));
        });
        assert!(slot.take().is_none());
    }
}
