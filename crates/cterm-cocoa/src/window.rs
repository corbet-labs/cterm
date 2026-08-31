//! Main window implementation for macOS
//!
//! Handles NSWindow creation and management using native macOS window tabbing.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSAutoresizingMaskOptions, NSMenu,
    NSMenuItem, NSView, NSWindow, NSWindowDelegate, NSWindowOcclusionState, NSWindowOrderingMode,
    NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use cterm_app::config::{Config, ShortcutsConfig};
use cterm_app::shortcuts::ShortcutManager;
use cterm_app::upgrade::{PaneLaunchContext, PaneUpgradeState, TabUpgradeState};
use cterm_app::{TemplateDaemonTarget, TemplateLaunchPlan};
use cterm_ui::{
    Action, PaneDirection, PaneId, PaneLayout, PaneLayoutError, PaneRect, SplitDirection,
    SplitPlacement, SplitRatio, SplitRequest, Theme,
};

use crate::cg_renderer::CGRenderer;
use crate::panes::{PaneDivider, PaneFrameView, PaneHostView, PaneRegistry};
use crate::quick_open::{OpenTabEntry, QuickOpenOverlay, QUICK_OPEN_HEIGHT};
use crate::terminal_view::TerminalView;

struct NativePane {
    terminal: Retained<TerminalView>,
    frame: Retained<PaneFrameView>,
}

#[derive(Clone, Copy)]
enum CloseTarget {
    Window,
    Pane(PaneId),
}

#[derive(Clone, Copy)]
enum AppAction {
    NewWindow,
    NextAlertedTab,
    OpenPreferences,
}

fn is_managed_restricted_action(action: &Action) -> bool {
    matches!(
        action,
        Action::NewTab
            | Action::SplitPane(_)
            | Action::ClosePane
            | Action::FocusPane(_)
            | Action::ResizePane(_)
            | Action::TogglePaneZoom
            | Action::NewWindow
            | Action::OpenPreferences
            | Action::QuickOpenTemplate
    )
}

fn config_with_shortcuts(config: &Config, shortcuts: &ShortcutsConfig) -> Config {
    let mut config = config.clone();
    config.shortcuts = shortcuts.clone();
    config
}

fn template_theme_name_matches(requested: &str, resolved: &str) -> bool {
    requested.eq_ignore_ascii_case(resolved)
        || matches!(
            (requested.to_ascii_lowercase().as_str(), resolved),
            ("dark", "Default Dark")
                | ("light", "Default Light")
                | ("tokyo_night" | "tokyo-night", "Tokyo Night")
                | ("dracula", "Dracula")
                | ("nord", "Nord")
        )
}

fn template_theme(config: &Config, fallback: &Theme, override_name: Option<&str>) -> Theme {
    let Some(override_name) = override_name.map(str::trim).filter(|name| !name.is_empty()) else {
        return fallback.clone();
    };

    if let Some(custom) = config.appearance.custom_theme.as_ref() {
        if override_name.eq_ignore_ascii_case("custom")
            || override_name.eq_ignore_ascii_case(&custom.name)
        {
            return custom.clone();
        }
    }

    let mut themed_config = config.clone();
    themed_config.appearance.theme = override_name.to_string();
    themed_config.appearance.custom_theme = None;
    let resolved = cterm_app::config::resolve_theme(&themed_config);
    if template_theme_name_matches(override_name, &resolved.name) {
        resolved
    } else {
        log::warn!("Unknown template theme '{override_name}', using the window theme");
        fallback.clone()
    }
}

fn template_remote_details(plan: &TemplateLaunchPlan) -> Option<(&str, &str, bool)> {
    match &plan.daemon {
        TemplateDaemonTarget::Local => None,
        TemplateDaemonTarget::Named(remote) => Some((
            remote.name.as_str(),
            remote.host.as_str(),
            remote.ssh_compression,
        )),
    }
}

struct DaemonProcessQuery {
    session_id: String,
    daemon_socket: Option<std::path::PathBuf>,
    title: String,
}

/// Window state stored in ivars
pub struct CtermWindowIvars {
    config: Config,
    theme: Theme,
    shortcut_config: RefCell<ShortcutsConfig>,
    shortcuts: RefCell<ShortcutManager>,
    active_terminal: RefCell<Option<Retained<TerminalView>>>,
    panes: RefCell<PaneRegistry<NativePane>>,
    pane_host: RefCell<Option<Retained<PaneHostView>>>,
    pane_drag: RefCell<Option<PaneDivider>>,
    /// Set before native close cleanup so late async session results can be
    /// destroyed instead of being attached to an already-closed tab.
    closed: Cell<bool>,
    close_query_in_progress: Cell<bool>,
    close_approved: Cell<bool>,
    pending_tab_color: RefCell<Option<String>>,
    quick_open: RefCell<Option<Retained<QuickOpenOverlay>>>,
    /// Whether this window has an active bell notification
    has_active_bell: std::cell::Cell<bool>,
}

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "CtermWindow"]
    #[ivars = CtermWindowIvars]
    pub struct CtermWindow;

    unsafe impl NSObjectProtocol for CtermWindow {}

    unsafe impl NSWindowDelegate for CtermWindow {
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            log::debug!("Window became key");
            // Make the terminal view first responder so it can receive keyboard input
            let active_terminal = self.ivars().active_terminal.borrow().clone();
            if let Some(terminal) = active_terminal {
                self.makeFirstResponder(Some(&terminal));
                // Send focus in event if DECSET 1004 is enabled
                terminal.send_focus_event(true);
                self.clear_bell_for_terminal(&terminal);
            } else {
                self.refresh_bell_indicator();
            }

            // Apply pending tab color if any (tab property becomes available after joining tab group)
            // Try immediately, and schedule a retry in case the tab isn't ready yet
            if !self.apply_pending_tab_color() {
                self.schedule_tab_color_retry();
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            log::debug!("Window resigned key");
            // Send focus out event if DECSET 1004 is enabled
            if let Some(terminal) = self.ivars().active_terminal.borrow().as_ref() {
                terminal.send_focus_event(false);
            }
        }

        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn window_did_change_occlusion_state(&self, _notification: &NSNotification) {
            let visibility = if self
                .occlusionState()
                .contains(NSWindowOcclusionState::Visible)
            {
                cterm_core::WindowVisibility::Visible
            } else {
                cterm_core::WindowVisibility::Hidden
            };
            for terminal in self.terminal_views() {
                terminal.set_window_visibility(visibility);
            }
        }

        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> objc2::runtime::Bool {
            if crate::app::is_relaunching()
                || self.ivars().close_approved.replace(false)
                || !self.ivars().config.general.confirm_close_with_running
            {
                return objc2::runtime::Bool::YES;
            }

            self.begin_close_check(CloseTarget::Window, self.terminal_views());
            objc2::runtime::Bool::NO
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            log::debug!("Window will close");
            self.ivars().closed.set(true);
            self.set_bell(false);
            self.cleanup_all_panes();

            // Notify AppDelegate to remove this window from tracking
            let mtm = MainThreadMarker::from(self);
            let app = NSApplication::sharedApplication(mtm);
            if let Some(delegate) = app.delegate() {
                // Call our custom method to remove the window
                let _: () = unsafe { msg_send![&*delegate, windowDidClose: self] };
            }
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            log::debug!("Window did resize");
            self.layout_panes();

            // Update Quick Open overlay width
            if let Some(ref overlay) = *self.ivars().quick_open.borrow() {
                let width = self.frame().size.width;
                overlay.update_width(width);
            }
        }
    }

    // Menu action handlers
    impl CtermWindow {
        #[unsafe(method(newTab:))]
        fn action_new_tab(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::NewTab);
        }

        #[unsafe(method(closeTab:))]
        fn action_close_tab(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::CloseTab);
        }

        #[unsafe(method(splitPaneHorizontal:))]
        fn action_split_pane_horizontal(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::SplitPane(SplitDirection::Horizontal));
        }

        #[unsafe(method(splitPaneVertical:))]
        fn action_split_pane_vertical(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::SplitPane(SplitDirection::Vertical));
        }

        #[unsafe(method(closePane:))]
        fn action_close_pane(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ClosePane);
        }

        #[unsafe(method(focusPaneLeft:))]
        fn action_focus_pane_left(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::FocusPane(PaneDirection::Left));
        }

        #[unsafe(method(focusPaneRight:))]
        fn action_focus_pane_right(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::FocusPane(PaneDirection::Right));
        }

        #[unsafe(method(focusPaneUp:))]
        fn action_focus_pane_up(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::FocusPane(PaneDirection::Up));
        }

        #[unsafe(method(focusPaneDown:))]
        fn action_focus_pane_down(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::FocusPane(PaneDirection::Down));
        }

        #[unsafe(method(resizePaneLeft:))]
        fn action_resize_pane_left(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ResizePane(PaneDirection::Left));
        }

        #[unsafe(method(resizePaneRight:))]
        fn action_resize_pane_right(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ResizePane(PaneDirection::Right));
        }

        #[unsafe(method(resizePaneUp:))]
        fn action_resize_pane_up(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ResizePane(PaneDirection::Up));
        }

        #[unsafe(method(resizePaneDown:))]
        fn action_resize_pane_down(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ResizePane(PaneDirection::Down));
        }

        #[unsafe(method(togglePaneZoom:))]
        fn action_toggle_pane_zoom(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::TogglePaneZoom);
        }

        #[unsafe(method(zoomIn:))]
        fn action_zoom_in(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ZoomIn);
        }

        #[unsafe(method(zoomOut:))]
        fn action_zoom_out(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ZoomOut);
        }

        #[unsafe(method(zoomReset:))]
        fn action_zoom_reset(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::ZoomReset);
        }

        #[unsafe(method(performFindPanelAction:))]
        fn action_find_text(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.dispatch_action(&Action::FindText);
        }

        #[unsafe(method(focusPaneForView:))]
        fn focus_pane_for_view(&self, terminal: &TerminalView) {
            if let Some(id) = self.pane_id_for_terminal(terminal) {
                self.focus_pane(id);
            }
        }

        #[unsafe(method(isActivePaneView:))]
        fn is_active_pane_view(&self, terminal: &TerminalView) -> bool {
            self.is_active_terminal(terminal)
        }

        #[unsafe(method(paneTitleDidChange:))]
        fn pane_title_did_change(&self, terminal: &TerminalView) {
            if self.is_active_terminal(terminal) {
                self.refresh_active_title();
            }
        }

        #[unsafe(method(paneBellDidRing:))]
        fn pane_bell_did_ring(&self, terminal: &TerminalView) {
            let focused = self.isKeyWindow() && self.is_active_terminal(terminal);
            if !focused {
                terminal.set_active_bell(true);
                self.refresh_bell_indicator();
            }
        }

        #[unsafe(method(paneDidExit:))]
        fn pane_did_exit(&self, terminal: &TerminalView) {
            self.close_exited_pane(terminal);
        }

        #[unsafe(method(beginPaneDividerDragAt:))]
        fn begin_pane_divider_drag_at(&self, point: NSPoint) -> bool {
            self.begin_pane_divider_drag(point)
        }

        #[unsafe(method(dragPaneDividerTo:))]
        fn drag_pane_divider_to(&self, point: NSPoint) -> bool {
            self.drag_pane_divider(point)
        }

        #[unsafe(method(endPaneDividerDrag))]
        fn end_pane_divider_drag_action(&self) -> bool {
            self.end_pane_divider_drag()
        }

        /// Called by macOS native tabbing when Command-T or tab bar + is pressed.
        /// Returns a new default window (not a template duplicate).
        #[unsafe(method(newWindowForTab:))]
        fn new_window_for_tab(&self, _sender: Option<&objc2::runtime::AnyObject>) -> *mut NSWindow {
            if crate::app::get_args().managed {
                log::warn!("Ignoring native new-tab request in managed mode");
                return std::ptr::null_mut();
            }
            let mtm = MainThreadMarker::from(self);

            let active = self.ivars().active_terminal.borrow();

            // Get the current working directory from the active terminal
            #[cfg(unix)]
            let cwd = active.as_ref().and_then(|t| t.foreground_cwd());
            #[cfg(not(unix))]
            let cwd: Option<String> = None;

            // Inherit the daemon socket from the active tab (for remote sessions)
            let daemon_socket = active.as_ref().and_then(|t| t.daemon_socket());
            drop(active);

            let config = self.config_with_live_shortcuts();
            let new_window = CtermWindow::new_with_cwd_and_socket(
                mtm,
                &config,
                &self.ivars().theme,
                cwd,
                daemon_socket,
            );

            // Register with AppDelegate for tracking
            let app = NSApplication::sharedApplication(mtm);
            if let Some(delegate) = app.delegate() {
                let _: () = unsafe { msg_send![&*delegate, registerWindow: &*new_window] };
            }

            // Explicitly add to tab group (macOS automatic tabbing doesn't always work)
            self.addTabbedWindow_ordered(&new_window, objc2_app_kit::NSWindowOrderingMode::Above);

            // Make the new tab key and visible
            new_window.makeKeyAndOrderFront(None);

            log::info!("Created new default tab via newWindowForTab:");
            Retained::into_raw(Retained::into_super(new_window))
        }

        /// Retry applying tab color (called via performSelector:afterDelay:)
        #[unsafe(method(retryTabColor))]
        fn retry_tab_color(&self) {
            if !self.apply_pending_tab_color() {
                // Still not ready, try again
                self.schedule_tab_color_retry();
            }
        }

        /// Set tab color via color picker dialog
        #[unsafe(method(setTabColor:))]
        fn action_set_tab_color(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let current = self.ivars().pending_tab_color.borrow().clone();
            match crate::dialogs::show_color_picker_dialog(mtm, current.as_deref()) {
                crate::dialogs::ColorPickerResult::Color(color) => {
                    self.set_tab_color(Some(&color));
                }
                crate::dialogs::ColorPickerResult::Clear => {
                    self.set_tab_color(None);
                }
                crate::dialogs::ColorPickerResult::Cancel => {
                    // Do nothing
                }
            }
        }

        // Window positioning actions
        #[unsafe(method(windowFill:))]
        fn action_window_fill(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_fill();
        }

        #[unsafe(method(windowCenter:))]
        fn action_window_center(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_center();
        }

        #[unsafe(method(windowLeftHalf:))]
        fn action_window_left_half(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_left_half();
        }

        #[unsafe(method(windowRightHalf:))]
        fn action_window_right_half(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_right_half();
        }

        #[unsafe(method(windowTopHalf:))]
        fn action_window_top_half(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_top_half();
        }

        #[unsafe(method(windowBottomHalf:))]
        fn action_window_bottom_half(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_bottom_half();
        }

        #[unsafe(method(windowTopLeftQuarter:))]
        fn action_window_top_left_quarter(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_top_left_quarter();
        }

        #[unsafe(method(windowTopRightQuarter:))]
        fn action_window_top_right_quarter(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_top_right_quarter();
        }

        #[unsafe(method(windowBottomLeftQuarter:))]
        fn action_window_bottom_left_quarter(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_bottom_left_quarter();
        }

        #[unsafe(method(windowBottomRightQuarter:))]
        fn action_window_bottom_right_quarter(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.position_bottom_right_quarter();
        }
    }
);

fn ensure_session_pixel_size(config: &Config, opts: &mut cterm_client::CreateSessionOpts) {
    let (cell_width, cell_height) =
        CGRenderer::measure_cell_size(&config.appearance.font.family, config.appearance.font.size);
    if opts.pixel_width == 0 {
        opts.pixel_width = (cell_width * opts.cols.max(1) as f64)
            .round()
            .clamp(1.0, u32::MAX as f64) as u32;
    }
    if opts.pixel_height == 0 {
        opts.pixel_height = (cell_height * opts.rows.max(1) as f64)
            .round()
            .clamp(1.0, u32::MAX as f64) as u32;
    }
}

fn configured_template_launch_context(config: &Config, name: &str) -> Option<PaneLaunchContext> {
    let template = config
        .sticky_tabs
        .iter()
        .find(|template| template.name == name)?;
    let plan = TemplateLaunchPlan::build(template, config).ok()?;
    let options = plan.session_options(0, 0);
    Some(PaneLaunchContext::capture(&options))
}

fn destroy_unattached_session(session: cterm_client::SessionHandle) {
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            if let Err(error) = runtime.block_on(session.destroy()) {
                log::warn!("Failed to destroy an unattached pane session: {error}");
            }
        }
    });
}

impl CtermWindow {
    /// Execute a shared semantic action against this window's active context.
    ///
    /// Keep this match exhaustive so newly added actions cannot silently fall
    /// through into terminal input on macOS.
    pub(crate) fn dispatch_action(&self, action: &Action) {
        if crate::app::get_args().managed && is_managed_restricted_action(action) {
            log::warn!("Ignoring {action:?} request in managed mode");
            return;
        }

        match action {
            Action::NewTab => self.create_new_tab(),
            Action::CloseTab => self.close_current_tab(),
            Action::NextTab => {
                let _: () =
                    unsafe { msg_send![self, selectNextTab: std::ptr::null::<AnyObject>()] };
            }
            Action::PrevTab => {
                let _: () =
                    unsafe { msg_send![self, selectPreviousTab: std::ptr::null::<AnyObject>()] };
            }
            Action::NextAlertedTab => self.dispatch_app_action(AppAction::NextAlertedTab),
            Action::Tab(number) => self.select_tab_number(*number),
            Action::SplitPane(direction) => self.create_pane(*direction),
            Action::ClosePane => self.close_active_pane(),
            Action::FocusPane(direction) => self.focus_pane_direction(*direction),
            Action::ResizePane(direction) => self.resize_active_pane(*direction),
            Action::TogglePaneZoom => self.toggle_active_pane_zoom(),
            Action::NewWindow => self.dispatch_app_action(AppAction::NewWindow),
            Action::CloseWindow => self.performClose(None),
            Action::Copy => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.copy_selection();
                }
            }
            Action::Paste => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.paste_clipboard();
                }
            }
            Action::SelectAll => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.select_all();
                }
            }
            Action::ZoomIn => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.zoom_in();
                }
            }
            Action::ZoomOut => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.zoom_out();
                }
            }
            Action::ZoomReset => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.zoom_reset();
                }
            }
            Action::ToggleFullscreen => self.toggleFullScreen(None),
            Action::ScrollUp => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_viewport_up(1);
                }
            }
            Action::ScrollDown => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_viewport_down(1);
                }
            }
            Action::ScrollPageUp => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_page_up();
                }
            }
            Action::ScrollPageDown => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_page_down();
                }
            }
            Action::ScrollToTop => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_to_top();
                }
            }
            Action::ScrollToBottom => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_to_bottom();
                }
            }
            Action::PromptPrevious => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_to_previous_prompt();
                }
            }
            Action::PromptNext => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.scroll_to_next_prompt();
                }
            }
            Action::OpenPreferences => self.dispatch_app_action(AppAction::OpenPreferences),
            Action::FindText => self.show_find_dialog(),
            Action::ResetTerminal => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.reset();
                }
            }
            Action::QuickOpenTemplate => self.show_quick_open(),
        }
    }

    fn dispatch_app_action(&self, action: AppAction) {
        let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
        let Some(delegate) = app.delegate() else {
            log::error!("Cannot dispatch Cocoa application action without a delegate");
            return;
        };
        let sender = std::ptr::null::<AnyObject>();
        unsafe {
            match action {
                AppAction::NewWindow => {
                    let _: () = msg_send![&*delegate, newWindow: sender];
                }
                AppAction::NextAlertedTab => {
                    let _: () = msg_send![&*delegate, selectNextAlertedTab: sender];
                }
                AppAction::OpenPreferences => {
                    let _: () = msg_send![&*delegate, showPreferences: sender];
                }
            }
        }
    }

    fn select_tab_number(&self, number: u8) {
        let windows: Option<Retained<NSArray<NSWindow>>> =
            unsafe { msg_send![self, tabbedWindows] };
        let Some(windows) = windows else {
            return;
        };
        let index = usize::from(number).saturating_sub(1);
        if let Some(window) = windows.iter().nth(index) {
            window.makeKeyAndOrderFront(None);
            log::debug!("Selected tab {}", index + 1);
        }
    }

    fn show_find_dialog(&self) {
        let mtm = MainThreadMarker::from(self);
        let Some(pattern) = crate::dialogs::show_input(
            mtm,
            None,
            "Find in Terminal",
            "Search the active terminal's scrollback and visible buffer:",
            "",
        ) else {
            return;
        };
        if pattern.is_empty() {
            return;
        }

        let count = self
            .active_terminal()
            .map(|terminal| terminal.find_text(&pattern, false, false))
            .unwrap_or(0);
        log::info!("Found {count} matches for: {pattern}");

        if count == 0 {
            let alert = objc2_app_kit::NSAlert::new(mtm);
            alert.setAlertStyle(NSAlertStyle::Informational);
            alert.setMessageText(&NSString::from_str("Find in Terminal"));
            alert.setInformativeText(&NSString::from_str(&format!(
                "No matches found for \"{pattern}\"."
            )));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }
    }

    /// Common window initialization: calculate size, allocate, init NSWindow,
    /// set min size, tabbing mode, and delegate.
    fn init_window(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        title: &str,
        pending_tab_color: Option<String>,
    ) -> Retained<Self> {
        let (cell_width, cell_height) = CGRenderer::measure_cell_size(
            &config.appearance.font.family,
            config.appearance.font.size,
        );
        let width = cell_width * 80.0;
        let height = cell_height * 24.0;

        let content_rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(width, height));
        let style_mask = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;

        let this = mtm.alloc::<Self>();
        let this = this.set_ivars(CtermWindowIvars {
            config: config.clone(),
            theme: theme.clone(),
            shortcut_config: RefCell::new(config.shortcuts.clone()),
            shortcuts: RefCell::new(ShortcutManager::from_config(&config.shortcuts)),
            active_terminal: RefCell::new(None),
            panes: RefCell::new(PaneRegistry::default()),
            pane_host: RefCell::new(None),
            pane_drag: RefCell::new(None),
            closed: Cell::new(false),
            close_query_in_progress: Cell::new(false),
            close_approved: Cell::new(false),
            pending_tab_color: RefCell::new(pending_tab_color),
            quick_open: RefCell::new(None),
            has_active_bell: std::cell::Cell::new(false),
        });

        let this: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: content_rect,
                styleMask: style_mask,
                backing: 2u64, // NSBackingStoreBuffered
                defer: false
            ]
        };

        this.setTitle(&NSString::from_str(title));
        this.setMinSize(NSSize::new(400.0, 200.0));
        unsafe { this.setReleasedWhenClosed(false) };
        this.setTabbingMode(NSWindowTabbingMode::Preferred);
        this.setDelegate(Some(ProtocolObject::from_ref(&*this)));

        let host_frame = this
            .contentView()
            .map(|view| view.bounds())
            .unwrap_or_else(|| NSRect::new(NSPoint::ZERO, content_rect.size));
        let pane_host = PaneHostView::new(mtm, host_frame);
        pane_host.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        this.setContentView(Some(&pane_host));
        *this.ivars().pane_host.borrow_mut() = Some(pane_host);

        this
    }

    /// Attach the first terminal view to this tab's pane host.
    fn attach_terminal_view(&self, terminal: Retained<TerminalView>) {
        let visibility = if self
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible)
        {
            cterm_core::WindowVisibility::Visible
        } else {
            cterm_core::WindowVisibility::Hidden
        };
        terminal.set_window_visibility(visibility);
        let mtm = MainThreadMarker::from(self);
        let frame = PaneFrameView::new(mtm, terminal.clone(), &self.ivars().theme);
        let entry = NativePane {
            terminal,
            frame: frame.clone(),
        };
        if self
            .ivars()
            .panes
            .borrow_mut()
            .insert_initial(entry)
            .is_err()
        {
            log::error!("Attempted to attach a second initial terminal to a pane host");
            return;
        }
        self.add_pane_frame(&frame);
        self.layout_panes();
        self.sync_active_terminal(true);
    }

    fn add_pane_frame(&self, frame: &PaneFrameView) {
        let host = self
            .ivars()
            .pane_host
            .borrow()
            .clone()
            .expect("pane host is initialized with the window");
        if let Some(overlay) = self.ivars().quick_open.borrow().as_ref() {
            host.addSubview_positioned_relativeTo(
                frame,
                NSWindowOrderingMode::Below,
                Some(overlay),
            );
        } else {
            host.addSubview(frame);
        }
    }

    fn pane_bounds(&self) -> PaneRect {
        let size = self
            .ivars()
            .pane_host
            .borrow()
            .as_ref()
            .map(|host| host.bounds().size)
            .unwrap_or(NSSize::new(0.0, 0.0));
        PaneRect::new(
            0,
            0,
            size.width.floor().clamp(0.0, u32::MAX as f64) as u32,
            size.height.floor().clamp(0.0, u32::MAX as f64) as u32,
        )
    }

    fn point_in_pane_host(&self, point_in_window: NSPoint) -> NSPoint {
        self.ivars()
            .pane_host
            .borrow()
            .as_ref()
            .map(|host| unsafe { host.convertPoint_fromView(point_in_window, None) })
            .unwrap_or(point_in_window)
    }

    fn begin_pane_divider_drag(&self, point_in_window: NSPoint) -> bool {
        let point = self.point_in_pane_host(point_in_window);
        let divider = self
            .ivars()
            .panes
            .borrow()
            .divider_at(self.pane_bounds(), point.x, point.y);
        let began = divider.is_some();
        *self.ivars().pane_drag.borrow_mut() = divider;
        began
    }

    fn drag_pane_divider(&self, point_in_window: NSPoint) -> bool {
        let Some(divider) = self.ivars().pane_drag.borrow().clone() else {
            return false;
        };
        let point = self.point_in_pane_host(point_in_window);
        let drag_result = {
            self.ivars()
                .panes
                .borrow_mut()
                .drag_divider(&divider, point.x, point.y)
        };
        match drag_result {
            Ok(true) => {
                self.layout_panes();
                self.sync_active_terminal(false);
            }
            Ok(false) => {}
            Err(error) => {
                log::debug!("Ignoring stale pane divider drag: {error}");
                *self.ivars().pane_drag.borrow_mut() = None;
            }
        }
        true
    }

    fn end_pane_divider_drag(&self) -> bool {
        let ended = self.ivars().pane_drag.borrow_mut().take().is_some();
        if ended {
            log::info!("Resized pane divider");
        }
        ended
    }

    fn layout_panes(&self) {
        let bounds = self.pane_bounds();
        let panes = self.ivars().panes.borrow();
        for entry in panes.values() {
            entry.frame.setHidden(true);
        }
        for positioned in panes.positions(bounds) {
            if let Some(entry) = panes.get(positioned.id) {
                let rect = positioned.rect;
                let frame = NSRect::new(
                    NSPoint::new(f64::from(rect.x), f64::from(rect.y)),
                    NSSize::new(f64::from(rect.width), f64::from(rect.height)),
                );
                let _: () = unsafe { msg_send![&*entry.frame, setFrame: frame] };
                entry.frame.setHidden(false);
            }
        }
        drop(panes);
        self.update_pane_focus_rings();
    }

    fn update_pane_focus_rings(&self) {
        let panes = self.ivars().panes.borrow();
        let active = panes.active_id();
        for id in panes.layout().pane_ids() {
            if let Some(entry) = panes.get(id) {
                entry.frame.set_active(id == active);
            }
        }
    }

    fn sync_active_terminal(&self, make_responder: bool) {
        let next = self
            .ivars()
            .panes
            .borrow()
            .active()
            .map(|entry| entry.terminal.clone());
        let previous = self.ivars().active_terminal.borrow().clone();
        let changed = previous
            .as_ref()
            .zip(next.as_ref())
            .is_none_or(|(previous, next)| Retained::as_ptr(previous) != Retained::as_ptr(next));

        if changed && self.isKeyWindow() {
            if let Some(previous) = previous.as_ref() {
                previous.send_focus_event(false);
            }
        }
        *self.ivars().active_terminal.borrow_mut() = next.clone();
        self.update_pane_focus_rings();

        if let Some(terminal) = next {
            let (cell_width, cell_height) = terminal.cell_size();
            self.setContentResizeIncrements(NSSize::new(cell_width, cell_height));
            if make_responder {
                self.makeFirstResponder(Some(&terminal));
                self.clear_bell_for_terminal(&terminal);
            }
            if changed && self.isKeyWindow() {
                terminal.send_focus_event(true);
            }
            self.refresh_active_title();
        }
    }

    fn refresh_active_title(&self) {
        let Some(terminal) = self.ivars().active_terminal.borrow().clone() else {
            return;
        };
        let title = match terminal.current_title() {
            title if !title.is_empty() => title,
            _ => "Terminal".to_string(),
        };
        let title = if self.ivars().has_active_bell.get() {
            format!("🔔 {title}")
        } else {
            title
        };
        self.setTitle(&NSString::from_str(&title));
    }

    fn refresh_bell_indicator(&self) {
        let active = self
            .terminal_views()
            .iter()
            .any(|terminal| terminal.has_active_bell());
        self.set_bell(active);
        self.refresh_active_title();
    }

    fn clear_bell_for_terminal(&self, terminal: &TerminalView) {
        terminal.set_active_bell(false);
        self.refresh_bell_indicator();
    }

    fn pane_id_for_terminal(&self, terminal: &TerminalView) -> Option<PaneId> {
        let pointer = terminal as *const TerminalView;
        self.ivars()
            .panes
            .borrow()
            .id_matching(|entry| std::ptr::eq(Retained::as_ptr(&entry.terminal), pointer))
    }

    fn is_active_terminal(&self, terminal: &TerminalView) -> bool {
        self.ivars()
            .active_terminal
            .borrow()
            .as_ref()
            .is_some_and(|active| std::ptr::eq(Retained::as_ptr(active), terminal))
    }

    fn focus_pane(&self, id: PaneId) {
        let focused = { self.ivars().panes.borrow_mut().set_active(id).is_ok() };
        if focused {
            log::info!("Focused pane {}", id);
            self.layout_panes();
            self.sync_active_terminal(true);
        }
    }

    fn focus_pane_direction(&self, direction: PaneDirection) {
        let bounds = self.pane_bounds();
        let focused = {
            self.ivars()
                .panes
                .borrow_mut()
                .focus_direction(direction, bounds)
        };
        if let Some(active) = focused {
            log::info!("Focused pane {} {:?}", active, direction);
            self.layout_panes();
            self.sync_active_terminal(true);
        }
    }

    fn resize_active_pane(&self, direction: PaneDirection) {
        let bounds = self.pane_bounds();
        let amount = self
            .active_terminal()
            .map(|terminal| {
                let (cell_width, cell_height) = terminal.cell_size();
                match direction {
                    PaneDirection::Left | PaneDirection::Right => cell_width,
                    PaneDirection::Up | PaneDirection::Down => cell_height,
                }
                .round()
                .max(1.0) as u32
            })
            .unwrap_or(1);
        let resized = {
            self.ivars()
                .panes
                .borrow_mut()
                .layout_mut()
                .adjust_active_size(direction, amount, bounds)
        };
        if resized {
            let active = self.ivars().panes.borrow().active_id();
            log::info!("Resized pane {} {:?}", active, direction);
            self.layout_panes();
            self.sync_active_terminal(true);
        }
    }

    fn toggle_active_pane_zoom(&self) {
        if self.ivars().panes.borrow().is_empty() {
            return;
        }
        let zoomed = self.ivars().panes.borrow_mut().layout_mut().toggle_zoom();
        log::info!("Pane zoom {}", zoomed);
        self.layout_panes();
        self.sync_active_terminal(true);
    }

    fn create_pane(&self, direction: SplitDirection) {
        if crate::app::get_args().managed {
            log::warn!("Ignoring split-pane request in managed mode");
            return;
        }

        let (target, terminal) = {
            let panes = self.ivars().panes.borrow();
            let Some(active) = panes.active() else {
                return;
            };
            (panes.active_id(), active.terminal.clone())
        };
        #[cfg(unix)]
        let cwd = terminal.foreground_cwd();
        #[cfg(not(unix))]
        let cwd: Option<String> = None;
        let daemon_socket = terminal.daemon_socket();
        let remote_name = terminal.remote_name();
        let mut launch_context = match terminal.pane_launch_context() {
            Some(context) => context,
            None => {
                log::warn!(
                    "Cannot split an attached session whose process/SSH launch context is unknown"
                );
                self.show_split_context_error();
                return;
            }
        };
        if terminal.is_remote_daemon_pane() {
            // Paths and default argv belong to the remote host. Preserve the
            // portable environment/TERM/native-SSH fields, but let ctermd
            // choose its own default shell for an ordinary remote split.
            launch_context.shell = None;
            launch_context.args.clear();
        }
        let terminal_frame: NSRect = unsafe { msg_send![&*terminal, frame] };
        let (cell_width, cell_height) = terminal.cell_size();
        let config = self.config_with_live_shortcuts();
        let theme = self.ivars().theme.clone();
        let mut opts = cterm_client::CreateSessionOpts {
            cols: (terminal_frame.size.width / cell_width).floor().max(1.0) as u32,
            rows: (terminal_frame.size.height / cell_height).floor().max(1.0) as u32,
            pixel_width: terminal_frame.size.width.round().max(1.0) as u32,
            pixel_height: terminal_frame.size.height.round().max(1.0) as u32,
            cwd,
            ..Default::default()
        };
        launch_context.apply_to(&mut opts);
        opts.base_palette = Some(terminal_palette(&theme, None));
        opts.frontend_state.appearance = theme.appearance();
        let window = unsafe {
            Retained::retain(self as *const _ as *mut CtermWindow)
                .expect("a live pane window can be retained")
        };
        let window = dispatch2::MainThreadBound::new(window, MainThreadMarker::from(self));

        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| cterm_client::ClientError::Connection(error.to_string()))
                .and_then(|runtime| {
                    runtime.block_on(async {
                        let connection = if let Some(ref path) = daemon_socket {
                            cterm_client::DaemonConnection::connect_unix(path, false).await?
                        } else {
                            cterm_client::DaemonConnection::connect_local().await?
                        };
                        connection.create_session(opts).await
                    })
                });

            dispatch2::Queue::main().exec_async(move || {
                let mtm = unsafe { MainThreadMarker::new_unchecked() };
                let window = window.into_inner(mtm);
                match result {
                    Ok(session) => {
                        if window.ivars().closed.get()
                            || window.ivars().panes.borrow().get(target).is_none()
                        {
                            destroy_unattached_session(session);
                            return;
                        }
                        let terminal = TerminalView::from_daemon(mtm, &config, &theme, session);
                        terminal.set_pane_launch_context(launch_context);
                        terminal.set_remote_name(remote_name);
                        window.attach_split_terminal(target, direction, terminal);
                    }
                    Err(error) => log::error!("Failed to create pane session: {error}"),
                }
            });
        });
    }

    fn attach_split_terminal(
        &self,
        target: PaneId,
        direction: SplitDirection,
        terminal: Retained<TerminalView>,
    ) {
        let visibility = if self
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible)
        {
            cterm_core::WindowVisibility::Visible
        } else {
            cterm_core::WindowVisibility::Hidden
        };
        terminal.set_window_visibility(visibility);
        let frame = PaneFrameView::new(
            MainThreadMarker::from(self),
            terminal.clone(),
            &self.ivars().theme,
        );
        let entry = NativePane {
            terminal,
            frame: frame.clone(),
        };
        let request = SplitRequest {
            direction,
            placement: SplitPlacement::Second,
            ratio: SplitRatio::HALF,
        };
        let split_result = {
            self.ivars()
                .panes
                .borrow_mut()
                .split(target, request, entry)
        };
        match split_result {
            Ok(pane_id) => {
                log::info!("Split pane {:?}: pane {}", direction, pane_id);
                self.add_pane_frame(&frame);
                self.layout_panes();
                self.sync_active_terminal(true);
            }
            Err((error, entry)) => {
                log::warn!("Discarding pane whose split target disappeared: {error}");
                entry.terminal.destroy_session();
            }
        }
    }

    fn close_active_pane(&self) {
        let (count, active) = {
            let panes = self.ivars().panes.borrow();
            (
                panes.len(),
                panes.active().map(|entry| entry.terminal.clone()),
            )
        };
        if count <= 1 {
            self.performClose(None);
            return;
        }
        let Some(active) = active else {
            return;
        };
        if self.ivars().config.general.confirm_close_with_running {
            let target = self.ivars().panes.borrow().active_id();
            self.begin_close_check(CloseTarget::Pane(target), vec![active]);
            return;
        }
        let target = self.ivars().panes.borrow().active_id();
        self.remove_pane(target);
    }

    fn close_exited_pane(&self, terminal: &TerminalView) {
        let Some(target) = self.pane_id_for_terminal(terminal) else {
            return;
        };
        if self.ivars().panes.borrow().len() <= 1 {
            self.performClose(None);
        } else {
            self.remove_pane(target);
        }
    }

    fn remove_pane(&self, target: PaneId) {
        let close_result = { self.ivars().panes.borrow_mut().close(target) };
        match close_result {
            Ok(entry) => {
                log::info!("Closed pane {}", target);
                entry.terminal.removeFromSuperview();
                entry.frame.removeFromSuperview();
                self.layout_panes();
                self.sync_active_terminal(true);
            }
            Err(PaneLayoutError::LastPane) => self.performClose(None),
            Err(error) => log::warn!("Could not close pane: {error}"),
        }
    }

    fn cleanup_all_panes(&self) {
        self.ivars().pane_drag.borrow_mut().take();
        *self.ivars().active_terminal.borrow_mut() = None;
        let entries = self.ivars().panes.borrow_mut().drain().collect::<Vec<_>>();
        for entry in entries {
            entry.terminal.removeFromSuperview();
            entry.frame.removeFromSuperview();
        }
    }

    pub fn new(mtm: MainThreadMarker, config: &Config, theme: &Theme) -> Retained<Self> {
        Self::new_with_cwd(mtm, config, theme, None)
    }

    pub fn new_with_cwd(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        cwd: Option<String>,
    ) -> Retained<Self> {
        Self::new_with_cwd_and_socket(mtm, config, theme, cwd, None)
    }

    pub fn new_with_cwd_and_socket(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        cwd: Option<String>,
        daemon_socket: Option<std::path::PathBuf>,
    ) -> Retained<Self> {
        let this = Self::init_window(mtm, config, theme, "Terminal", None);
        let config = this.config_with_live_shortcuts();
        let opts = cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            shell: config.general.default_shell.clone(),
            args: config.general.shell_args.clone(),
            cwd,
            ..Default::default()
        };
        this.spawn_initial_daemon_session_with_opts(opts, None, daemon_socket, false);
        this
    }

    /// Spawn a daemon session in the background and attach the terminal when ready.
    /// Used for initial window creation where the window must exist immediately.
    fn spawn_initial_daemon_session(&self, cwd: Option<String>) {
        let config = self.config_with_live_shortcuts();
        let opts = cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            shell: config.general.default_shell.clone(),
            args: config.general.shell_args.clone(),
            cwd,
            ..Default::default()
        };
        self.spawn_initial_daemon_session_with_opts(opts, None, None, false);
    }

    /// Spawn a daemon session with custom options in the background and attach when ready.
    ///
    /// If `daemon_socket` is `Some`, connect to that specific daemon socket instead
    /// of the local default. This is used to inherit the daemon context from the
    /// current tab (e.g. when opening a new tab on a remote ctermd).
    fn spawn_initial_daemon_session_with_opts(
        &self,
        mut opts: cterm_client::CreateSessionOpts,
        background_color: Option<String>,
        daemon_socket: Option<std::path::PathBuf>,
        title_locked: bool,
    ) {
        let config = self.config_with_live_shortcuts();
        ensure_session_pixel_size(&config, &mut opts);
        let theme = self.ivars().theme.clone();
        opts.base_palette = Some(terminal_palette(&theme, background_color.as_deref()));
        opts.frontend_state.appearance = theme.appearance();
        let launch_context = PaneLaunchContext::capture(&opts);
        let window = unsafe {
            Retained::retain(self as *const _ as *mut CtermWindow)
                .expect("a live terminal window can be retained")
        };
        let window = dispatch2::MainThreadBound::new(window, MainThreadMarker::from(self));

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();

            let result = match rt {
                Ok(rt) => rt.block_on(async {
                    let conn = if let Some(ref path) = daemon_socket {
                        cterm_client::DaemonConnection::connect_unix(path, false).await?
                    } else {
                        cterm_client::DaemonConnection::connect_local().await?
                    };
                    let session = conn.create_session(opts).await?;
                    Ok::<_, cterm_client::ClientError>(session)
                }),
                Err(e) => Err(cterm_client::ClientError::Connection(e.to_string())),
            };

            match result {
                Ok(session) => {
                    dispatch2::Queue::main().exec_async(move || {
                        let mtm = unsafe { MainThreadMarker::new_unchecked() };
                        let window = window.into_inner(mtm);
                        if window.ivars().closed.get() {
                            destroy_unattached_session(session);
                            return;
                        }
                        let terminal_view =
                            TerminalView::from_daemon(mtm, &config, &theme, session);
                        terminal_view.set_pane_launch_context(launch_context);
                        if let Some(ref bg) = background_color {
                            terminal_view.set_background_override(Some(bg));
                        }
                        if title_locked {
                            terminal_view.set_current_title(window.title().to_string());
                        }
                        terminal_view.set_title_locked(title_locked);
                        window.attach_terminal_view(terminal_view);
                    });
                }
                Err(e) => {
                    log::error!("Failed to create initial daemon session: {}", e);
                }
            }
        });
    }

    /// Create a window and spawn a daemon session with specific options
    pub fn new_daemon(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        opts: cterm_client::CreateSessionOpts,
        title: String,
        color: Option<String>,
        background_color: Option<String>,
    ) -> Retained<Self> {
        let this = Self::init_window(mtm, config, theme, &title, color.clone());
        this.spawn_initial_daemon_session_with_opts(opts, background_color, None, false);
        this
    }

    /// Create a CLI-launched window, optionally locking an explicit title
    /// against subsequent OSC title updates.
    pub fn new_cli_daemon(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        opts: cterm_client::CreateSessionOpts,
        title: String,
        title_locked: bool,
    ) -> Retained<Self> {
        let this = Self::init_window(mtm, config, theme, &title, None);
        this.spawn_initial_daemon_session_with_opts(opts, None, None, title_locked);
        this
    }

    /// Create a window connected to a daemon session
    pub fn from_daemon(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        session: cterm_client::SessionHandle,
    ) -> Retained<Self> {
        let this = Self::init_window(mtm, config, theme, "Terminal", None);
        let terminal_view = TerminalView::from_daemon(mtm, config, theme, session);
        this.attach_terminal_view(terminal_view);
        this
    }

    /// Create a window connected to a reconnected daemon session (with screen snapshot)
    pub fn from_daemon_with_screen(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        recon: cterm_app::daemon_reconnect::ReconnectedSession,
    ) -> Retained<Self> {
        let has_custom_title = !recon.custom_title.is_empty();
        let title = if has_custom_title {
            recon.custom_title.clone()
        } else if !recon.title.is_empty() {
            recon.title.clone()
        } else {
            "Terminal".to_string()
        };
        let tab_color = if recon.tab_color.is_empty() {
            None
        } else {
            Some(recon.tab_color.clone())
        };
        let this = Self::init_window(mtm, config, theme, &title, tab_color);
        let terminal_view = TerminalView::from_daemon_with_screen(mtm, config, theme, recon);
        if has_custom_title {
            terminal_view.set_title_locked(true);
        }
        this.attach_terminal_view(terminal_view);
        this
    }

    /// Restore every live daemon session and split owned by one native tab.
    pub fn from_daemon_panes(
        mtm: MainThreadMarker,
        config: &Config,
        theme: &Theme,
        tab_state: &TabUpgradeState,
        layout: PaneLayout,
        reconnected: Vec<cterm_app::daemon_reconnect::ReconnectedSession>,
    ) -> Option<Retained<Self>> {
        let pane_ids = layout.pane_ids();
        if pane_ids.is_empty()
            || pane_ids.len() != tab_state.panes.len()
            || pane_ids.len() != reconnected.len()
        {
            log::error!(
                "Cannot restore pane tab '{}': layout={}, records={}, sessions={}",
                tab_state.title,
                pane_ids.len(),
                tab_state.panes.len(),
                reconnected.len()
            );
            return None;
        }

        let active_index = pane_ids
            .iter()
            .position(|id| *id == layout.active())
            .unwrap_or(0);
        let active_title = tab_state.panes[active_index].title.as_str();
        let title = if active_title.is_empty() {
            tab_state.title.as_str()
        } else {
            active_title
        };
        let this = Self::init_window(mtm, config, theme, title, tab_state.color.clone());
        *this.ivars().panes.borrow_mut() = PaneRegistry::from_layout(layout);

        let mut frames = Vec::with_capacity(pane_ids.len());
        for ((pane_id, pane_state), recon) in pane_ids
            .into_iter()
            .zip(tab_state.panes.iter())
            .zip(reconnected)
        {
            let fallback_title = if !recon.custom_title.is_empty() {
                recon.custom_title.clone()
            } else if !recon.title.is_empty() {
                recon.title.clone()
            } else {
                "Terminal".to_string()
            };
            let pane_title = if pane_state.title.is_empty() {
                fallback_title
            } else {
                pane_state.title.clone()
            };
            let terminal = TerminalView::from_daemon_with_screen(mtm, config, theme, recon);
            terminal.set_current_title(pane_title);
            terminal.set_title_locked(pane_state.title_locked);
            terminal.set_template_name(pane_state.template_name.clone());
            terminal.set_keep_open(pane_state.keep_open);
            terminal.set_remote_name(pane_state.remote_name.clone());
            let launch_context = pane_state.launch_context.clone().or_else(|| {
                pane_state
                    .template_name
                    .as_deref()
                    .and_then(|name| configured_template_launch_context(config, name))
            });
            if let Some(launch_context) = launch_context {
                terminal.set_pane_launch_context(launch_context);
            }

            let frame = PaneFrameView::new(mtm, terminal.clone(), theme);
            let entry = NativePane {
                terminal,
                frame: frame.clone(),
            };
            if this
                .ivars()
                .panes
                .borrow_mut()
                .insert_restored(pane_id, entry)
                .is_err()
            {
                log::error!("Serialized pane layout contains inconsistent pane IDs");
                this.cleanup_all_panes();
                return None;
            }
            frames.push(frame);
        }

        for frame in frames {
            this.add_pane_frame(&frame);
        }
        this.layout_panes();
        this.sync_active_terminal(true);
        Some(this)
    }

    /// Create a new tab connected to a daemon session (using native macOS window tabbing)
    pub fn create_daemon_tab(&self, session: cterm_client::SessionHandle) {
        let mtm = MainThreadMarker::from(self);

        let config = self.config_with_live_shortcuts();
        let new_window = CtermWindow::from_daemon(mtm, &config, &self.ivars().theme, session);

        // Register with AppDelegate
        let app = NSApplication::sharedApplication(mtm);
        if let Some(delegate) = app.delegate() {
            let _: () = unsafe { msg_send![&*delegate, registerWindow: &*new_window] };
        }

        // Add as tab to this window
        self.addTabbedWindow_ordered(&new_window, objc2_app_kit::NSWindowOrderingMode::Above);
        new_window.makeKeyAndOrderFront(None);

        log::info!("Created daemon tab");
    }

    /// Create a new tab (daemon-backed via ctermd)
    pub fn create_new_tab(&self) {
        if crate::app::get_args().managed {
            log::warn!("Ignoring new-tab request in managed mode");
            return;
        }
        let active = self.ivars().active_terminal.borrow();

        // Get the current working directory from the active terminal
        #[cfg(unix)]
        let cwd = active.as_ref().and_then(|t| t.foreground_cwd());
        #[cfg(not(unix))]
        let cwd: Option<String> = None;

        // Inherit the daemon socket from the active tab (for remote sessions)
        let daemon_socket = active.as_ref().and_then(|t| t.daemon_socket());
        drop(active);

        let config = self.config_with_live_shortcuts();
        let opts = cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            shell: config.general.default_shell.clone(),
            args: config.general.shell_args.clone(),
            cwd,
            ..Default::default()
        };

        self.spawn_daemon_tab(opts, None, None, None, None, daemon_socket);
    }

    /// Spawn a daemon session in a background thread and create a tab when ready.
    ///
    /// If `remote` is `Some((manager, name, host))`, the session is created on
    /// the remote ctermd (connecting via SSH if needed). If `daemon_socket` is
    /// `Some`, connect to that specific daemon socket. Otherwise uses the local
    /// daemon.
    pub fn spawn_daemon_tab(
        &self,
        opts: cterm_client::CreateSessionOpts,
        template_name: Option<String>,
        color: Option<String>,
        background_color: Option<String>,
        remote: Option<(cterm_client::RemoteManager, String, String, bool)>,
        daemon_socket: Option<std::path::PathBuf>,
    ) {
        self.spawn_daemon_tab_with_theme(
            opts,
            template_name,
            color,
            background_color,
            false,
            self.ivars().theme.clone(),
            remote,
            daemon_socket,
        );
    }

    /// Launch one normalized template plan into this window's native tab group.
    pub(crate) fn spawn_template_plan(
        &self,
        plan: TemplateLaunchPlan,
        remote_manager: cterm_client::RemoteManager,
    ) -> bool {
        if crate::app::get_args().managed {
            log::warn!("Ignoring template launch plan in managed mode");
            return false;
        }
        if let Some((directory, git_remote)) = plan.local_workspace_preparation() {
            if let Err(error) = cterm_app::prepare_working_directory(directory, git_remote) {
                log::error!(
                    "Failed to prepare workspace for template '{}': {error}",
                    plan.template_name
                );
                return false;
            }
        }

        let config = self.config_with_live_shortcuts();
        let theme = template_theme(
            &config,
            &self.ivars().theme,
            plan.appearance.theme.as_deref(),
        );
        let opts = plan.session_options(80, 24);
        let remote = template_remote_details(&plan).map(|(name, host, ssh_compression)| {
            (
                remote_manager,
                name.to_string(),
                host.to_string(),
                ssh_compression,
            )
        });
        self.spawn_daemon_tab_with_theme(
            opts,
            Some(plan.template_name),
            plan.appearance.tab_color,
            plan.appearance.background_color,
            plan.keep_open,
            theme,
            remote,
            None,
        );
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_daemon_tab_with_theme(
        &self,
        mut opts: cterm_client::CreateSessionOpts,
        template_name: Option<String>,
        color: Option<String>,
        background_color: Option<String>,
        keep_open: bool,
        theme: Theme,
        remote: Option<(cterm_client::RemoteManager, String, String, bool)>,
        daemon_socket: Option<std::path::PathBuf>,
    ) {
        let config = self.config_with_live_shortcuts();
        ensure_session_pixel_size(&config, &mut opts);
        opts.base_palette = Some(terminal_palette(&theme, background_color.as_deref()));
        opts.frontend_state.appearance = theme.appearance();
        let launch_context = PaneLaunchContext::capture(&opts);
        let remote_name = remote.as_ref().map(|(_, name, _, _)| name.clone());
        let window = unsafe {
            Retained::retain(self as *const _ as *mut CtermWindow)
                .expect("a live tab owner can be retained")
        };
        let window = dispatch2::MainThreadBound::new(window, MainThreadMarker::from(self));

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();

            let result = match rt {
                Ok(rt) => rt.block_on(async {
                    let conn = if let Some((mgr, ref name, ref host, compress)) = remote {
                        mgr.get_or_connect(name, host, compress).await?
                    } else if let Some(ref path) = daemon_socket {
                        cterm_client::DaemonConnection::connect_unix(path, false).await?
                    } else {
                        cterm_client::DaemonConnection::connect_local().await?
                    };
                    let session = conn.create_session(opts).await?;
                    Ok::<_, cterm_client::ClientError>(session)
                }),
                Err(e) => Err(cterm_client::ClientError::Connection(e.to_string())),
            };

            match result {
                Ok(session) => {
                    dispatch2::Queue::main().exec_async(move || {
                        let mtm = unsafe { MainThreadMarker::new_unchecked() };
                        let window = window.into_inner(mtm);
                        if window.ivars().closed.get() {
                            destroy_unattached_session(session);
                            return;
                        }

                        let title = template_name
                            .clone()
                            .unwrap_or_else(|| "Terminal".to_string());

                        let new_window = CtermWindow::from_daemon(mtm, &config, &theme, session);
                        new_window.setTitle(&NSString::from_str(&title));

                        // Store template name and apply background color on the terminal view
                        if let Some(tv) = new_window.active_terminal() {
                            tv.set_pane_launch_context(launch_context);
                            tv.set_remote_name(remote_name);
                            tv.set_keep_open(keep_open);
                            if let Some(ref name) = template_name {
                                tv.set_current_title(title.clone());
                                tv.set_title_locked(true);
                                tv.set_template_name(Some(name.clone()));
                                tv.set_template_name_on_daemon(name);
                            }
                            if let Some(ref bg) = background_color {
                                tv.set_background_override(Some(bg));
                            }
                        }

                        let app = NSApplication::sharedApplication(mtm);
                        if let Some(delegate) = app.delegate() {
                            let _: () =
                                unsafe { msg_send![&*delegate, registerWindow: &*new_window] };
                        }

                        window.addTabbedWindow_ordered(
                            &new_window,
                            objc2_app_kit::NSWindowOrderingMode::Above,
                        );
                        new_window.makeKeyAndOrderFront(None);

                        if let Some(ref c) = color {
                            new_window.set_tab_color(Some(c));
                        }

                        log::info!("Created daemon tab: {}", title);
                    });
                }
                Err(e) => {
                    log::error!("Failed to create daemon session: {}", e);
                }
            }
        });
    }

    /// Close current tab
    pub fn close_current_tab(&self) {
        // With native tabbing, just close the window
        // macOS will handle showing the next tab
        // Use performClose to trigger windowShouldClose: delegate method
        self.performClose(None);
    }

    /// Get config reference
    pub fn config(&self) -> &Config {
        &self.ivars().config
    }

    /// Get theme reference
    pub fn theme(&self) -> &Theme {
        &self.ivars().theme
    }

    /// Get a reference to the active terminal view
    pub fn active_terminal(&self) -> Option<Retained<TerminalView>> {
        self.ivars().active_terminal.borrow().clone()
    }

    /// Return all terminal views in deterministic pane order.
    pub fn terminal_views(&self) -> Vec<Retained<TerminalView>> {
        let panes = self.ivars().panes.borrow();
        panes
            .layout()
            .pane_ids()
            .into_iter()
            .filter_map(|id| panes.get(id).map(|entry| entry.terminal.clone()))
            .collect()
    }

    /// Focus the pane carrying a reusable template, if this tab owns one.
    pub(crate) fn focus_template(&self, template_name: &str) -> bool {
        let Some(terminal) = self
            .terminal_views()
            .into_iter()
            .find(|terminal| terminal.template_name().as_deref() == Some(template_name))
        else {
            return false;
        };
        if let Some(pane_id) = self.pane_id_for_terminal(&terminal) {
            self.focus_pane(pane_id);
        }
        self.makeKeyAndOrderFront(None);
        true
    }

    /// Replace configured bindings for this window and every pane it owns.
    pub(crate) fn reload_shortcuts(&self, shortcuts: &ShortcutsConfig) -> usize {
        *self.ivars().shortcut_config.borrow_mut() = shortcuts.clone();
        *self.ivars().shortcuts.borrow_mut() = ShortcutManager::from_config(shortcuts);

        let terminals = self.terminal_views();
        for terminal in &terminals {
            terminal.reload_shortcuts(shortcuts);
        }
        terminals.len()
    }

    fn config_with_live_shortcuts(&self) -> Config {
        config_with_shortcuts(&self.ivars().config, &self.ivars().shortcut_config.borrow())
    }

    /// Keep daemon sessions alive when this frontend hands them to a new process.
    pub fn preserve_panes_for_upgrade(&self) {
        let completions = self
            .terminal_views()
            .iter()
            .filter_map(|terminal| terminal.prepare_session_for_handoff())
            .collect::<Vec<_>>();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for completion in completions {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match completion.recv_timeout(remaining) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!("Daemon detach before upgrade failed: {error}"),
                Err(error) => log::warn!("Timed out detaching daemon pane before upgrade: {error}"),
            }
        }
    }

    /// Collect the complete split topology and its sessions in layout preorder.
    pub fn pane_upgrade_state(&self) -> Option<(PaneLayout, Vec<PaneUpgradeState>)> {
        let panes = self.ivars().panes.borrow();
        if panes.is_empty() {
            return None;
        }
        let layout = panes.layout().clone();
        let records = layout
            .pane_ids()
            .into_iter()
            .map(|pane_id| {
                let terminal = &panes
                    .get(pane_id)
                    .expect("pane resources mirror the pane layout")
                    .terminal;
                let mut state = PaneUpgradeState::new(terminal.session_id());
                state.title = terminal.current_title();
                state.title_locked = terminal.is_title_locked();
                state.template_name = terminal.template_name();
                state.cwd = terminal.foreground_cwd();
                state.keep_open = terminal.keep_open();
                state.daemon_socket = terminal.daemon_socket();
                state.remote_name = terminal.remote_name();
                state.launch_context = terminal.pane_launch_context();
                state
            })
            .collect();
        Some((layout, records))
    }

    fn begin_close_check(&self, target: CloseTarget, terminals: Vec<Retained<TerminalView>>) {
        if self.ivars().close_query_in_progress.replace(true) {
            return;
        }

        let mut running = Vec::new();
        let mut daemon_queries = Vec::new();
        for terminal in terminals {
            if let Some(session_id) = terminal.session_id() {
                daemon_queries.push(DaemonProcessQuery {
                    session_id,
                    daemon_socket: terminal.daemon_socket(),
                    title: terminal.current_title(),
                });
                continue;
            }
            #[cfg(unix)]
            if terminal.has_foreground_process() {
                running.push(
                    terminal
                        .foreground_process_name()
                        .unwrap_or_else(|| "a process".to_string()),
                );
            }
        }

        let window = unsafe {
            Retained::retain(self as *const _ as *mut CtermWindow)
                .expect("a live close-check window can be retained")
        };
        let window = dispatch2::MainThreadBound::new(window, MainThreadMarker::from(self));
        std::thread::spawn(move || {
            if !daemon_queries.is_empty() {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(async {
                        for query in daemon_queries {
                            let connection = if let Some(path) = query.daemon_socket.as_ref() {
                                cterm_client::DaemonConnection::connect_unix(path, false).await
                            } else {
                                cterm_client::DaemonConnection::connect_local().await
                            };
                            let info = match connection {
                                Ok(connection) => connection.get_session(&query.session_id).await,
                                Err(error) => {
                                    log::warn!(
                                        "Could not query pane '{}' before close: {error}",
                                        query.title
                                    );
                                    running.push(format!(
                                        "{} (process status unavailable)",
                                        query.title
                                    ));
                                    continue;
                                }
                            };
                            match info {
                                Ok(info) if info.has_foreground_process => {
                                    running.push(if info.foreground_process_name.is_empty() {
                                        "a process".to_string()
                                    } else {
                                        info.foreground_process_name
                                    })
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    log::warn!(
                                        "Could not query pane '{}' before close: {error}",
                                        query.title
                                    );
                                    running.push(format!(
                                        "{} (process status unavailable)",
                                        query.title
                                    ));
                                }
                            }
                        }
                    }),
                    Err(error) => {
                        log::warn!("Could not create close-query runtime: {error}");
                        running.extend(
                            daemon_queries.into_iter().map(|query| {
                                format!("{} (process status unavailable)", query.title)
                            }),
                        );
                    }
                }
            }

            dispatch2::Queue::main().exec_async(move || {
                let mtm = unsafe { MainThreadMarker::new_unchecked() };
                let window = window.into_inner(mtm);
                window.finish_close_check(target, running);
            });
        });
    }

    fn finish_close_check(&self, target: CloseTarget, running: Vec<String>) {
        self.ivars().close_query_in_progress.set(false);
        if self.ivars().closed.get()
            || (!running.is_empty() && !self.show_close_confirmation(&running))
        {
            return;
        }
        match target {
            CloseTarget::Window => {
                self.ivars().close_approved.set(true);
                self.performClose(None);
            }
            CloseTarget::Pane(pane_id) => {
                if self.ivars().panes.borrow().get(pane_id).is_some() {
                    self.remove_pane(pane_id);
                }
            }
        }
    }

    /// Set the bell state for this window and update dock badge
    pub fn set_bell(&self, active: bool) {
        let was_active = self.ivars().has_active_bell.get();
        if active == was_active {
            return; // No change
        }
        self.ivars().has_active_bell.set(active);

        let mtm = MainThreadMarker::from(self);
        let app = NSApplication::sharedApplication(mtm);
        if let Some(delegate) = app.delegate() {
            // Cast to our AppDelegate type via raw pointer
            let delegate_ptr = Retained::as_ptr(&delegate) as *const crate::app::AppDelegate;
            let app_delegate: &crate::app::AppDelegate = unsafe { &*delegate_ptr };
            if active {
                app_delegate.increment_bell_count();
            } else {
                app_delegate.decrement_bell_count();
            }
        }
    }

    /// Check if this window has an active bell notification
    pub fn has_bell(&self) -> bool {
        self.ivars().has_active_bell.get()
    }

    /// Show the Quick Open overlay for template selection and tab switching
    pub fn show_quick_open(&self) {
        if crate::app::get_args().managed {
            log::warn!("Ignoring quick-open request in managed mode");
            return;
        }
        let mtm = MainThreadMarker::from(self);

        // Load templates
        let templates = cterm_app::config::load_sticky_tabs().unwrap_or_default();

        // Collect open tabs with custom names
        let open_tabs = self.collect_open_tabs();

        // Create the overlay if it doesn't exist
        if self.ivars().quick_open.borrow().is_none() {
            let width = self.frame().size.width;
            let overlay = QuickOpenOverlay::new(mtm, width, templates.clone());

            // Set up the callback to open the selected template
            let window_ptr = self as *const Self;
            overlay.set_on_select(move |template| unsafe {
                let window = &*window_ptr;
                window.open_template_tab(&template);
            });

            // Set up callback for switching to an open tab
            overlay.set_on_switch_tab(move |target_ptr| unsafe {
                let target_window = target_ptr as *const NSWindow;
                let _: () = msg_send![target_window, makeKeyAndOrderFront: std::ptr::null::<objc2::runtime::AnyObject>()];
            });

            overlay.set_open_tabs(open_tabs);

            // Add to window content view
            if let Some(content_view) = self.contentView() {
                unsafe {
                    content_view.addSubview(&overlay);
                }

                // Position at top of window
                let content_bounds = content_view.bounds();
                let overlay_frame = NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(content_bounds.size.width, QUICK_OPEN_HEIGHT),
                );
                unsafe {
                    let _: () = msg_send![&*overlay, setFrame: overlay_frame];
                }
            }

            *self.ivars().quick_open.borrow_mut() = Some(overlay);
        } else {
            // Update templates and open tabs in case they changed
            if let Some(ref overlay) = *self.ivars().quick_open.borrow() {
                overlay.set_templates_and_tabs(templates, open_tabs);
            }
        }

        // Show the overlay
        if let Some(ref overlay) = *self.ivars().quick_open.borrow() {
            overlay.show();
        }
    }

    /// Collect open tabs with custom names for Quick Open
    fn collect_open_tabs(&self) -> Vec<OpenTabEntry> {
        let mut entries = Vec::new();

        // Get all tabbed windows in this window group
        let tabbed_windows: Option<Retained<NSArray<NSWindow>>> =
            unsafe { msg_send![self, tabbedWindows] };

        if let Some(windows) = tabbed_windows {
            for window in windows.iter() {
                // Try to cast to CtermWindow and check for custom title
                let window_ptr = Retained::as_ptr(&window) as *const CtermWindow;
                let cterm_window: &CtermWindow = unsafe { &*window_ptr };

                if let Some(terminal_view) = cterm_window.active_terminal() {
                    if terminal_view.is_title_locked() {
                        let title = window.title().to_string();
                        if !title.is_empty() {
                            entries.push(OpenTabEntry {
                                name: title,
                                window_ptr: Retained::as_ptr(&window) as usize,
                            });
                        }
                    }
                }
            }
        }

        entries
    }

    /// Open a new tab from a template (daemon-backed via ctermd)
    fn open_template_tab(&self, template: &cterm_app::config::StickyTabConfig) {
        if crate::app::get_args().managed {
            log::warn!("Ignoring tab-template request in managed mode");
            return;
        }
        let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
        let Some(delegate) = app.delegate() else {
            log::error!("Cannot launch template without the Cocoa application delegate");
            return;
        };
        let delegate = unsafe { &*(Retained::as_ptr(&delegate) as *const crate::app::AppDelegate) };
        delegate.open_template_in_window(template, self);
    }

    /// Get the current tab color
    pub fn tab_color(&self) -> Option<String> {
        self.ivars().pending_tab_color.borrow().clone()
    }

    /// Set the tab color indicator for native macOS tabs
    ///
    /// Creates a small colored circle as the tab's accessory view.
    /// If the tab is not yet available, stores the color for later application.
    pub fn set_tab_color(&self, color: Option<&str>) {
        // Store the color for later if needed
        *self.ivars().pending_tab_color.borrow_mut() = color.map(|s| s.to_string());

        // Persist to daemon
        if let Some(ref tv) = *self.ivars().active_terminal.borrow() {
            tv.set_tab_color_on_daemon(color.unwrap_or(""));
        }

        unsafe {
            // Get the window's tab object
            let tab: *mut objc2::runtime::AnyObject = msg_send![self, tab];
            if tab.is_null() {
                log::debug!("No tab object available, stored for later");
                return;
            }

            self.apply_tab_color_to_tab(tab, color);
        }
    }

    /// Apply pending tab color if the tab is now available
    /// Returns true if color was applied, false if tab not yet available
    fn apply_pending_tab_color(&self) -> bool {
        let pending = self.ivars().pending_tab_color.borrow().clone();
        if pending.is_none() {
            return true; // Nothing to apply
        }

        unsafe {
            let tab: *mut objc2::runtime::AnyObject = msg_send![self, tab];
            if tab.is_null() {
                log::debug!("Tab not available yet for pending color");
                return false;
            }

            self.apply_tab_color_to_tab(tab, pending.as_deref());
            // Clear pending after successful application
            *self.ivars().pending_tab_color.borrow_mut() = None;
            log::debug!("Applied pending tab color: {:?}", pending);
            true
        }
    }

    /// Schedule a retry for applying tab color after a short delay
    fn schedule_tab_color_retry(&self) {
        unsafe {
            let _: () = msg_send![
                self,
                performSelector: objc2::sel!(retryTabColor),
                withObject: std::ptr::null::<objc2::runtime::AnyObject>(),
                afterDelay: 0.1f64
            ];
        }
    }

    /// Internal: Apply color to a tab object
    unsafe fn apply_tab_color_to_tab(
        &self,
        tab: *mut objc2::runtime::AnyObject,
        color: Option<&str>,
    ) {
        if let Some(hex) = color {
            // Parse hex color
            let hex = hex.trim_start_matches('#');
            if hex.len() == 6 {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                ) {
                    // Create a small colored circle view
                    let frame = NSRect::new(NSPoint::ZERO, NSSize::new(12.0, 12.0));
                    let view: *mut objc2::runtime::AnyObject =
                        msg_send![objc2::class!(NSView), alloc];
                    let view: *mut objc2::runtime::AnyObject =
                        msg_send![view, initWithFrame: frame];

                    // Enable layer-backing and set the background color
                    let _: () = msg_send![view, setWantsLayer: true];
                    let layer: *mut objc2::runtime::AnyObject = msg_send![view, layer];
                    if !layer.is_null() {
                        // Create NSColor from RGB
                        let ns_color: *mut objc2::runtime::AnyObject = msg_send![
                            objc2::class!(NSColor),
                            colorWithRed: (r as f64 / 255.0),
                            green: (g as f64 / 255.0),
                            blue: (b as f64 / 255.0),
                            alpha: 1.0f64
                        ];
                        let cg_color: *mut objc2::runtime::AnyObject = msg_send![ns_color, CGColor];
                        let _: () = msg_send![layer, setBackgroundColor: cg_color];
                        // Make it a circle
                        let _: () = msg_send![layer, setCornerRadius: 6.0f64];
                    }

                    // Add width and height constraints (required since translatesAutoresizingMaskIntoConstraints is false)
                    let width_constraint: *mut objc2::runtime::AnyObject = msg_send![
                        objc2::class!(NSLayoutConstraint),
                        constraintWithItem: view,
                        attribute: 7i64,  // NSLayoutAttributeWidth
                        relatedBy: 0i64,  // NSLayoutRelationEqual
                        toItem: std::ptr::null::<objc2::runtime::AnyObject>(),
                        attribute: 0i64,  // NSLayoutAttributeNotAnAttribute
                        multiplier: 1.0f64,
                        constant: 12.0f64
                    ];
                    let height_constraint: *mut objc2::runtime::AnyObject = msg_send![
                        objc2::class!(NSLayoutConstraint),
                        constraintWithItem: view,
                        attribute: 8i64,  // NSLayoutAttributeHeight
                        relatedBy: 0i64,  // NSLayoutRelationEqual
                        toItem: std::ptr::null::<objc2::runtime::AnyObject>(),
                        attribute: 0i64,  // NSLayoutAttributeNotAnAttribute
                        multiplier: 1.0f64,
                        constant: 12.0f64
                    ];
                    let _: () = msg_send![width_constraint, setActive: true];
                    let _: () = msg_send![height_constraint, setActive: true];

                    // Set as tab's accessory view
                    let _: () = msg_send![tab, setAccessoryView: view];
                    log::debug!("Set tab color to #{}", hex);
                }
            }
        } else {
            // Clear the accessory view
            let null_view: *mut objc2::runtime::AnyObject = std::ptr::null_mut();
            let _: () = msg_send![tab, setAccessoryView: null_view];
        }
    }

    /// Show a confirmation dialog when closing with a running process
    fn show_close_confirmation(&self, process_names: &[String]) -> bool {
        use objc2_app_kit::NSAlert;

        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);

        let message = if process_names.len() == 1 {
            format!("\"{}\" is still running", process_names[0])
        } else {
            format!("{} processes are still running", process_names.len())
        };
        alert.setMessageText(&NSString::from_str(&message));
        alert.setInformativeText(&NSString::from_str(
            "Closing this terminal will terminate the running process. Are you sure you want to close?",
        ));
        alert.setAlertStyle(NSAlertStyle::Warning);

        alert.addButtonWithTitle(&NSString::from_str("Close"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let response = alert.runModal();
        response == NSAlertFirstButtonReturn
    }

    fn show_split_context_error(&self) {
        use objc2_app_kit::NSAlert;

        let alert = NSAlert::new(MainThreadMarker::from(self));
        alert.setMessageText(&NSString::from_str("Cannot Split This Session"));
        alert.setInformativeText(&NSString::from_str(
            "cterm cannot safely reconstruct the local or SSH launch context of this attached session. Open a new tab instead; the existing session was left unchanged.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        alert.runModal();
    }

    // Window positioning methods

    /// Get the visible frame of the screen (excluding menu bar and dock)
    fn screen_visible_frame(&self) -> NSRect {
        if let Some(screen) = self.screen() {
            screen.visibleFrame()
        } else {
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(800.0, 600.0))
        }
    }

    /// Fill the screen (like maximize but respects menu bar and dock)
    fn position_fill(&self) {
        let frame = self.screen_visible_frame();
        self.setFrame_display(frame, true);
    }

    /// Center the window on screen
    fn position_center(&self) {
        self.center();
    }

    /// Position window to left half of screen
    fn position_left_half(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x, screen.origin.y),
            NSSize::new(screen.size.width / 2.0, screen.size.height),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to right half of screen
    fn position_right_half(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x + screen.size.width / 2.0, screen.origin.y),
            NSSize::new(screen.size.width / 2.0, screen.size.height),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to top half of screen
    fn position_top_half(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x, screen.origin.y + screen.size.height / 2.0),
            NSSize::new(screen.size.width, screen.size.height / 2.0),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to bottom half of screen
    fn position_bottom_half(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x, screen.origin.y),
            NSSize::new(screen.size.width, screen.size.height / 2.0),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to top-left quarter of screen
    fn position_top_left_quarter(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x, screen.origin.y + screen.size.height / 2.0),
            NSSize::new(screen.size.width / 2.0, screen.size.height / 2.0),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to top-right quarter of screen
    fn position_top_right_quarter(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(
                screen.origin.x + screen.size.width / 2.0,
                screen.origin.y + screen.size.height / 2.0,
            ),
            NSSize::new(screen.size.width / 2.0, screen.size.height / 2.0),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to bottom-left quarter of screen
    fn position_bottom_left_quarter(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x, screen.origin.y),
            NSSize::new(screen.size.width / 2.0, screen.size.height / 2.0),
        );
        self.setFrame_display(frame, true);
    }

    /// Position window to bottom-right quarter of screen
    fn position_bottom_right_quarter(&self) {
        let screen = self.screen_visible_frame();
        let frame = NSRect::new(
            NSPoint::new(screen.origin.x + screen.size.width / 2.0, screen.origin.y),
            NSSize::new(screen.size.width / 2.0, screen.size.height / 2.0),
        );
        self.setFrame_display(frame, true);
    }
}

fn terminal_palette(theme: &Theme, background: Option<&str>) -> cterm_core::ColorPalette {
    let mut palette = theme.colors.clone();
    palette.cursor = theme.cursor.color;
    if let Some(background) = background.and_then(cterm_core::Rgb::from_hex) {
        palette.background = background;
    }
    palette
}

#[cfg(test)]
mod action_dispatch_tests {
    use super::*;

    #[test]
    fn template_theme_override_ignores_the_global_custom_theme() {
        let config = Config {
            appearance: cterm_app::config::AppearanceConfig {
                custom_theme: Some(Theme::dark()),
                ..Default::default()
            },
            ..Default::default()
        };

        let resolved = template_theme(&config, &Theme::tokyo_night(), Some("Default Light"));
        let custom = template_theme(&config, &Theme::tokyo_night(), Some("custom"));
        let unknown = template_theme(&config, &Theme::tokyo_night(), Some("not-a-theme"));
        let fallback = template_theme(&config, &Theme::light(), None);

        assert_eq!(resolved.name, "Default Light");
        assert_eq!(custom.name, "Default Dark");
        assert_eq!(unknown.name, "Tokyo Night");
        assert_eq!(fallback.name, "Default Light");
    }

    #[test]
    fn configured_template_context_uses_native_ssh_from_shared_plan() {
        let mut config = Config::default();
        config.sticky_tabs.push(cterm_app::config::StickyTabConfig {
            name: "Production".into(),
            command: Some("ignored-for-native-ssh".into()),
            args: vec!["ignored".into()],
            ssh: Some(cterm_app::config::SshTabConfig {
                host: "shell.example.com".into(),
                port: Some(2222),
                username: Some("deploy".into()),
                ..Default::default()
            }),
            ..Default::default()
        });

        let context = configured_template_launch_context(&config, "Production").unwrap();

        assert!(context.shell.is_none());
        assert!(context.args.is_empty());
        let ssh = context.ssh.unwrap();
        assert_eq!(ssh.host, "shell.example.com");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.username.as_deref(), Some("deploy"));
    }

    #[test]
    fn configured_template_context_keeps_shared_docker_argv() {
        let mut config = Config::default();
        config.sticky_tabs.push(cterm_app::config::StickyTabConfig {
            name: "Container".into(),
            docker: Some(cterm_app::config::DockerTabConfig {
                mode: cterm_app::config::DockerMode::Run,
                image: Some("alpine:latest".into()),
                shell: Some("/bin/sh".into()),
                docker_args: vec!["--pull=never".into()],
                ..Default::default()
            }),
            ..Default::default()
        });

        let context = configured_template_launch_context(&config, "Container").unwrap();

        assert_eq!(context.shell.as_deref(), Some("docker"));
        assert_eq!(
            context.args,
            [
                "run",
                "-it",
                "--rm",
                "--pull=never",
                "alpine:latest",
                "/bin/sh"
            ]
        );
        assert!(context.ssh.is_none());
    }

    #[test]
    fn named_remote_template_keeps_daemon_intent_and_skips_local_preparation() {
        let mut config = Config::default();
        config.remotes.push(cterm_app::config::RemoteConfig {
            name: "builder".into(),
            host: "dev@build.example.com".into(),
            ssh_compression: false,
        });
        let template = cterm_app::config::StickyTabConfig {
            name: "Remote build".into(),
            command: Some("just".into()),
            args: vec!["test".into()],
            working_directory: Some("/srv/project".into()),
            remote: Some("builder".into()),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &config).unwrap();

        assert_eq!(
            template_remote_details(&plan),
            Some(("builder", "dev@build.example.com", false))
        );
        assert!(plan.local_workspace_preparation().is_none());
        let options = plan.session_options(80, 24);
        assert_eq!(options.cwd.as_deref(), Some("/srv/project"));
    }

    #[test]
    fn live_shortcuts_are_inherited_by_future_terminal_configs() {
        let config = Config::default();
        let original = config.shortcuts.new_tab.clone();
        let shortcuts = ShortcutsConfig {
            new_tab: "Ctrl+Alt+T".to_string(),
            ..ShortcutsConfig::default()
        };

        let merged = config_with_shortcuts(&config, &shortcuts);

        assert_eq!(merged.shortcuts.new_tab, "Ctrl+Alt+T");
        assert_eq!(config.shortcuts.new_tab, original);
    }

    #[test]
    fn managed_mode_restricts_secondary_topology_and_configuration_actions() {
        let restricted = [
            Action::NewTab,
            Action::SplitPane(SplitDirection::Horizontal),
            Action::SplitPane(SplitDirection::Vertical),
            Action::ClosePane,
            Action::FocusPane(PaneDirection::Left),
            Action::FocusPane(PaneDirection::Right),
            Action::FocusPane(PaneDirection::Up),
            Action::FocusPane(PaneDirection::Down),
            Action::ResizePane(PaneDirection::Left),
            Action::ResizePane(PaneDirection::Right),
            Action::ResizePane(PaneDirection::Up),
            Action::ResizePane(PaneDirection::Down),
            Action::TogglePaneZoom,
            Action::NewWindow,
            Action::OpenPreferences,
            Action::QuickOpenTemplate,
        ];

        for action in restricted {
            assert!(
                is_managed_restricted_action(&action),
                "expected {action:?} to be restricted"
            );
        }
    }

    #[test]
    fn managed_mode_keeps_session_safe_actions_available() {
        let allowed = [
            Action::CloseTab,
            Action::NextTab,
            Action::PrevTab,
            Action::NextAlertedTab,
            Action::Tab(1),
            Action::CloseWindow,
            Action::Copy,
            Action::Paste,
            Action::SelectAll,
            Action::ZoomIn,
            Action::ZoomOut,
            Action::ZoomReset,
            Action::ToggleFullscreen,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::ScrollPageUp,
            Action::ScrollPageDown,
            Action::ScrollToTop,
            Action::ScrollToBottom,
            Action::PromptPrevious,
            Action::PromptNext,
            Action::FindText,
            Action::ResetTerminal,
        ];

        for action in allowed {
            assert!(
                !is_managed_restricted_action(&action),
                "expected {action:?} to remain available"
            );
        }
    }
}
