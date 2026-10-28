//! Run through the host project's native-fixture xtask on an isolated desktop.
#[cfg(target_os = "windows")]
#[path = "window_lifecycle/windows.rs"]
mod windows;

fn main() {
    #[cfg(target_os = "windows")]
    std::process::exit(windows::run());
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("The window lifecycle fixture requires Windows");
        std::process::exit(2);
    }
}
