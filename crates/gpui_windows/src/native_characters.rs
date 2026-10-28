//! Layout translation for explicitly opted-in, non-IME targets.
use std::cell::Cell;
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const MAX_UNITS: usize = 32;
/// ToUnicodeEx flags that keep the layout's dead-key state, as
/// TranslateMessage does.
const KEEP_DEAD_KEY_STATE: u32 = 0;

/// What a key down typed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Translation {
    /// The characters, including a dead key's when the next key doesn't
    /// compose with it.
    Text(String),
    /// A dead key, kept for the next key.
    DeadKey,
}

/// Native messages retire key authority before deferred GPUI focus observers.
pub(crate) struct Authority {
    state: Cell<AuthorityState>,
    generation: Cell<Option<u64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AuthorityState {
    focused: bool,
    active: bool,
    application_active: bool,
    layout: isize,
    retired: bool,
}

impl Authority {
    pub(crate) fn new(window: HWND) -> Self {
        let state = unsafe {
            let thread = GetWindowThreadProcessId(window, None);
            AuthorityState {
                focused: GetFocus() == window,
                active: GetActiveWindow() == window,
                application_active: thread != 0
                    && GetWindowThreadProcessId(GetForegroundWindow(), None) == thread,
                layout: GetKeyboardLayout(0).0 as isize,
                retired: false,
            }
        };
        Self::from_state(state)
    }

    fn from_state(state: AuthorityState) -> Self {
        Self {
            state: Cell::new(state),
            generation: Cell::new(Some(0)),
        }
    }

    fn update(&self, change: impl FnOnce(&mut AuthorityState)) {
        let old = self.state.get();
        if old.retired {
            return;
        }
        let mut next = old;
        change(&mut next);
        if next != old {
            self.state.set(next);
            // Exhaustion is terminal; a wrapped generation could revive a key.
            self.generation
                .set(self.generation.get().and_then(|value| value.checked_add(1)));
        }
    }

    pub(crate) fn observe(&self, message: u32, wparam: WPARAM, lparam: LPARAM) {
        self.update(|state| match message {
            WM_SETFOCUS => state.focused = true,
            WM_KILLFOCUS => state.focused = false,
            WM_ACTIVATE => match wparam.0 as u32 & 0xffff {
                WA_INACTIVE => state.active = false,
                WA_ACTIVE | WA_CLICKACTIVE => state.active = true,
                _ => (),
            },
            WM_ACTIVATEAPP => state.application_active = wparam.0 != 0,
            WM_INPUTLANGCHANGE => state.layout = lparam.0,
            WM_DESTROY | WM_NCDESTROY => state.retired = true,
            _ => (),
        });
    }

    pub(crate) fn retire(&self) {
        self.update(|state| state.retired = true);
    }

    fn current(&self) -> Option<u64> {
        let state = self.state.get();
        (state.focused && state.active && state.application_active && !state.retired)
            .then(|| self.generation.get())
            .flatten()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    window: isize,
    pub target: (u64, u64),
    layout: isize,
    generation: u64,
}
impl Origin {
    pub(crate) fn capture(window: HWND, target: (u64, u64), authority: &Authority) -> Option<Self> {
        let layout = unsafe { GetKeyboardLayout(0) }.0 as isize;
        authority.update(|state| state.layout = layout);
        let origin = Self {
            window: window.0 as isize,
            target,
            layout,
            generation: authority.current()?,
        };
        origin.is_current(authority).then_some(origin)
    }

    pub(crate) fn is_current(self, authority: &Authority) -> bool {
        self.matches(
            authority,
            unsafe { GetFocus() }.0 as isize,
            unsafe { GetKeyboardLayout(0) }.0 as isize,
        )
    }

    fn matches(self, authority: &Authority, focus: isize, layout: isize) -> bool {
        authority.current() == Some(self.generation)
            && authority.state.get().layout == self.layout
            && self.window == focus
            && self.layout == layout
    }
}

fn resolve(
    translate: impl FnOnce(&mut [u16; MAX_UNITS], u32) -> Result<i32, &'static str>,
) -> Result<Translation, &'static str> {
    let mut units = [0; MAX_UNITS];
    let count = translate(&mut units, KEEP_DEAD_KEY_STATE)?;
    if count < 0 {
        return Ok(Translation::DeadKey);
    }
    if count == 0 {
        return Err("The native keyboard produced no text for this key");
    }
    let count = usize::try_from(count).map_err(|_| "Invalid native character length")?;
    let units = units
        .get(..count)
        .ok_or("Native character output exceeds its limit")?;
    let text =
        String::from_utf16(units).map_err(|_| "The native keyboard produced invalid UTF-16")?;
    if text.chars().any(char::is_control) {
        return Err("The native keyboard produced non-text input");
    }
    Ok(Translation::Text(text))
}

/// Consumed by WM_GPUI_KEYDOWN before either pump calls TranslateMessage,
/// so this translation takes its place: a dead key stays with the layout,
/// and the next key composes with it, or types both characters when they
/// don't compose, as in any Windows application.
pub(crate) fn translate(
    origin: Origin,
    authority: &Authority,
    key: VIRTUAL_KEY,
    scan: u32,
) -> Result<Translation, &'static str> {
    resolve(|units, flags| {
        if !origin.is_current(authority) {
            return Err("The native character target changed");
        }
        let mut keyboard = [0; 256];
        unsafe { GetKeyboardState(&mut keyboard) }
            .map_err(|_| "The native keyboard state is unavailable")?;
        Ok(unsafe {
            ToUnicodeEx(
                u32::from(key.0),
                scan,
                &keyboard,
                units,
                flags,
                Some(HKL(origin.layout as *mut _)),
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{Authority, AuthorityState, KEEP_DEAD_KEY_STATE, Origin, Translation, resolve};
    use std::cell::Cell;
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::*,
    };

    fn active_authority() -> Authority {
        Authority::from_state(AuthorityState {
            focused: true,
            active: true,
            application_active: true,
            layout: 10,
            retired: false,
        })
    }

    fn captured(authority: &Authority) -> Origin {
        Origin {
            window: 17,
            target: (3, 9),
            layout: 10,
            generation: authority.current().unwrap(),
        }
    }

    #[test]
    fn native_focus_and_activation_roundtrips_cannot_revive_the_same_window_origin() {
        for (lost, lost_value, gained, gained_value) in [
            (WM_KILLFOCUS, 0, WM_SETFOCUS, 0),
            (
                WM_ACTIVATE,
                WA_INACTIVE as usize,
                WM_ACTIVATE,
                WA_ACTIVE as usize,
            ),
            (WM_ACTIVATEAPP, 0, WM_ACTIVATEAPP, 1),
        ] {
            let authority = active_authority();
            let origin = captured(&authority);
            assert!(origin.matches(&authority, 17, 10));
            authority.observe(lost, WPARAM(lost_value), LPARAM(0));
            assert!(!origin.matches(&authority, 17, 10));
            authority.observe(gained, WPARAM(gained_value), LPARAM(0));
            // HWND, HKL and deferred GPUI generations are all unchanged.
            assert!(!origin.matches(&authority, 17, 10));
            let fresh = captured(&authority);
            assert_eq!(fresh.target, origin.target);
            assert!(fresh.matches(&authority, 17, 10));
        }
    }

    #[test]
    fn layout_roundtrip_rejects_stale_text_and_error_authority() {
        let authority = active_authority();
        let origin = captured(&authority);
        authority.observe(WM_INPUTLANGCHANGE, WPARAM(0), LPARAM(20));
        assert!(!origin.matches(&authority, 17, 20));
        authority.observe(WM_INPUTLANGCHANGE, WPARAM(0), LPARAM(10));
        assert!(!origin.matches(&authority, 17, 10));
        assert!(captured(&authority).matches(&authority, 17, 10));
    }

    #[test]
    fn duplicate_native_state_and_non_authoritative_messages_preserve_origin() {
        let authority = active_authority();
        let origin = captured(&authority);
        for (message, wparam, lparam) in [
            (WM_SETFOCUS, 0, 0),
            (WM_ACTIVATE, WA_ACTIVE as usize, 0),
            (WM_ACTIVATE, WA_CLICKACTIVE as usize, 0),
            (WM_ACTIVATEAPP, 1, 0),
            (WM_INPUTLANGCHANGE, 0, 10),
            (WM_INPUTLANGCHANGEREQUEST, 0, 20),
            (WM_NCACTIVATE, 0, 0),
            (WM_PAINT, 0, 0),
            (WM_CLOSE, 0, 0),
        ] {
            authority.observe(message, WPARAM(wparam), LPARAM(lparam));
            assert!(origin.matches(&authority, 17, 10), "message {message}");
        }
    }

    #[test]
    fn retirement_and_generation_exhaustion_permanently_refuse_authority() {
        for message in [WM_DESTROY, WM_NCDESTROY] {
            let authority = active_authority();
            let origin = captured(&authority);
            authority.observe(message, WPARAM(0), LPARAM(0));
            authority.observe(WM_SETFOCUS, WPARAM(0), LPARAM(0));
            authority.observe(WM_ACTIVATE, WPARAM(WA_ACTIVE as usize), LPARAM(0));
            assert!(!origin.matches(&authority, 17, 10));
            assert!(authority.current().is_none());
        }
        let authority = active_authority();
        let origin = captured(&authority);
        authority.retire();
        assert!(!origin.matches(&authority, 17, 10));
        assert!(authority.current().is_none());

        let authority = active_authority();
        authority.generation.set(Some(u64::MAX));
        let origin = captured(&authority);
        authority.observe(WM_KILLFOCUS, WPARAM(0), LPARAM(0));
        authority.observe(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        assert!(!origin.matches(&authority, 17, 10));
        assert!(authority.current().is_none());
        assert!(authority.generation.get().is_none());
    }

    #[test]
    fn native_bmp_surrogate_and_multi_scalar_outputs_translate_once() {
        for text in ["@", "é", "👋", "´x", "e\u{301}", " "] {
            let calls = Cell::new(0);
            let result = resolve(|units, flags| {
                assert_eq!(flags, KEEP_DEAD_KEY_STATE);
                calls.set(calls.get() + 1);
                let encoded: Vec<_> = text.encode_utf16().collect();
                units[..encoded.len()].copy_from_slice(&encoded);
                Ok(encoded.len() as i32)
            })
            .unwrap();
            assert_eq!(result, Translation::Text(text.into()));
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn malformed_empty_control_and_oversized_output_never_replays_the_key() {
        for (count, first) in [(0, 0), (1, 0xd800), (1, 0xdc4b), (1, 13), (33, 0)] {
            let calls = Cell::new(0);
            assert!(
                resolve(|units, flags| {
                    assert_eq!(flags, KEEP_DEAD_KEY_STATE);
                    calls.set(calls.get() + 1);
                    units[0] = first;
                    Ok(count)
                })
                .is_err()
            );
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn a_dead_key_types_nothing_and_the_next_key_types_what_the_layout_composed() {
        assert_eq!(
            resolve(|units, flags| {
                assert_eq!(flags, KEEP_DEAD_KEY_STATE);
                units[0] = '´' as u16;
                Ok(-1)
            }),
            Ok(Translation::DeadKey)
        );
        for composed in ["é", "´x"] {
            assert_eq!(
                resolve(|units, _| {
                    let encoded: Vec<_> = composed.encode_utf16().collect();
                    units[..encoded.len()].copy_from_slice(&encoded);
                    Ok(encoded.len() as i32)
                }),
                Ok(Translation::Text(composed.into()))
            );
        }
        assert!(resolve(|_, _| Err("Injected native keyboard failure")).is_err());
    }
}
