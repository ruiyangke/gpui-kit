//! Native API admission only; this does not qualify visible transparency.
use super::{dwm_set_window_composition_attribute, set_window_composition_attribute};
use anyhow::{Context as _, Result, ensure};
use windows::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::Dwm::{DWMWA_SYSTEMBACKDROP_TYPE, DwmGetWindowAttribute},
        System::SystemInformation::OSVERSIONINFOW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

struct OwnedWindow(HWND);

impl OwnedWindow {
    fn close(&mut self) -> Result<()> {
        if !self.0.is_invalid() {
            unsafe { DestroyWindow(self.0) }.context("Destroying owned appearance window")?;
            self.0 = HWND::default();
        }
        Ok(())
    }
}

impl Drop for OwnedWindow {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("Owned appearance window cleanup failed: {error:#}");
        }
    }
}

#[test]
#[ignore = "Requires explicit native fixture admission on Windows 11 build 22621 or newer"]
fn native_appearance_helpers_apply_and_report_rejection() -> Result<()> {
    ensure!(
        std::env::var("GPUI_WINDOW_LIFECYCLE_FIXTURE").as_deref() == Ok("1"),
        "Requires GPUI_WINDOW_LIFECYCLE_FIXTURE=1",
    );
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut version) }
        .ok()
        .context("Querying native fixture support")?;
    ensure!(
        version.dwBuildNumber >= 22621,
        "Native appearance fixture requires Windows build 22621; found {}",
        version.dwBuildNumber,
    );
    let foreground = unsafe { GetForegroundWindow() };
    let mut window = OwnedWindow(unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
            w!("STATIC"),
            w!("Owned native appearance contract"),
            WS_POPUP,
            0,
            0,
            64,
            64,
            None,
            None,
            None,
            None,
        )
    }?);
    let outcome = (|| -> Result<()> {
        let mut before = RECT::default();
        unsafe { GetWindowRect(window.0, &mut before) }?;
        let style = unsafe { GetWindowLongPtrW(window.0, GWL_STYLE) };
        let extended = unsafe { GetWindowLongPtrW(window.0, GWL_EXSTYLE) };
        for state in [0, 2, 0] {
            ensure!(
                set_window_composition_attribute(window.0, None, state)?,
                "Composition API unexpectedly unavailable on this supported host",
            );
        }
        for backdrop in [2_u32, 4, 0] {
            ensure!(
                dwm_set_window_composition_attribute(window.0, backdrop)?,
                "DWM backdrop unexpectedly unavailable on this supported host",
            );
            let mut actual = u32::MAX;
            unsafe {
                DwmGetWindowAttribute(
                    window.0,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    (&mut actual as *mut u32).cast(),
                    size_of::<u32>() as u32,
                )
            }?;
            ensure!(
                actual == backdrop,
                "DWM backdrop readback differs: {actual} != {backdrop}",
            );
        }
        // Null is unambiguously invalid; never reuse a destroyed HWND that
        // could have been reassigned to an unrelated window.
        ensure!(
            set_window_composition_attribute(HWND::default(), None, 2).is_err(),
            "Composition API rejection was swallowed",
        );
        ensure!(
            dwm_set_window_composition_attribute(HWND::default(), 2).is_err(),
            "DWM backdrop rejection was swallowed",
        );
        let mut after = RECT::default();
        unsafe { GetWindowRect(window.0, &mut after) }?;
        ensure!(before == after, "Appearance requests changed window bounds");
        ensure!(
            unsafe { GetWindowLongPtrW(window.0, GWL_STYLE) } == style
                && unsafe { GetWindowLongPtrW(window.0, GWL_EXSTYLE) } == extended,
            "Appearance requests changed window styles",
        );
        ensure!(
            !unsafe { IsWindowVisible(window.0) }.as_bool(),
            "Owned window became visible",
        );
        Ok(())
    })();
    window.close()?;
    ensure!(
        unsafe { GetForegroundWindow() } == foreground,
        "Appearance fixture changed the foreground window",
    );
    outcome?;
    eprintln!(
        "NATIVE_APPEARANCE_CONTRACT build={} created=1 destroyed=1 api_admission=true visual_qualified=false",
        version.dwBuildNumber,
    );
    Ok(())
}
