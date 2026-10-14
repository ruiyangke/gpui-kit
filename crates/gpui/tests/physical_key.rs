use gpui::prelude::*;
use std::{cell::RefCell, rc::Rc};

struct KeyObserver {
    focus: gpui::FocusHandle,
    seen: Rc<
        RefCell<
            Vec<(
                Option<gpui::PhysicalKey>,
                Option<gpui::KeyEventMetadata>,
                String,
                bool,
            )>,
        >,
    >,
}

impl Render for KeyObserver {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let down = self.seen.clone();
        let up = self.seen.clone();
        gpui::div()
            .track_focus(&self.focus)
            .on_key_down(move |event, _, _| {
                down.borrow_mut().push((
                    event.physical_key.clone(),
                    event.key_metadata.clone(),
                    event.keystroke.key.clone(),
                    event.is_held,
                ));
            })
            .on_key_up(move |event, _, _| {
                up.borrow_mut().push((
                    event.physical_key.clone(),
                    event.key_metadata.clone(),
                    event.keystroke.key.clone(),
                    false,
                ));
            })
    }
}

#[gpui::test]
fn physical_key_identity_survives_dispatch_modifier_changes_and_repeat(
    cx: &mut gpui::TestAppContext,
) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (view, cx) = cx.add_window_view(|_, cx| KeyObserver {
        focus: cx.focus_handle(),
        seen: seen.clone(),
    });
    cx.update(|window, cx| {
        let focus = view.read(cx).focus.clone();
        focus.focus(window, cx);
        window.activate_window();
        window.draw(cx).clear(cx);
    });
    let physical = Some(gpui::PhysicalKey::Native(18));
    let metadata = Some(gpui::KeyEventMetadata {
        unmodified_text: Some("1".into()),
        modifiers: gpui::Modifiers {
            shift: true,
            ..Default::default()
        },
        caps_lock: true,
        num_lock: false,
    });
    for is_held in [false, true] {
        cx.simulate_event(gpui::KeyDownEvent {
            physical_key: physical.clone(),
            key_metadata: metadata.clone(),
            keystroke: gpui::Keystroke::parse("!").unwrap(),
            is_held,
            prefer_character_input: false,
        });
    }
    cx.simulate_event(gpui::KeyUpEvent {
        physical_key: physical.clone(),
        key_metadata: None,
        keystroke: gpui::Keystroke::parse("1").unwrap(),
    });
    // Synthetic text has no claimed hardware origin.
    cx.simulate_event(gpui::KeyDownEvent {
        physical_key: None,
        key_metadata: None,
        keystroke: gpui::Keystroke::parse("x").unwrap(),
        is_held: false,
        prefer_character_input: false,
    });
    assert_eq!(
        *seen.borrow(),
        vec![
            (physical.clone(), metadata.clone(), "!".into(), false),
            (physical.clone(), metadata, "!".into(), true),
            (physical, None, "1".into(), false),
            (None, None, "x".into(), false),
        ]
    );
}

#[gpui::test]
fn numlock_query_reports_unknown_then_current_backend_state(cx: &mut gpui::TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, cx| KeyObserver {
        focus: cx.focus_handle(),
        seen: Rc::new(RefCell::new(Vec::new())),
    });
    assert_eq!(cx.update(|window, _| window.numlock()), None);
    cx.simulate_numlock_change(true);
    assert_eq!(cx.update(|window, _| window.numlock()), Some(true));
    cx.simulate_numlock_change(false);
    assert_eq!(cx.update(|window, _| window.numlock()), Some(false));
}
