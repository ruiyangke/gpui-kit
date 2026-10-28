use crate::{
    App, AppContext as _, Bounds, Context, FocusHandle, InputHandler, InputPreference,
    InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, Keystroke, Modifiers,
    ParentElement as _, Pixels, PlatformInput, PlatformWindow as _, Point, Render, Styled as _,
    TestAppContext, TestWindow, UTF16Selection, Window, actions, canvas, div, px,
};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

actions!(native_character_tests, [LocalShortcut]);

struct View {
    focus: FocusHandle,
    other: FocusHandle,
    actions: Rc<Cell<usize>>,
    native_context: Option<u64>,
    ime: bool,
    focused_handler: bool,
    received: Rc<RefCell<Vec<(u64, String)>>>,
    move_focus_on_key: bool,
    delivery_probe: Option<Rc<DeliveryProbe>>,
}
impl Render for View {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let actions = self.actions.clone();
        let focus = self.focus.clone();
        let handler = Handler {
            focus: if self.focused_handler {
                self.focus.clone()
            } else {
                self.other.clone()
            },
            native_context: self.native_context,
            ime: self.ime,
            received: self.received.clone(),
            delivery_probe: self.delivery_probe.clone(),
        };
        let move_focus = self.move_focus_on_key;
        let other = self.other.clone();
        div()
            .track_focus(&self.focus)
            .on_action(move |_: &LocalShortcut, _, cx| {
                actions.set(actions.get() + 1);
                cx.stop_propagation();
            })
            .on_key_down(move |_, window, cx| {
                if move_focus {
                    window.focus(&other, cx);
                }
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        window.handle_input(&focus, handler, cx);
                    },
                )
                .size(px(1.)),
            )
    }
}

struct Handler {
    focus: FocusHandle,
    native_context: Option<u64>,
    ime: bool,
    received: Rc<RefCell<Vec<(u64, String)>>>,
    delivery_probe: Option<Rc<DeliveryProbe>>,
}

#[derive(Default)]
struct DeliveryProbe {
    native_generation: Cell<u64>,
    revoke_on_query: Cell<bool>,
    steps: RefCell<Vec<&'static str>>,
}
impl InputHandler for Handler {
    fn accepts_text_input(&mut self, _: &mut Window, _: &mut App) -> bool {
        self.ime
    }
    fn native_character_context(&mut self, window: &mut Window, _: &mut App) -> Option<u64> {
        if let Some(probe) = &self.delivery_probe {
            probe.steps.borrow_mut().push("context");
            if probe.revoke_on_query.replace(false) {
                probe
                    .native_generation
                    .set(probe.native_generation.get() + 1);
            }
        }
        self.focus
            .is_focused(window)
            .then_some(self.native_context)
            .flatten()
    }
    fn handle_native_characters(
        &mut self,
        context: u64,
        text: Result<&str, &str>,
        _: &mut Window,
        _: &mut App,
    ) {
        if let Some(probe) = &self.delivery_probe {
            probe
                .steps
                .borrow_mut()
                .push(if text.is_ok() { "text" } else { "error" });
            if text.is_err() {
                return;
            }
        }
        self.received
            .borrow_mut()
            .push((context, text.expect("Native text").to_owned()));
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut App,
    ) -> Option<UTF16Selection> {
        None
    }
    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<Range<usize>> {
        None
    }
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: &mut Window,
        _: &mut App,
    ) {
        panic!("Unexpected IME text insertion");
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) {
        panic!("Unexpected IME composition");
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut App) {}
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<Bounds<Pixels>> {
        None
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }
}

#[gpui::test]
fn altgr_skips_local_binding_only_for_the_current_opted_in_handler(cx: &mut TestAppContext) {
    cx.update(|cx| cx.bind_keys([KeyBinding::new("ctrl-alt-q", LocalShortcut, None)]));
    let actions = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let actions = actions.clone();
        move |_, cx| View {
            focus: cx.focus_handle(),
            other: cx.focus_handle(),
            actions,
            native_context: None,
            ime: false,
            focused_handler: true,
            received: Default::default(),
            move_focus_on_key: false,
            delivery_probe: None,
        }
    });
    for (name, native_context, ime, focused, active, characters) in [
        ("ordinary local", None, false, true, true, false),
        ("native remote", Some(1), false, true, true, true),
        ("stale remote context", None, false, true, true, false),
        ("other focused element", Some(1), false, false, true, false),
        ("inactive window", Some(1), false, true, false, false),
        ("existing IME context", None, true, true, true, true),
    ] {
        let previous = actions.get();
        let focus = window
            .update(cx, |view, _, cx| {
                view.native_context = native_context;
                view.ime = ime;
                view.focused_handler = focused;
                cx.notify();
                view.focus.clone()
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.focus(&focus, cx);
            window.active.set(active);
            window.draw(cx).clear(cx);
            let event = KeyDownEvent {
                keystroke: Keystroke {
                    key: "q".into(),
                    key_char: Some("@".into()),
                    modifiers: Modifiers {
                        control: true,
                        alt: true,
                        ..Modifiers::default()
                    },
                },
                physical_key: None,
                key_metadata: None,
                is_held: false,
                prefer_character_input: true,
            };
            assert_eq!(
                window.input_preference(&event, cx),
                if characters {
                    InputPreference::CharacterInput
                } else {
                    InputPreference::KeyBindings
                },
                "{name}"
            );
            window.dispatch_event(PlatformInput::KeyDown(event), cx);
        })
        .unwrap();
        assert_eq!(actions.get() - previous, usize::from(!characters), "{name}");
    }
}

fn altgr() -> PlatformInput {
    PlatformInput::KeyDown(KeyDownEvent {
        keystroke: Keystroke {
            key: "q".into(),
            key_char: Some("@".into()),
            modifiers: Modifiers {
                control: true,
                alt: true,
                ..Modifiers::default()
            },
        },
        physical_key: None,
        key_metadata: None,
        is_held: false,
        prefer_character_input: true,
    })
}

fn capture(platform: &mut TestWindow) -> Option<(u64, u64)> {
    let mut handler = platform.take_input_handler()?;
    let target = handler.native_character_target();
    platform.set_input_handler(handler);
    target
}

fn deliver(platform: &mut TestWindow, target: (u64, u64), text: &str) -> bool {
    let Some(mut handler) = platform.take_input_handler() else {
        return false;
    };
    let delivered = handler.handle_native_characters(target, Ok(text));
    platform.set_input_handler(handler);
    delivered
}

#[gpui::test]
fn first_native_character_prepares_dirty_focus_without_a_pre_draw(cx: &mut TestAppContext) {
    // Keep TestPlatform, but prevent test-mode effect flushing from drawing
    // before the native preparation callback under test.
    cx.app.borrow_mut().mode = crate::app::GpuiMode::Production;
    cx.update(|cx| cx.bind_keys([KeyBinding::new("ctrl-alt-q", LocalShortcut, None)]));
    let actions = Rc::new(Cell::new(0));
    let received = Rc::new(RefCell::new(Vec::new()));
    let window = cx.add_window({
        let actions = actions.clone();
        let received = received.clone();
        move |_, cx| View {
            focus: cx.focus_handle(),
            other: cx.focus_handle(),
            actions,
            received,
            native_context: None,
            ime: false,
            focused_handler: true,
            move_focus_on_key: false,
            delivery_probe: None,
        }
    });
    let mut platform = cx.test_window(window.into());
    assert!(capture(&mut platform).is_none());
    for generation in [7, 9] {
        let focus = window
            .update(cx, |view, _, cx| {
                view.native_context = Some(generation);
                cx.notify();
                view.focus.clone()
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.focus(&focus, cx);
            window.active.set(true);
            window.refresh();
            assert!(window.invalidator.is_dirty());
        })
        .unwrap();
        assert_eq!(
            capture(&mut platform).map(|target| target.1),
            (generation == 9).then_some(7)
        );
        cx.update_window(window.into(), |_, window, _| {
            assert!(window.invalidator.is_dirty());
        })
        .unwrap();
        // Construction paints once; no draw has prepared this focus or the
        // changed generation before this production pre-key callback.
        assert!(platform.simulate_prepare_key_input(true));
        let target = capture(&mut platform).expect("Current handler prepared before capture");
        assert_eq!(target.1, generation);
        assert!(!platform.simulate_input(altgr()));
        assert!(deliver(&mut platform, target, "@"));
    }
    assert_eq!(&*received.borrow(), &[(7, "@".into()), (9, "@".into())]);
    assert_eq!(actions.get(), 0);
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    assert!(!platform.simulate_prepare_key_input(true));
}

#[gpui::test]
fn key_listener_focus_change_rejects_captured_native_characters(cx: &mut TestAppContext) {
    // Do not let automatic test draws remove the stale handler before delivery.
    cx.app.borrow_mut().mode = crate::app::GpuiMode::Production;
    cx.update(|cx| cx.bind_keys([KeyBinding::new("ctrl-alt-q", LocalShortcut, None)]));
    let actions = Rc::new(Cell::new(0));
    let received = Rc::new(RefCell::new(Vec::new()));
    let window = cx.add_window({
        let actions = actions.clone();
        let received = received.clone();
        move |_, cx| View {
            focus: cx.focus_handle(),
            other: cx.focus_handle(),
            actions,
            received,
            native_context: Some(3),
            ime: false,
            focused_handler: true,
            move_focus_on_key: true,
            delivery_probe: None,
        }
    });
    let focus = window.update(cx, |view, _, _| view.focus.clone()).unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.focus(&focus, cx);
        window.active.set(true);
        window.refresh();
    })
    .unwrap();
    let mut platform = cx.test_window(window.into());
    assert!(platform.simulate_prepare_key_input(true));
    let target = capture(&mut platform).expect("Focused target");
    assert!(!platform.simulate_input(altgr()));
    let handler = platform
        .take_input_handler()
        .expect("The old painted handler remains until another draw");
    platform.set_input_handler(handler);
    assert!(!deliver(&mut platform, target, "@"));
    assert!(received.borrow().is_empty());
    assert_eq!(actions.get(), 0);
}

#[gpui::test]
fn context_query_revokes_native_text_and_error_delivery_before_the_final_guard(
    cx: &mut TestAppContext,
) {
    cx.app.borrow_mut().mode = crate::app::GpuiMode::Production;
    let probe = Rc::new(DeliveryProbe::default());
    let received = Rc::new(RefCell::new(Vec::new()));
    let window = cx.add_window({
        let probe = probe.clone();
        let received = received.clone();
        move |_, cx| View {
            focus: cx.focus_handle(),
            other: cx.focus_handle(),
            actions: Default::default(),
            native_context: Some(13),
            ime: false,
            focused_handler: true,
            received,
            move_focus_on_key: false,
            delivery_probe: Some(probe),
        }
    });
    let focus = window.update(cx, |view, _, _| view.focus.clone()).unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.focus(&focus, cx);
        window.active.set(true);
        window.refresh();
    })
    .unwrap();
    let mut platform = cx.test_window(window.into());
    assert!(platform.simulate_prepare_key_input(true));

    for text in [Ok("@"), Err("Native translation unavailable")] {
        let target = capture(&mut platform).expect("Focused native character target");
        let generation = probe.native_generation.get();
        probe.steps.borrow_mut().clear();
        probe.revoke_on_query.set(true);
        let mut handler = platform
            .take_input_handler()
            .expect("Painted input handler");
        assert!(!handler.handle_native_characters_if(target, text, || {
            probe.steps.borrow_mut().push("authority");
            probe.native_generation.get() == generation
        }));
        platform.set_input_handler(handler);
        assert_eq!(&*probe.steps.borrow(), &["context", "authority"]);
        assert_eq!(probe.native_generation.get(), generation + 1);
        assert!(received.borrow().is_empty());

        // A fresh native origin with unchanged logical focus can deliver again.
        // The journal also rejects any context callback after the final guard.
        let fresh = capture(&mut platform).expect("Fresh native character target");
        assert_eq!(fresh, target);
        let generation = probe.native_generation.get();
        probe.steps.borrow_mut().clear();
        let mut handler = platform
            .take_input_handler()
            .expect("Painted input handler");
        assert!(handler.handle_native_characters_if(fresh, text, || {
            probe.steps.borrow_mut().push("authority");
            probe.native_generation.get() == generation
        }));
        platform.set_input_handler(handler);
        let delivery = if text.is_ok() { "text" } else { "error" };
        assert_eq!(&*probe.steps.borrow(), &["context", "authority", delivery]);
        if text.is_ok() {
            assert_eq!(&*received.borrow(), &[(13, "@".into())]);
        } else {
            assert!(received.borrow().is_empty());
        }
        received.borrow_mut().clear();
    }
}
