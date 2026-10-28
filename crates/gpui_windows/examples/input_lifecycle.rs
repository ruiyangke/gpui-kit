//! Owned-window native input acceptance. Run with the host's fixture harness.
#[cfg(target_os = "windows")]
#[path = "input_lifecycle/windows.rs"]
mod windows;

fn main() {
    #[cfg(target_os = "windows")]
    std::process::exit(windows::run());
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("The input lifecycle fixture requires Windows");
        std::process::exit(2);
    }
}
