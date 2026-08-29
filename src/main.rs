//! cterm - A high-performance terminal emulator
//!
//! This is the main entry point that selects the appropriate UI backend
//! based on the target platform.

// The native Windows backend is a GUI application. Diagnostics still flow to
// cterm's capture/logger instead of opening a second console window.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "macos")]
    {
        cterm_cocoa::run();
    }

    #[cfg(target_os = "windows")]
    {
        cterm_win32::run();
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        cterm_gtk::run();
    }
}
