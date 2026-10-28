use ::windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::Threading::GetCurrentProcessId,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
use anyhow::{Result, anyhow, ensure};
use gpui::{
    App, AppContext, Application, AsyncApp, Bounds, Context, FocusHandle, InputHandler,
    InteractiveElement, IntoElement, ParentElement, Pixels, Point, QuitMode, Render, Styled,
    UTF16Selection, Window, WindowBounds, WindowHandle, WindowOptions, canvas, div, point, px,
    size,
};
use gpui_windows::WindowsPlatform;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

const HELP: &str = "input_lifecycle --isolated-profile NEW_TEMP_DIRECTORY; requires GPUI_INPUT_LIFECYCLE_FIXTURE=1";

#[derive(Default)]
struct TextState {
    text: Vec<u16>,
    selected: Range<usize>,
    marked: Option<Range<usize>>,
    journal: Vec<Value>,
    keys: Vec<Value>,
    accept: bool,
    context_generation: u64,
}
impl TextState {
    fn reset(&mut self, text: &str, marked: bool) {
        self.text = text.encode_utf16().collect();
        self.selected = self.text.len()..self.text.len();
        self.marked = marked.then_some(0..self.text.len());
        self.journal.clear();
        self.keys.clear();
        self.accept = true;
    }
    fn value(&self) -> String {
        String::from_utf16(&self.text).expect("Fixture text must be valid UTF-16")
    }
    fn replace(&mut self, range: Option<Range<usize>>, text: &str, mark: bool) {
        assert!(self.journal.len() < 128, "Input journal exceeded its bound");
        self.journal.push(json!({"operation": if mark { "preedit" } else { "replace" }, "text": text, "accepted": self.accept}));
        if !self.accept {
            return;
        }
        let range = range
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        assert!(range.start <= range.end && range.end <= self.text.len());
        let inserted: Vec<_> = text.encode_utf16().collect();
        let end = range.start + inserted.len();
        self.marked = (mark && !inserted.is_empty()).then_some(range.start..end);
        self.text.splice(range, inserted);
        self.selected = end..end;
    }
}
#[derive(Clone)]
struct Handler(Rc<RefCell<TextState>>);
impl InputHandler for Handler {
    fn text_input_context_generation(&mut self, _: &mut Window, _: &mut App) -> u64 {
        self.0.borrow().context_generation
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.0.borrow().selected.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<Range<usize>> {
        self.0.borrow().marked.clone()
    }
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        let state = self.0.borrow();
        let value = String::from_utf16(state.text.get(range.clone())?).ok()?;
        *actual = Some(range);
        Some(value)
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        _: &mut App,
    ) {
        self.0.borrow_mut().replace(range, text, false);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) {
        let mut state = self.0.borrow_mut();
        state.replace(range, text, true);
        if let (Some(marked), Some(selected)) = (state.marked.clone(), selected) {
            state.selected = marked.start + selected.start..marked.start + selected.end;
        }
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut App) {
        self.0.borrow_mut().marked = None;
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(point(px(10.), px(10.)), size(px(1.), px(20.))))
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        Some(0)
    }
    fn accepts_text_input(&mut self, _: &mut Window, _: &mut App) -> bool {
        self.0.borrow().accept
    }
}
struct Probe {
    fields: [Rc<RefCell<TextState>>; 2],
    focus: [FocusHandle; 2],
    current: usize,
    painted: Rc<Cell<usize>>,
}
impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let field = self.fields[self.current].clone();
        let keys = field.clone();
        let focus = self.focus[self.current].clone();
        let painted = self.painted.clone();
        let current = self.current;
        div().size_full().track_focus(&focus).on_key_down(move |event, _, cx| {
            let mut state = keys.borrow_mut();
            assert!(state.keys.len() < 128, "Key journal exceeded its bound");
            state.keys.push(json!({"key": event.keystroke.key, "character": event.keystroke.key_char,
                "control": event.keystroke.modifiers.control, "alt": event.keystroke.modifiers.alt,
                "platform": event.keystroke.modifiers.platform, "preferCharacter": event.prefer_character_input}));
            // This fixture consumes explicit shortcuts; provider routing has its own tests.
            if event.keystroke.modifiers.control || event.keystroke.modifiers.alt || event.keystroke.modifiers.platform {
                cx.stop_propagation();
            }
        }).child(canvas(|_, _, _| (), move |_, _, window, cx| {
            window.handle_input(&focus, Handler(field.clone()), cx);
            painted.set(current);
        }).size_full())
    }
}
struct Owned {
    window: WindowHandle<Probe>,
    hwnd: HWND,
    fields: [Rc<RefCell<TextState>>; 2],
    painted: Rc<Cell<usize>>,
}
#[derive(Default)]
struct Report {
    observations: Vec<Value>,
    failures: Vec<String>,
    created: usize,
    destroyed: usize,
    completed: bool,
}

fn hwnd(window: &Window) -> Result<HWND> {
    match HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow!("{error}"))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as _)),
        _ => Err(anyhow!("Expected a Win32 fixture window")),
    }
}
fn open(cx: &mut App) -> Result<Owned> {
    let fields: [Rc<RefCell<TextState>>; 2] = std::array::from_fn(|_| {
        let mut state = TextState::default();
        state.reset("", false);
        Rc::new(RefCell::new(state))
    });
    let painted = Rc::new(Cell::new(usize::MAX));
    let fields_view = fields.clone();
    let painted_view = painted.clone();
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(200.), px(200.)),
                size(px(440.), px(180.)),
            ))),
            focus: false,
            ..Default::default()
        },
        |window, cx| {
            cx.new(|cx| {
                let focus = [cx.focus_handle(), cx.focus_handle()];
                focus[0].focus(window, cx);
                Probe {
                    fields: fields_view,
                    focus,
                    current: 0,
                    painted: painted_view,
                }
            })
        },
    )?;
    let hwnd = window.update(cx, |_, window, _| hwnd(window))??;
    Ok(Owned {
        window,
        hwnd,
        fields,
        painted,
    })
}
async fn until(cx: &AsyncApp, reason: &str, mut ready: impl FnMut() -> bool) -> Result<()> {
    let started = Instant::now();
    while !ready() {
        ensure!(
            started.elapsed() < Duration::from_secs(3),
            "Timed out: {reason}"
        );
        cx.background_executor()
            .timer(Duration::from_millis(20))
            .await;
    }
    Ok(())
}
fn message(window: HWND, message: u32, value: usize) -> Result<()> {
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut process));
    }
    ensure!(
        process == unsafe { GetCurrentProcessId() },
        "Input target is not owned by this fixture"
    );
    unsafe {
        SendMessageW(window, message, Some(WPARAM(value)), Some(LPARAM(0)));
    }
    Ok(())
}
fn expect(
    state: &Rc<RefCell<TextState>>,
    text: &str,
    calls: usize,
    case: &str,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    let state = state.borrow();
    report.borrow_mut().observations.push(
        json!({"case": case, "text": state.value(), "journal": state.journal, "keys": state.keys}),
    );
    ensure!(
        state.value() == text && state.journal.len() == calls,
        "{case}: unexpected text or callback count"
    );
    Ok(())
}
async fn activate(cx: &mut AsyncApp, target: &Owned) -> Result<()> {
    target
        .window
        .update(cx, |_, window, _| window.activate_window())?;
    until(
        cx,
        "owned window activation and input handler",
        || unsafe { GetForegroundWindow() == target.hwnd } && target.painted.get() != usize::MAX,
    )
    .await
}
struct KeyboardState(Option<[u8; 256]>);
impl KeyboardState {
    fn scoped(control: bool) -> Result<Self> {
        let mut saved = [0; 256];
        unsafe { GetKeyboardState(&mut saved) }?;
        let mut state = [0; 256];
        if control {
            state[VK_CONTROL.0 as usize] = 0x80;
            state[VK_LCONTROL.0 as usize] = 0x80;
        }
        unsafe { SetKeyboardState(&state) }?;
        Ok(Self(Some(saved)))
    }
    fn restore(&mut self) -> Result<()> {
        if let Some(saved) = self.0.as_ref() {
            unsafe { SetKeyboardState(saved) }?;
            let mut actual = [0; 256];
            unsafe { GetKeyboardState(&mut actual) }?;
            ensure!(&actual == saved, "Thread keyboard state was not restored");
            self.0 = None;
        }
        Ok(())
    }
}
impl Drop for KeyboardState {
    fn drop(&mut self) {
        if let Some(saved) = self.0.as_ref() {
            let _ = unsafe { SetKeyboardState(saved) };
        }
    }
}
async fn translated_key(
    cx: &mut AsyncApp,
    target: &Owned,
    key: VIRTUAL_KEY,
    control: bool,
) -> Result<()> {
    let mut state = KeyboardState::scoped(control)?;
    let result = async {
        let scan = unsafe { MapVirtualKeyW(key.0 as u32, MAPVK_VK_TO_VSC) };
        unsafe {
            PostMessageW(
                Some(target.hwnd),
                WM_KEYDOWN,
                WPARAM(key.0 as usize),
                LPARAM(((scan << 16) | 1) as isize),
            )
        }?;
        until(cx, "translated fixture key", || {
            !target.fields[0].borrow().keys.is_empty()
        })
        .await?;
        // TranslateMessage posts WM_CHAR behind the original key. Inspect only
        // this owned HWND's queue; the production pump must deliver the text.
        until(cx, "translated character delivery", || {
            let mut message = MSG::default();
            !unsafe {
                PeekMessageW(
                    &mut message,
                    Some(target.hwnd),
                    WM_KEYFIRST,
                    WM_KEYLAST,
                    PM_NOREMOVE,
                )
                .as_bool()
            }
        })
        .await
    }
    .await;
    let restored = state.restore();
    match (result, restored) {
        (Err(input), Err(restore)) => Err(anyhow!("{input}; keyboard cleanup: {restore}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn exercise(
    cx: &mut AsyncApp,
    owned: &mut Vec<Owned>,
    report: &Rc<RefCell<Report>>,
) -> Result<()> {
    for _ in 0..2 {
        owned.push(cx.update(open)?);
        report.borrow_mut().created += 1;
    }
    let first = &owned[0];
    let second = &owned[1];
    activate(cx, first).await?;
    let state = &first.fields[0];
    for character in "a👋e\u{301}".encode_utf16() {
        message(first.hwnd, WM_CHAR, character as usize)?;
    }
    expect(
        state,
        "a👋e\u{301}",
        4,
        "unicode-and-surrogate-pair",
        report,
    )?;
    state.borrow_mut().reset("", false);
    for character in [0xdc4b, 0xd83d, b'a' as u16, 0xdc4b] {
        message(first.hwnd, WM_CHAR, character as usize)?;
    }
    expect(state, "a", 1, "malformed-and-interrupted-pair", report)?;

    state.borrow_mut().reset("", false);
    message(first.hwnd, WM_CHAR, 0xd83d)?;
    first.window.update(cx, |view, window, cx| {
        view.current = 1;
        view.focus[1].focus(window, cx);
        cx.notify();
    })?;
    message(first.hwnd, WM_CHAR, 0xdc4b)?;
    until(cx, "new field handler", || first.painted.get() == 1).await?;
    expect(state, "", 0, "old-field-after-focus-change", report)?;
    expect(
        &first.fields[1],
        "",
        0,
        "new-field-rejects-old-surrogate",
        report,
    )?;
    message(first.hwnd, WM_CHAR, 0xd83d)?;
    message(first.hwnd, WM_CHAR, 0xdc4b)?;
    expect(
        &first.fields[1],
        "👋",
        1,
        "new-field-accepts-new-pair",
        report,
    )?;
    first.window.update(cx, |view, window, cx| {
        view.current = 0;
        view.focus[0].focus(window, cx);
        cx.notify();
    })?;
    until(cx, "original field handler", || first.painted.get() == 0).await?;

    message(first.hwnd, WM_CHAR, 0xd83d)?;
    state.borrow_mut().accept = false;
    message(first.hwnd, WM_CHAR, 0xdc4b)?;
    state.borrow_mut().accept = true;
    message(first.hwnd, WM_CHAR, 0xdc4b)?;
    expect(
        state,
        "",
        0,
        "disabled-context-rejects-pending-pair",
        report,
    )?;
    message(first.hwnd, WM_CHAR, 0xd83d)?;
    state.borrow_mut().context_generation += 1;
    message(first.hwnd, WM_CHAR, 0xdc4b)?;
    expect(
        state,
        "",
        0,
        "same-field-new-context-rejects-old-surrogate",
        report,
    )?;
    message(first.hwnd, WM_CHAR, 0xd83d)?;
    activate(cx, second).await?;
    message(first.hwnd, WM_CHAR, 0xdc4b)?;
    message(second.hwnd, WM_CHAR, 0xdc4b)?;
    expect(
        state,
        "",
        0,
        "deactivated-window-rejects-pending-pair",
        report,
    )?;
    expect(
        &second.fields[0],
        "",
        0,
        "other-window-does-not-inherit-surrogate",
        report,
    )?;
    activate(cx, first).await?;

    state.borrow_mut().reset("marked", true);
    message(first.hwnd, WM_IME_ENDCOMPOSITION, 0)?;
    expect(state, "", 1, "native-end-cancels-only-marked-text", report)?;
    state.borrow_mut().reset("committed", false);
    state.borrow_mut().selected = 0..9;
    message(first.hwnd, WM_IME_ENDCOMPOSITION, 0)?;
    expect(
        state,
        "committed",
        0,
        "native-end-preserves-committed-selection",
        report,
    )?;

    state.borrow_mut().reset("", false);
    translated_key(cx, first, VK_A, false).await?;
    expect(state, "a", 1, "english-through-native-message-loop", report)?;
    ensure!(
        state.borrow().keys.len() == 1,
        "English key delivered more than once"
    );
    state.borrow_mut().reset("", false);
    translated_key(cx, first, VK_C, true).await?;
    expect(
        state,
        "",
        0,
        "consumed-control-shortcut-no-character",
        report,
    )?;
    ensure!(
        state.borrow().keys.len() == 1 && state.borrow().keys[0]["control"] == true,
        "Control modifier was lost"
    );
    Ok(())
}

fn arguments(args: Vec<OsString>) -> Result<PathBuf> {
    ensure!(args.len() == 2 && args[0] == "--isolated-profile", "{HELP}");
    let path = PathBuf::from(&args[1]);
    ensure!(path.is_absolute(), "Profile must be absolute");
    Ok(path)
}
pub(super) fn run() -> i32 {
    let report = Rc::new(RefCell::new(Report::default()));
    let setup = (|| -> Result<PathBuf> {
        ensure!(
            std::env::var("GPUI_INPUT_LIFECYCLE_FIXTURE").as_deref() == Ok("1"),
            "{HELP}"
        );
        let path = arguments(std::env::args_os().skip(1).collect())?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Missing profile parent"))?
            .canonicalize()?;
        ensure!(
            parent.starts_with(std::env::temp_dir().canonicalize()?),
            "Profile must be inside the fixture temporary directory"
        );
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| anyhow!("Missing profile name"))?,
        );
        fs::create_dir(&path)?;
        Ok(path)
    })();
    let profile = match setup {
        Ok(path) => path,
        Err(error) => {
            println!(
                "{}",
                json!({"schemaVersion": 1, "ok": false, "setupFailure": error.to_string()})
            );
            return 2;
        }
    };
    let original = unsafe { GetForegroundWindow() };
    let mut original_process = 0;
    let original_thread =
        unsafe { GetWindowThreadProcessId(original, Some(&mut original_process)) };
    let layout = unsafe { GetKeyboardLayout(0) }.0 as usize;
    match WindowsPlatform::new(false) {
        Ok(platform) => {
            let result = report.clone();
            Application::with_platform(Rc::new(platform))
                .with_quit_mode(QuitMode::Explicit)
                .run(move |cx| {
                    cx.spawn(async move |cx| {
                        let mut owned = Vec::new();
                        if let Err(error) = exercise(cx, &mut owned, &result).await {
                            result.borrow_mut().failures.push(error.to_string());
                        }
                        cx.update(|cx| {
                            for window in cx.windows() {
                                if let Err(error) =
                                    window.update(cx, |_, window, _| window.remove_window())
                                {
                                    result
                                        .borrow_mut()
                                        .failures
                                        .push(format!("Owned window cleanup: {error}"));
                                }
                            }
                        });
                        if let Err(error) = until(cx, "owned HWND destruction", || {
                            owned
                                .iter()
                                .all(|window| !unsafe { IsWindow(Some(window.hwnd)).as_bool() })
                        })
                        .await
                        {
                            result.borrow_mut().failures.push(error.to_string());
                        }
                        let mut result = result.borrow_mut();
                        result.destroyed = owned
                            .iter()
                            .filter(|window| !unsafe { IsWindow(Some(window.hwnd)).as_bool() })
                            .count();
                        if result.destroyed != result.created {
                            result
                                .failures
                                .push("Owned window cleanup count differs".into());
                        }
                        result.completed = true;
                        drop(result);
                        cx.update(|cx| cx.quit());
                    })
                    .detach();
                });
        }
        Err(error) => report.borrow_mut().failures.push(error.to_string()),
    }
    if let Err(error) = fs::remove_dir(&profile) {
        report
            .borrow_mut()
            .failures
            .push(format!("Profile cleanup: {error}"));
    }
    if unsafe { IsWindow(Some(original)).as_bool() } {
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(original, Some(&mut process)) };
        if process == original_process && thread == original_thread {
            if unsafe { GetForegroundWindow() } != original
                && !unsafe { SetForegroundWindow(original).as_bool() }
            {
                report.borrow_mut().failures.push(
                    "Cannot restore the original foreground window after fixture cleanup".into(),
                );
            }
        } else {
            report
                .borrow_mut()
                .failures
                .push("Original foreground window identity changed during the fixture".into());
        }
    }
    let foreground = unsafe { GetForegroundWindow() };
    let report = report.borrow();
    let ok = report.completed && report.failures.is_empty();
    println!(
        "{}",
        json!({"schemaVersion": 1, "ok": ok, "completed": report.completed, "observations": report.observations,
        "failures": report.failures, "created": report.created, "destroyed": report.destroyed, "keyboardLayout": format!("{layout:016x}"),
        "initialForeground": original.0 as usize, "finalForeground": foreground.0 as usize,
        "unavailable": ["Native AltGr/dead-key layout coverage", "Actual CJK IMM service", "TSF-only service and candidate UI", "Remote provider/agent integration"],
        "scope": "Owned GPUI native-window delivery and English translation; seeded marked state is not an actual IME service"})
    );
    if ok { 0 } else { 1 }
}
