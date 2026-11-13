//! Conversion from UIKit events to GPUI input events.

use gpui::{
    BackGestureEvent, BackGesturePhase, Keystroke, Modifiers, Pixels, Point, ScrollDelta, ScrollEvent,
    ScrollSource, TouchId, TouchPhase, px,
};
use objc2_core_foundation::CGPoint;
use objc2_ui_kit::{
    UIGestureRecognizerState, UIKey, UIKeyModifierFlags, UIPanGestureRecognizer, UIRectEdge,
    UIScreenEdgePanGestureRecognizer, UITouch, UITouchPhase, UIView,
};

/// A release faster than this, in points per second toward going back, commits
/// a back gesture regardless of how far it travelled.
const BACK_COMMIT_VELOCITY: f64 = 300.;

/// Converts a step of the screen-edge back gesture, whose `Started` phase was
/// already delivered when the recognizer asked to begin.
///
/// Like `UINavigationController`, a release commits when the gesture is past
/// halfway without flicking back, or when it flicks forward quickly. Returns
/// `None` for states that carry no step.
pub fn back_event(
    recognizer: &UIScreenEdgePanGestureRecognizer,
    view: &UIView,
    start_position: Point<Pixels>,
) -> Option<BackGestureEvent> {
    // Gestures from the right edge move content leftward toward going back.
    let direction = if recognizer.edges() == UIRectEdge::Right {
        -1.
    } else {
        1.
    };
    let width = view.bounds().size.width.max(1.);
    let travel = recognizer.translationInView(Some(view)).x * direction;
    let progress = (travel / width).clamp(0., 1.);
    let phase = match recognizer.state() {
        UIGestureRecognizerState::Began | UIGestureRecognizerState::Changed => {
            BackGesturePhase::Progressed
        }
        UIGestureRecognizerState::Ended => {
            let velocity = recognizer.velocityInView(Some(view)).x * direction;
            if velocity > BACK_COMMIT_VELOCITY
                || (progress > 0.5 && velocity > -BACK_COMMIT_VELOCITY)
            {
                BackGesturePhase::Committed
            } else {
                BackGesturePhase::Cancelled
            }
        }
        UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
            BackGesturePhase::Cancelled
        }
        _ => return None,
    };
    Some(BackGestureEvent {
        phase,
        start_position,
        progress: progress as f32,
    })
}

pub fn touch_location_in_view(touch: &UITouch, view: &UIView) -> Point<Pixels> {
    let location = touch.locationInView(Some(view));
    Point::new(px(location.x as f32), px(location.y as f32))
}

pub fn touch_phase(touch: &UITouch) -> TouchPhase {
    match touch.phase() {
        UITouchPhase::Began => TouchPhase::Started,
        UITouchPhase::Moved | UITouchPhase::Stationary => TouchPhase::Moved,
        UITouchPhase::Ended => TouchPhase::Ended,
        _ => TouchPhase::Cancelled,
    }
}

pub fn touch_id(touch: &UITouch) -> TouchId {
    TouchId(touch as *const UITouch as usize as u64)
}

/// Converts one step of a trackpad or mouse-wheel scroll gesture.
///
/// The recognizer's translation is reset after each step, so every event carries
/// only the movement since the previous one. UIKit reports the translation in
/// the direction the content should move, which matches GPUI's pixel deltas.
/// Returns `None` for states that carry no scroll.
pub fn scroll_event(recognizer: &UIPanGestureRecognizer, view: &UIView) -> Option<ScrollEvent> {
    let touch_phase = match recognizer.state() {
        UIGestureRecognizerState::Began => TouchPhase::Started,
        UIGestureRecognizerState::Changed => TouchPhase::Moved,
        UIGestureRecognizerState::Ended => TouchPhase::Ended,
        UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
            TouchPhase::Cancelled
        }
        _ => return None,
    };
    let translation = recognizer.translationInView(Some(view));
    recognizer.setTranslation_inView(CGPoint::ZERO, Some(view));
    let location = recognizer.locationInView(Some(view));
    Some(ScrollEvent {
        position: Point::new(px(location.x as f32), px(location.y as f32)),
        delta: ScrollDelta::Pixels(Point::new(
            px(translation.x as f32),
            px(translation.y as f32),
        )),
        modifiers: modifiers(recognizer.modifierFlags()),
        touch_phase,
        source: ScrollSource::Pointer,
    })
}

fn modifiers(flags: UIKeyModifierFlags) -> Modifiers {
    Modifiers {
        control: flags.contains(UIKeyModifierFlags::Control),
        alt: flags.contains(UIKeyModifierFlags::Alternate),
        shift: flags.contains(UIKeyModifierFlags::Shift),
        platform: flags.contains(UIKeyModifierFlags::Command),
        function: false,
    }
}

pub fn touch_force(touch: &UITouch) -> Option<f32> {
    let maximum_force = touch.maximumPossibleForce();
    if maximum_force <= 0.0 {
        return None;
    }
    Some((touch.force() / maximum_force).clamp(0.0, 1.0) as f32)
}

/// A hardware key as GPUI names it, or `None` for a modifier key alone.
pub(super) fn keystroke(key: &UIKey) -> Option<Keystroke> {
    let modifiers = modifiers(key.modifierFlags());
    let usage = key.keyCode().0;
    let named = match usage {
        0x28 | 0x58 => Some("enter".to_owned()),
        0x29 => Some("escape".to_owned()),
        0x2A => Some("backspace".to_owned()),
        0x2B => Some("tab".to_owned()),
        0x2C => Some("space".to_owned()),
        0x49 => Some("insert".to_owned()),
        0x4A => Some("home".to_owned()),
        0x4B => Some("pageup".to_owned()),
        0x4C => Some("delete".to_owned()),
        0x4D => Some("end".to_owned()),
        0x4E => Some("pagedown".to_owned()),
        0x4F => Some("right".to_owned()),
        0x50 => Some("left".to_owned()),
        0x51 => Some("down".to_owned()),
        0x52 => Some("up".to_owned()),
        0x3A..=0x45 => Some(format!("f{}", usage - 0x39)),
        0xE0..=0xE7 => return None,
        _ => None,
    };
    let typing = !modifiers.control && !modifiers.platform;
    if let Some(name) = named {
        let key_char = (typing && name == "space").then(|| " ".to_owned());
        return Some(Keystroke { modifiers, key: name, key_char });
    }
    let key_name = key.charactersIgnoringModifiers().to_string().to_lowercase();
    if key_name.is_empty() {
        return None;
    }
    let characters = key.characters().to_string();
    let key_char = (typing && !characters.chars().any(char::is_control) && !characters.is_empty())
        .then_some(characters);
    Some(Keystroke { modifiers, key: key_name, key_char })
}

/// Whether a hardware key belongs to the text system while a text field has
/// the keyboard: characters typed without Control or Command, Return and
/// Backspace (input methods compose from them). Everything else (arrows,
/// Escape, Tab, function keys, shortcuts) goes to GPUI as a key event.
pub(super) fn key_is_text(key: &UIKey) -> bool {
    let flags = key.modifierFlags();
    if flags.contains(UIKeyModifierFlags::Control) || flags.contains(UIKeyModifierFlags::Command) {
        return false;
    }
    match key.keyCode().0 {
        0x28 | 0x58 | 0x2A => true,
        0x29 | 0x2B | 0x3A..=0x52 => false,
        _ => {
            let characters = key.characters().to_string();
            !characters.is_empty() && !characters.chars().any(char::is_control)
        }
    }
}
