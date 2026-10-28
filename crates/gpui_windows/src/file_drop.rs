//! Owned WM_DROPFILES admission. Never calls foreign COM objects or the filesystem.
use crate::WindowsWindowInner;
use gpui::{NativeFileDrop, WindowVisibility, point, px, size};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    rc::Rc,
};
use windows::Win32::{
    Foundation::{HGLOBAL, HWND, POINT, RECT},
    System::Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::IsWindowEnabled,
        Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, HDROP},
        WindowsAndMessaging::GetClientRect,
    },
};

const MAX_FILES: usize = 256;
const MAX_BYTES: usize = 256 * 1024;
const MAX_PATH_UNITS: usize = 32_767;
const HEADER_BYTES: usize = 20;
type Context = Rc<dyn Fn() -> Option<u64>>;
type Callback = Box<dyn FnMut(NativeFileDrop)>;

#[derive(Default)]
struct Admission {
    enabled: bool,
    generation: u64,
    exhausted: bool,
    pending: bool,
}
impl Admission {
    fn invalidate(&mut self) {
        match self.generation.checked_add(1) {
            Some(next) => self.generation = next,
            None => {
                self.exhausted = true;
                self.enabled = false;
            }
        }
    }
    fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.invalidate();
        }
        self.enabled = enabled && !self.exhausted;
    }
    fn begin(&mut self) -> Option<u64> {
        if !self.enabled || self.exhausted || self.pending {
            return None;
        }
        self.pending = true;
        Some(self.generation)
    }
    fn current(&self, generation: u64) -> bool {
        self.enabled && !self.exhausted && self.generation == generation
    }
}

pub(crate) struct LocalFileDrop {
    opted_in: bool,
    admission: RefCell<Admission>,
    context: RefCell<Option<Context>>,
    callback: Cell<Option<Callback>>,
}
impl LocalFileDrop {
    pub(crate) fn new(opted_in: bool) -> Self {
        Self {
            opted_in,
            admission: RefCell::new(Admission::default()),
            context: RefCell::new(None),
            callback: Cell::new(None),
        }
    }
    pub(crate) fn install(&self, context: Context, callback: Callback) {
        self.invalidate();
        *self.context.borrow_mut() = Some(context);
        self.callback.set(Some(callback));
    }
    pub(crate) fn invalidate(&self) {
        self.admission.borrow_mut().invalidate();
    }
    pub(crate) fn enable(&self, hwnd: HWND, enabled: bool) {
        if !self.opted_in {
            return;
        }
        let enabled = enabled && self.context.borrow().is_some();
        let changed = {
            let mut admission = self.admission.borrow_mut();
            let previous = admission.enabled;
            admission.set_enabled(enabled);
            (previous != admission.enabled).then_some(admission.enabled)
        };
        if let Some(enabled) = changed {
            unsafe {
                DragAcceptFiles(hwnd, enabled);
            }
        }
    }
    pub(crate) fn retire(&self, hwnd: HWND) {
        self.enable(hwnd, false);
        self.invalidate();
        self.context.borrow_mut().take();
        self.callback.take();
    }
    fn context(&self) -> Option<u64> {
        let selector = self.context.borrow().clone()?;
        selector()
    }
}

impl WindowsWindowInner {
    pub(crate) fn handle_local_file_drop(self: &Rc<Self>, handle: HDROP) {
        // Even disabled, malformed, or duplicate messages own their incoming HDROP.
        let owned = DropHandle(handle);
        let generation = self.local_file_drop.admission.borrow_mut().begin();
        let event = generation.and_then(|generation| {
            if self.state.closing.get()
                || self.visibility() != WindowVisibility::Visible
                || !unsafe { IsWindowEnabled(self.native_handle()) }.as_bool()
            {
                return None;
            }
            let context = self.local_file_drop.context()?;
            capture(self.native_handle(), owned.0, context)
                .ok()
                .map(|event| (generation, event))
        });
        // No native allocation, lock, borrowed path, or callback escapes DragFinish.
        drop(owned);
        let Some((generation, event)) = event else {
            if generation.is_some() {
                self.local_file_drop.admission.borrow_mut().pending = false;
            }
            return;
        };
        let weak = Rc::downgrade(self);
        self.executor
            .spawn(async move {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let valid = this.local_file_drop.admission.borrow().current(generation)
                    && !this.state.closing.get()
                    && this.visibility() == WindowVisibility::Visible
                    && unsafe { IsWindowEnabled(this.native_handle()) }.as_bool()
                    && this.local_file_drop.context() == Some(event.context);
                // Keep the single pending slot occupied through the callback, which
                // can synchronously disable admission when its upload starts.
                if valid {
                    let selector = this.local_file_drop.context.borrow().clone();
                    if let Some(mut callback) = this.local_file_drop.callback.take() {
                        callback(event);
                        // Admission can be disabled by a successful upload. Keep
                        // the handler for the next drop, unless replaced/retired.
                        let same_handler = selector
                            .as_ref()
                            .zip(this.local_file_drop.context.borrow().as_ref())
                            .is_some_and(|(old, current)| Rc::ptr_eq(old, current));
                        if same_handler && !this.state.closing.get() {
                            let replacement = this.local_file_drop.callback.take();
                            this.local_file_drop
                                .callback
                                .set(replacement.or(Some(callback)));
                        }
                    }
                }
                this.local_file_drop.admission.borrow_mut().pending = false;
            })
            .detach();
    }
}

struct DropHandle(HDROP);
impl Drop for DropHandle {
    fn drop(&mut self) {
        unsafe {
            DragFinish(self.0);
        }
    }
}
struct Locked(HGLOBAL);
impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.0);
        }
    }
}

fn capture(hwnd: HWND, handle: HDROP, context: u64) -> Result<NativeFileDrop, &'static str> {
    let global = HGLOBAL(handle.0);
    let bytes = unsafe { GlobalSize(global) };
    if !(HEADER_BYTES + 4..=MAX_BYTES).contains(&bytes) {
        return Err("File-drop metadata exceeds bounds");
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err("Cannot lock file-drop metadata");
    }
    let _locked = Locked(global);
    // The allocation stays locked through the bounded Shell queries. Validate
    // all string terminators before giving a native API this potentially bad block.
    let parsed = parse(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), bytes) })?;
    if unsafe { DragQueryFileW(handle, u32::MAX, None) } as usize != parsed.paths.len() {
        return Err("Inconsistent file-drop count");
    }
    let mut paths = Vec::with_capacity(parsed.paths.len());
    for (index, expected) in parsed.paths.iter().enumerate() {
        let length = unsafe { DragQueryFileW(handle, index as u32, None) } as usize;
        if length != expected.len() {
            return Err("Inconsistent file-drop length");
        }
        let mut output = vec![0; length + 1];
        if unsafe { DragQueryFileW(handle, index as u32, Some(&mut output)) } as usize != length
            || output[length] != 0
            || output[..length] != expected[..]
        {
            return Err("Inconsistent file-drop path");
        }
        // Preserve every UTF-16 unit. The worker rejects unsupported paths; no
        // lossy Unicode conversion, normalization, deduplication, or renaming.
        let path = PathBuf::from(OsString::from_wide(expected));
        if !path.is_absolute() {
            return Err("File-drop path is relative");
        }
        paths.push(path);
    }
    let mut position = POINT::default();
    let client = unsafe { DragQueryPoint(handle, &mut position) }.as_bool();
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }.map_err(|_| "Cannot read drop geometry")?;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if !client
        || parsed.non_client
        || [position.x, position.y] != parsed.point
        || dpi == 0
        || rect.left != 0
        || rect.top != 0
        || position.x < 0
        || position.y < 0
        || position.x >= rect.right
        || position.y >= rect.bottom
    {
        return Err("Drop is outside the client area");
    }
    let scale_factor = dpi as f32 / 96.;
    Ok(NativeFileDrop {
        paths,
        position: point(
            px(position.x as f32 / scale_factor),
            px(position.y as f32 / scale_factor),
        ),
        client_size: size(
            px(rect.right as f32 / scale_factor),
            px(rect.bottom as f32 / scale_factor),
        ),
        scale_factor,
        context,
    })
}

#[derive(Debug, PartialEq)]
struct Files {
    paths: Vec<Vec<u16>>,
    point: [i32; 2],
    non_client: bool,
}
fn parse(bytes: &[u8]) -> Result<Files, &'static str> {
    if !(HEADER_BYTES + 4..=MAX_BYTES).contains(&bytes.len()) {
        return Err("File-drop metadata exceeds bounds");
    }
    let word = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let offset = word(0) as usize;
    if offset < HEADER_BYTES || offset > bytes.len() - 4 || offset % 2 != 0 || word(16) == 0 {
        return Err("Invalid Unicode file-drop header");
    }
    let units: Vec<u16> = bytes[offset..]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut paths = Vec::new();
    let mut start = 0;
    loop {
        if units.get(start) == Some(&0) {
            if paths.is_empty() {
                return Err("Empty file drop");
            }
            break;
        }
        if paths.len() == MAX_FILES {
            return Err("Too many dropped paths");
        }
        let end = units
            .get(start..)
            .and_then(|tail| tail.iter().position(|unit| *unit == 0))
            .map(|length| start + length)
            .ok_or("Unterminated file-drop list")?;
        if end - start > MAX_PATH_UNITS {
            return Err("Dropped path is too long");
        }
        paths.push(units[start..end].to_vec());
        start = end + 1;
    }
    Ok(Files {
        paths,
        point: [word(4) as i32, word(8) as i32],
        non_client: word(12) != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::{Admission, MAX_BYTES, MAX_FILES, MAX_PATH_UNITS, parse};
    fn block(paths: &[Vec<u16>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in [20_u32, 42, 17, 0, 1] {
            bytes.extend(word.to_le_bytes());
        }
        for path in paths {
            for unit in path.iter().copied().chain([0]) {
                bytes.extend(unit.to_le_bytes());
            }
        }
        bytes.extend([0, 0]);
        bytes
    }
    #[test]
    fn paths_preserve_order_extended_prefix_and_unpaired_surrogates() {
        let paths = vec![
            "\\\\?\\C:\\café-你好-🦀".encode_utf16().collect(),
            vec![67, 58, 92, 0xd800],
        ];
        let parsed = parse(&block(&paths)).unwrap();
        assert_eq!(parsed.paths, paths);
        assert_eq!(parsed.point, [42, 17]);
        assert!(!parsed.non_client);
    }
    #[test]
    fn malformed_and_independently_oversized_metadata_is_refused() {
        let valid = block(&["C:\\a".encode_utf16().collect()]);
        for end in 0..valid.len() {
            assert!(parse(&valid[..end]).is_err(), "accepted truncation {end}");
        }
        for offset in [0_u32, 19, 21, u32::MAX] {
            let mut bytes = valid.clone();
            bytes[..4].copy_from_slice(&offset.to_le_bytes());
            assert!(parse(&bytes).is_err());
        }
        let mut ansi = valid.clone();
        ansi[16..20].fill(0);
        assert!(parse(&ansi).is_err());
        assert!(parse(&block(&vec![vec![65]; MAX_FILES])).is_ok());
        assert!(parse(&block(&vec![vec![65]; MAX_FILES + 1])).is_err());
        assert!(parse(&block(&[vec![65; MAX_PATH_UNITS]])).is_ok());
        assert!(parse(&block(&[vec![65; MAX_PATH_UNITS + 1]])).is_err());
        let mut large = valid;
        large.resize(MAX_BYTES + 1, 0);
        assert!(parse(&large).is_err());
        assert!(parse(&block(&[])).is_err());
    }
    #[test]
    fn disable_reenable_and_geometry_changes_cannot_revive_a_queued_drop() {
        let mut state = Admission::default();
        assert_eq!(state.begin(), None);
        state.set_enabled(true);
        let first = state.begin().unwrap();
        assert_eq!(state.begin(), None); // second message cannot replace it
        state.set_enabled(false);
        state.set_enabled(true);
        assert!(!state.current(first));
        assert_eq!(state.begin(), None); // the old queued task still owns its slot
        state.pending = false;
        let next = state.begin().unwrap();
        state.invalidate();
        assert!(!state.current(next));
        state.generation = u64::MAX;
        state.invalidate();
        state.set_enabled(true);
        assert!(!state.current(u64::MAX));
        assert_eq!(state.begin(), None);
    }
}
