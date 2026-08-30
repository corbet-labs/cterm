//! cterm-win32: Native Windows UI for cterm
//!
//! This crate implements the cterm terminal emulator UI using native Windows APIs,
//! Direct2D for rendering, and DirectWrite for text.

// Allow raw pointer handling - this is a Windows GUI crate
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod clipboard;
mod desktop_notification;
pub mod dialog_utils;
pub mod dialogs;
pub mod docker_dialog;
pub mod dpi;
pub mod keycode;
pub mod log_viewer;
pub mod menu;
pub mod mouse;
pub mod notification_bar;
pub mod preferences_dialog;
pub mod quick_open;
pub mod remotes_dialog;
pub mod session_dialog;
pub mod ssh_prompt;
pub mod tab_bar;
pub mod templates_dialog;
pub mod terminal_canvas;
pub mod update_dialog;
pub mod upgrade_receiver;
pub mod window;

use clap::Parser;
pub use cterm_app::cli::Args;

use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

/// Global application arguments (accessible from window creation)
static APP_ARGS: std::sync::OnceLock<Args> = std::sync::OnceLock::new();

/// Get the application arguments (call only after parse_args())
pub fn get_args() -> &'static Args {
    APP_ARGS.get().expect("Args not initialized")
}

fn fatal_startup_error(message: &str, exit_code: i32) -> ! {
    dialogs::show_error(std::ptr::null_mut(), "cterm startup error", message);
    std::process::exit(exit_code);
}

/// Run the Windows application
pub fn run() {
    // Parse command-line arguments
    let args = Args::parse();
    if args.managed {
        let executable = std::env::current_exe().unwrap_or_else(|error| {
            fatal_startup_error(
                &format!("Managed mode could not resolve the UI executable: {error}"),
                2,
            )
        });
        if let Err(error) = args.initialize_runtime(&executable) {
            fatal_startup_error(&format!("Invalid managed runtime: {error}"), 2);
        }
    }

    // Initialize logging
    cterm_app::log_capture::init();
    if let Err(error) = args.preflight_managed_daemon() {
        log::error!("{error}");
        fatal_startup_error(&error, 1);
    }

    log::info!("Starting cterm (Windows native UI)");

    // Check if we're in upgrade receiver mode
    if let Some(ref state_path) = args.upgrade_state {
        log::info!(
            "Running in upgrade receiver mode with state file {}",
            state_path
        );
        let exit_code = upgrade_receiver::run_receiver(state_path);
        std::process::exit(exit_code);
    }

    // Store args for later access
    let _ = APP_ARGS.set(args);

    // Set up DPI awareness
    dpi::setup_dpi_awareness();

    // Initialize common controls for dialogs
    dialog_utils::init_common_controls();

    // Load configuration
    let config = match cterm_app::load_config() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to load config, using defaults: {}", e);
            cterm_app::Config::default()
        }
    };

    // Load theme
    let theme = load_theme(&config);

    // Initialize NWG (if needed for dialogs)
    // native_windows_gui::init().expect("Failed to initialize NWG");

    // Register window class and create window
    if let Err(e) = run_main_loop(&config, &theme) {
        log::error!("Application error: {}", e);
        std::process::exit(1);
    }
}

/// Load the theme based on config
fn load_theme(config: &cterm_app::Config) -> cterm_ui::theme::Theme {
    cterm_app::resolve_theme(config)
}

/// Run the main message loop
fn run_main_loop(
    config: &cterm_app::Config,
    theme: &cterm_ui::theme::Theme,
) -> windows::core::Result<()> {
    // Register window class
    window::register_window_class()?;

    // Create main window
    let _hwnd = window::create_window(config, theme)?;

    // Message loop
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };

        if ret.0 == 0 {
            // WM_QUIT
            break;
        }

        if ret.0 == -1 {
            // Error
            return Err(windows::core::Error::from_win32());
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    log::info!("cterm exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_args_parsing() {
        let args = Args::try_parse_from(["cterm"]).unwrap();
        assert!(!args.fullscreen);
        assert!(args.upgrade_state.is_none());
    }
}
