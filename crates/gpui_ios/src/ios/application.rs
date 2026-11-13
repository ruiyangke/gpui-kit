use super::{
    CallbackSlot,
    platform::IosPlatformState,
    window::IosWindowState,
};
use gpui::{AppLifecyclePhase, WindowVisibility};
use gpui_apple::main_thread_marker;
use objc2::{
    ClassType, DefinedClass, MainThreadOnly, Message, define_class, extern_class, extern_methods,
    msg_send,
    rc::{Allocated, Retained},
    runtime::AnyObject,
    sel,
};
use objc2_foundation::{
    NSDictionary, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSSet, NSString,
    NSUserActivity, NSUserActivityTypeBrowsingWeb,
};
use objc2_quartz_core::CADisplayLink;
use objc2_ui_kit::{
    UIApplication, UIApplicationDelegate, UIApplicationLaunchOptionsKey, UIOpenURLContext, UIScene,
    UISceneActivationRequestOptions, UISceneConfiguration, UISceneConnectionOptions, UISceneDelegate,
    UISceneSession, UIWindowScene,
    UIWindowSceneDelegate,
};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

#[derive(Default)]
pub(super) struct IosApplicationState {
    running: Cell<bool>,
    finish_launching: CallbackSlot<Box<dyn FnOnce()>>,
    pub(super) quit: CallbackSlot<Box<dyn FnMut()>>,
    pub(super) open_urls: CallbackSlot<Box<dyn FnMut(Vec<String>)>>,
    pub(super) app_lifecycle: CallbackSlot<Box<dyn FnMut(AppLifecyclePhase)>>,
    pub(super) memory_warning: CallbackSlot<Box<dyn FnMut()>>,
    /// Connected scenes, each showing one normal window.
    scenes: RefCell<Vec<Rc<SceneEntry>>>,
    /// Normal windows waiting for the scene UIKit was asked to connect.
    pending: RefCell<Vec<Weak<IosWindowState>>>,
    /// Sessions of hidden iPad windows asked to come back for a window.
    reusing: RefCell<Vec<String>>,
    /// Windows whose iPad window the person closed, asked to close: one the
    /// app keeps (to ask about unsaved work) gets an iPad window again.
    closing: RefCell<Vec<(Weak<IosWindowState>, std::time::Instant)>>,
    /// Windows whose scene UIKit disconnected without discarding its session:
    /// they return to that session's next scene.
    detached: RefCell<Vec<(String, Weak<IosWindowState>)>>,
    /// The scene the user last worked in; pop-ups and presented controllers go there.
    active_scene: RefCell<Option<Retained<UIWindowScene>>>,
    launched: Cell<bool>,
    // The link retains its delegate target, which only weakly references this state.
    display_link: RefCell<Option<Retained<CADisplayLink>>>,
    windows: RefCell<Vec<Weak<IosWindowState>>>,
}

/// One connected UIKit scene and the normal window it shows.
pub(super) struct SceneEntry {
    scene: Retained<UIWindowScene>,
    session: String,
    group: RefCell<Option<String>>,
    windows: RefCell<Vec<Weak<IosWindowState>>>,
    shown: RefCell<Weak<IosWindowState>>,
    foreground: Cell<bool>,
    active: Cell<bool>,
    /// Asked to close: it stays connected a moment, but takes no window.
    closing: Cell<bool>,
}

impl SceneEntry {
    fn live(&self) -> Vec<Rc<IosWindowState>> {
        self.windows.borrow().iter().filter_map(Weak::upgrade).collect()
    }

    fn add(&self, window: &Rc<IosWindowState>) {
        let weak = Rc::downgrade(window);
        let mut windows = self.windows.borrow_mut();
        windows.retain(|entry| entry.upgrade().is_some());
        if !windows.iter().any(|entry| entry.ptr_eq(&weak)) {
            windows.push(weak.clone());
        }
        *self.shown.borrow_mut() = weak;
    }

    fn remove(&self, window: &Rc<IosWindowState>) {
        let weak = Rc::downgrade(window);
        self.windows
            .borrow_mut()
            .retain(|entry| !entry.ptr_eq(&weak) && entry.upgrade().is_some());
        if self.shown.borrow().ptr_eq(&weak) {
            *self.shown.borrow_mut() = Weak::new();
        }
    }
}

thread_local! {
    // UIKit constructs delegates by class name, including on scene restoration.
    // Only delegate initialization and the legacy status-bar setter consult this.
    static CURRENT_PLATFORM: RefCell<Weak<IosPlatformState>> = RefCell::default();
}

pub(super) fn current_platform() -> Option<Rc<IosPlatformState>> {
    CURRENT_PLATFORM.with_borrow(Weak::upgrade)
}

pub(super) fn run(platform: &Rc<IosPlatformState>, on_finish_launching: Box<dyn FnOnce()>) {
    let main_thread = main_thread_marker().expect("UIKit requires the main thread");
    CURRENT_PLATFORM.with_borrow_mut(|current| {
        assert!(
            current.upgrade().is_none(),
            "the iOS application is already running"
        );
        *current = Rc::downgrade(platform);
    });
    assert!(
        !platform.application.running.replace(true),
        "the iOS application is already running"
    );
    platform
        .application
        .finish_launching
        .set(on_finish_launching);
    // UIKit can restore a scene delegate by class name without asking for a new configuration.
    SceneDelegate::class();
    let delegate_class = NSString::from_class(AppDelegate::class());
    UIApplication::main(None, Some(&delegate_class), main_thread);
}

impl IosApplicationState {
    fn set_display_link_paused(&self, paused: bool) {
        let display_link = self.display_link.borrow().clone();
        if let Some(display_link) = display_link {
            display_link.setPaused(paused);
        }
    }

    fn stop_display_link(&self) {
        let display_link = self.display_link.borrow_mut().take();
        if let Some(display_link) = display_link {
            display_link.invalidate();
        }
    }

    /// The scene the user last worked in, else any connected one.
    pub(super) fn window_scene(&self) -> Option<Retained<UIWindowScene>> {
        self.active_scene
            .borrow()
            .clone()
            .or_else(|| self.scenes.borrow().first().map(|entry| entry.scene.clone()))
    }

    /// A connected scene that shows no window yet: the launch scene, or one
    /// UIKit connected for windows that have since closed.
    pub(super) fn free_scene(&self) -> Option<Retained<UIWindowScene>> {
        self.scenes
            .borrow()
            .iter()
            .find(|entry| {
                !entry.closing.get() && entry.group.borrow().is_none() && entry.live().is_empty()
            })
            .map(|entry| entry.scene.clone())
    }

    fn entry_for_scene(&self, scene: &UIScene) -> Option<Rc<SceneEntry>> {
        self.scenes
            .borrow()
            .iter()
            .find(|entry| std::ptr::eq::<UIScene>(&**entry.scene, scene))
            .cloned()
    }

    fn entry_for_window(&self, window: &IosWindowState) -> Option<Rc<SceneEntry>> {
        self.scenes
            .borrow()
            .iter()
            .find(|entry| entry.live().iter().any(|shown| std::ptr::eq(&**shown, window)))
            .cloned()
    }

    fn entry_for_group(&self, group: &str) -> Option<Rc<SceneEntry>> {
        self.scenes
            .borrow()
            .iter()
            .find(|entry| entry.group.borrow().as_deref() == Some(group))
            .cloned()
    }

    /// The window a raw handle names.
    pub(super) fn window_for_handle(
        &self,
        handle: raw_window_handle::RawWindowHandle,
    ) -> Option<Rc<IosWindowState>> {
        self.windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|window| window.is_handle(handle))
    }

    /// The tab an app's iPad window shows.
    pub(super) fn shown_in_group(&self, group: &str) -> Option<Rc<IosWindowState>> {
        self.entry_for_group(group)?.shown.borrow().upgrade()
    }

    /// A normal window of the app `group` names, shown or still waiting for
    /// its iPad window.
    pub(super) fn any_in_group(&self, group: &str) -> Option<Rc<IosWindowState>> {
        self.windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|window| window.is_normal() && window.group().as_deref() == Some(group))
    }

    /// Gives a normal window an iPad window (scene) of its own: the free one
    /// it was built in, else a new one from UIKit.
    pub(super) fn place_window(&self, window: &Rc<IosWindowState>) {
        if let Some(scene) = window.scene()
            && let Some(entry) = self.entry_for_scene(&scene)
            && !entry.closing.get()
            && entry.group.borrow().is_none()
            && entry.live().is_empty()
        {
            *entry.group.borrow_mut() = window.group();
            entry.add(window);
            window.restrict_size(&scene);
            return;
        }
        self.pending.borrow_mut().push(Rc::downgrade(window));
        self.request_scene();
    }

    /// Asks UIKit for a scene, opened from the one in use, so Stage Manager
    /// puts it beside that one. An iPad window a killed run left behind is
    /// reused first: iPadOS keeps those sessions as hidden windows, can't
    /// close them while they have no scene, and would otherwise gain one
    /// for every window each run opens.
    fn request_scene(&self) {
        let Some(main_thread) = main_thread_marker() else {
            return;
        };
        let application = UIApplication::sharedApplication(main_thread);
        let options = self.active_scene.borrow().clone().map(|scene| {
            let options = UISceneActivationRequestOptions::new(main_thread);
            options.setRequestingScene(Some(&scene));
            options
        });
        let orphan = {
            let mut reusing = self.reusing.borrow_mut();
            // A window the app hid keeps its session for when it comes back.
            let detached = self.detached.borrow();
            let orphan = application.openSessions().iter().find(|session| {
                let id = session.persistentIdentifier().to_string();
                session.scene().is_none()
                    && !reusing.contains(&id)
                    && !detached
                        .iter()
                        .any(|(kept, window)| *kept == id && window.upgrade().is_some())
            });
            if let Some(orphan) = &orphan {
                reusing.push(orphan.persistentIdentifier().to_string());
            }
            orphan
        };
        // A session iPadOS refuses to bring back still gets a new one.
        let fallback = block2::RcBlock::new(move |_error: std::ptr::NonNull<objc2_foundation::NSError>| {
            if let Some(main_thread) = main_thread_marker() {
                #[allow(deprecated)]
                UIApplication::sharedApplication(main_thread)
                    .requestSceneSessionActivation_userActivity_options_errorHandler(
                        None, None, None, None,
                    );
            }
        });
        #[allow(deprecated)] // activateSceneSessionForRequest: needs iOS 17.
        application.requestSceneSessionActivation_userActivity_options_errorHandler(
            orphan.as_deref(),
            None,
            options.as_deref(),
            orphan.is_some().then_some(&*fallback),
        );
    }

    /// Whether `window` is shown in a connected scene of its own.
    pub(super) fn has_scene(&self, window: &IosWindowState) -> bool {
        self.entry_for_window(window).is_some()
    }

    /// Brings the scene showing `window` to the front.
    pub(super) fn activate_window(&self, window: &IosWindowState) {
        let (Some(entry), Some(main_thread)) = (self.entry_for_window(window), main_thread_marker())
        else {
            return;
        };
        #[allow(deprecated)]
        UIApplication::sharedApplication(main_thread)
            .requestSceneSessionActivation_userActivity_options_errorHandler(
                Some(&entry.scene.session()),
                None,
                None,
                None,
            );
    }

    /// A window closed: its scene closes with it, unless it is the app's last.
    pub(super) fn window_closed(&self, window: &Rc<IosWindowState>) {
        let weak = Rc::downgrade(window);
        self.pending.borrow_mut().retain(|entry| !entry.ptr_eq(&weak));
        let detached_session = {
            let mut detached = self.detached.borrow_mut();
            let session = detached
                .iter()
                .find(|(_, entry)| entry.ptr_eq(&weak))
                .map(|(session, _)| session.clone());
            detached.retain(|(_, entry)| !entry.ptr_eq(&weak) && entry.upgrade().is_some());
            session.filter(|session| !detached.iter().any(|(other, _)| other == session))
        };
        if let Some(session) = detached_session {
            // Its iPad window was in the background, disconnected: without
            // this it would stay in the app's windows, empty.
            destroy_session(&session);
        }
        let Some(entry) = self.entry_for_window(window) else {
            return;
        };
        entry.remove(window);
        if !entry.live().is_empty() {
            return;
        }
        *entry.group.borrow_mut() = None;
        if self.scenes.borrow().len() > 1
            && let Some(main_thread) = main_thread_marker()
        {
            entry.closing.set(true);
            UIApplication::sharedApplication(main_thread)
                .requestSceneSessionDestruction_options_errorHandler(
                    &entry.scene.session(),
                    None,
                    None,
                );
        }
    }

    /// The person closed an iPad window (its session was discarded): each of
    /// its windows decides, as GPUI's should-close says.
    fn session_discarded(&self, session: &str) {
        let windows: Vec<_> = {
            let mut detached = self.detached.borrow_mut();
            let (closing, kept) = std::mem::take(&mut *detached)
                .into_iter()
                .partition(|(id, _)| id == session);
            *detached = kept;
            closing
        };
        for (_, window) in windows {
            if let Some(window) = window.upgrade() {
                // Listed first: an app that hides the window while deciding
                // takes it off the list (`app_hid`).
                self.closing
                    .borrow_mut()
                    .push((Rc::downgrade(&window), std::time::Instant::now()));
                window.request_close();
            }
        }
    }

    /// The app hid itself while its windows decided whether to close: the
    /// ones it kept wait, without an iPad window, until a new iPad window
    /// shows them again (`windows_for_new_scene`).
    pub(super) fn app_hid(&self) {
        self.closing.borrow_mut().clear();
    }

    /// A window still open a while after the person closed its iPad window
    /// was kept by its app (unsaved work): it gets an iPad window again.
    fn reopen_kept_windows(&self) {
        let due: Vec<_> = {
            let mut closing = self.closing.borrow_mut();
            let (due, waiting) = std::mem::take(&mut *closing)
                .into_iter()
                .partition(|(_, since)| since.elapsed() > std::time::Duration::from_millis(1500));
            *closing = waiting;
            due
        };
        for (window, _) in due {
            if let Some(window) = window.upgrade()
                && self.entry_for_window(&window).is_none()
            {
                self.place_window(&window);
            }
        }
    }

    /// The window a newly connected scene shows: its session's own, else the
    /// oldest waiting one, else a normal window left without a scene (one the
    /// app hid when its iPad window closed).
    fn windows_for_new_scene(&self, session: &str) -> Vec<Rc<IosWindowState>> {
        let restored: Vec<_> = {
            let mut detached = self.detached.borrow_mut();
            let (restored, kept) = std::mem::take(&mut *detached)
                .into_iter()
                .partition(|(id, _)| id == session);
            *detached = kept;
            restored.into_iter().filter_map(|(_, window)| window.upgrade()).collect()
        };
        if !restored.is_empty() {
            return restored;
        }
        let pending: Vec<_> = self.pending.borrow().iter().filter_map(Weak::upgrade).collect();
        if let Some(first) = pending.first().cloned() {
            let weak = Rc::downgrade(&first);
            self.pending.borrow_mut().retain(|waiting| !waiting.ptr_eq(&weak));
            return vec![first];
        }
        let windows = self.windows.borrow().clone();
        let detached = self.detached.borrow();
        windows
            .iter()
            .filter_map(Weak::upgrade)
            .find(|window| {
                let weak = Rc::downgrade(window);
                window.is_normal()
                    && self.entry_for_window(window).is_none()
                    && !detached.iter().any(|(_, entry)| entry.ptr_eq(&weak))
            })
            .into_iter()
            .collect()
    }

    fn aggregate(&self) -> (bool, bool) {
        let scenes = self.scenes.borrow();
        (
            scenes.iter().any(|entry| entry.foreground.get()),
            scenes.iter().any(|entry| entry.active.get()),
        )
    }

    pub(super) fn register_window(&self, window: &Rc<IosWindowState>) {
        self.windows.borrow_mut().push(Rc::downgrade(window));
    }

    pub(super) fn unregister_window(&self, window: &Rc<IosWindowState>) {
        let window = Rc::downgrade(window);
        self.windows
            .borrow_mut()
            .retain(|entry| !entry.ptr_eq(&window));
    }

    pub(super) fn with_windows(&self, mut callback: impl FnMut(&IosWindowState)) {
        // Native callbacks may open or close windows. Do not hold a registry borrow
        // across them, and skip snapshot entries that have since been unregistered.
        let windows = self.windows.borrow().clone();
        for window in windows {
            if self
                .windows
                .borrow()
                .iter()
                .any(|entry| entry.ptr_eq(&window))
            {
                // The snapshot's Weak keeps its allocation identity from being reused.
                // The strong guard survives synchronous closure of this window.
                if let Some(window) = window.upgrade() {
                    callback(&window);
                }
            }
        }
    }

    fn finish_launching(&self) {
        let callback = self.finish_launching.take();
        if let Some(callback) = callback {
            callback();
        }
    }

    fn notify_memory_warning(&self) {
        self.memory_warning.with(|callback| callback());
    }

    fn open_urls(&self, contexts: &NSSet<UIOpenURLContext>) {
        if contexts.is_empty() {
            return;
        }
        let urls = contexts
            .iter()
            .filter_map(|context| context.URL().absoluteString().map(|url| url.to_string()))
            .collect();
        self.open_urls.with(|callback| callback(urls));
    }

    fn continue_user_activity(&self, activity: &NSUserActivity) {
        if &*activity.activityType() != unsafe { NSUserActivityTypeBrowsingWeb } {
            return;
        }
        let Some(url) = activity.webpageURL().and_then(|url| url.absoluteString()) else {
            return;
        };
        self.open_urls
            .with(|callback| callback(vec![url.to_string()]));
    }
}

impl Drop for IosApplicationState {
    fn drop(&mut self) {
        self.stop_display_link();
    }
}

extern_class!(
    #[unsafe(super = NSObject)]
    #[name = "UISceneConnectionOptions"]
    struct SceneConnectionOptions;
);

impl SceneConnectionOptions {
    extern_methods!(
        // iOS 18 can return nil on an ordinary launch despite the SDK's nonnull annotation.
        #[unsafe(method(URLContexts))]
        #[unsafe(method_family = none)]
        fn url_contexts(&self) -> Option<Retained<NSSet<UIOpenURLContext>>>;

        #[unsafe(method(userActivities))]
        #[unsafe(method_family = none)]
        fn user_activities(&self) -> Option<Retained<NSSet<NSUserActivity>>>;
    );
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIIosAppDelegate"]
    #[ivars = Weak<IosPlatformState>]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    impl AppDelegate {
        #[unsafe(method_id(init))]
        fn init(this: Allocated<Self>) -> Retained<Self> {
            let platform = current_platform()
                .map(|platform| Rc::downgrade(&platform))
                .unwrap_or_default();
            let this = this.set_ivars(platform);
            unsafe { msg_send![super(this), init] }
        }
    }

    unsafe impl UIApplicationDelegate for AppDelegate {
        #[unsafe(method(application:didFinishLaunchingWithOptions:))]
        unsafe fn did_finish_launching(
            &self,
            _application: &UIApplication,
            _options: Option<&NSDictionary<UIApplicationLaunchOptionsKey, AnyObject>>,
        ) -> bool {
            // GPUI's launch callback runs when a scene is ready to host its window.
            true
        }

        #[unsafe(method_id(application:configurationForConnectingSceneSession:options:))]
        fn configuration_for_scene(
            &self,
            _application: &UIApplication,
            session: &UISceneSession,
            _options: &UISceneConnectionOptions,
        ) -> Retained<UISceneConfiguration> {
            let configuration = UISceneConfiguration::initWithName_sessionRole(
                UISceneConfiguration::alloc(self.mtm()),
                Some(&NSString::from_str("Default Configuration")),
                &session.role(),
            );
            unsafe {
                configuration.setDelegateClass(Some(SceneDelegate::class()));
            }
            configuration
        }

        #[unsafe(method(application:didDiscardSceneSessions:))]
        fn did_discard_scene_sessions(
            &self,
            _application: &UIApplication,
            sessions: &NSSet<UISceneSession>,
        ) {
            if let Some(platform) = self.ivars().upgrade() {
                for session in sessions.iter() {
                    platform
                        .application
                        .session_discarded(&session.persistentIdentifier().to_string());
                }
            }
        }

        #[unsafe(method(applicationDidReceiveMemoryWarning:))]
        fn did_receive_memory_warning(&self, _application: &UIApplication) {
            if let Some(platform) = self.ivars().upgrade() {
                platform.application.notify_memory_warning();
            }
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _application: &UIApplication) {
            if let Some(platform) = self.ivars().upgrade() {
                if let Some(mut callback) = platform.application.quit.take() {
                    callback();
                }
            }
        }
    }
);

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIIosSceneDelegate"]
    #[ivars = Weak<IosPlatformState>]
    struct SceneDelegate;

    unsafe impl NSObjectProtocol for SceneDelegate {}
    unsafe impl UIWindowSceneDelegate for SceneDelegate {
        /// Moving, resizing or the window controls appearing: lay out again.
        #[unsafe(method(windowScene:didUpdateEffectiveGeometry:))]
        fn did_update_geometry(&self, scene: &UIWindowScene, _previous: &AnyObject) {
            // Every window in the scene, including one not yet counted as
            // shown there (a size restriction can resize it first).
            if let Some(platform) = self.platform_for_scene(scene) {
                platform.application.with_windows(|window| {
                    if window.scene().is_some_and(|own| &*own == scene) {
                        window.relayout();
                    }
                });
            }
        }
    }

    impl SceneDelegate {
        #[unsafe(method_id(init))]
        fn init(this: Allocated<Self>) -> Retained<Self> {
            let platform = current_platform()
                .map(|platform| Rc::downgrade(&platform))
                .unwrap_or_default();
            let this = this.set_ivars(platform);
            unsafe { msg_send![super(this), init] }
        }

        #[unsafe(method(renderFrame:))]
        fn render_frame(&self, _display_link: &CADisplayLink) {
            if let Some(platform) = self.ivars().upgrade() {
                platform.application.with_windows(IosWindowState::request_frame);
                platform.application.reopen_kept_windows();
            }
        }
    }

    unsafe impl UISceneDelegate for SceneDelegate {
        #[unsafe(method(scene:willConnectToSession:options:))]
        fn will_connect(
            &self,
            scene: &UIScene,
            _session: &UISceneSession,
            options: &SceneConnectionOptions,
        ) {
            let Some(scene) = scene.downcast_ref::<UIWindowScene>() else {
                return;
            };
            let Some(platform) = self.ivars().upgrade() else {
                return;
            };
            let application = &platform.application;
            let session = scene.session().persistentIdentifier().to_string();
            application.reusing.borrow_mut().retain(|reused| *reused != session);
            let entry = Rc::new(SceneEntry {
                scene: scene.retain(),
                session: session.clone(),
                group: RefCell::default(),
                windows: RefCell::default(),
                shown: RefCell::default(),
                foreground: Cell::new(false),
                active: Cell::new(false),
                closing: Cell::new(false),
            });
            application.scenes.borrow_mut().push(entry.clone());
            if application.active_scene.borrow().is_none() {
                *application.active_scene.borrow_mut() = Some(scene.retain());
            }
            if !application.launched.replace(true) {
                // GPUI's first window takes this, the launch scene.
                application.finish_launching();
            } else if let windows @ [first, ..] =
                application.windows_for_new_scene(&session).as_slice()
            {
                *entry.group.borrow_mut() = first.group();
                for window in windows {
                    window.attach_to_scene(scene);
                    entry.add(window);
                }
            } else if let Some(main_thread) = main_thread_marker() {
                // A scene UIKit restored for a window that no longer exists.
                UIApplication::sharedApplication(main_thread)
                    .requestSceneSessionDestruction_options_errorHandler(
                        &scene.session(),
                        None,
                        None,
                    );
            }
            if let Some(contexts) = options.url_contexts() {
                application.open_urls(&contexts);
            }
            if let Some(activities) = options.user_activities() {
                for activity in activities.iter() {
                    application.continue_user_activity(&activity);
                }
            }

            if application.display_link.borrow().is_some() {
                return;
            }
            let display_link = unsafe {
                let display_link =
                    CADisplayLink::displayLinkWithTarget_selector(self, sel!(renderFrame:));
                display_link.setPaused(true);
                display_link.addToRunLoop_forMode(
                    &NSRunLoop::mainRunLoop(),
                    NSRunLoopCommonModes,
                );
                display_link
            };
            *application.display_link.borrow_mut() = Some(display_link);
        }

        #[unsafe(method(sceneDidDisconnect:))]
        fn did_disconnect(&self, scene: &UIScene) {
            let Some(platform) = self.platform_for_scene(scene) else {
                return;
            };
            let application = &platform.application;
            let Some(entry) = application.entry_for_scene(scene) else {
                return;
            };
            application
                .scenes
                .borrow_mut()
                .retain(|other| !Rc::ptr_eq(other, &entry));
            let was_active = application
                .active_scene
                .borrow()
                .as_ref()
                .is_some_and(|active| std::ptr::eq::<UIScene>(&***active, scene));
            if was_active {
                let next = application.scenes.borrow().first().map(|e| e.scene.clone());
                *application.active_scene.borrow_mut() = next;
            }
            for window in entry.live() {
                window.notify_active_status_change(false);
                window.notify_visibility_change(WindowVisibility::Hidden);
                application
                    .detached
                    .borrow_mut()
                    .push((entry.session.clone(), Rc::downgrade(&window)));
            }
            if application.scenes.borrow().is_empty() {
                application.stop_display_link();
                application.app_lifecycle.with(|callback| callback(AppLifecyclePhase::Disconnected));
            }
        }

        #[unsafe(method(sceneWillEnterForeground:))]
        fn will_enter_foreground(&self, scene: &UIScene) {
            self.scene_changed(scene, Some(true), None);
        }

        #[unsafe(method(sceneDidBecomeActive:))]
        fn did_become_active(&self, scene: &UIScene) {
            self.scene_changed(scene, None, Some(true));
        }

        #[unsafe(method(sceneWillResignActive:))]
        fn will_resign_active(&self, scene: &UIScene) {
            self.scene_changed(scene, None, Some(false));
        }

        #[unsafe(method(sceneDidEnterBackground:))]
        fn did_enter_background(&self, scene: &UIScene) {
            self.scene_changed(scene, Some(false), None);
        }

        #[unsafe(method(scene:openURLContexts:))]
        fn open_url_contexts(&self, scene: &UIScene, contexts: &NSSet<UIOpenURLContext>) {
            if let Some(platform) = self.platform_for_scene(scene) {
                platform.application.open_urls(contexts);
            }
        }

        #[unsafe(method(scene:continueUserActivity:))]
        fn continue_user_activity(&self, scene: &UIScene, activity: &NSUserActivity) {
            if let Some(platform) = self.platform_for_scene(scene) {
                platform.application.continue_user_activity(activity);
            }
        }
    }
);

/// Closes the iPad window whose session is `persistent_id`.
fn destroy_session(persistent_id: &str) {
    let Some(main_thread) = main_thread_marker() else {
        return;
    };
    let application = UIApplication::sharedApplication(main_thread);
    if let Some(session) = application
        .openSessions()
        .iter()
        .find(|session| session.persistentIdentifier().to_string() == persistent_id)
    {
        application.requestSceneSessionDestruction_options_errorHandler(&session, None, None);
    }
}

impl SceneDelegate {
    fn platform_for_scene(&self, scene: &UIScene) -> Option<Rc<IosPlatformState>> {
        let platform = self.ivars().upgrade()?;
        let owns_scene = platform.application.entry_for_scene(scene).is_some();
        owns_scene.then_some(platform)
    }

    /// One scene moved to or from the foreground, or gained or lost focus:
    /// its own window follows it, the application follows them all.
    fn scene_changed(&self, scene: &UIScene, foreground: Option<bool>, active: Option<bool>) {
        let Some(platform) = self.platform_for_scene(scene) else {
            return;
        };
        let application = &platform.application;
        let Some(entry) = application.entry_for_scene(scene) else {
            return;
        };
        let (was_foreground, was_active) = application.aggregate();
        let window = entry.shown.borrow().upgrade();
        if let Some(foreground) = foreground {
            entry.foreground.set(foreground);
            if let Some(window) = &window {
                window.notify_visibility_change(if foreground {
                    WindowVisibility::Visible
                } else {
                    WindowVisibility::Hidden
                });
            }
        }
        if let Some(active) = active {
            entry.active.set(active);
            if active {
                *application.active_scene.borrow_mut() = Some(entry.scene.clone());
            }
            if let Some(window) = &window {
                window.notify_active_status_change(active);
            }
        }
        let (is_foreground, is_active) = application.aggregate();
        if is_foreground != was_foreground {
            application.set_display_link_paused(!is_foreground);
            let phase = if is_foreground {
                AppLifecyclePhase::Foreground
            } else {
                AppLifecyclePhase::Background
            };
            application.app_lifecycle.with(|callback| callback(phase));
        }
        if is_active != was_active {
            let phase = if is_active {
                AppLifecyclePhase::Active
            } else {
                AppLifecyclePhase::Inactive
            };
            application.app_lifecycle.with(|callback| callback(phase));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn browsing_activity_delivers_url_without_losing_query_or_fragment() {
        use objc2::AnyThread;
        use objc2_foundation::NSURL;

        let state = IosApplicationState::default();
        let received = Rc::new(RefCell::new(Vec::new()));
        state.open_urls.set(Box::new({
            let received = received.clone();
            move |urls| received.borrow_mut().extend(urls)
        }));
        let url_string = "https://example.com/thread/123?source=link#selection";
        let url = NSURL::URLWithString(&NSString::from_str(url_string)).expect("test URL is valid");
        let activity = NSUserActivity::initWithActivityType(NSUserActivity::alloc(), unsafe {
            NSUserActivityTypeBrowsingWeb
        });
        state.continue_user_activity(&activity);
        assert!(received.borrow().is_empty());

        activity.setWebpageURL(Some(&url));
        state.continue_user_activity(&activity);
        assert_eq!(*received.borrow(), vec![url_string.to_string()]);

        let unrelated = NSUserActivity::initWithActivityType(
            NSUserActivity::alloc(),
            &NSString::from_str("com.example.edit"),
        );
        unrelated.setWebpageURL(Some(&url));
        state.continue_user_activity(&unrelated);
        assert_eq!(*received.borrow(), vec![url_string.to_string()]);
    }

    #[test]
    fn launch_callback_runs_once_and_can_register_callbacks() {
        let state = Rc::new(IosApplicationState::default());
        let launches = Rc::new(Cell::new(0));
        state.finish_launching.set(Box::new({
            let state = Rc::downgrade(&state);
            let launches = launches.clone();
            move || {
                launches.set(launches.get() + 1);
                state.upgrade().unwrap().memory_warning.set(Box::new(|| {}));
            }
        }));
        state.finish_launching();
        state.finish_launching();
        assert_eq!(launches.get(), 1);
        assert!(state.memory_warning.take().is_some());
    }

    #[test]
    fn lifecycle_callback_replacement_survives_dispatch() {
        let state = Rc::new(IosApplicationState::default());
        let events = Rc::new(RefCell::new(Vec::new()));
        state.app_lifecycle.set(Box::new({
            let state = Rc::downgrade(&state);
            let events = events.clone();
            move |phase| {
                events.borrow_mut().push((0, phase));
                state.upgrade().unwrap().app_lifecycle.set(Box::new({
                    let events = events.clone();
                    move |phase| events.borrow_mut().push((1, phase))
                }));
            }
        }));
        state.app_lifecycle.with(|callback| callback(AppLifecyclePhase::Foreground));
        state.app_lifecycle.with(|callback| callback(AppLifecyclePhase::Active));
        assert_eq!(
            *events.borrow(),
            [
                (0, AppLifecyclePhase::Foreground),
                (1, AppLifecyclePhase::Active)
            ]
        );
    }

    #[test]
    fn memory_warning_callback_is_restored_after_dispatch() {
        let state = Rc::new(IosApplicationState::default());
        let warnings = Rc::new(Cell::new(0));
        state.memory_warning.set(Box::new({
            let state = Rc::downgrade(&state);
            let warnings = warnings.clone();
            move || {
                warnings.set(warnings.get() + 1);
                state.upgrade().unwrap().quit.set(Box::new(|| {}));
            }
        }));
        state.notify_memory_warning();
        state.notify_memory_warning();
        assert_eq!(warnings.get(), 2);
        assert!(state.quit.take().is_some());
    }

    #[test]
    fn lifecycle_removal_survives_reentrant_dispatch() {
        let state = Rc::new(IosApplicationState::default());
        let events = Rc::new(RefCell::new(Vec::new()));
        state.app_lifecycle.set(Box::new({
            let state = Rc::downgrade(&state);
            let events = events.clone();
            move |phase| {
                events.borrow_mut().push(phase);
                let state = state.upgrade().unwrap();
                state.app_lifecycle.take();
                state.app_lifecycle.with(|callback| callback(AppLifecyclePhase::Inactive));
            }
        }));
        state.app_lifecycle.with(|callback| callback(AppLifecyclePhase::Disconnected));
        state.app_lifecycle.with(|callback| callback(AppLifecyclePhase::Foreground));
        assert_eq!(*events.borrow(), [AppLifecyclePhase::Disconnected]);
    }

    #[test]
    fn application_callbacks_are_not_shared_between_platforms() {
        let first = IosApplicationState::default();
        let second = IosApplicationState::default();
        let warnings = Rc::new(Cell::new(0));
        first.memory_warning.set(Box::new({
            let warnings = warnings.clone();
            move || warnings.set(warnings.get() + 1)
        }));
        second.notify_memory_warning();
        assert_eq!(warnings.get(), 0);
        first.notify_memory_warning();
        assert_eq!(warnings.get(), 1);
    }
}
