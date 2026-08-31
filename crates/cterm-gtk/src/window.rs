//! Main window implementation

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Application, ApplicationWindow, Box as GtkBox, EventControllerKey, Notebook,
    Orientation, PopoverMenuBar,
};

use cterm_app::config::Config;
use cterm_app::file_transfer::PendingFileManager;
use cterm_app::shortcuts::ShortcutManager;
use cterm_app::{TemplateDaemonTarget, TemplateInstancePolicy, TemplateLaunchPlan};
use cterm_ui::events::{Action, KeyCode, Modifiers, Shortcut};
use cterm_ui::theme::Theme;
use cterm_ui::{
    PaneDirection, PaneId, PaneLayout, PaneTree, SplitDirection, SplitPlacement, SplitRatio,
    SplitRequest,
};

use crate::dialogs;
use crate::docker_dialog::{self, DockerSelection};
use crate::menu;
use crate::notification_bar::NotificationBar;
use crate::pane::PaneSet;
use crate::quick_open::QuickOpenOverlay;
use crate::tab_bar::TabBar;
use crate::terminal_widget::{frontend_palette, parse_rgb, CellDimensions, TerminalWidget};

/// One daemon-backed terminal session displayed in a pane.
struct PaneEntry {
    terminal: Rc<TerminalWidget>,
    title: String,
    title_locked: bool,
    template_name: Option<String>,
    keep_open: bool,
    session_id: Option<String>,
    daemon_socket: Option<std::path::PathBuf>,
    remote_name: Option<String>,
    /// Native SSH fallback for reconnect data that predates `launch_context`.
    native_ssh: Option<cterm_client::SshParams>,
    /// Stable process fields used to create sibling panes after UI handoff.
    launch_context: Option<cterm_app::upgrade::PaneLaunchContext>,
}

/// Tab entry tracking a pane tree and its stable tab ID.
struct TabEntry {
    id: u64,
    title: String,
    /// Cached active terminal for existing tab-oriented operations.
    terminal: Rc<TerminalWidget>,
    /// Whether title was explicitly set (locks out OSC updates)
    title_locked: bool,
    /// Tab color override
    color: Option<String>,
    /// Daemon session ID (for upgrade state preservation)
    session_id: Option<String>,
    /// Daemon socket path (None = local daemon, Some = remote/SSH-tunneled)
    daemon_socket: Option<std::path::PathBuf>,
    /// Configured remote name (the `RemoteManager` key) when the tab was opened
    /// via `spawn_daemon_tab` against a configured remote. Tabs with `Some(name)`
    /// can be torn down via the right-click "Disconnect" menu item, which kills
    /// the shared SSH tunnel and removes every tab with the same name.
    remote_name: Option<String>,
    pane_container: GtkBox,
    panes: PaneSet<PaneEntry>,
}

impl TabEntry {
    fn active_pane_id(&self) -> PaneId {
        self.panes.active_id()
    }

    fn activate_pane(&mut self, id: PaneId) -> bool {
        let rebuild = self.panes.is_zoomed();
        if self.panes.set_active(id).is_err() {
            return false;
        }
        self.sync_active_pane(rebuild);
        true
    }

    fn focus_pane(&mut self, direction: PaneDirection) -> Option<PaneId> {
        let rebuild = self.panes.is_zoomed();
        let id = self.panes.focus(direction)?;
        self.sync_active_pane(rebuild);
        Some(id)
    }

    fn sync_active_pane(&mut self, rebuild: bool) {
        let pane = self.panes.active();
        self.terminal = Rc::clone(&pane.terminal);
        self.session_id = pane.session_id.clone();
        self.daemon_socket = pane.daemon_socket.clone();
        self.remote_name = pane.remote_name.clone();
        if !self.title_locked && !pane.title_locked {
            self.title = pane.title.clone();
        }
        if rebuild {
            self.rebuild_panes();
        } else {
            self.panes
                .update_styles(|pane| pane.terminal.widget().clone().upcast());
        }
    }

    fn rebuild_panes(&self) {
        self.panes.rebuild(&self.pane_container, |pane| {
            pane.terminal.widget().clone().upcast()
        });
    }
}

fn report_window_visibility(tabs: &Rc<RefCell<Vec<TabEntry>>>, window_visible: bool) {
    for tab in tabs.borrow().iter() {
        for (_, pane) in tab.panes.iter() {
            let visibility = if window_visible && pane.terminal.widget().is_mapped() {
                cterm_core::WindowVisibility::Visible
            } else {
                cterm_core::WindowVisibility::Hidden
            };
            pane.terminal.set_window_visibility(visibility);
        }
    }
}

/// Main window container
pub struct CtermWindow {
    pub window: ApplicationWindow,
    pub notebook: Notebook,
    pub tab_bar: TabBar,
    pub config: Rc<RefCell<Config>>,
    pub theme: Theme,
    pub shortcuts: Rc<RefCell<ShortcutManager>>,
    tabs: Rc<RefCell<Vec<TabEntry>>>,
    next_tab_id: Rc<RefCell<u64>>,
    menu_bar: PopoverMenuBar,
    has_bell: Rc<RefCell<bool>>,
    notification_bar: NotificationBar,
    file_manager: Rc<RefCell<PendingFileManager>>,
    quick_open: QuickOpenOverlay,
    remote_manager: cterm_client::RemoteManager,
}

#[derive(Clone)]
struct PaneActionContext {
    notebook: Notebook,
    tabs: Rc<RefCell<Vec<TabEntry>>>,
    config: Rc<RefCell<Config>>,
    theme: Theme,
    tab_bar: TabBar,
    window: ApplicationWindow,
    has_bell: Rc<RefCell<bool>>,
    file_manager: Rc<RefCell<PendingFileManager>>,
    notification_bar: NotificationBar,
}

#[derive(Clone)]
struct TerminalViewActionContext {
    notebook: Notebook,
    tabs: Rc<RefCell<Vec<TabEntry>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GtkActionActivation {
    name: &'static str,
    parameter: Option<String>,
}

impl GtkActionActivation {
    fn simple(name: &'static str) -> Self {
        Self {
            name,
            parameter: None,
        }
    }

    fn with_string(name: &'static str, parameter: impl Into<String>) -> Self {
        Self {
            name,
            parameter: Some(parameter.into()),
        }
    }
}

/// Map a shared semantic action onto the canonical GTK window action.
///
/// Keep this match exhaustive: adding an `Action` must require an explicit GTK
/// dispatch decision instead of silently falling through to the terminal.
fn gtk_action_activation(action: &Action) -> GtkActionActivation {
    match action {
        Action::NewTab => GtkActionActivation::simple("new-tab"),
        Action::CloseTab => GtkActionActivation::simple("close-tab"),
        Action::NextTab => GtkActionActivation::simple("next-tab"),
        Action::PrevTab => GtkActionActivation::simple("prev-tab"),
        Action::NextAlertedTab => GtkActionActivation::simple("next-alerted-tab"),
        Action::Tab(tab) => GtkActionActivation::with_string("select-tab-index", tab.to_string()),
        Action::SplitPane(SplitDirection::Horizontal) => {
            GtkActionActivation::simple("split-pane-horizontal")
        }
        Action::SplitPane(SplitDirection::Vertical) => {
            GtkActionActivation::simple("split-pane-vertical")
        }
        Action::ClosePane => GtkActionActivation::simple("close-pane"),
        Action::FocusPane(PaneDirection::Left) => GtkActionActivation::simple("focus-pane-left"),
        Action::FocusPane(PaneDirection::Right) => GtkActionActivation::simple("focus-pane-right"),
        Action::FocusPane(PaneDirection::Up) => GtkActionActivation::simple("focus-pane-up"),
        Action::FocusPane(PaneDirection::Down) => GtkActionActivation::simple("focus-pane-down"),
        Action::ResizePane(PaneDirection::Left) => GtkActionActivation::simple("resize-pane-left"),
        Action::ResizePane(PaneDirection::Right) => {
            GtkActionActivation::simple("resize-pane-right")
        }
        Action::ResizePane(PaneDirection::Up) => GtkActionActivation::simple("resize-pane-up"),
        Action::ResizePane(PaneDirection::Down) => GtkActionActivation::simple("resize-pane-down"),
        Action::TogglePaneZoom => GtkActionActivation::simple("toggle-pane-zoom"),
        Action::NewWindow => GtkActionActivation::simple("new-window"),
        Action::CloseWindow => GtkActionActivation::simple("close-window"),
        Action::Copy => GtkActionActivation::simple("copy"),
        Action::Paste => GtkActionActivation::simple("paste"),
        Action::SelectAll => GtkActionActivation::simple("select-all"),
        Action::ZoomIn => GtkActionActivation::simple("zoom-in"),
        Action::ZoomOut => GtkActionActivation::simple("zoom-out"),
        Action::ZoomReset => GtkActionActivation::simple("zoom-reset"),
        Action::ToggleFullscreen => GtkActionActivation::simple("toggle-fullscreen"),
        Action::ScrollUp => GtkActionActivation::simple("scroll-up"),
        Action::ScrollDown => GtkActionActivation::simple("scroll-down"),
        Action::ScrollPageUp => GtkActionActivation::simple("scroll-page-up"),
        Action::ScrollPageDown => GtkActionActivation::simple("scroll-page-down"),
        Action::ScrollToTop => GtkActionActivation::simple("scroll-to-top"),
        Action::ScrollToBottom => GtkActionActivation::simple("scroll-to-bottom"),
        Action::PromptPrevious => GtkActionActivation::simple("prompt-previous"),
        Action::PromptNext => GtkActionActivation::simple("prompt-next"),
        Action::OpenPreferences => GtkActionActivation::simple("preferences"),
        Action::FindText => GtkActionActivation::simple("find"),
        Action::ResetTerminal => GtkActionActivation::simple("reset"),
        Action::QuickOpenTemplate => GtkActionActivation::simple("quick-open"),
    }
}

fn activate_shared_action(window: &ApplicationWindow, action: &Action) {
    let activation = gtk_action_activation(action);
    let parameter = activation
        .parameter
        .as_ref()
        .map(|value| glib::Variant::from(value.as_str()));

    if window.lookup_action(activation.name).is_none() {
        log::error!(
            "GTK action '{}' is not registered for shared action {:?}",
            activation.name,
            action
        );
        return;
    }

    gtk4::prelude::ActionGroupExt::activate_action(window, activation.name, parameter.as_ref());
}

fn shortcut_manager(config: &Config) -> ShortcutManager {
    let mut shortcuts = ShortcutManager::from_config(&config.shortcuts);
    // Fullscreen is not yet configurable in the shared config. Keep the
    // conventional desktop binding local to GTK until it becomes one.
    shortcuts.bind(
        Shortcut::new(KeyCode::F11, Modifiers::empty()),
        Action::ToggleFullscreen,
    );
    shortcuts
}

/// Show an error dialog when a seamless upgrade fails.
fn show_upgrade_error_dialog(window: &ApplicationWindow, error: &dyn std::fmt::Display) {
    let dialog = gtk4::MessageDialog::new(
        Some(window),
        gtk4::DialogFlags::MODAL,
        gtk4::MessageType::Error,
        gtk4::ButtonsType::Ok,
        format!("Upgrade failed: {}", error),
    );
    dialog.connect_response(|d, _| d.close());
    dialog.present();
}

fn reject_managed_secondary_action(action: &str) -> bool {
    if crate::get_args().managed {
        log::warn!("Ignoring {action} request in managed mode");
        true
    } else {
        false
    }
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

fn reject_managed_action(action: &Action) -> bool {
    if crate::get_args().managed && is_managed_restricted_action(action) {
        log::warn!("Ignoring {action:?} request in managed mode");
        true
    } else {
        false
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

fn theme_name_matches(requested: &str, resolved: &str) -> bool {
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

/// Resolve a template theme without changing the window-wide UI theme.
///
/// `resolve_theme` intentionally prefers a configured custom theme globally,
/// so a named per-template built-in must temporarily ignore that global custom
/// value. Unknown names stay on the window theme instead of silently becoming
/// Default Dark.
fn resolve_template_theme(config: &Config, window_theme: &Theme, requested: Option<&str>) -> Theme {
    let Some(requested) = requested.map(str::trim).filter(|name| !name.is_empty()) else {
        return window_theme.clone();
    };

    if let Some(custom) = config.appearance.custom_theme.as_ref() {
        if requested.eq_ignore_ascii_case("custom") || requested.eq_ignore_ascii_case(&custom.name)
        {
            return custom.clone();
        }
    }

    let mut themed_config = config.clone();
    themed_config.appearance.custom_theme = None;
    themed_config.appearance.theme = requested.to_string();
    let resolved = cterm_app::resolve_theme(&themed_config);
    if theme_name_matches(requested, &resolved.name) {
        resolved
    } else {
        log::warn!("Unknown template theme '{requested}', using the window theme");
        window_theme.clone()
    }
}

fn reusable_template_location<'a, P: Copy>(
    policy: TemplateInstancePolicy,
    template_name: &str,
    candidates: impl IntoIterator<Item = (usize, P, Option<&'a str>)>,
) -> Option<(usize, P)> {
    if policy != TemplateInstancePolicy::ReuseExisting {
        return None;
    }

    candidates
        .into_iter()
        .find(|(_, _, candidate)| candidate.is_some_and(|name| name == template_name))
        .map(|(tab_index, pane_id, _)| (tab_index, pane_id))
}

fn focus_reusable_template(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    plan: &TemplateLaunchPlan,
) -> bool {
    let location = {
        let tabs = tabs.borrow();
        reusable_template_location(
            plan.instance_policy,
            &plan.template_name,
            tabs.iter().enumerate().flat_map(|(tab_index, tab)| {
                tab.panes
                    .iter()
                    .map(move |(pane_id, pane)| (tab_index, pane_id, pane.template_name.as_deref()))
            }),
        )
    };
    let Some((tab_index, pane_id)) = location else {
        return false;
    };

    let tab_id = {
        let mut tabs = tabs.borrow_mut();
        let Some(tab) = tabs.get_mut(tab_index) else {
            return false;
        };
        if !tab.activate_pane(pane_id) {
            return false;
        }
        tab.id
    };

    notebook.set_current_page(Some(tab_index as u32));
    tab_bar.set_active(tab_id);
    focus_current_terminal(notebook, tabs);
    true
}

fn perform_pane_action(context: &PaneActionContext, action: Action) {
    match action {
        Action::SplitPane(direction) => spawn_daemon_pane(context, direction),
        Action::ClosePane => {
            let Some(page) = context.notebook.current_page() else {
                return;
            };
            let Some((tab_id, pane_id, pane_count)) = context
                .tabs
                .borrow()
                .get(page as usize)
                .map(|tab| (tab.id, tab.active_pane_id(), tab.panes.len()))
            else {
                return;
            };
            if pane_count == 1 {
                request_close_tab_by_id(
                    &context.notebook,
                    &context.tabs,
                    &context.tab_bar,
                    &context.window,
                    &context.config,
                    tab_id,
                );
            } else {
                request_close_pane_by_id(
                    &context.notebook,
                    &context.tabs,
                    &context.tab_bar,
                    &context.window,
                    &context.config,
                    tab_id,
                    pane_id,
                );
            }
        }
        Action::FocusPane(direction) => {
            let terminal = {
                let Some(page) = context.notebook.current_page() else {
                    return;
                };
                let mut tabs = context.tabs.borrow_mut();
                let Some(tab) = tabs.get_mut(page as usize) else {
                    return;
                };
                if tab.focus_pane(direction).is_none() {
                    return;
                }
                context.tab_bar.set_title(tab.id, &tab.title);
                context.window.set_title(Some(&tab.title));
                Rc::clone(&tab.terminal)
            };
            terminal.widget().grab_focus();
        }
        Action::ResizePane(direction) => {
            let terminal = {
                let Some(page) = context.notebook.current_page() else {
                    return;
                };
                let mut tabs = context.tabs.borrow_mut();
                let Some(tab) = tabs.get_mut(page as usize) else {
                    return;
                };
                if !tab.panes.resize(direction, 400) {
                    return;
                }
                tab.rebuild_panes();
                Rc::clone(&tab.terminal)
            };
            terminal.widget().grab_focus();
        }
        Action::TogglePaneZoom => {
            let terminal = {
                let Some(page) = context.notebook.current_page() else {
                    return;
                };
                let mut tabs = context.tabs.borrow_mut();
                let Some(tab) = tabs.get_mut(page as usize) else {
                    return;
                };
                tab.panes.toggle_zoom();
                tab.rebuild_panes();
                Rc::clone(&tab.terminal)
            };
            terminal.widget().grab_focus();
        }
        _ => {}
    }
}

fn register_pane_actions(window: &ApplicationWindow, context: &PaneActionContext) {
    for (name, action) in [
        (
            "split-pane-horizontal",
            Action::SplitPane(SplitDirection::Horizontal),
        ),
        (
            "split-pane-vertical",
            Action::SplitPane(SplitDirection::Vertical),
        ),
        ("close-pane", Action::ClosePane),
        ("focus-pane-left", Action::FocusPane(PaneDirection::Left)),
        ("focus-pane-right", Action::FocusPane(PaneDirection::Right)),
        ("focus-pane-up", Action::FocusPane(PaneDirection::Up)),
        ("focus-pane-down", Action::FocusPane(PaneDirection::Down)),
        ("resize-pane-left", Action::ResizePane(PaneDirection::Left)),
        (
            "resize-pane-right",
            Action::ResizePane(PaneDirection::Right),
        ),
        ("resize-pane-up", Action::ResizePane(PaneDirection::Up)),
        ("resize-pane-down", Action::ResizePane(PaneDirection::Down)),
        ("toggle-pane-zoom", Action::TogglePaneZoom),
    ] {
        let context = context.clone();
        let gtk_action = gio::SimpleAction::new(name, None);
        gtk_action.connect_activate(move |_, _| {
            if reject_managed_action(&action) {
                return;
            }
            perform_pane_action(&context, action.clone());
        });
        window.add_action(&gtk_action);
    }
}

fn perform_terminal_view_action(context: &TerminalViewActionContext, action: &Action) {
    let Some(page_idx) = context.notebook.current_page() else {
        return;
    };
    let tabs = context.tabs.borrow();
    let Some(tab) = tabs.get(page_idx as usize) else {
        return;
    };

    match action {
        Action::ZoomIn => tab.terminal.zoom_in(),
        Action::ZoomOut => tab.terminal.zoom_out(),
        Action::ZoomReset => tab.terminal.zoom_reset(),
        Action::ScrollUp => tab.terminal.scroll_viewport_up(1),
        Action::ScrollDown => tab.terminal.scroll_viewport_down(1),
        Action::ScrollPageUp => tab.terminal.scroll_viewport_page(true),
        Action::ScrollPageDown => tab.terminal.scroll_viewport_page(false),
        Action::ScrollToTop => tab.terminal.scroll_viewport_edge(true),
        Action::ScrollToBottom => tab.terminal.scroll_viewport_edge(false),
        Action::PromptPrevious => tab.terminal.scroll_to_shell_prompt(true),
        Action::PromptNext => tab.terminal.scroll_to_shell_prompt(false),
        _ => unreachable!("non-view action registered as a terminal view action"),
    }
}

fn register_terminal_view_actions(window: &ApplicationWindow, context: &TerminalViewActionContext) {
    for (name, action) in [
        ("zoom-in", Action::ZoomIn),
        ("zoom-out", Action::ZoomOut),
        ("zoom-reset", Action::ZoomReset),
        ("scroll-up", Action::ScrollUp),
        ("scroll-down", Action::ScrollDown),
        ("scroll-page-up", Action::ScrollPageUp),
        ("scroll-page-down", Action::ScrollPageDown),
        ("scroll-to-top", Action::ScrollToTop),
        ("scroll-to-bottom", Action::ScrollToBottom),
        ("prompt-previous", Action::PromptPrevious),
        ("prompt-next", Action::PromptNext),
    ] {
        let context = context.clone();
        let gtk_action = gio::SimpleAction::new(name, None);
        gtk_action.connect_activate(move |_, _| perform_terminal_view_action(&context, &action));
        window.add_action(&gtk_action);
    }
}

#[derive(Debug, Clone, Copy)]
enum PaneCiStep {
    WaitInitial,
    WaitHorizontalSplit,
    WaitVerticalSplit,
    Focus,
    Resize,
    Zoom,
    Unzoom,
    WaitClose,
    WaitTemplate,
    WaitUniqueReuse,
}

#[derive(Clone)]
struct PaneCiSnapshot {
    pane_count: usize,
    active: PaneId,
    layout: PaneLayout,
}

fn pane_ci_snapshot(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
) -> Option<PaneCiSnapshot> {
    let page = notebook.current_page()?;
    let tabs = tabs.borrow();
    let tab = tabs.get(page as usize)?;
    Some(PaneCiSnapshot {
        pane_count: tab.panes.len(),
        active: tab.active_pane_id(),
        layout: tab.panes.layout().clone(),
    })
}

struct TemplateCiSnapshot {
    tab_count: usize,
    tab_id: u64,
    session_id: Option<String>,
    template_name: Option<String>,
    keep_open: bool,
    color: Option<String>,
    screen_text: String,
}

fn template_ci_snapshot(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
) -> Option<TemplateCiSnapshot> {
    let page = notebook.current_page()?;
    let tabs = tabs.borrow();
    let tab = tabs.get(page as usize)?;
    let pane = tab.panes.active();
    let screen_text = pane.terminal.terminal().lock().screen().grid().text();
    Some(TemplateCiSnapshot {
        tab_count: tabs.len(),
        tab_id: tab.id,
        session_id: pane.session_id.clone(),
        template_name: pane.template_name.clone(),
        keep_open: pane.keep_open,
        color: tab.color.clone(),
        screen_text,
    })
}

fn pane_ci_activate(window: &ApplicationWindow, name: &str) -> Result<(), String> {
    let action = window
        .lookup_action(name)
        .ok_or_else(|| format!("window action '{name}' is not registered"))?;
    if !action.is_enabled() {
        return Err(format!("window action '{name}' is disabled"));
    }
    action.activate(None);
    Ok(())
}

fn pane_ci_marker(marker: &str) {
    log::info!("{marker}");
    eprintln!("{marker}");
}

fn pane_ci_fail(step: PaneCiStep, reason: impl std::fmt::Display) -> ! {
    let marker = format!("CTERM_PANE_CI FAIL step={step:?} reason={reason}");
    log::error!("{marker}");
    eprintln!("{marker}");
    std::process::exit(2);
}

impl CtermWindow {
    /// Create a new window
    pub fn new(app: &Application, config: &Config, theme: &Theme) -> Self {
        // Calculate cell dimensions for initial window sizing
        let cell_dims = calculate_initial_cell_dimensions(config);

        // Calculate window size for 80x24 terminal plus chrome (menu bar ~30px, tab bar ~24px)
        let chrome_height = 54; // Approximate height for menu bar + tab bar
        let default_width = (cell_dims.width * 80.0).ceil() as i32 + 20; // Add some padding
        let default_height = (cell_dims.height * 24.0).ceil() as i32 + chrome_height + 20;

        // Create the main window
        let window = ApplicationWindow::builder()
            .application(app)
            .title("cterm")
            .default_width(default_width)
            .default_height(default_height)
            .build();

        // Create the main container
        let main_box = GtkBox::new(Orientation::Vertical, 0);

        // Create menu bar
        let menu_model = menu::create_menu_model_with_options(
            config.general.show_debug_menu,
            crate::get_args().updater_enabled(),
            crate::get_args().managed,
        );
        let menu_bar = PopoverMenuBar::from_model(Some(&menu_model));
        main_box.append(&menu_bar);

        // Create tab bar
        let tab_bar = TabBar::new();
        tab_bar.set_new_tab_visible(!crate::get_args().managed);
        main_box.append(tab_bar.widget());

        // Create notification bar for file transfers (initially hidden)
        let notification_bar = NotificationBar::new();
        main_box.append(notification_bar.widget());

        // Create Quick Open overlay (initially hidden)
        let quick_open = QuickOpenOverlay::new();
        main_box.append(quick_open.widget());

        // Create notebook for terminal tabs (hidden tabs, we use custom tab bar)
        let notebook = Notebook::builder()
            .show_tabs(false)
            .show_border(false)
            .vexpand(true)
            .hexpand(true)
            .build();

        main_box.append(&notebook);

        window.set_child(Some(&main_box));

        // Create shortcut manager
        let shortcuts = Rc::new(RefCell::new(shortcut_manager(config)));

        let has_bell = Rc::new(RefCell::new(false));
        let file_manager = Rc::new(RefCell::new(PendingFileManager::new()));

        let cterm_window = Self {
            window: window.clone(),
            notebook: notebook.clone(),
            tab_bar,
            config: Rc::new(RefCell::new(config.clone())),
            theme: theme.clone(),
            shortcuts,
            tabs: Rc::new(RefCell::new(Vec::new())),
            next_tab_id: Rc::new(RefCell::new(0)),
            menu_bar,
            has_bell,
            notification_bar,
            file_manager,
            quick_open,
            remote_manager: cterm_client::RemoteManager::new(),
        };

        // Set up window actions
        cterm_window.setup_actions();

        // Set up Quick Open callback
        cterm_window.setup_quick_open();

        // Set up key event handling
        cterm_window.setup_key_handler();

        // Set up window focus handler to clear bell on focus
        cterm_window.setup_focus_handler();

        // Set up terminal focus restoration after menu interactions
        cterm_window.setup_terminal_focus_restore();

        // Set up notification bar callbacks for file transfers
        cterm_window.setup_notification_bar();

        // Create initial tab
        cterm_window.new_tab();

        // Initially hide tab bar (only one tab)
        cterm_window.tab_bar.update_visibility();

        // Set up tab bar callbacks
        cterm_window.setup_tab_bar_callbacks();

        // Update window title when switching tabs
        cterm_window.setup_tab_switch_handler();

        // Set up close request handler for process confirmation
        cterm_window.setup_close_request_handler();
        cterm_window.setup_visibility_handler();

        cterm_window
    }

    /// Create a new window without an initial tab.
    ///
    /// Used for daemon reconnection where tabs will be added from existing sessions.
    /// The caller must add at least one tab before presenting the window.
    pub fn new_empty(app: &Application, config: &Config, theme: &Theme) -> Self {
        Self::new_empty_with_remote_manager(app, config, theme, cterm_client::RemoteManager::new())
    }

    pub(crate) fn new_empty_with_remote_manager(
        app: &Application,
        config: &Config,
        theme: &Theme,
        remote_manager: cterm_client::RemoteManager,
    ) -> Self {
        // Calculate cell dimensions for initial window sizing
        let cell_dims = calculate_initial_cell_dimensions(config);

        // Calculate window size for 80x24 terminal plus chrome (menu bar ~30px, tab bar ~24px)
        let chrome_height = 54;
        let default_width = (cell_dims.width * 80.0).ceil() as i32 + 20;
        let default_height = (cell_dims.height * 24.0).ceil() as i32 + chrome_height + 20;

        let window = ApplicationWindow::builder()
            .application(app)
            .title("cterm")
            .default_width(default_width)
            .default_height(default_height)
            .build();

        let main_box = GtkBox::new(Orientation::Vertical, 0);

        let menu_model = menu::create_menu_model_with_options(
            config.general.show_debug_menu,
            crate::get_args().updater_enabled(),
            crate::get_args().managed,
        );
        let menu_bar = PopoverMenuBar::from_model(Some(&menu_model));
        main_box.append(&menu_bar);

        let tab_bar = TabBar::new();
        tab_bar.set_new_tab_visible(!crate::get_args().managed);
        main_box.append(tab_bar.widget());

        let notification_bar = NotificationBar::new();
        main_box.append(notification_bar.widget());

        let quick_open = QuickOpenOverlay::new();
        main_box.append(quick_open.widget());

        let notebook = Notebook::builder()
            .show_tabs(false)
            .show_border(false)
            .vexpand(true)
            .hexpand(true)
            .build();

        main_box.append(&notebook);
        window.set_child(Some(&main_box));

        let shortcuts = Rc::new(RefCell::new(shortcut_manager(config)));
        let has_bell = Rc::new(RefCell::new(false));
        let file_manager = Rc::new(RefCell::new(PendingFileManager::new()));

        let cterm_window = Self {
            window: window.clone(),
            notebook: notebook.clone(),
            tab_bar,
            config: Rc::new(RefCell::new(config.clone())),
            theme: theme.clone(),
            shortcuts,
            tabs: Rc::new(RefCell::new(Vec::new())),
            next_tab_id: Rc::new(RefCell::new(0)),
            menu_bar,
            has_bell,
            notification_bar,
            file_manager,
            quick_open,
            remote_manager,
        };

        cterm_window.setup_actions();
        cterm_window.setup_quick_open();
        cterm_window.setup_key_handler();
        cterm_window.setup_focus_handler();
        cterm_window.setup_terminal_focus_restore();
        cterm_window.setup_notification_bar();

        // No initial tab — caller will add tabs

        cterm_window.tab_bar.update_visibility();
        cterm_window.setup_tab_bar_callbacks();
        cterm_window.setup_tab_switch_handler();
        cterm_window.setup_close_request_handler();
        cterm_window.setup_visibility_handler();

        cterm_window
    }

    /// Create a window whose first daemon session has explicit process
    /// options (used by the command-line launch path).
    pub fn new_with_initial_session(
        app: &Application,
        config: &Config,
        theme: &Theme,
        opts: cterm_client::CreateSessionOpts,
        title: String,
        title_locked: bool,
    ) -> Self {
        let window = Self::new_empty(app, config, theme);
        spawn_daemon_tab(
            &window.notebook,
            &window.tabs,
            &window.next_tab_id,
            &window.config,
            &window.theme,
            &window.tab_bar,
            &window.window,
            &window.has_bell,
            &window.file_manager,
            &window.notification_bar,
            opts,
            title,
            None,
            None,
            None,
            false,
            title_locked,
            None,
            None,
        );
        window
    }

    /// Forward GTK map/minimize state to terminal applications that enabled
    /// foot's visibility-reporting extension.
    fn setup_visibility_handler(&self) {
        let tabs = Rc::clone(&self.tabs);
        self.window.connect_map(move |_| {
            report_window_visibility(&tabs, true);
        });

        let tabs = Rc::clone(&self.tabs);
        self.window.connect_unmap(move |_| {
            report_window_visibility(&tabs, false);
        });

        let tabs = Rc::clone(&self.tabs);
        self.window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                return;
            };
            let Ok(toplevel) = surface.dynamic_cast::<gdk::Toplevel>() else {
                return;
            };
            let notify_tabs = Rc::clone(&tabs);
            toplevel.connect_state_notify(move |toplevel| {
                let visible = !toplevel.state().contains(gdk::ToplevelState::MINIMIZED);
                report_window_visibility(&notify_tabs, visible);
            });
        });
    }

    /// Set up window actions for the menu
    fn setup_actions(&self) {
        let window = &self.window;
        let notebook = self.notebook.clone();
        let tabs = Rc::clone(&self.tabs);
        let next_tab_id = Rc::clone(&self.next_tab_id);
        let config = Rc::clone(&self.config);
        let theme = self.theme.clone();
        let tab_bar = self.tab_bar.clone();
        let has_bell = Rc::clone(&self.has_bell);
        let menu_bar = self.menu_bar.clone();
        let pane_context = PaneActionContext {
            notebook: notebook.clone(),
            tabs: Rc::clone(&tabs),
            config: Rc::clone(&config),
            theme: theme.clone(),
            tab_bar: tab_bar.clone(),
            window: window.clone(),
            has_bell: Rc::clone(&has_bell),
            file_manager: Rc::clone(&self.file_manager),
            notification_bar: self.notification_bar.clone(),
        };
        register_pane_actions(window, &pane_context);
        let terminal_view_context = TerminalViewActionContext {
            notebook: notebook.clone(),
            tabs: Rc::clone(&tabs),
        };
        register_terminal_view_actions(window, &terminal_view_context);

        // File menu actions
        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let next_tab_id = Rc::clone(&next_tab_id);
            let config = Rc::clone(&config);
            let theme = theme.clone();
            let tab_bar = tab_bar.clone();
            let window_clone = window.clone();
            let has_bell = Rc::clone(&has_bell);
            let file_manager = Rc::clone(&self.file_manager);
            let notification_bar = self.notification_bar.clone();
            let action = gio::SimpleAction::new("new-tab", None);
            action.connect_activate(move |_, _| {
                if reject_managed_action(&Action::NewTab) {
                    return;
                }
                // Get info from the active terminal
                let (cwd, daemon_socket) = {
                    let tabs_borrow = tabs.borrow();
                    if let Some(page_idx) = notebook.current_page() {
                        let entry = tabs_borrow.get(page_idx as usize);
                        #[cfg(unix)]
                        let cwd = entry.and_then(|e| e.terminal.foreground_cwd());
                        #[cfg(not(unix))]
                        let cwd: Option<String> = None;
                        let socket = entry.and_then(|e| e.daemon_socket.clone());
                        (cwd, socket)
                    } else {
                        (None, None)
                    }
                };

                create_new_tab(
                    &notebook,
                    &tabs,
                    &next_tab_id,
                    &config,
                    &theme,
                    &tab_bar,
                    &window_clone,
                    &has_bell,
                    &file_manager,
                    &notification_bar,
                    cwd,
                    daemon_socket,
                );
            });
            window.add_action(&action);
        }

        {
            let app = window.application().unwrap();
            let config = Rc::clone(&config);
            let theme = theme.clone();
            let action = gio::SimpleAction::new("new-window", None);
            action.connect_activate(move |_, _| {
                if reject_managed_action(&Action::NewWindow) {
                    return;
                }
                let cfg = config.borrow();
                if let Some(gtk_app) = app.downcast_ref::<Application>() {
                    let new_win = CtermWindow::new(gtk_app, &cfg, &theme);
                    new_win.present();
                }
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let window_clone = window.clone();
            let config = Rc::clone(&config);
            let action = gio::SimpleAction::new("close-tab", None);
            action.connect_activate(move |_, _| {
                close_current_tab(&notebook, &tabs, &tab_bar, &window_clone, &config);
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let window_clone = window.clone();
            let config = Rc::clone(&config);
            let action = gio::SimpleAction::new("close-other-tabs", None);
            action.connect_activate(move |_, _| {
                close_other_tabs(&notebook, &tabs, &tab_bar, &window_clone, &config);
            });
            window.add_action(&action);
        }

        {
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("quit", None);
            action.connect_activate(move |_, _| {
                window_clone.close();
            });
            window.add_action(&action);
        }

        {
            // `CloseWindow` is a window-scoped semantic action. Keep it
            // separate from the menu's historical "quit" action so it cannot
            // accidentally acquire application-wide semantics later.
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("close-window", None);
            action.connect_activate(move |_, _| {
                window_clone.close();
            });
            window.add_action(&action);
        }

        // Quick Open Template action
        {
            let quick_open = self.quick_open.clone();
            let action = gio::SimpleAction::new("quick-open", None);
            action.connect_activate(move |_, _| {
                if reject_managed_action(&Action::QuickOpenTemplate) {
                    return;
                }
                // Load templates and show overlay
                let templates = cterm_app::config::load_sticky_tabs().unwrap_or_default();
                quick_open.set_templates(templates);
                quick_open.show();
            });
            window.add_action(&action);
        }

        // Docker picker action
        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let next_tab_id = Rc::clone(&next_tab_id);
            let config = Rc::clone(&config);
            let theme = theme.clone();
            let tab_bar = tab_bar.clone();
            let window_clone = window.clone();
            let has_bell = Rc::clone(&has_bell);
            let file_manager = Rc::clone(&self.file_manager);
            let notification_bar = self.notification_bar.clone();
            let action = gio::SimpleAction::new("docker-picker", None);
            action.connect_activate(move |_, _| {
                if reject_managed_secondary_action("Docker terminal") {
                    return;
                }
                let notebook = notebook.clone();
                let tabs = Rc::clone(&tabs);
                let next_tab_id = Rc::clone(&next_tab_id);
                let config = Rc::clone(&config);
                let theme = theme.clone();
                let tab_bar = tab_bar.clone();
                let window_inner = window_clone.clone();
                let has_bell = Rc::clone(&has_bell);
                let file_manager = Rc::clone(&file_manager);
                let notification_bar = notification_bar.clone();

                docker_dialog::show_docker_picker(&window_clone, move |selection| {
                    let (command, args, title) = match &selection {
                        DockerSelection::ExecContainer(c) => {
                            let (cmd, args) = cterm_app::docker::build_exec_command(&c.name, None);
                            (cmd, args, format!("Docker: {}", c.name))
                        }
                        DockerSelection::RunImage(i) => {
                            let (cmd, args) = cterm_app::docker::build_run_command(
                                &format!("{}:{}", i.repository, i.tag),
                                None,
                                true,
                                &[],
                            );
                            (cmd, args, format!("Docker: {}:{}", i.repository, i.tag))
                        }
                    };

                    create_docker_tab(
                        &notebook,
                        &tabs,
                        &next_tab_id,
                        &config,
                        &theme,
                        &tab_bar,
                        &window_inner,
                        &has_bell,
                        &file_manager,
                        &notification_bar,
                        &command,
                        &args,
                        &title,
                    );
                });
            });
            window.add_action(&action);
        }

        // Session actions (daemon attach)
        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let next_tab_id = Rc::clone(&next_tab_id);
            let config = Rc::clone(&config);
            let theme = theme.clone();
            let tab_bar = tab_bar.clone();
            let window_clone = window.clone();
            let has_bell = Rc::clone(&has_bell);
            let file_manager = Rc::clone(&self.file_manager);
            let notification_bar = self.notification_bar.clone();
            let action = gio::SimpleAction::new("attach-session", None);
            action.connect_activate(move |_, _| {
                if reject_managed_secondary_action("session attach") {
                    return;
                }
                let notebook = notebook.clone();
                let tabs = Rc::clone(&tabs);
                let next_tab_id = Rc::clone(&next_tab_id);
                let config = Rc::clone(&config);
                let theme = theme.clone();
                let tab_bar = tab_bar.clone();
                let window_inner = window_clone.clone();
                let has_bell = Rc::clone(&has_bell);
                let file_manager = Rc::clone(&file_manager);
                let notification_bar = notification_bar.clone();

                crate::session_dialog::show_session_picker(&window_clone, move |session_id| {
                    create_daemon_tab(
                        &notebook,
                        &tabs,
                        &next_tab_id,
                        &config,
                        &theme,
                        &tab_bar,
                        &window_inner,
                        &has_bell,
                        &file_manager,
                        &notification_bar,
                        &session_id,
                    );
                });
            });
            window.add_action(&action);
        }

        // SSH connect action
        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let next_tab_id = Rc::clone(&next_tab_id);
            let config = Rc::clone(&config);
            let theme = theme.clone();
            let tab_bar = tab_bar.clone();
            let window_clone = window.clone();
            let has_bell = Rc::clone(&has_bell);
            let file_manager = Rc::clone(&self.file_manager);
            let notification_bar = self.notification_bar.clone();
            let action = gio::SimpleAction::new("ssh-connect", None);
            action.connect_activate(move |_, _| {
                if reject_managed_secondary_action("SSH session") {
                    return;
                }
                let notebook = notebook.clone();
                let tabs = Rc::clone(&tabs);
                let next_tab_id = Rc::clone(&next_tab_id);
                let config = Rc::clone(&config);
                let theme = theme.clone();
                let tab_bar = tab_bar.clone();
                let window_inner = window_clone.clone();
                let has_bell = Rc::clone(&has_bell);
                let file_manager = Rc::clone(&file_manager);
                let notification_bar = notification_bar.clone();

                let cursor = {
                    let config = config.borrow();
                    cterm_client::CursorDefaults {
                        style: config.appearance.cursor_style.core_style(),
                        blink: config.appearance.cursor_blink,
                    }
                };
                crate::session_dialog::show_ssh_dialog(&window_clone, cursor, move |sessions| {
                    for recon in sessions {
                        let sid = recon.handle.session_id().to_string();
                        let daemon_socket = recon.handle.socket_path().map(|p| p.to_owned());
                        let (title, title_locked) = if !recon.custom_title.is_empty() {
                            (recon.custom_title.clone(), true)
                        } else if !recon.title.is_empty() {
                            (recon.title.clone(), false)
                        } else {
                            ("SSH".to_string(), false)
                        };

                        let tab_color = if recon.tab_color.is_empty() {
                            None
                        } else {
                            Some(recon.tab_color.clone())
                        };

                        let cfg = config.borrow();
                        let terminal = TerminalWidget::from_daemon_with_screen(recon, &cfg, &theme);
                        drop(cfg);

                        let tab_id = generate_tab_id(&next_tab_id);
                        tab_bar.add_tab(tab_id, &title);

                        setup_tab_callbacks(
                            &notebook,
                            &tabs,
                            &config,
                            &tab_bar,
                            &window_inner,
                            &has_bell,
                            &file_manager,
                            &notification_bar,
                            &terminal,
                            tab_id,
                            PaneLayout::new().active(),
                            false,
                        );

                        finalize_new_tab(
                            &notebook,
                            &tabs,
                            &tab_bar,
                            tab_id,
                            title,
                            terminal,
                            title_locked,
                            false,
                            Some(sid),
                            daemon_socket,
                            None,
                            None,
                            None,
                            None,
                        );

                        if let Some(ref color) = tab_color {
                            tab_bar.set_color(tab_id, Some(color));
                            if let Some(tab) = tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id)
                            {
                                tab.color = tab_color;
                            }
                        }
                    }
                });
            });
            window.add_action(&action);
        }

        // Manage Remotes
        {
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("manage-remotes", None);
            action.connect_activate(move |_, _| {
                if reject_managed_secondary_action("remote management") {
                    return;
                }
                crate::remotes_dialog::show_remotes_dialog(&window_clone, || {
                    log::info!("Remotes configuration saved");
                });
            });
            window.add_action(&action);
        }

        // URL actions (for hyperlink context menu)
        {
            let action =
                gio::SimpleAction::new("open-url", Some(glib::VariantTy::new("s").unwrap()));
            action.connect_activate(|_, param| {
                if let Some(url) = param.and_then(|v| v.get::<String>()) {
                    if let Err(e) = open::that(&url) {
                        log::error!("Failed to open URL: {}", e);
                    }
                }
            });
            window.add_action(&action);

            let action =
                gio::SimpleAction::new("copy-url", Some(glib::VariantTy::new("s").unwrap()));
            action.connect_activate(|_, param| {
                if let Some(url) = param.and_then(|v| v.get::<String>()) {
                    if let Some(display) = gdk::Display::default() {
                        let clipboard = display.clipboard();
                        clipboard.set_text(&url);
                    }
                }
            });
            window.add_action(&action);
        }

        // Edit menu actions
        {
            // Copy selection to clipboard
            let notebook_copy = notebook.clone();
            let tabs_copy = Rc::clone(&tabs);
            let action = gio::SimpleAction::new("copy", None);
            action.connect_activate(move |_, _| {
                if let Some(page_idx) = notebook_copy.current_page() {
                    let tabs = tabs_copy.borrow();
                    if let Some(tab) = tabs.get(page_idx as usize) {
                        tab.terminal.copy_selection();
                    }
                }
            });
            window.add_action(&action);
        }

        {
            // Copy as HTML
            let notebook_copy_html = notebook.clone();
            let tabs_copy_html = Rc::clone(&tabs);
            let action = gio::SimpleAction::new("copy-html", None);
            action.connect_activate(move |_, _| {
                if let Some(page_idx) = notebook_copy_html.current_page() {
                    let tabs = tabs_copy_html.borrow();
                    if let Some(tab) = tabs.get(page_idx as usize) {
                        tab.terminal.copy_selection_html();
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let action = gio::SimpleAction::new("paste", None);
            action.connect_activate(move |_, _| {
                if let Some(display) = gdk::Display::default() {
                    let clipboard = display.clipboard();
                    let tabs_paste = Rc::clone(&tabs);
                    let notebook_paste = notebook.clone();
                    clipboard.read_text_async(None::<&gio::Cancellable>, move |result| {
                        if let Ok(Some(text)) = result {
                            if let Some(page_idx) = notebook_paste.current_page() {
                                let tabs = tabs_paste.borrow();
                                if let Some(tab) = tabs.get(page_idx as usize) {
                                    tab.terminal.write_str(&text);
                                }
                            }
                        }
                    });
                }
            });
            window.add_action(&action);
        }

        {
            // Select All
            let notebook_select = notebook.clone();
            let tabs_select = Rc::clone(&tabs);
            let action = gio::SimpleAction::new("select-all", None);
            action.connect_activate(move |_, _| {
                if let Some(page_idx) = notebook_select.current_page() {
                    let tabs = tabs_select.borrow();
                    if let Some(tab) = tabs.get(page_idx as usize) {
                        tab.terminal.select_all();
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("toggle-fullscreen", None);
            action.connect_activate(move |_, _| {
                if window_clone.is_fullscreen() {
                    window_clone.unfullscreen();
                } else {
                    window_clone.fullscreen();
                }
            });
            window.add_action(&action);
        }

        // Terminal menu actions
        {
            let window_clone = window.clone();
            let tabs = Rc::clone(&tabs);
            let notebook = notebook.clone();
            let tab_bar = tab_bar.clone();
            let action = gio::SimpleAction::new("set-title", None);
            action.connect_activate(move |_, _| {
                // Find the tab_id for the current page
                let tab_id = notebook
                    .current_page()
                    .and_then(|idx| tabs.borrow().get(idx as usize).map(|t| t.id));
                if let Some(tab_id) = tab_id {
                    show_rename_tab_dialog(&window_clone, &tabs, &tab_bar, tab_id);
                }
            });
            window.add_action(&action);
        }

        {
            let window_clone = window.clone();
            let tabs = Rc::clone(&tabs);
            let notebook = notebook.clone();
            let tab_bar = tab_bar.clone();
            let action = gio::SimpleAction::new("set-color", None);
            action.connect_activate(move |_, _| {
                let tabs_clone = Rc::clone(&tabs);
                let notebook_clone = notebook.clone();
                let tab_bar_clone = tab_bar.clone();
                dialogs::show_set_color_dialog(&window_clone, move |color| {
                    if let Some(page_idx) = notebook_clone.current_page() {
                        let mut tabs = tabs_clone.borrow_mut();
                        if let Some(tab) = tabs.get_mut(page_idx as usize) {
                            tab_bar_clone.set_color(tab.id, color.as_deref());
                            tab.terminal
                                .set_tab_color_on_daemon(color.as_deref().unwrap_or(""));
                            tab.color = color;
                        }
                    }
                });
            });
            window.add_action(&action);
        }

        {
            let window_clone = window.clone();
            let tabs = Rc::clone(&tabs);
            let notebook = notebook.clone();
            let action = gio::SimpleAction::new("find", None);
            action.connect_activate(move |_, _| {
                let tabs = Rc::clone(&tabs);
                let notebook = notebook.clone();
                dialogs::show_find_dialog(&window_clone, move |text, case_sensitive, regex| {
                    log::info!("Find: '{}' case={} regex={}", text, case_sensitive, regex);
                    if let Some(page_idx) = notebook.current_page() {
                        let tabs = tabs.borrow();
                        if let Some(tab) = tabs.get(page_idx as usize) {
                            let count = tab.terminal.find(&text, case_sensitive, regex);
                            log::info!("Found {} matches", count);
                        }
                    }
                });
            });
            window.add_action(&action);
        }

        {
            let action =
                gio::SimpleAction::new("set-encoding", Some(&glib::VariantType::new("s").unwrap()));
            action.connect_activate(|_, param| {
                if let Some(encoding) = param.and_then(|p| p.get::<String>()) {
                    if encoding == "utf8" {
                        log::info!("Encoding set to UTF-8");
                    } else {
                        // Terminal currently only supports UTF-8
                        log::warn!(
                            "Encoding '{}' requested but only UTF-8 is currently supported",
                            encoding
                        );
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let tabs = Rc::clone(&tabs);
            let notebook = notebook.clone();
            let action =
                gio::SimpleAction::new("send-signal", Some(&glib::VariantType::new("s").unwrap()));
            action.connect_activate(move |_, param| {
                if let Some(signal_str) = param.and_then(|p| p.get::<String>()) {
                    if let Ok(signal) = signal_str.parse::<i32>() {
                        if let Some(page_idx) = notebook.current_page() {
                            let tabs = tabs.borrow();
                            if let Some(tab) = tabs.get(page_idx as usize) {
                                log::info!("Sending signal {} to terminal", signal);
                                tab.terminal.send_signal(signal);
                            }
                        }
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let tabs = Rc::clone(&tabs);
            let notebook = notebook.clone();
            let action = gio::SimpleAction::new("reset", None);
            action.connect_activate(move |_, _| {
                if let Some(page_idx) = notebook.current_page() {
                    let tabs = tabs.borrow();
                    if let Some(tab) = tabs.get(page_idx as usize) {
                        tab.terminal.reset();
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let tabs = Rc::clone(&tabs);
            let notebook = notebook.clone();
            let action = gio::SimpleAction::new("clear-reset", None);
            action.connect_activate(move |_, _| {
                if let Some(page_idx) = notebook.current_page() {
                    let tabs = tabs.borrow();
                    if let Some(tab) = tabs.get(page_idx as usize) {
                        tab.terminal.clear_scrollback_and_reset();
                    }
                }
            });
            window.add_action(&action);
        }

        // Tabs menu actions
        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let action = gio::SimpleAction::new("prev-tab", None);
            action.connect_activate(move |_, _| {
                let n = notebook.n_pages();
                if n > 0 {
                    let current = notebook.current_page().unwrap_or(0);
                    let prev = if current == 0 { n - 1 } else { current - 1 };
                    notebook.set_current_page(Some(prev));
                    sync_tab_bar_active(&tab_bar, &tabs, &notebook);
                    focus_current_terminal(&notebook, &tabs);
                }
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let action = gio::SimpleAction::new(
                "select-tab-index",
                Some(&glib::VariantType::new("s").unwrap()),
            );
            action.connect_activate(move |_, param| {
                let Some(tab_number) = param
                    .and_then(|value| value.get::<String>())
                    .and_then(|value| value.parse::<u32>().ok())
                else {
                    return;
                };
                let page = tab_number.saturating_sub(1);
                if page < notebook.n_pages() {
                    notebook.set_current_page(Some(page));
                    sync_tab_bar_active(&tab_bar, &tabs, &notebook);
                    focus_current_terminal(&notebook, &tabs);
                }
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let action = gio::SimpleAction::new("next-tab", None);
            action.connect_activate(move |_, _| {
                let n = notebook.n_pages();
                if n > 0 {
                    let current = notebook.current_page().unwrap_or(0);
                    notebook.set_current_page(Some((current + 1) % n));
                    sync_tab_bar_active(&tab_bar, &tabs, &notebook);
                    focus_current_terminal(&notebook, &tabs);
                }
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let action = gio::SimpleAction::new("next-alerted-tab", None);
            action.connect_activate(move |_, _| {
                let n = notebook.n_pages();
                if n > 0 {
                    let current = notebook.current_page().unwrap_or(0) as usize;
                    let tabs_ref = tabs.borrow();
                    for offset in 1..tabs_ref.len() {
                        let idx = (current + offset) % tabs_ref.len();
                        if let Some(entry) = tabs_ref.get(idx) {
                            if tab_bar.has_bell(entry.id) {
                                drop(tabs_ref);
                                notebook.set_current_page(Some(idx as u32));
                                sync_tab_bar_active(&tab_bar, &tabs, &notebook);
                                focus_current_terminal(&notebook, &tabs);
                                return;
                            }
                        }
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let tab_bar = tab_bar.clone();
            let action =
                gio::SimpleAction::new("switch-tab", Some(&glib::VariantType::new("s").unwrap()));
            action.connect_activate(move |_, param| {
                if let Some(id_str) = param.and_then(|p| p.get::<String>()) {
                    if let Ok(id) = id_str.parse::<u64>() {
                        let tabs_ref = tabs.borrow();
                        if let Some(idx) = tabs_ref.iter().position(|t| t.id == id) {
                            notebook.set_current_page(Some(idx as u32));
                            drop(tabs_ref);
                            sync_tab_bar_active(&tab_bar, &tabs, &notebook);
                        }
                    }
                }
            });
            window.add_action(&action);
        }

        // Tools menu actions
        {
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let window_clone = window.clone();
            let action = gio::SimpleAction::new(
                "run-tool-shortcut",
                Some(&glib::VariantType::new("s").unwrap()),
            );
            action.connect_activate(move |_, param| {
                if reject_managed_secondary_action("tool shortcut") {
                    return;
                }
                if let Some(idx_str) = param.and_then(|p| p.get::<String>()) {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if let Ok(shortcuts) = cterm_app::config::load_tool_shortcuts() {
                            if let Some(shortcut) = shortcuts.get(idx) {
                                // Get CWD from active terminal
                                #[cfg(unix)]
                                let cwd = {
                                    let tabs_borrow = tabs.borrow();
                                    if let Some(page_idx) = notebook.current_page() {
                                        tabs_borrow
                                            .get(page_idx as usize)
                                            .and_then(|entry| entry.terminal.foreground_cwd())
                                    } else {
                                        None
                                    }
                                };
                                #[cfg(not(unix))]
                                let cwd: Option<String> = None;

                                let cwd = cwd.unwrap_or_else(|| {
                                    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
                                });

                                if let Err(e) = shortcut.execute(std::path::Path::new(&cwd)) {
                                    let dialog = gtk4::MessageDialog::new(
                                        Some(&window_clone),
                                        gtk4::DialogFlags::MODAL,
                                        gtk4::MessageType::Error,
                                        gtk4::ButtonsType::Ok,
                                        format!(
                                            "Failed to launch \"{}\"\n\nCommand '{}' failed: {}",
                                            shortcut.name, shortcut.command, e
                                        ),
                                    );
                                    dialog.connect_response(|d, _| d.close());
                                    dialog.present();
                                }
                            }
                        }
                    }
                }
            });
            window.add_action(&action);
        }

        // Help menu actions
        {
            let window_clone = window.clone();
            let config = Rc::clone(&config);
            let shortcuts = Rc::clone(&self.shortcuts);
            let menu_bar_clone = menu_bar.clone();
            let action = gio::SimpleAction::new("preferences", None);
            action.connect_activate(move |_, _| {
                if reject_managed_action(&Action::OpenPreferences) {
                    return;
                }
                let cfg = config.borrow().clone();
                let config_for_save = Rc::clone(&config);
                let shortcuts_for_save = Rc::clone(&shortcuts);
                let menu_bar = menu_bar_clone.clone();
                dialogs::show_preferences_dialog(&window_clone, &cfg, move |new_config| {
                    log::info!("Preferences saved");
                    // Save to disk
                    if let Err(e) = cterm_app::config::save_config(&new_config) {
                        log::error!("Failed to save config: {}", e);
                    } else {
                        log::info!("Configuration saved to disk");
                    }
                    // Rebuild menu bar to reflect debug menu preference
                    menu::rebuild_menu_bar(
                        &menu_bar,
                        new_config.general.show_debug_menu,
                        crate::get_args().updater_enabled(),
                        crate::get_args().managed,
                    );
                    // The key controller borrows this shared manager for every
                    // event, so shortcut edits take effect immediately.
                    *shortcuts_for_save.borrow_mut() = shortcut_manager(&new_config);
                    // Update internal config state
                    *config_for_save.borrow_mut() = new_config;
                });
            });
            window.add_action(&action);
        }

        // Check for updates action
        if crate::get_args().updater_enabled() {
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("check-updates", None);
            action.connect_activate(move |_, _| {
                crate::update_dialog::show_update_dialog(&window_clone);
            });
            window.add_action(&action);
        }

        // Execute upgrade action (called from update dialog)
        if crate::get_args().updater_enabled() {
            let tabs = Rc::clone(&tabs);
            let window_clone = window.clone();
            let notebook_upgrade = notebook.clone();
            let action = gio::SimpleAction::new(
                "execute-upgrade",
                Some(&glib::VariantType::new("s").unwrap()),
            );
            action.connect_activate(move |_, param| {
                if let Some(binary_path) = param.and_then(|p| p.get::<String>()) {
                    log::info!("Executing seamless upgrade with binary: {}", binary_path);

                    // Collect upgrade state from current window
                    let mut tabs_borrowed = tabs.borrow_mut();

                    // Build upgrade state
                    let mut upgrade_state = cterm_app::upgrade::UpgradeState::new();

                    // Collect window state
                    let mut window_state = cterm_app::upgrade::WindowUpgradeState::new();
                    window_state.width = window_clone.width();
                    window_state.height = window_clone.height();
                    window_state.maximized = window_clone.is_maximized();
                    window_state.fullscreen = window_clone.is_fullscreen();

                    for tab in tabs_borrowed.iter_mut() {
                        tab.panes.flush_divider_ratios();
                        let mut tab_state = cterm_app::upgrade::TabUpgradeState::new(tab.id);
                        tab_state.title = tab.title.clone();
                        if tab.title_locked {
                            tab_state.custom_title = Some(tab.title.clone());
                        }
                        tab_state.color = tab.color.clone();
                        tab_state.session_id = tab.session_id.clone();

                        // Get working directory from terminal
                        #[cfg(unix)]
                        {
                            let term = tab.terminal.terminal().lock();
                            tab_state.cwd = term
                                .foreground_cwd()
                                .map(|p| p.to_string_lossy().into_owned());
                        }

                        tab_state.pane_layout = Some(tab.panes.layout().clone());
                        tab_state.panes = tab
                            .panes
                            .pane_ids()
                            .into_iter()
                            .map(|pane_id| {
                                let pane = tab
                                    .panes
                                    .get(pane_id)
                                    .expect("pane resources mirror the pane layout");
                                let mut pane_state = cterm_app::upgrade::PaneUpgradeState::new(
                                    pane.session_id.clone(),
                                );
                                pane_state.title = pane.title.clone();
                                pane_state.title_locked = pane.title_locked;
                                pane_state.template_name = pane.template_name.clone();
                                #[cfg(unix)]
                                {
                                    pane_state.cwd = pane.terminal.foreground_cwd();
                                }
                                pane_state.keep_open = pane.keep_open;
                                pane_state.daemon_socket = pane.daemon_socket.clone();
                                pane_state.remote_name = pane.remote_name.clone();
                                pane_state.launch_context = pane.launch_context.clone();
                                pane_state
                            })
                            .collect();

                        window_state.tabs.push(tab_state);
                    }

                    // Set active tab
                    window_state.active_tab = notebook_upgrade.current_page().unwrap_or(0) as usize;

                    upgrade_state.windows.push(window_state);

                    drop(tabs_borrowed);

                    log::info!(
                        "Collected upgrade state: {} windows, {} tabs",
                        upgrade_state.windows.len(),
                        upgrade_state
                            .windows
                            .iter()
                            .map(|w| w.tabs.len())
                            .sum::<usize>()
                    );

                    // Execute the upgrade
                    let binary = std::path::Path::new(&binary_path);
                    match cterm_app::upgrade::execute_upgrade(binary, &upgrade_state) {
                        Ok(()) => {
                            log::info!("Upgrade successful, exiting");
                            std::process::exit(0);
                        }
                        Err(e) => {
                            log::error!("Upgrade failed: {}", e);
                            show_upgrade_error_dialog(&window_clone, &e);
                        }
                    }
                }
            });
            window.add_action(&action);
        }

        {
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("about", None);
            action.connect_activate(move |_, _| {
                dialogs::show_about_dialog(&window_clone);
            });
            window.add_action(&action);
        }

        // Tab Templates action
        {
            let window_clone = window.clone();
            let notebook = notebook.clone();
            let tabs = Rc::clone(&tabs);
            let next_tab_id = Rc::clone(&next_tab_id);
            let config = Rc::clone(&config);
            let theme = theme.clone();
            let tab_bar = tab_bar.clone();
            let has_bell = Rc::clone(&has_bell);
            let file_manager = Rc::clone(&self.file_manager);
            let notification_bar = self.notification_bar.clone();
            let remote_manager = self.remote_manager.clone();
            let action = gio::SimpleAction::new("tab-templates", None);
            action.connect_activate(move |_, _| {
                if reject_managed_secondary_action("tab templates") {
                    return;
                }
                let notebook = notebook.clone();
                let tabs = Rc::clone(&tabs);
                let next_tab_id = Rc::clone(&next_tab_id);
                let config = Rc::clone(&config);
                let theme = theme.clone();
                let tab_bar = tab_bar.clone();
                let window_for_tab = window_clone.clone();
                let has_bell = Rc::clone(&has_bell);
                let file_manager = Rc::clone(&file_manager);
                let notification_bar = notification_bar.clone();
                let remote_manager = remote_manager.clone();
                crate::tab_templates_dialog::show_tab_templates_dialog_with_open(
                    &window_clone,
                    || {
                        log::info!("Tab templates saved");
                    },
                    move |template| {
                        create_tab_from_template(
                            &notebook,
                            &tabs,
                            &next_tab_id,
                            &config,
                            &theme,
                            &tab_bar,
                            &window_for_tab,
                            &has_bell,
                            &file_manager,
                            &notification_bar,
                            &template,
                            &remote_manager,
                        );
                    },
                );
            });
            window.add_action(&action);
        }

        // View Logs action (debug menu)
        {
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("view-logs", None);
            action.connect_activate(move |_, _| {
                crate::log_viewer::show_log_viewer(&window_clone);
            });
            window.add_action(&action);
        }

        // Debug menu actions (hidden unless Shift is held when opening Help menu)
        {
            // Re-launch cterm - triggers seamless upgrade to the same binary (for testing)
            let tabs = Rc::clone(&tabs);
            let window_clone = window.clone();
            let action = gio::SimpleAction::new("debug-relaunch", None);
            action.connect_activate(move |_, _| {
                if !crate::get_args().updater_enabled() {
                    log::warn!("Ignoring debug relaunch request in managed mode");
                    return;
                }
                log::info!("Debug: Re-launching cterm for seamless upgrade test");

                // Use the executable path captured at startup (immune to binary replacement)
                let current_exe = crate::get_exe_path();
                log::info!("Re-launching from: {:?}", current_exe);

                // Get the current tabs for state collection
                let tabs_borrowed = tabs.borrow();
                let tab_count = tabs_borrowed.len();

                log::info!(
                    "Re-launch would preserve {} tabs (not yet fully implemented)",
                    tab_count
                );

                // Trigger upgrade to same binary via the execute-upgrade action
                let path_str = current_exe.to_string_lossy().to_string();
                if let Err(e) = gtk4::prelude::WidgetExt::activate_action(
                    &window_clone,
                    "win.execute-upgrade",
                    Some(&path_str.to_variant()),
                ) {
                    log::error!("Failed to activate execute-upgrade action: {}", e);
                }
            });
            window.add_action(&action);
        }

        {
            // Re-launch ctermd - trigger daemon exec-in-place relaunch
            let action = gio::SimpleAction::new("debug-relaunch-daemon", None);
            action.connect_activate(move |_, _| {
                if !crate::get_args().updater_enabled() {
                    log::warn!("Ignoring debug daemon relaunch request in managed mode");
                    return;
                }
                log::info!("Debug: Requesting ctermd relaunch");
                std::thread::spawn(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime");
                    rt.block_on(async {
                        let socket_path = cterm_client::default_socket_path();
                        match cterm_client::DaemonConnection::connect_unix(&socket_path, false)
                            .await
                        {
                            Ok(conn) => match conn.relaunch_daemon("").await {
                                Ok(resp) => {
                                    if resp.success {
                                        log::info!("ctermd relaunch succeeded");
                                    } else {
                                        log::error!("ctermd relaunch failed: {}", resp.reason);
                                    }
                                }
                                Err(e) => {
                                    log::info!(
                                        "ctermd relaunch in progress (connection dropped: {})",
                                        e
                                    );
                                }
                            },
                            Err(e) => {
                                log::error!("Failed to connect to ctermd for relaunch: {}", e);
                            }
                        }
                    });
                });
            });
            window.add_action(&action);
        }

        {
            // Kill Local ctermd - force shutdown the local daemon
            let action = gio::SimpleAction::new("debug-kill-daemon", None);
            action.connect_activate(move |_, _| {
                if !crate::get_args().updater_enabled() {
                    log::warn!("Ignoring debug daemon shutdown request in managed mode");
                    return;
                }
                log::info!("Debug: Requesting ctermd force shutdown");
                std::thread::spawn(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime");
                    rt.block_on(async {
                        let socket_path = cterm_client::default_socket_path();
                        match cterm_client::DaemonConnection::connect_unix(&socket_path, false)
                            .await
                        {
                            Ok(conn) => match conn.shutdown(true).await {
                                Ok(resp) => {
                                    if resp.success {
                                        log::info!("ctermd shutdown succeeded");
                                    } else {
                                        log::error!("ctermd shutdown failed: {}", resp.reason);
                                    }
                                }
                                Err(e) => {
                                    log::info!(
                                        "ctermd shutdown in progress (connection dropped: {})",
                                        e
                                    );
                                }
                            },
                            Err(e) => {
                                log::error!("Failed to connect to ctermd for shutdown: {}", e);
                            }
                        }
                    });
                });
            });
            window.add_action(&action);
        }

        {
            // Dump State - dump current terminal state for debugging
            let tabs = Rc::clone(&tabs);
            let action = gio::SimpleAction::new("debug-dump-state", None);
            action.connect_activate(move |_, _| {
                log::info!("Debug: Dumping terminal state");
                let tabs = tabs.borrow();
                log::info!("Number of tabs: {}", tabs.len());
                for (i, tab) in tabs.iter().enumerate() {
                    log::info!("Tab {}: id={}, title=\"{}\"", i, tab.id, tab.title);
                }
            });
            window.add_action(&action);
        }
    }

    /// Present the window and focus the terminal
    pub fn present(&self) {
        self.window.present();

        // Focus the current terminal after the window is presented
        let notebook = self.notebook.clone();
        let tabs = Rc::clone(&self.tabs);
        glib::idle_add_local_once(move || {
            if let Some(page_idx) = notebook.current_page() {
                let tabs_ref = tabs.borrow();
                if let Some(tab) = tabs_ref.get(page_idx as usize) {
                    tab.terminal.widget().grab_focus();
                }
            }
        });
    }

    /// Run the deterministic Wayland pane action sequence used by Linux CI.
    ///
    /// This is deliberately unavailable unless the exact opt-in environment
    /// variable is present. It activates the registered GTK window actions,
    /// avoiding compositor-specific input injection while still exercising the
    /// production action and asynchronous daemon-session paths.
    pub(crate) fn start_wayland_pane_ci_driver(&self) {
        if std::env::var("CTERM_WAYLAND_PANE_CI").as_deref() != Ok("1") {
            return;
        }

        let Some(display) = gdk::Display::default() else {
            pane_ci_fail(PaneCiStep::WaitInitial, "no GDK display");
        };
        if !display.backend().is_wayland() {
            pane_ci_fail(PaneCiStep::WaitInitial, "GDK backend is not Wayland");
        }
        if std::env::var_os("DISPLAY").is_some() {
            pane_ci_fail(PaneCiStep::WaitInitial, "DISPLAY is set");
        }

        let Some(application) = self.window.application() else {
            pane_ci_fail(PaneCiStep::WaitInitial, "window has no GTK application");
        };
        let window = self.window.clone();
        let notebook = self.notebook.clone();
        let tabs = Rc::clone(&self.tabs);
        let quick_open = self.quick_open.clone();
        let step = Rc::new(RefCell::new(PaneCiStep::WaitInitial));
        let started = std::time::Instant::now();
        let template_name = std::env::var("CTERM_WAYLAND_TEMPLATE_CI_NAME")
            .unwrap_or_else(|_| pane_ci_fail(PaneCiStep::WaitTemplate, "template name is unset"));
        let template_marker = std::env::var("CTERM_WAYLAND_TEMPLATE_CI_MARKER")
            .unwrap_or_else(|_| pane_ci_fail(PaneCiStep::WaitTemplate, "template marker is unset"));
        let template_ready = std::env::var_os("CTERM_WAYLAND_TEMPLATE_CI_READY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                pane_ci_fail(PaneCiStep::WaitTemplate, "template ready path is unset")
            });
        let template_visible = std::env::var_os("CTERM_WAYLAND_TEMPLATE_CI_VISIBLE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                pane_ci_fail(PaneCiStep::WaitTemplate, "template visible path is unset")
            });
        let template_done = std::env::var_os("CTERM_WAYLAND_TEMPLATE_CI_DONE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                pane_ci_fail(
                    PaneCiStep::WaitTemplate,
                    "template completion path is unset",
                )
            });
        let template_workspace = std::env::var_os("CTERM_WAYLAND_TEMPLATE_CI_WORKSPACE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                pane_ci_fail(PaneCiStep::WaitTemplate, "template workspace is unset")
            });
        let mut template_completed_at = None;
        let mut template_tab_id = None;
        let mut template_session_id = None;
        let mut unique_requested_at = None;

        pane_ci_marker("CTERM_PANE_CI START backend=wayland");
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let current_step = *step.borrow();
            if started.elapsed() > std::time::Duration::from_secs(40) {
                pane_ci_fail(current_step, "timed out after 40 seconds");
            }

            let Some(snapshot) = pane_ci_snapshot(&notebook, &tabs) else {
                return glib::ControlFlow::Continue;
            };

            match current_step {
                PaneCiStep::WaitInitial => {
                    if snapshot.pane_count != 1 || !window.is_mapped() {
                        return glib::ControlFlow::Continue;
                    }
                    pane_ci_marker("CTERM_PANE_CI READY panes=1");
                    pane_ci_activate(&window, "split-pane-horizontal")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    *step.borrow_mut() = PaneCiStep::WaitHorizontalSplit;
                }
                PaneCiStep::WaitHorizontalSplit => {
                    if snapshot.pane_count == 1 {
                        return glib::ControlFlow::Continue;
                    }
                    let horizontal = matches!(
                        snapshot.layout.tree(),
                        PaneTree::Split {
                            direction: SplitDirection::Horizontal,
                            ..
                        }
                    );
                    if snapshot.pane_count != 2 || !horizontal {
                        pane_ci_fail(current_step, "horizontal split topology mismatch");
                    }
                    pane_ci_marker("CTERM_PANE_CI SPLIT_HORIZONTAL_OK panes=2");
                    pane_ci_activate(&window, "split-pane-vertical")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    *step.borrow_mut() = PaneCiStep::WaitVerticalSplit;
                }
                PaneCiStep::WaitVerticalSplit => {
                    if snapshot.pane_count == 2 {
                        return glib::ControlFlow::Continue;
                    }
                    let nested_vertical = matches!(
                        snapshot.layout.tree(),
                        PaneTree::Split {
                            direction: SplitDirection::Horizontal,
                            second,
                            ..
                        } if matches!(
                            *second,
                            PaneTree::Split {
                                direction: SplitDirection::Vertical,
                                ..
                            }
                        )
                    );
                    if snapshot.pane_count != 3 || !nested_vertical {
                        pane_ci_fail(current_step, "vertical split topology mismatch");
                    }
                    pane_ci_marker("CTERM_PANE_CI SPLIT_VERTICAL_OK panes=3");
                    *step.borrow_mut() = PaneCiStep::Focus;
                }
                PaneCiStep::Focus => {
                    pane_ci_activate(&window, "focus-pane-up")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    let after = pane_ci_snapshot(&notebook, &tabs)
                        .unwrap_or_else(|| pane_ci_fail(current_step, "active tab disappeared"));
                    if after.active == snapshot.active {
                        pane_ci_fail(current_step, "focus action did not change the active pane");
                    }
                    pane_ci_marker("CTERM_PANE_CI FOCUS_OK direction=up");
                    *step.borrow_mut() = PaneCiStep::Resize;
                }
                PaneCiStep::Resize => {
                    pane_ci_activate(&window, "resize-pane-left")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    let after = pane_ci_snapshot(&notebook, &tabs)
                        .unwrap_or_else(|| pane_ci_fail(current_step, "active tab disappeared"));
                    if after.layout == snapshot.layout {
                        pane_ci_fail(current_step, "resize action did not change pane ratios");
                    }
                    pane_ci_marker("CTERM_PANE_CI RESIZE_OK direction=left");
                    *step.borrow_mut() = PaneCiStep::Zoom;
                }
                PaneCiStep::Zoom => {
                    pane_ci_activate(&window, "toggle-pane-zoom")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    let after = pane_ci_snapshot(&notebook, &tabs)
                        .unwrap_or_else(|| pane_ci_fail(current_step, "active tab disappeared"));
                    if after.layout.zoomed() != Some(after.active) {
                        pane_ci_fail(current_step, "zoom action did not zoom the active pane");
                    }
                    pane_ci_marker("CTERM_PANE_CI ZOOM_OK");
                    *step.borrow_mut() = PaneCiStep::Unzoom;
                }
                PaneCiStep::Unzoom => {
                    pane_ci_activate(&window, "toggle-pane-zoom")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    let after = pane_ci_snapshot(&notebook, &tabs)
                        .unwrap_or_else(|| pane_ci_fail(current_step, "active tab disappeared"));
                    if after.layout.zoomed().is_some() {
                        pane_ci_fail(current_step, "second zoom action did not restore the tree");
                    }
                    pane_ci_marker("CTERM_PANE_CI UNZOOM_OK");
                    pane_ci_activate(&window, "close-pane")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    *step.borrow_mut() = PaneCiStep::WaitClose;
                }
                PaneCiStep::WaitClose => {
                    if snapshot.pane_count == 3 {
                        return glib::ControlFlow::Continue;
                    }
                    if snapshot.pane_count != 2 {
                        pane_ci_fail(
                            current_step,
                            "close action produced an unexpected pane count",
                        );
                    }
                    pane_ci_marker("CTERM_PANE_CI CLOSE_OK panes=2");

                    pane_ci_activate(&window, "quick-open")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    if !quick_open.is_visible() {
                        pane_ci_fail(current_step, "Quick Open overlay did not become visible");
                    }
                    if !quick_open.confirm_selection_for_ci() {
                        pane_ci_fail(current_step, "Quick Open had no template selection");
                    }
                    pane_ci_marker("CTERM_TEMPLATE_CI INGRESS_OK source=quick-open");
                    *step.borrow_mut() = PaneCiStep::WaitTemplate;
                }
                PaneCiStep::WaitTemplate => {
                    let Some(template) = template_ci_snapshot(&notebook, &tabs) else {
                        return glib::ControlFlow::Continue;
                    };
                    if template.tab_count == 1 {
                        return glib::ControlFlow::Continue;
                    }
                    if template.tab_count != 2 {
                        pane_ci_fail(
                            current_step,
                            format!(
                                "template launch produced {} tabs instead of 2",
                                template.tab_count
                            ),
                        );
                    }
                    if template.template_name.as_deref() != Some(template_name.as_str()) {
                        pane_ci_fail(current_step, "new tab lost its template identity");
                    }
                    let Some(session_id) = template.session_id.clone() else {
                        return glib::ControlFlow::Continue;
                    };
                    if !template_ready.exists() {
                        std::fs::write(&template_ready, b"attached\n").unwrap_or_else(|error| {
                            pane_ci_fail(
                                current_step,
                                format!("cannot signal attached template session: {error}"),
                            )
                        });
                    }
                    if !template.screen_text.contains(&template_marker) {
                        return glib::ControlFlow::Continue;
                    }
                    if !template_visible.exists() {
                        std::fs::write(&template_visible, b"visible\n").unwrap_or_else(|error| {
                            pane_ci_fail(
                                current_step,
                                format!("cannot acknowledge visible template output: {error}"),
                            )
                        });
                    }
                    let completed_cwd = match std::fs::read_to_string(&template_done) {
                        Ok(cwd) => cwd,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return glib::ControlFlow::Continue;
                        }
                        Err(error) => pane_ci_fail(
                            current_step,
                            format!("cannot read template completion evidence: {error}"),
                        ),
                    };
                    if std::path::Path::new(completed_cwd.trim()) != template_workspace {
                        pane_ci_fail(
                            current_step,
                            format!(
                                "template cwd was {:?}, expected {:?}",
                                completed_cwd.trim(),
                                template_workspace
                            ),
                        );
                    }

                    let completed_at =
                        template_completed_at.get_or_insert_with(std::time::Instant::now);
                    if completed_at.elapsed() < std::time::Duration::from_millis(750) {
                        return glib::ControlFlow::Continue;
                    }
                    if !template.keep_open {
                        pane_ci_fail(current_step, "exited template tab was not marked keep_open");
                    }
                    if template.color.as_deref() != Some("#2a7fff") {
                        pane_ci_fail(current_step, "template tab color was not applied");
                    }
                    template_tab_id = Some(template.tab_id);
                    template_session_id = Some(session_id);
                    pane_ci_marker(
                        "CTERM_TEMPLATE_CI LAUNCH_OK argv=visible cwd=prepared keep_open=true color=#2a7fff",
                    );

                    pane_ci_activate(&window, "prev-tab")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    let left_template = template_ci_snapshot(&notebook, &tabs)
                        .is_some_and(|current| current.tab_id != template.tab_id);
                    if !left_template {
                        pane_ci_fail(current_step, "could not leave the unique template tab");
                    }
                    pane_ci_activate(&window, "quick-open")
                        .unwrap_or_else(|error| pane_ci_fail(current_step, error));
                    if !quick_open.is_visible() || !quick_open.confirm_selection_for_ci() {
                        pane_ci_fail(current_step, "second Quick Open selection failed");
                    }
                    unique_requested_at = Some(std::time::Instant::now());
                    *step.borrow_mut() = PaneCiStep::WaitUniqueReuse;
                }
                PaneCiStep::WaitUniqueReuse => {
                    if unique_requested_at.is_none_or(|requested| {
                        requested.elapsed() < std::time::Duration::from_secs(1)
                    }) {
                        return glib::ControlFlow::Continue;
                    }
                    let template = template_ci_snapshot(&notebook, &tabs)
                        .unwrap_or_else(|| pane_ci_fail(current_step, "active tab disappeared"));
                    if template.tab_count != 2 {
                        pane_ci_fail(
                            current_step,
                            format!(
                                "unique template created a duplicate; tab count is {}",
                                template.tab_count
                            ),
                        );
                    }
                    if Some(template.tab_id) != template_tab_id
                        || template.session_id != template_session_id
                        || template.template_name.as_deref() != Some(template_name.as_str())
                    {
                        pane_ci_fail(
                            current_step,
                            "unique template did not focus the existing tab/session",
                        );
                    }
                    pane_ci_marker("CTERM_TEMPLATE_CI UNIQUE_OK tabs=2 session=reused");
                    pane_ci_marker("CTERM_PANE_CI COMPLETE");
                    destroy_all_pane_sessions(&tabs);
                    window.destroy();
                    application.quit();
                    return glib::ControlFlow::Break;
                }
            }

            glib::ControlFlow::Continue
        });
    }

    /// Set up keyboard event handler
    fn setup_key_handler(&self) {
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        // Disable IM on the window shortcut controller so IBus doesn't
        // swallow Ctrl+Shift+letter events before key-pressed fires.
        key_controller.set_im_context(None::<&gtk4::IMContext>);

        let shortcuts = Rc::clone(&self.shortcuts);
        let window = self.window.clone();

        key_controller.connect_key_pressed(move |_, keyval, _keycode, state| {
            // Convert GTK modifiers to our modifiers
            let mut modifiers = gtk_modifiers_to_modifiers(state);

            // Some keyboard layouts consume Shift to produce uppercase keyvals.
            // Recover the logical modifier from the resulting key value.
            if !modifiers.contains(Modifiers::SHIFT) {
                if let Some(c) = keyval.to_unicode() {
                    if c.is_uppercase() {
                        modifiers.insert(Modifiers::SHIFT);
                    }
                }
            }

            let Some(key) = keyval_to_keycode(keyval) else {
                return glib::Propagation::Proceed;
            };
            let action = shortcuts.borrow().match_event(key, modifiers).cloned();
            let Some(action) = action else {
                return glib::Propagation::Proceed;
            };

            activate_shared_action(&window, &action);
            glib::Propagation::Stop
        });

        self.window.add_controller(key_controller);
    }

    /// Set up window focus handler to clear bell when window becomes active
    /// and send focus events to the terminal (DECSET 1004)
    fn setup_focus_handler(&self) {
        let has_bell = Rc::clone(&self.has_bell);
        let window = self.window.clone();
        let tab_bar = self.tab_bar.clone();
        let tabs = Rc::clone(&self.tabs);
        let notebook = self.notebook.clone();

        self.window.connect_is_active_notify(move |win| {
            let is_active = win.is_active();

            // Send focus event to the active terminal (DECSET 1004)
            if let Some(page_idx) = notebook.current_page() {
                let tabs_borrowed = tabs.borrow();
                if let Some(tab) = tabs_borrowed.get(page_idx as usize) {
                    tab.terminal.send_focus_event(is_active);
                }
            }

            if is_active {
                // Window became active, clear bell indicator
                let mut bell = has_bell.borrow_mut();
                if *bell {
                    *bell = false;
                    window.set_title(Some("cterm"));

                    // Clear bell on the currently active tab
                    if let Some(page_idx) = notebook.current_page() {
                        let tabs = tabs.borrow();
                        if let Some(tab) = tabs.get(page_idx as usize) {
                            tab_bar.clear_bell(tab.id);
                            tab.terminal.clear_alert();
                        }
                    }
                }
            }
        });
    }

    /// Set up terminal focus restoration
    ///
    /// When keys are pressed and focus is not on the terminal (e.g., after
    /// closing a menu), automatically restore focus to the terminal and
    /// forward the key to the terminal so it's not lost.
    fn setup_terminal_focus_restore(&self) {
        let focus_controller = EventControllerKey::new();
        focus_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        // Disable IM on the focus-restore controller too, so IBus doesn't
        // consume key events before our handler runs.
        focus_controller.set_im_context(None::<&gtk4::IMContext>);

        let notebook = self.notebook.clone();
        let tabs = Rc::clone(&self.tabs);

        focus_controller.connect_key_pressed(move |_controller, keyval, _keycode, state| {
            // Skip modifier keys and menu activation keys
            let is_modifier = matches!(
                keyval,
                gdk::Key::Shift_L
                    | gdk::Key::Shift_R
                    | gdk::Key::Control_L
                    | gdk::Key::Control_R
                    | gdk::Key::Alt_L
                    | gdk::Key::Alt_R
                    | gdk::Key::Super_L
                    | gdk::Key::Super_R
                    | gdk::Key::Meta_L
                    | gdk::Key::Meta_R
                    | gdk::Key::F10
            );

            if is_modifier {
                return glib::Propagation::Proceed;
            }

            // Check if the terminal widget itself has focus.
            // (focus_child() only returns the direct child, not the deeply
            // nested DrawingArea, so we check has_focus() on the actual widget.)
            let terminal_has_focus = notebook
                .current_page()
                .and_then(|idx| {
                    let tabs_ref = tabs.borrow();
                    tabs_ref
                        .get(idx as usize)
                        .map(|tab| tab.terminal.widget().has_focus())
                })
                .unwrap_or(false);

            if !terminal_has_focus {
                // Focus is not on terminal - restore it and forward the key
                if let Some(page_idx) = notebook.current_page() {
                    let tabs_ref = tabs.borrow();
                    if let Some(tab) = tabs_ref.get(page_idx as usize) {
                        // Grab focus
                        tab.terminal.widget().grab_focus();

                        // Forward the key to the terminal
                        let has_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
                        let has_alt = state.contains(gdk::ModifierType::ALT_MASK);
                        let has_shift = state.contains(gdk::ModifierType::SHIFT_MASK)
                            || keyval.to_unicode().is_some_and(|c| c.is_uppercase());

                        // Don't forward Ctrl+Shift combinations - those are
                        // shortcuts handled by the key_controller.
                        if has_ctrl && has_shift {
                            return glib::Propagation::Proceed;
                        }

                        if let Some(c) = keyval.to_unicode() {
                            if has_ctrl && !has_alt {
                                // Ctrl+key - convert to control character
                                let ctrl_char = match c.to_ascii_lowercase() {
                                    'a'..='z' => {
                                        Some((c.to_ascii_lowercase() as u8 - b'a' + 1) as char)
                                    }
                                    '[' | '3' => Some('\x1b'), // Escape
                                    '\\' | '4' => Some('\x1c'),
                                    ']' | '5' => Some('\x1d'),
                                    '^' | '6' => Some('\x1e'),
                                    '_' | '7' => Some('\x1f'),
                                    '@' | '2' => Some('\x00'),
                                    _ => None,
                                };
                                if let Some(ctrl) = ctrl_char {
                                    tab.terminal.write_str(&ctrl.to_string());
                                    tab.terminal.widget().queue_draw();
                                    return glib::Propagation::Stop;
                                }
                            } else if !has_ctrl && !has_alt {
                                // Simple character - write directly
                                let mut s = [0u8; 4];
                                let s = c.encode_utf8(&mut s);
                                tab.terminal.write_str(s);
                                tab.terminal.widget().queue_draw();
                                return glib::Propagation::Stop;
                            }
                        }

                        // For special keys or Alt combinations, let the terminal's
                        // key handler process it. Focus is now on the terminal.
                    }
                }
            }

            glib::Propagation::Proceed
        });

        self.window.add_controller(focus_controller);
    }

    /// Set up notification bar callbacks for file transfers
    fn setup_notification_bar(&self) {
        let file_manager = Rc::clone(&self.file_manager);
        let notification_bar = self.notification_bar.clone();
        let window = self.window.clone();

        // Save button - save to default location (Downloads or last saved dir)
        let file_manager_save = Rc::clone(&file_manager);
        let notification_bar_save = notification_bar.clone();
        notification_bar.set_on_save(move |id| {
            let mut manager = file_manager_save.borrow_mut();
            if let Some(path) = manager.default_save_path() {
                match manager.save_to_path(id, &path) {
                    Ok(size) => {
                        log::info!("Saved file to {:?} ({} bytes)", path, size);
                    }
                    Err(e) => {
                        log::error!("Failed to save file: {}", e);
                    }
                }
            }
            drop(manager);
            notification_bar_save.hide();
        });

        // Save As button - show file chooser dialog
        let file_manager_save_as = Rc::clone(&file_manager);
        let notification_bar_save_as = notification_bar.clone();
        notification_bar.set_on_save_as(move |id| {
            let manager = file_manager_save_as.borrow();
            let suggested_name = manager.suggested_filename().map(|s| s.to_string());
            let initial_dir = manager.last_save_dir().cloned();
            drop(manager);

            let file_chooser = gtk4::FileChooserDialog::new(
                Some("Save File As"),
                Some(&window),
                gtk4::FileChooserAction::Save,
                &[
                    ("Cancel", gtk4::ResponseType::Cancel),
                    ("Save", gtk4::ResponseType::Accept),
                ],
            );

            // Set suggested filename
            if let Some(name) = suggested_name {
                file_chooser.set_current_name(&name);
            }

            // Set initial folder
            if let Some(dir) = initial_dir {
                let file = gio::File::for_path(&dir);
                file_chooser.set_current_folder(Some(&file)).ok();
            } else if let Some(downloads) = cterm_app::file_transfer::dirs::download_dir() {
                let file = gio::File::for_path(&downloads);
                file_chooser.set_current_folder(Some(&file)).ok();
            }

            let file_manager_dialog = Rc::clone(&file_manager_save_as);
            let notification_bar_dialog = notification_bar_save_as.clone();

            file_chooser.connect_response(move |dialog, response| {
                if response == gtk4::ResponseType::Accept {
                    if let Some(file) = dialog.file() {
                        if let Some(path) = file.path() {
                            let mut manager = file_manager_dialog.borrow_mut();
                            match manager.save_to_path(id, &path) {
                                Ok(size) => {
                                    log::info!("Saved file to {:?} ({} bytes)", path, size);
                                }
                                Err(e) => {
                                    log::error!("Failed to save file: {}", e);
                                }
                            }
                        }
                    }
                }
                notification_bar_dialog.hide();
                dialog.close();
            });

            file_chooser.present();
        });

        // Discard button - discard the pending file
        let file_manager_discard = Rc::clone(&file_manager);
        let notification_bar_discard = notification_bar.clone();
        notification_bar.set_on_discard(move |id| {
            file_manager_discard.borrow_mut().discard(id);
            notification_bar_discard.hide();
            log::debug!("Discarded pending file {}", id);
        });
    }

    /// Set up Quick Open overlay callback
    fn setup_quick_open(&self) {
        let notebook = self.notebook.clone();
        let tabs = Rc::clone(&self.tabs);
        let next_tab_id = Rc::clone(&self.next_tab_id);
        let config = Rc::clone(&self.config);
        let theme = self.theme.clone();
        let tab_bar = self.tab_bar.clone();
        let window = self.window.clone();
        let has_bell = Rc::clone(&self.has_bell);
        let file_manager = Rc::clone(&self.file_manager);
        let notification_bar = self.notification_bar.clone();
        let remote_manager = self.remote_manager.clone();

        self.quick_open.set_on_select(move |template| {
            if reject_managed_secondary_action("quick-open selection") {
                return;
            }
            create_tab_from_template(
                &notebook,
                &tabs,
                &next_tab_id,
                &config,
                &theme,
                &tab_bar,
                &window,
                &has_bell,
                &file_manager,
                &notification_bar,
                &template,
                &remote_manager,
            );
            log::info!("Opened template tab from Quick Open: {}", template.name);
        });
    }

    /// Set up tab bar callbacks
    fn setup_tab_bar_callbacks(&self) {
        let notebook = self.notebook.clone();
        let tabs = Rc::clone(&self.tabs);
        let next_tab_id = Rc::clone(&self.next_tab_id);
        let config = self.config.clone();
        let theme = self.theme.clone();
        let tab_bar = self.tab_bar.clone();
        let window = self.window.clone();
        let has_bell = Rc::clone(&self.has_bell);
        let file_manager = Rc::clone(&self.file_manager);
        let notification_bar = self.notification_bar.clone();

        // New tab button
        self.tab_bar.set_on_new_tab(move || {
            if reject_managed_secondary_action("new-tab button") {
                return;
            }
            // Get info from the active terminal
            let (cwd, daemon_socket) = {
                let tabs_borrow = tabs.borrow();
                if let Some(page_idx) = notebook.current_page() {
                    let entry = tabs_borrow.get(page_idx as usize);
                    #[cfg(unix)]
                    let cwd = entry.and_then(|e| e.terminal.foreground_cwd());
                    #[cfg(not(unix))]
                    let cwd: Option<String> = None;
                    let socket = entry.and_then(|e| e.daemon_socket.clone());
                    (cwd, socket)
                } else {
                    (None, None)
                }
            };

            create_new_tab(
                &notebook,
                &tabs,
                &next_tab_id,
                &config,
                &theme,
                &tab_bar,
                &window,
                &has_bell,
                &file_manager,
                &notification_bar,
                cwd,
                daemon_socket,
            );
        });

        // Rename tab (right-click context menu)
        {
            let tabs = Rc::clone(&self.tabs);
            let tab_bar = self.tab_bar.clone();
            let window = self.window.clone();
            self.tab_bar.set_on_rename(move |tab_id| {
                show_rename_tab_dialog(&window, &tabs, &tab_bar, tab_id);
            });
        }

        // Set tab color (right-click context menu)
        {
            let tabs = Rc::clone(&self.tabs);
            let tab_bar = self.tab_bar.clone();
            let window = self.window.clone();
            self.tab_bar.set_on_set_color(move |tab_id| {
                let tab_bar_clone = tab_bar.clone();
                let tabs_clone = Rc::clone(&tabs);
                dialogs::show_set_color_dialog(&window, move |color| {
                    let mut tabs = tabs_clone.borrow_mut();
                    if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab_bar_clone.set_color(tab_id, color.as_deref());
                        tab.terminal
                            .set_tab_color_on_daemon(color.as_deref().unwrap_or(""));
                        tab.color = color;
                    }
                });
            });
        }

        // Disconnect remote (right-click context menu, only on remote tabs)
        {
            let tabs = Rc::clone(&self.tabs);
            let notebook = self.notebook.clone();
            let tab_bar = self.tab_bar.clone();
            let window = self.window.clone();
            let remote_manager = self.remote_manager.clone();
            self.tab_bar.set_on_disconnect(move |tab_id| {
                // Resolve which remote this tab belongs to and how many tabs
                // share it — needed for the dialog wording.
                let (remote_name, tab_count) = {
                    let tabs_ref = tabs.borrow();
                    let Some(name) = tabs_ref.iter().find(|t| t.id == tab_id).and_then(|tab| {
                        tab.panes
                            .iter()
                            .find_map(|(_, pane)| pane.remote_name.clone())
                    }) else {
                        return; // Not a remote tab — shouldn't happen, menu is hidden.
                    };
                    let count = tabs_ref
                        .iter()
                        .filter(|tab| {
                            tab.panes
                                .iter()
                                .any(|(_, pane)| pane.remote_name.as_deref() == Some(name.as_str()))
                        })
                        .count();
                    (name, count)
                };

                let tabs_inner = Rc::clone(&tabs);
                let notebook_inner = notebook.clone();
                let tab_bar_inner = tab_bar.clone();
                let window_inner = window.clone();
                let remote_manager_inner = remote_manager.clone();
                let remote_name_inner = remote_name.clone();
                dialogs::show_disconnect_confirmation_dialog(
                    &window,
                    &remote_name,
                    tab_count,
                    move |confirmed| {
                        if !confirmed {
                            return;
                        }
                        disconnect_remote(
                            &notebook_inner,
                            &tabs_inner,
                            &tab_bar_inner,
                            &window_inner,
                            &remote_manager_inner,
                            &remote_name_inner,
                        );
                    },
                );
            });
        }
    }

    /// Set up close request handler to confirm when closing with running processes
    #[cfg(unix)]
    fn setup_close_request_handler(&self) {
        let tabs = Rc::clone(&self.tabs);
        let config = Rc::clone(&self.config);
        let window = self.window.clone();

        self.window.connect_close_request(move |win| {
            let confirm_close = config.borrow().general.confirm_close_with_running;
            if !confirm_close {
                destroy_all_pane_sessions(&tabs);
                return glib::Propagation::Proceed;
            }

            // Collect session info for daemon queries
            let tab_infos: Vec<(String, Option<std::path::PathBuf>, String)> = {
                let tabs = tabs.borrow();
                tabs.iter()
                    .flat_map(|tab| {
                        tab.panes.iter().filter_map(|(_, pane)| {
                            let sid = pane.session_id.clone()?;
                            if sid.is_empty() {
                                return None;
                            }
                            Some((sid, pane.daemon_socket.clone(), pane.title.clone()))
                        })
                    })
                    .collect()
            };

            if tab_infos.is_empty() {
                destroy_all_pane_sessions(&tabs);
                return glib::Propagation::Proceed;
            }

            let window_to_destroy = window.clone();
            let tabs_to_destroy = Rc::clone(&tabs);
            confirm_running_sessions(win, tab_infos, move || {
                destroy_all_pane_sessions(&tabs_to_destroy);
                window_to_destroy.destroy();
            });

            glib::Propagation::Stop
        });
    }

    /// Set up close request handler (non-Unix fallback - no process detection)
    #[cfg(not(unix))]
    fn setup_close_request_handler(&self) {
        // No process detection on non-Unix platforms
    }

    /// Create a new tab
    pub fn new_tab(&self) {
        if reject_managed_secondary_action("new tab") {
            return;
        }
        create_new_tab(
            &self.notebook,
            &self.tabs,
            &self.next_tab_id,
            &self.config,
            &self.theme,
            &self.tab_bar,
            &self.window,
            &self.has_bell,
            &self.file_manager,
            &self.notification_bar,
            None,
            None,
        );
    }

    /// Add a tab for a reconnected daemon session (with screen snapshot).
    ///
    /// Used during startup reconnection to create tabs for existing daemon sessions.
    pub fn add_reconnected_tab(
        &self,
        recon: cterm_app::daemon_reconnect::ReconnectedSession,
        tab_color: Option<String>,
    ) {
        let sid = recon.handle.session_id().to_string();
        let daemon_socket = recon.handle.socket_path().map(|p| p.to_owned());
        // Prefer custom_title (user-set), then daemon title, then fallback
        let (title, title_locked) = if !recon.custom_title.is_empty() {
            (recon.custom_title.clone(), true)
        } else if !recon.title.is_empty() {
            (recon.title.clone(), false)
        } else {
            ("Terminal".to_string(), false)
        };

        // Use explicit tab_color if provided (upgrade path), otherwise use daemon's
        let effective_color = tab_color.or_else(|| {
            if recon.tab_color.is_empty() {
                None
            } else {
                Some(recon.tab_color.clone())
            }
        });

        let cfg = self.config.borrow();
        let template_name = (!recon.template_name.is_empty()).then(|| recon.template_name.clone());
        let native_ssh = template_name.as_deref().and_then(|name| {
            cfg.sticky_tabs
                .iter()
                .find(|template| template.name == name)
                .and_then(|template| template.ssh.as_ref())
                .map(|ssh| ssh.to_ssh_params())
        });
        let terminal = TerminalWidget::from_daemon_with_screen(recon, &cfg, &self.theme);
        drop(cfg);

        let tab_id = generate_tab_id(&self.next_tab_id);
        self.tab_bar.add_tab(tab_id, &title);

        setup_tab_callbacks(
            &self.notebook,
            &self.tabs,
            &self.config,
            &self.tab_bar,
            &self.window,
            &self.has_bell,
            &self.file_manager,
            &self.notification_bar,
            &terminal,
            tab_id,
            PaneLayout::new().active(),
            false,
        );

        finalize_new_tab(
            &self.notebook,
            &self.tabs,
            &self.tab_bar,
            tab_id,
            title,
            terminal,
            title_locked,
            false,
            Some(sid),
            daemon_socket,
            None,
            template_name,
            native_ssh,
            None,
        );

        // Restore tab color if available
        if let Some(ref color) = effective_color {
            self.tab_bar.set_color(tab_id, Some(color));
            if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
                tab.color = effective_color;
            }
        }
    }

    /// Restore every session and split in one upgraded tab.
    pub fn add_reconnected_pane_tab(
        &self,
        tab_state: cterm_app::upgrade::TabUpgradeState,
        layout: PaneLayout,
        reconnected: Vec<cterm_app::daemon_reconnect::ReconnectedSession>,
    ) -> bool {
        let pane_ids = layout.pane_ids();
        if pane_ids.len() != tab_state.panes.len() || pane_ids.len() != reconnected.len() {
            log::error!(
                "Cannot restore pane tab '{}': layout={}, records={}, sessions={}",
                tab_state.title,
                pane_ids.len(),
                tab_state.panes.len(),
                reconnected.len()
            );
            return false;
        }

        let tab_id = generate_tab_id(&self.next_tab_id);
        let mut callback_terminals = Vec::with_capacity(pane_ids.len());
        let entries = pane_ids
            .iter()
            .copied()
            .zip(tab_state.panes.iter().cloned())
            .zip(reconnected)
            .map(|((pane_id, pane_state), recon)| {
                let session_id = Some(recon.handle.session_id().to_string());
                let daemon_socket = recon
                    .handle
                    .socket_path()
                    .map(|path| path.to_owned())
                    .or_else(|| pane_state.daemon_socket.clone());
                let title = if !pane_state.title.is_empty() {
                    pane_state.title.clone()
                } else if !recon.custom_title.is_empty() {
                    recon.custom_title.clone()
                } else if !recon.title.is_empty() {
                    recon.title.clone()
                } else {
                    "Terminal".to_string()
                };
                let config = self.config.borrow();
                let native_ssh = pane_state.template_name.as_deref().and_then(|name| {
                    config
                        .sticky_tabs
                        .iter()
                        .find(|template| template.name == name)
                        .and_then(|template| template.ssh.as_ref())
                        .map(|ssh| ssh.to_ssh_params())
                });
                let terminal = Rc::new(TerminalWidget::from_daemon_with_screen(
                    recon,
                    &config,
                    &self.theme,
                ));
                drop(config);
                callback_terminals.push((pane_id, Rc::clone(&terminal), pane_state.keep_open));
                (
                    pane_id,
                    PaneEntry {
                        terminal,
                        title,
                        title_locked: pane_state.title_locked,
                        template_name: pane_state.template_name,
                        keep_open: pane_state.keep_open,
                        session_id,
                        daemon_socket,
                        remote_name: pane_state.remote_name,
                        native_ssh,
                        launch_context: pane_state.launch_context,
                    },
                )
            })
            .collect::<Vec<_>>();

        let panes = match PaneSet::from_layout(layout, entries) {
            Ok(panes) => panes,
            Err(error) => {
                log::error!(
                    "Cannot restore pane layout for '{}': {error}",
                    tab_state.title
                );
                return false;
            }
        };
        let active = panes.active();
        let title = if tab_state.custom_title.is_some() || active.title.is_empty() {
            tab_state.title.clone()
        } else {
            active.title.clone()
        };
        let pane_container = GtkBox::new(Orientation::Vertical, 0);
        pane_container.set_hexpand(true);
        pane_container.set_vexpand(true);
        panes.rebuild(&pane_container, |pane| {
            pane.terminal.widget().clone().upcast()
        });
        let page_num = self
            .notebook
            .append_page(&pane_container, None::<&gtk4::Widget>);

        self.tab_bar.add_tab(tab_id, &title);
        if panes.iter().any(|(_, pane)| pane.remote_name.is_some()) {
            self.tab_bar.mark_tab_remote(tab_id);
        }
        if let Some(color) = tab_state.color.as_deref() {
            self.tab_bar.set_color(tab_id, Some(color));
        }

        self.tabs.borrow_mut().push(TabEntry {
            id: tab_id,
            title,
            terminal: Rc::clone(&active.terminal),
            title_locked: tab_state.custom_title.is_some(),
            color: tab_state.color,
            session_id: active.session_id.clone(),
            daemon_socket: active.daemon_socket.clone(),
            remote_name: active.remote_name.clone(),
            pane_container,
            panes,
        });

        for (pane_id, terminal, keep_open) in callback_terminals {
            setup_tab_callbacks(
                &self.notebook,
                &self.tabs,
                &self.config,
                &self.tab_bar,
                &self.window,
                &self.has_bell,
                &self.file_manager,
                &self.notification_bar,
                &terminal,
                tab_id,
                pane_id,
                keep_open,
            );
        }

        self.tab_bar.update_visibility();
        self.notebook.set_current_page(Some(page_num));
        self.tab_bar.set_active(tab_id);
        if let Some(tab) = self.tabs.borrow().last() {
            tab.terminal.widget().grab_focus();
        }
        true
    }

    /// Update window title when switching tabs
    fn setup_tab_switch_handler(&self) {
        let tabs = Rc::clone(&self.tabs);
        let window = self.window.clone();
        let tab_bar = self.tab_bar.clone();
        let has_bell = Rc::clone(&self.has_bell);
        self.notebook.connect_switch_page(move |_, _, page_num| {
            let tabs = tabs.borrow();
            if let Some(tab) = tabs.get(page_num as usize) {
                window.set_title(Some(&tab.title));
                tab_bar.set_active(tab.id);
                tab_bar.clear_bell(tab.id);
                tab.terminal.clear_alert();
                *has_bell.borrow_mut() = false;
            }
        });
    }
}

/// Show the rename dialog for a tab and persist the new title.
/// Used by both the menu bar "Set Title" action and the right-click context menu.
fn show_rename_tab_dialog(
    window: &ApplicationWindow,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    tab_id: u64,
) {
    let current_title = {
        let tabs = tabs.borrow();
        tabs.iter()
            .find(|t| t.id == tab_id)
            .map(|t| t.title.clone())
            .unwrap_or_default()
    };
    let tabs_clone = Rc::clone(tabs);
    let tab_bar_clone = tab_bar.clone();
    let window_clone = window.clone();
    dialogs::show_set_title_dialog(window, &current_title, move |new_title| {
        let mut tabs = tabs_clone.borrow_mut();
        if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.title = new_title.clone();
            tab.title_locked = true;
            let active = tab.active_pane_id();
            if let Some(pane) = tab.panes.get_mut(active) {
                pane.title = new_title.clone();
                pane.title_locked = true;
            }
            tab_bar_clone.set_title(tab_id, &new_title);
            window_clone.set_title(Some(&new_title));
            // Persist custom title to daemon
            tab.terminal.set_custom_title(&new_title);
        }
    });
}

/// Generate a unique tab ID from the shared counter
fn generate_tab_id(next_tab_id: &Rc<RefCell<u64>>) -> u64 {
    let mut id = next_tab_id.borrow_mut();
    let current = *id;
    *id += 1;
    current
}

/// Set up all standard callbacks for a tab (close, click, exit, bell, title, file transfer)
#[allow(clippy::too_many_arguments)]
fn setup_tab_callbacks(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    config: &Rc<RefCell<Config>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    has_bell: &Rc<RefCell<bool>>,
    file_manager: &Rc<RefCell<PendingFileManager>>,
    notification_bar: &NotificationBar,
    terminal: &TerminalWidget,
    tab_id: u64,
    pane_id: PaneId,
    keep_open: bool,
) {
    // Close callback (with confirmation for running processes)
    let notebook_close = notebook.clone();
    let tabs_close = Rc::clone(tabs);
    let tab_bar_close = tab_bar.clone();
    let window_close = window.clone();
    let config_close = Rc::clone(config);
    tab_bar.set_on_close(tab_id, move || {
        request_close_tab_by_id(
            &notebook_close,
            &tabs_close,
            &tab_bar_close,
            &window_close,
            &config_close,
            tab_id,
        );
    });

    // Click callback
    let notebook_click = notebook.clone();
    let tabs_click = Rc::clone(tabs);
    let tab_bar_click = tab_bar.clone();
    tab_bar.set_on_click(tab_id, move || {
        let tabs = tabs_click.borrow();
        if let Some(idx) = tabs.iter().position(|t| t.id == tab_id) {
            notebook_click.set_current_page(Some(idx as u32));
            tab_bar_click.set_active(tab_id);
            tab_bar_click.clear_bell(tab_id);
            tabs[idx].terminal.clear_alert();
            tabs[idx].terminal.widget().grab_focus();
        }
    });

    // Exit callback
    let notebook_exit = notebook.clone();
    let tabs_exit = Rc::clone(tabs);
    let tab_bar_exit = tab_bar.clone();
    let window_exit = window.clone();
    terminal.set_on_exit(move || {
        if !keep_open {
            close_pane_by_id(
                &notebook_exit,
                &tabs_exit,
                &tab_bar_exit,
                &window_exit,
                tab_id,
                pane_id,
                false,
            );
        }
    });

    // Bell callback
    let tab_bar_bell = tab_bar.clone();
    let notebook_bell = notebook.clone();
    let tabs_bell = Rc::clone(tabs);
    let window_bell = window.clone();
    let has_bell_bell = Rc::clone(has_bell);
    terminal.set_on_bell(move || {
        let is_window_active = window_bell.is_active();
        let is_current_pane = if let Some(current_page) = notebook_bell.current_page() {
            let tabs = tabs_bell.borrow();
            tabs.get(current_page as usize)
                .map(|t| t.id == tab_id && t.active_pane_id() == pane_id)
                .unwrap_or(false)
        } else {
            false
        };

        if !is_current_pane || !is_window_active {
            tab_bar_bell.set_bell(tab_id, true);
        }

        if !is_window_active {
            *has_bell_bell.borrow_mut() = true;
            window_bell.set_title(Some("🔔 cterm"));
        }
    });

    // Title change callback
    let tab_bar_title = tab_bar.clone();
    let tabs_title = Rc::clone(tabs);
    let window_title = window.clone();
    let notebook_title = notebook.clone();
    let has_bell_title = Rc::clone(has_bell);
    terminal.set_on_title_change(move |title| {
        let active_title_changed = {
            let mut tabs = tabs_title.borrow_mut();
            let Some(entry) = tabs.iter_mut().find(|t| t.id == tab_id) else {
                return;
            };
            let Some(pane) = entry.panes.get_mut(pane_id) else {
                return;
            };
            if pane.title_locked || entry.title_locked {
                return;
            }
            pane.title = title.to_string();
            if entry.active_pane_id() != pane_id {
                false
            } else {
                entry.title = title.to_string();
                true
            }
        };

        if !active_title_changed {
            return;
        }
        tab_bar_title.set_title(tab_id, title);

        // Update window title if this is the active tab
        if let Some(current_page) = notebook_title.current_page() {
            let tabs = tabs_title.borrow();
            if tabs
                .get(current_page as usize)
                .map(|t| t.id == tab_id)
                .unwrap_or(false)
            {
                *has_bell_title.borrow_mut() = false;
                window_title.set_title(Some(title));
            }
        }
    });

    let tabs_focus = Rc::clone(tabs);
    let tab_bar_focus = tab_bar.clone();
    let window_focus = window.clone();
    terminal.widget().connect_has_focus_notify(move |widget| {
        if !widget.has_focus() {
            return;
        }
        let title = {
            let mut tabs = tabs_focus.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return;
            };
            let previous = Rc::clone(&tab.terminal);
            if !tab.activate_pane(pane_id) {
                return;
            }
            previous.send_focus_event(false);
            tab.terminal.send_focus_event(true);
            tab.title.clone()
        };
        tab_bar_focus.set_title(tab_id, &title);
        window_focus.set_title(Some(&title));
    });

    // File transfer callback
    let file_manager_transfer = Rc::clone(file_manager);
    let notification_bar_transfer = notification_bar.clone();
    terminal.set_on_file_transfer(move |transfer| {
        use cterm_core::FileTransferOperation;

        match transfer {
            FileTransferOperation::FileReceived { id, name, data } => {
                log::info!(
                    "File received: id={}, name={:?}, size={}",
                    id,
                    name,
                    data.len()
                );
                let size = data.len();
                let mut manager = file_manager_transfer.borrow_mut();
                manager.set_pending(id, name.clone(), data);
                drop(manager);
                notification_bar_transfer.show_file(id, name.as_deref(), size);
            }
            FileTransferOperation::StreamingFileReceived { id, result } => {
                log::info!(
                    "Streaming file received: id={}, name={:?}, size={}",
                    id,
                    result.params.name,
                    result.total_bytes
                );
                let size = result.total_bytes;
                let name = result.params.name.clone();
                let mut manager = file_manager_transfer.borrow_mut();
                manager.set_pending_streaming(id, name.clone(), result.data);
                drop(manager);
                notification_bar_transfer.show_file(id, name.as_deref(), size);
            }
        }
    });
}

/// Finalize a new tab: store entry, update visibility, switch to it, and focus
#[allow(clippy::too_many_arguments)]
fn finalize_new_tab(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    tab_id: u64,
    title: String,
    terminal: TerminalWidget,
    title_locked: bool,
    keep_open: bool,
    session_id: Option<String>,
    daemon_socket: Option<std::path::PathBuf>,
    remote_name: Option<String>,
    template_name: Option<String>,
    native_ssh: Option<cterm_client::SshParams>,
    launch_context: Option<cterm_app::upgrade::PaneLaunchContext>,
) {
    if remote_name.is_some() {
        tab_bar.mark_tab_remote(tab_id);
    }

    let pane_container = GtkBox::new(Orientation::Vertical, 0);
    pane_container.set_hexpand(true);
    pane_container.set_vexpand(true);
    let terminal = Rc::new(terminal);
    let panes = PaneSet::new(PaneEntry {
        terminal: Rc::clone(&terminal),
        title: title.clone(),
        title_locked,
        template_name,
        keep_open,
        session_id: session_id.clone(),
        daemon_socket: daemon_socket.clone(),
        remote_name: remote_name.clone(),
        native_ssh,
        launch_context,
    });
    panes.rebuild(&pane_container, |pane| {
        pane.terminal.widget().clone().upcast()
    });
    let page_num = notebook.append_page(&pane_container, None::<&gtk4::Widget>);

    tabs.borrow_mut().push(TabEntry {
        id: tab_id,
        title,
        terminal: Rc::clone(&terminal),
        title_locked,
        color: None,
        session_id,
        daemon_socket,
        remote_name,
        pane_container,
        panes,
    });

    tab_bar.update_visibility();
    notebook.set_current_page(Some(page_num));
    tab_bar.set_active(tab_id);

    terminal.widget().grab_focus();
}

/// Create a new terminal tab (daemon-backed via ctermd)
///
/// If `daemon_socket` is Some, creates the session on that specific daemon
/// (e.g. an SSH-tunneled remote). Otherwise uses the local daemon.
#[allow(clippy::too_many_arguments)]
fn create_new_tab(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    next_tab_id: &Rc<RefCell<u64>>,
    config: &Rc<RefCell<Config>>,
    theme: &Theme,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    has_bell: &Rc<RefCell<bool>>,
    file_manager: &Rc<RefCell<PendingFileManager>>,
    notification_bar: &NotificationBar,
    cwd: Option<String>,
    daemon_socket: Option<std::path::PathBuf>,
) {
    let cfg = config.borrow();

    // Get shell basename for initial title
    let shell = cfg
        .general
        .default_shell
        .clone()
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
    let initial_title = std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Terminal")
        .to_string();

    // Build daemon session options — for remote daemons, don't pass local shell/args
    let opts = if daemon_socket.is_some() {
        cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            ..Default::default()
        }
    } else {
        cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            shell: cfg.general.default_shell.clone(),
            args: cfg.general.shell_args.clone(),
            cwd,
            ..Default::default()
        }
    };
    drop(cfg);

    spawn_daemon_tab(
        notebook,
        tabs,
        next_tab_id,
        config,
        theme,
        tab_bar,
        window,
        has_bell,
        file_manager,
        notification_bar,
        opts,
        initial_title,
        None,
        None,
        None,
        false,
        false,
        None,
        daemon_socket,
    );
}

/// Create a new Docker terminal tab (daemon-backed via ctermd)
#[allow(clippy::too_many_arguments)]
fn create_docker_tab(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    next_tab_id: &Rc<RefCell<u64>>,
    config: &Rc<RefCell<Config>>,
    theme: &Theme,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    has_bell: &Rc<RefCell<bool>>,
    file_manager: &Rc<RefCell<PendingFileManager>>,
    notification_bar: &NotificationBar,
    command: &str,
    args: &[String],
    title: &str,
) {
    let opts = cterm_client::CreateSessionOpts {
        cols: 80,
        rows: 24,
        shell: Some(command.to_string()),
        args: args.to_vec(),
        ..Default::default()
    };

    spawn_daemon_tab(
        notebook,
        tabs,
        next_tab_id,
        config,
        theme,
        tab_bar,
        window,
        has_bell,
        file_manager,
        notification_bar,
        opts,
        title.to_string(),
        None,
        Some("#0db7ed".to_string()),
        None,
        false,
        false,
        None,
        None,
    );
}

/// Create a new daemon session beside the active pane in the same tab.
fn inherited_pane_session_options(
    config: &Config,
    remote_backend: bool,
    cwd: Option<String>,
    native_ssh: Option<cterm_client::SshParams>,
    launch_context: Option<&cterm_app::upgrade::PaneLaunchContext>,
) -> cterm_client::CreateSessionOpts {
    let mut options = if remote_backend {
        cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            cwd,
            ..Default::default()
        }
    } else {
        cterm_client::CreateSessionOpts {
            cols: 80,
            rows: 24,
            shell: config.general.default_shell.clone(),
            args: config.general.shell_args.clone(),
            cwd,
            ssh: native_ssh,
            ..Default::default()
        }
    };
    if let Some(launch_context) = launch_context {
        launch_context.apply_to(&mut options);
    }
    options
}

fn spawn_daemon_pane(context: &PaneActionContext, direction: SplitDirection) {
    if reject_managed_secondary_action("pane split") {
        return;
    }

    let Some(page) = context.notebook.current_page() else {
        return;
    };
    let (
        tab_id,
        target,
        cwd,
        daemon_socket,
        remote_name,
        native_ssh,
        launch_context,
        template_name,
        cell_dims,
    ) = {
        let tabs = context.tabs.borrow();
        let Some(tab) = tabs.get(page as usize) else {
            return;
        };
        let pane = tab.panes.active();
        (
            tab.id,
            tab.active_pane_id(),
            pane.terminal.foreground_cwd(),
            pane.daemon_socket.clone(),
            pane.remote_name.clone(),
            pane.native_ssh.clone(),
            pane.launch_context.clone(),
            pane.template_name.clone(),
            pane.terminal.cell_dimensions(),
        )
    };

    if remote_name.is_some() && daemon_socket.is_none() {
        log::error!("Cannot split remote pane without its owning daemon socket");
        return;
    }

    let remote_backend = remote_name.is_some()
        || daemon_socket
            .as_ref()
            .is_some_and(|path| path != &cterm_client::default_socket_path());
    let config = context.config.borrow();
    let shell = config
        .general
        .default_shell
        .clone()
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
    let title = if remote_backend {
        "Terminal".to_string()
    } else {
        std::path::Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Terminal")
            .to_string()
    };
    let mut opts = inherited_pane_session_options(
        &config,
        remote_backend,
        cwd,
        native_ssh,
        launch_context.as_ref(),
    );
    opts.base_palette = Some(frontend_palette(&context.theme, None));
    opts.frontend_state.appearance = context.theme.appearance();
    opts.set_cursor_defaults(
        config.appearance.cursor_style.core_style(),
        config.appearance.cursor_blink,
    );
    opts.pixel_width = (cell_dims.width * 80.0).round().max(1.0) as u32;
    opts.pixel_height = (cell_dims.height * 24.0).round().max(1.0) as u32;
    let split_native_ssh = opts.ssh.clone();
    let split_launch_context = cterm_app::upgrade::PaneLaunchContext::capture(&opts);
    drop(config);

    let context = context.clone();
    let socket_for_connect = daemon_socket.clone();
    let (sender, receiver) = std::sync::mpsc::channel::<DaemonAttachResult>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let result = match runtime {
            Ok(runtime) => runtime.block_on(async {
                let connection = if let Some(path) = socket_for_connect.as_ref() {
                    cterm_client::DaemonConnection::connect_unix(path, false).await?
                } else {
                    cterm_client::DaemonConnection::connect_local().await?
                };
                connection.create_session(opts).await
            }),
            Err(error) => Err(cterm_client::ClientError::Connection(error.to_string())),
        };
        let _ = sender.send(result);
    });

    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return glib::ControlFlow::Break;
            }
        };

        match result {
            Ok(session) => {
                let session_id = Some(session.session_id().to_string());
                let session_socket = session
                    .socket_path()
                    .map(|path| path.to_owned())
                    .or_else(|| daemon_socket.clone());
                let config = context.config.borrow();
                let terminal = Rc::new(TerminalWidget::from_daemon(
                    session,
                    &config,
                    &context.theme,
                ));
                drop(config);

                let request = SplitRequest {
                    direction,
                    placement: SplitPlacement::Second,
                    ratio: SplitRatio::HALF,
                };
                let pane_id = {
                    let mut tabs = context.tabs.borrow_mut();
                    let Some(tab) = tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                        terminal.destroy_session();
                        return glib::ControlFlow::Break;
                    };
                    if tab.panes.get(target).is_none() {
                        terminal.destroy_session();
                        return glib::ControlFlow::Break;
                    }
                    let entry = PaneEntry {
                        terminal: Rc::clone(&terminal),
                        title: title.clone(),
                        session_id: session_id.clone(),
                        daemon_socket: session_socket.clone(),
                        remote_name: remote_name.clone(),
                        native_ssh: split_native_ssh.clone(),
                        launch_context: Some(split_launch_context.clone()),
                        title_locked: false,
                        template_name: template_name.clone(),
                        keep_open: false,
                    };
                    match tab.panes.split(target, request, entry) {
                        Ok(pane_id) => {
                            tab.sync_active_pane(true);
                            pane_id
                        }
                        Err(error) => {
                            log::error!("Failed to split terminal pane: {error}");
                            terminal.destroy_session();
                            return glib::ControlFlow::Break;
                        }
                    }
                };

                setup_tab_callbacks(
                    &context.notebook,
                    &context.tabs,
                    &context.config,
                    &context.tab_bar,
                    &context.window,
                    &context.has_bell,
                    &context.file_manager,
                    &context.notification_bar,
                    &terminal,
                    tab_id,
                    pane_id,
                    false,
                );
                terminal.widget().grab_focus();
            }
            Err(error) => log::error!("Failed to create pane session: {error}"),
        }
        glib::ControlFlow::Break
    });
}

/// Spawn a new daemon-backed tab: connects to ctermd, creates session, and wires up the tab
///
/// Connection priority:
/// 1. `daemon_socket` — connect to a specific socket (e.g. SSH-tunneled remote)
/// 2. `remote` — connect via RemoteManager (template-based remotes)
/// 3. Neither — connect to local daemon
#[allow(clippy::too_many_arguments)]
fn spawn_daemon_tab(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    next_tab_id: &Rc<RefCell<u64>>,
    config: &Rc<RefCell<Config>>,
    theme: &Theme,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    has_bell: &Rc<RefCell<bool>>,
    file_manager: &Rc<RefCell<PendingFileManager>>,
    notification_bar: &NotificationBar,
    mut opts: cterm_client::CreateSessionOpts,
    title: String,
    template_name: Option<String>,
    color: Option<String>,
    background_color: Option<String>,
    keep_open: bool,
    title_locked: bool,
    remote: Option<(cterm_client::RemoteManager, String, String, bool)>,
    daemon_socket: Option<std::path::PathBuf>,
) {
    let native_ssh = opts.ssh.clone();
    let launch_context = cterm_app::upgrade::PaneLaunchContext::capture(&opts);
    opts.base_palette = Some(frontend_palette(
        theme,
        background_color.as_deref().and_then(parse_rgb),
    ));
    opts.frontend_state.appearance = theme.appearance();
    {
        let config = config.borrow();
        opts.set_cursor_defaults(
            config.appearance.cursor_style.core_style(),
            config.appearance.cursor_blink,
        );
    }
    if opts.pixel_width == 0 || opts.pixel_height == 0 {
        let cell_dims = calculate_initial_cell_dimensions(&config.borrow());
        opts.pixel_width = (cell_dims.width * opts.cols.max(1) as f64)
            .round()
            .clamp(1.0, u32::MAX as f64) as u32;
        opts.pixel_height = (cell_dims.height * opts.rows.max(1) as f64)
            .round()
            .clamp(1.0, u32::MAX as f64) as u32;
    }
    let notebook = notebook.clone();
    let tabs = Rc::clone(tabs);
    let next_tab_id = Rc::clone(next_tab_id);
    let config = Rc::clone(config);
    let theme = theme.clone();
    let tab_bar = tab_bar.clone();
    let window = window.clone();
    let has_bell = Rc::clone(has_bell);
    let file_manager = Rc::clone(file_manager);
    let notification_bar = notification_bar.clone();

    // Capture the remote name (if any) for the resulting TabEntry — enables
    // the "Disconnect" right-click menu item.
    let remote_name = remote.as_ref().map(|(_, name, _, _)| name.clone());

    let (tx, rx) = std::sync::mpsc::channel::<DaemonAttachResult>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();

        let result = match rt {
            Ok(rt) => rt.block_on(async {
                let conn = if let Some(ref path) = daemon_socket {
                    cterm_client::DaemonConnection::connect_unix(path, false).await?
                } else if let Some((ref mgr, ref name, ref host, compress)) = remote {
                    mgr.get_or_connect(name, host, compress).await?
                } else {
                    cterm_client::DaemonConnection::connect_local().await?
                };
                let session = conn.create_session(opts).await?;
                Ok(session)
            }),
            Err(e) => Err(cterm_client::ClientError::Connection(e.to_string())),
        };

        let _ = tx.send(result);
    });

    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        match rx.try_recv() {
            Ok(result) => {
                match result {
                    Ok(session) => {
                        let sid = Some(session.session_id().to_string());
                        let daemon_socket = session.socket_path().map(|p| p.to_owned());
                        let cfg = config.borrow();
                        let terminal = TerminalWidget::from_daemon(session, &cfg, &theme);
                        drop(cfg);

                        // Apply background color override from template
                        if let Some(ref bg) = background_color {
                            terminal.set_background_override(Some(bg));
                        }

                        let tab_id = generate_tab_id(&next_tab_id);
                        tab_bar.add_tab(tab_id, &title);

                        if let Some(ref c) = color {
                            tab_bar.set_color(tab_id, Some(c));
                        }

                        setup_tab_callbacks(
                            &notebook,
                            &tabs,
                            &config,
                            &tab_bar,
                            &window,
                            &has_bell,
                            &file_manager,
                            &notification_bar,
                            &terminal,
                            tab_id,
                            PaneLayout::new().active(),
                            keep_open,
                        );

                        finalize_new_tab(
                            &notebook,
                            &tabs,
                            &tab_bar,
                            tab_id,
                            title.clone(),
                            terminal,
                            title_locked,
                            keep_open,
                            sid,
                            daemon_socket,
                            remote_name.clone(),
                            template_name.clone(),
                            native_ssh.clone(),
                            Some(launch_context.clone()),
                        );

                        // Store color in tab entry and send metadata to daemon
                        if color.is_some() {
                            if let Some(tab) = tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id)
                            {
                                tab.color = color.clone();
                            }
                        }
                        // Persist tab metadata to daemon
                        if let Some(tab) = tabs.borrow().iter().find(|t| t.id == tab_id) {
                            if let Some(ref c) = color {
                                tab.terminal.set_tab_color_on_daemon(c);
                            }
                            if !title.is_empty() {
                                tab.terminal.set_template_name_on_daemon(&title);
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to create daemon session: {}", e);
                    }
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

/// Create a new daemon-backed tab by attaching to a session
#[allow(clippy::too_many_arguments)]
fn create_daemon_tab(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    next_tab_id: &Rc<RefCell<u64>>,
    config: &Rc<RefCell<Config>>,
    theme: &Theme,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    has_bell: &Rc<RefCell<bool>>,
    file_manager: &Rc<RefCell<PendingFileManager>>,
    notification_bar: &NotificationBar,
    session_id: &str,
) {
    let cfg = config.borrow();
    let session_id = session_id.to_string();

    let notebook = notebook.clone();
    let tabs = Rc::clone(tabs);
    let next_tab_id = Rc::clone(next_tab_id);
    let config = Rc::clone(config);
    let theme = theme.clone();
    let tab_bar = tab_bar.clone();
    let window = window.clone();
    let has_bell = Rc::clone(has_bell);
    let file_manager = Rc::clone(file_manager);
    let notification_bar = notification_bar.clone();

    // Calculate terminal dimensions from current allocation
    let cell_dims = calculate_initial_cell_dimensions(&cfg);
    let alloc = notebook.allocation();
    let cols = ((alloc.width() as f64) / cell_dims.width).floor().max(80.0) as u32;
    let rows = ((alloc.height() as f64) / cell_dims.height)
        .floor()
        .max(24.0) as u32;
    drop(cfg);

    // Connect and attach in background thread, then create tab on main thread
    let (tx, rx) = std::sync::mpsc::channel::<DaemonAttachResult>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();

        let result = match rt {
            Ok(rt) => rt.block_on(async {
                let conn = cterm_client::DaemonConnection::connect_local().await?;
                let (session, _initial_screen) =
                    conn.attach_session(&session_id, cols, rows).await?;
                Ok(session)
            }),
            Err(e) => Err(cterm_client::ClientError::Connection(e.to_string())),
        };

        let _ = tx.send(result);
    });

    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        match rx.try_recv() {
            Ok(result) => {
                match result {
                    Ok(session) => {
                        let sid = session.session_id().to_string();
                        let title = "Terminal".to_string();
                        let cfg = config.borrow();
                        let terminal = TerminalWidget::from_daemon(session, &cfg, &theme);

                        let tab_id = generate_tab_id(&next_tab_id);
                        tab_bar.add_tab(tab_id, &title);

                        setup_tab_callbacks(
                            &notebook,
                            &tabs,
                            &config,
                            &tab_bar,
                            &window,
                            &has_bell,
                            &file_manager,
                            &notification_bar,
                            &terminal,
                            tab_id,
                            PaneLayout::new().active(),
                            false,
                        );

                        finalize_new_tab(
                            &notebook,
                            &tabs,
                            &tab_bar,
                            tab_id,
                            title,
                            terminal,
                            false,
                            false,
                            Some(sid),
                            None,
                            None,
                            None,
                            None,
                            None,
                        );
                    }
                    Err(e) => {
                        log::error!("Failed to attach to daemon session: {}", e);
                    }
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

type DaemonAttachResult =
    std::result::Result<cterm_client::SessionHandle, cterm_client::ClientError>;

/// Create a new terminal tab from a template
#[allow(clippy::too_many_arguments)]
fn create_tab_from_template(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    next_tab_id: &Rc<RefCell<u64>>,
    config: &Rc<RefCell<Config>>,
    theme: &Theme,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    has_bell: &Rc<RefCell<bool>>,
    file_manager: &Rc<RefCell<PendingFileManager>>,
    notification_bar: &NotificationBar,
    template: &cterm_app::config::StickyTabConfig,
    remote_manager: &cterm_client::RemoteManager,
) {
    // This is the canonical GTK template-launch ingress. Keep the policy here
    // as well as at menu/shortcut actions so dialog and overlay callbacks can
    // never bypass managed mode.
    if reject_managed_secondary_action("template launch") {
        return;
    }

    let (plan, launch_theme) = {
        let cfg = config.borrow();
        let plan = match TemplateLaunchPlan::build(template, &cfg) {
            Ok(plan) => plan,
            Err(error) => {
                log::error!("Cannot launch template '{}': {error}", template.name);
                return;
            }
        };
        let launch_theme = resolve_template_theme(&cfg, theme, plan.appearance.theme.as_deref());
        (plan, launch_theme)
    };

    if focus_reusable_template(notebook, tabs, tab_bar, &plan) {
        log::info!("Focused existing template tab: {}", plan.template_name);
        return;
    }

    // Only an explicit template workspace owned by the local daemon is ever
    // prepared here. Named-remote paths belong to that daemon host.
    if let Some((working_directory, git_remote)) = plan.local_workspace_preparation() {
        if let Err(error) = cterm_app::prepare_working_directory(working_directory, git_remote) {
            log::error!(
                "Failed to prepare workspace for template '{}': {error}",
                plan.template_name
            );
            return;
        }
    }

    let opts = plan.session_options(80, 24);
    let remote = template_remote_details(&plan).map(|(name, host, compression)| {
        (
            remote_manager.clone(),
            name.to_string(),
            host.to_string(),
            compression,
        )
    });

    spawn_daemon_tab(
        notebook,
        tabs,
        next_tab_id,
        config,
        &launch_theme,
        tab_bar,
        window,
        has_bell,
        file_manager,
        notification_bar,
        opts,
        plan.template_name.clone(),
        Some(plan.template_name.clone()),
        plan.appearance.tab_color.clone(),
        plan.appearance.background_color.clone(),
        plan.keep_open,
        false,
        remote,
        None,
    );
}

/// Close current tab (with confirmation if process is running)
fn close_current_tab(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    config: &Rc<RefCell<Config>>,
) {
    if let Some(page_idx) = notebook.current_page() {
        let tab_id = {
            let tabs = tabs.borrow();
            tabs.get(page_idx as usize).map(|t| t.id)
        };
        if let Some(id) = tab_id {
            request_close_tab_by_id(notebook, tabs, tab_bar, window, config, id);
        }
    }
}

/// Close tab by ID (unconditionally - used when process has already exited)
fn close_tab_by_id(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    id: u64,
) {
    // Find index of this tab
    let index = {
        let tabs = tabs.borrow();
        tabs.iter().position(|t| t.id == id)
    };

    let Some(index) = index else { return };

    // Destroy every daemon session owned by the tab.
    {
        let tabs = tabs.borrow();
        for (_, pane) in tabs[index].panes.iter() {
            pane.terminal.destroy_session();
        }
    }

    remove_tab_from_ui(notebook, tabs, tab_bar, window, id);
}

fn destroy_all_pane_sessions(tabs: &Rc<RefCell<Vec<TabEntry>>>) {
    for tab in tabs.borrow().iter() {
        for (_, pane) in tab.panes.iter() {
            pane.terminal.destroy_session();
        }
    }
}

/// Close one pane, or close the tab when it contains only that pane.
fn close_pane_by_id(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    tab_id: u64,
    pane_id: PaneId,
    destroy_session: bool,
) {
    let close_tab = tabs
        .borrow()
        .iter()
        .find(|tab| tab.id == tab_id)
        .is_some_and(|tab| tab.panes.len() == 1 && tab.active_pane_id() == pane_id);
    if close_tab {
        if destroy_session {
            close_tab_by_id(notebook, tabs, tab_bar, window, tab_id);
        } else {
            remove_tab_from_ui(notebook, tabs, tab_bar, window, tab_id);
        }
        return;
    }

    let terminal_to_focus = {
        let mut tabs = tabs.borrow_mut();
        let Some(tab) = tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        let Ok(pane) = tab.panes.close(pane_id) else {
            return;
        };
        if destroy_session {
            pane.terminal.destroy_session();
        }
        tab.sync_active_pane(true);
        Rc::clone(&tab.terminal)
    };
    terminal_to_focus.widget().grab_focus();
}

/// Remove a tab from the UI (notebook, tabs vec, tab bar) WITHOUT issuing any
/// destroy/detach RPC. Closes the window when the last tab is gone.
///
/// Callers that need a graceful daemon-side shutdown must invoke either
/// `terminal.destroy_session()` (kills the PTY) or `terminal.detach_session()`
/// (keeps the PTY) BEFORE calling this.
fn remove_tab_from_ui(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    id: u64,
) {
    let index = {
        let tabs = tabs.borrow();
        tabs.iter().position(|t| t.id == id)
    };
    let Some(index) = index else { return };

    notebook.remove_page(Some(index as u32));
    tabs.borrow_mut().remove(index);
    tab_bar.remove_tab(id);
    tab_bar.update_visibility();

    if tabs.borrow().is_empty() {
        window.close();
        return;
    }

    sync_tab_bar_active(tab_bar, tabs, notebook);

    focus_current_terminal(notebook, tabs);
}

/// Disconnect from a remote: send `detach` to each tab's daemon session
/// (keeps the remote PTYs alive on the server), kill the shared SSH tunnel
/// via `RemoteManager`, and remove every tab from the UI without firing the
/// kill-session RPC.
fn disconnect_remote(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    remote_manager: &cterm_client::RemoteManager,
    remote_name: &str,
) {
    // Snapshot the tab IDs to remove and tell each one's daemon I/O loop to
    // detach (best-effort — the loop sends DetachSession over the still-live
    // shared channel before we kill the tunnel).
    let tab_ids: Vec<u64> = {
        let tabs_ref = tabs.borrow();
        tabs_ref
            .iter()
            .filter_map(|tab| {
                let mut matched = false;
                for (_, pane) in tab.panes.iter() {
                    if pane.remote_name.as_deref() == Some(remote_name) {
                        pane.terminal.detach_session();
                        matched = true;
                    }
                }
                matched.then_some(tab.id)
            })
            .collect()
    };

    if tab_ids.is_empty() {
        return;
    }

    // Tear down the SSH tunnel asynchronously. Fire-and-forget — the tunnel
    // process gets SIGTERM and the gRPC channel breaks; the per-tab reader
    // threads observe the broken stream and exit on their own.
    let mgr = remote_manager.clone();
    let name = remote_name.to_string();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("Failed to build runtime for disconnect: {}", e);
                return;
            }
        };
        rt.block_on(async {
            mgr.disconnect(&name).await;
        });
    });

    // Remove tabs from the UI immediately (don't wait on the disconnect thread).
    for id in tab_ids {
        remove_tab_from_ui(notebook, tabs, tab_bar, window, id);
    }
}

type SessionQueryInfo = (String, Option<std::path::PathBuf>, String);
type PendingOperation = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

/// Query session processes away from the GTK thread, then run an operation
/// after confirmation when any pane still has a foreground process.
#[cfg(unix)]
fn confirm_running_sessions(
    window: &ApplicationWindow,
    sessions: Vec<SessionQueryInfo>,
    operation: impl FnOnce() + 'static,
) {
    if sessions.is_empty() {
        operation();
        return;
    }

    let operation: PendingOperation = Rc::new(RefCell::new(Some(Box::new(operation))));
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let running = match runtime {
            Ok(runtime) => runtime.block_on(async {
                let mut running = Vec::new();
                for (session_id, daemon_socket, title) in sessions {
                    let connection = match if let Some(path) = daemon_socket.as_ref() {
                        cterm_client::DaemonConnection::connect_unix(path, false).await
                    } else {
                        cterm_client::DaemonConnection::connect_local().await
                    } {
                        Ok(connection) => connection,
                        Err(error) => {
                            log::warn!("Could not query pane '{title}' before close: {error}");
                            running
                                .push((title, "process status could not be checked".to_string()));
                            continue;
                        }
                    };
                    let session = match connection.get_session(&session_id).await {
                        Ok(session) => session,
                        Err(error) => {
                            log::warn!("Could not query pane '{title}' before close: {error}");
                            running
                                .push((title, "process status could not be checked".to_string()));
                            continue;
                        }
                    };
                    if session.has_foreground_process {
                        let process = if session.foreground_process_name.is_empty() {
                            "a process".to_string()
                        } else {
                            session.foreground_process_name
                        };
                        running.push((title, process));
                    }
                }
                running
            }),
            Err(error) => {
                log::warn!("Failed to create runtime for close confirmation: {error}");
                vec![(
                    "Terminal".to_string(),
                    "process status could not be checked".to_string(),
                )]
            }
        };
        let _ = result_tx.send(running);
    });

    let window = window.clone();
    let started = std::time::Instant::now();
    glib::timeout_add_local(std::time::Duration::from_millis(25), move || {
        let running = match result_rx.try_recv() {
            Ok(running) => running,
            Err(std::sync::mpsc::TryRecvError::Empty)
                if started.elapsed() < std::time::Duration::from_secs(2) =>
            {
                return glib::ControlFlow::Continue;
            }
            Err(error) => {
                log::warn!("Could not check pane processes before close: {error}");
                vec![(
                    "Terminal".to_string(),
                    "process status check timed out".to_string(),
                )]
            }
        };
        if running.is_empty() {
            if let Some(operation) = operation.borrow_mut().take() {
                operation();
            }
            return glib::ControlFlow::Break;
        }

        let confirmed_operation = Rc::clone(&operation);
        dialogs::show_close_confirmation_dialog(&window, running, move |confirmed| {
            if confirmed {
                if let Some(operation) = confirmed_operation.borrow_mut().take() {
                    operation();
                }
            }
        });
        glib::ControlFlow::Break
    });
}

/// Request to close a tab, aggregating foreground processes from every pane.
#[cfg(unix)]
fn request_close_tab_by_id(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    config: &Rc<RefCell<Config>>,
    id: u64,
) {
    let confirm_close = config.borrow().general.confirm_close_with_running;
    if !confirm_close {
        close_tab_by_id(notebook, tabs, tab_bar, window, id);
        return;
    }

    let sessions: Vec<SessionQueryInfo> = {
        let tabs = tabs.borrow();
        let Some(tab) = tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        tab.panes
            .iter()
            .filter_map(|(_, pane)| {
                pane.session_id
                    .clone()
                    .map(|session_id| (session_id, pane.daemon_socket.clone(), pane.title.clone()))
            })
            .collect()
    };

    let notebook = notebook.clone();
    let tabs = Rc::clone(tabs);
    let tab_bar = tab_bar.clone();
    let close_window = window.clone();
    confirm_running_sessions(window, sessions, move || {
        close_tab_by_id(&notebook, &tabs, &tab_bar, &close_window, id);
    });
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn request_close_pane_by_id(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    config: &Rc<RefCell<Config>>,
    tab_id: u64,
    pane_id: PaneId,
) {
    if !config.borrow().general.confirm_close_with_running {
        close_pane_by_id(notebook, tabs, tab_bar, window, tab_id, pane_id, true);
        return;
    }

    let sessions = {
        let tabs_ref = tabs.borrow();
        let Some(pane) = tabs_ref
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.panes.get(pane_id))
        else {
            return;
        };
        pane.session_id
            .clone()
            .map(|session_id| vec![(session_id, pane.daemon_socket.clone(), pane.title.clone())])
            .unwrap_or_default()
    };

    let notebook = notebook.clone();
    let tabs = Rc::clone(tabs);
    let tab_bar = tab_bar.clone();
    let close_window = window.clone();
    confirm_running_sessions(window, sessions, move || {
        close_pane_by_id(
            &notebook,
            &tabs,
            &tab_bar,
            &close_window,
            tab_id,
            pane_id,
            true,
        );
    });
}

/// Request to close tab by ID - non-Unix fallback (no process detection)
#[cfg(not(unix))]
fn request_close_tab_by_id(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    _config: &Rc<RefCell<Config>>,
    id: u64,
) {
    close_tab_by_id(notebook, tabs, tab_bar, window, id);
}

#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
fn request_close_pane_by_id(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    _config: &Rc<RefCell<Config>>,
    tab_id: u64,
    pane_id: PaneId,
) {
    close_pane_by_id(notebook, tabs, tab_bar, window, tab_id, pane_id, true);
}

/// Close all tabs except the current one after checking every pane process.
#[cfg(unix)]
fn close_other_tabs(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    window: &ApplicationWindow,
    config: &Rc<RefCell<Config>>,
) {
    let Some(current_id) = notebook.current_page().and_then(|page_idx| {
        let tabs = tabs.borrow();
        tabs.get(page_idx as usize).map(|tab| tab.id)
    }) else {
        return;
    };

    let (ids_to_close, sessions): (Vec<u64>, Vec<SessionQueryInfo>) = {
        let tabs = tabs.borrow();
        let other_tabs = tabs.iter().filter(|tab| tab.id != current_id);
        let ids = other_tabs.clone().map(|tab| tab.id).collect();
        let sessions = other_tabs
            .flat_map(|tab| {
                tab.panes.iter().filter_map(|(_, pane)| {
                    pane.session_id.clone().map(|session_id| {
                        (session_id, pane.daemon_socket.clone(), pane.title.clone())
                    })
                })
            })
            .collect();
        (ids, sessions)
    };

    if !config.borrow().general.confirm_close_with_running {
        close_other_tabs_now(notebook, tabs, tab_bar, &ids_to_close);
        return;
    }

    let notebook = notebook.clone();
    let tabs = Rc::clone(tabs);
    let tab_bar = tab_bar.clone();
    confirm_running_sessions(window, sessions, move || {
        close_other_tabs_now(&notebook, &tabs, &tab_bar, &ids_to_close);
    });
}

#[cfg(not(unix))]
fn close_other_tabs(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    _window: &ApplicationWindow,
    _config: &Rc<RefCell<Config>>,
) {
    let Some(current_id) = notebook.current_page().and_then(|page_idx| {
        let tabs = tabs.borrow();
        tabs.get(page_idx as usize).map(|tab| tab.id)
    }) else {
        return;
    };
    let ids_to_close = tabs
        .borrow()
        .iter()
        .filter(|tab| tab.id != current_id)
        .map(|tab| tab.id)
        .collect::<Vec<_>>();
    close_other_tabs_now(notebook, tabs, tab_bar, &ids_to_close);
}

fn close_other_tabs_now(
    notebook: &Notebook,
    tabs: &Rc<RefCell<Vec<TabEntry>>>,
    tab_bar: &TabBar,
    ids_to_close: &[u64],
) {
    for id in ids_to_close {
        let index = {
            let tabs = tabs.borrow();
            tabs.iter().position(|tab| tab.id == *id)
        };

        if let Some(index) = index {
            {
                let tabs_ref = tabs.borrow();
                for (_, pane) in tabs_ref[index].panes.iter() {
                    pane.terminal.destroy_session();
                }
            }
            notebook.remove_page(Some(index as u32));
            tabs.borrow_mut().remove(index);
            tab_bar.remove_tab(*id);
        }
    }

    // Update tab bar visibility (hide if only one tab)
    tab_bar.update_visibility();

    // Update active tab in tab bar
    sync_tab_bar_active(tab_bar, tabs, notebook);
}

/// Sync tab bar active state with notebook
/// Focus the active terminal in the currently visible notebook page.
fn focus_current_terminal(notebook: &Notebook, tabs: &Rc<RefCell<Vec<TabEntry>>>) {
    let widget = notebook.current_page().and_then(|page| {
        tabs.borrow()
            .get(page as usize)
            .map(|tab| tab.terminal.widget().clone())
    });
    if let Some(widget) = widget {
        // `grab_focus` synchronously emits `notify::has-focus`. Drop the tab
        // registry borrow first because that callback activates the pane and
        // therefore needs a mutable borrow of the same registry.
        widget.grab_focus();
    }
}

fn sync_tab_bar_active(tab_bar: &TabBar, tabs: &Rc<RefCell<Vec<TabEntry>>>, notebook: &Notebook) {
    if let Some(page_idx) = notebook.current_page() {
        let tabs = tabs.borrow();
        if let Some(tab) = tabs.get(page_idx as usize) {
            tab_bar.set_active(tab.id);
            // Clear bell when tab becomes active
            tab_bar.clear_bell(tab.id);
            tab.terminal.clear_alert();
        }
    }
}

/// Convert GTK modifier state to our Modifiers
fn gtk_modifiers_to_modifiers(state: gdk::ModifierType) -> Modifiers {
    let mut modifiers = Modifiers::empty();

    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        modifiers.insert(Modifiers::CTRL);
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        modifiers.insert(Modifiers::SHIFT);
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        modifiers.insert(Modifiers::ALT);
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        modifiers.insert(Modifiers::SUPER);
    }

    modifiers
}

/// Convert GDK keyval to our KeyCode
fn keyval_to_keycode(keyval: gdk::Key) -> Option<KeyCode> {
    use gdk::Key;

    Some(match keyval {
        Key::a | Key::A => KeyCode::A,
        Key::b | Key::B => KeyCode::B,
        Key::c | Key::C => KeyCode::C,
        Key::d | Key::D => KeyCode::D,
        Key::e | Key::E => KeyCode::E,
        Key::f | Key::F => KeyCode::F,
        Key::g | Key::G => KeyCode::G,
        Key::h | Key::H => KeyCode::H,
        Key::i | Key::I => KeyCode::I,
        Key::j | Key::J => KeyCode::J,
        Key::k | Key::K => KeyCode::K,
        Key::l | Key::L => KeyCode::L,
        Key::m | Key::M => KeyCode::M,
        Key::n | Key::N => KeyCode::N,
        Key::o | Key::O => KeyCode::O,
        Key::p | Key::P => KeyCode::P,
        Key::q | Key::Q => KeyCode::Q,
        Key::r | Key::R => KeyCode::R,
        Key::s | Key::S => KeyCode::S,
        Key::t | Key::T => KeyCode::T,
        Key::u | Key::U => KeyCode::U,
        Key::v | Key::V => KeyCode::V,
        Key::w | Key::W => KeyCode::W,
        Key::x | Key::X => KeyCode::X,
        Key::y | Key::Y => KeyCode::Y,
        Key::z | Key::Z => KeyCode::Z,
        Key::_0 => KeyCode::Key0,
        Key::_1 => KeyCode::Key1,
        Key::_2 => KeyCode::Key2,
        Key::_3 => KeyCode::Key3,
        Key::_4 => KeyCode::Key4,
        Key::_5 => KeyCode::Key5,
        Key::_6 => KeyCode::Key6,
        Key::_7 => KeyCode::Key7,
        Key::_8 => KeyCode::Key8,
        Key::_9 => KeyCode::Key9,
        Key::F1 => KeyCode::F1,
        Key::F2 => KeyCode::F2,
        Key::F3 => KeyCode::F3,
        Key::F4 => KeyCode::F4,
        Key::F5 => KeyCode::F5,
        Key::F6 => KeyCode::F6,
        Key::F7 => KeyCode::F7,
        Key::F8 => KeyCode::F8,
        Key::F9 => KeyCode::F9,
        Key::F10 => KeyCode::F10,
        Key::F11 => KeyCode::F11,
        Key::F12 => KeyCode::F12,
        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::Page_Up | Key::KP_Page_Up => KeyCode::PageUp,
        Key::Page_Down | Key::KP_Page_Down => KeyCode::PageDown,
        Key::Insert => KeyCode::Insert,
        Key::Delete => KeyCode::Delete,
        Key::BackSpace => KeyCode::Backspace,
        Key::Return | Key::KP_Enter => KeyCode::Enter,
        Key::Tab | Key::ISO_Left_Tab => KeyCode::Tab,
        Key::Escape => KeyCode::Escape,
        Key::space => KeyCode::Space,
        Key::minus | Key::underscore => KeyCode::Minus,
        Key::equal | Key::plus => KeyCode::Equals,
        Key::comma => KeyCode::Comma,
        Key::period => KeyCode::Period,
        Key::slash => KeyCode::Slash,
        Key::backslash | Key::bar => KeyCode::Backslash,
        Key::semicolon => KeyCode::Semicolon,
        Key::apostrophe => KeyCode::Quote,
        Key::bracketleft => KeyCode::LeftBracket,
        Key::bracketright => KeyCode::RightBracket,
        Key::grave => KeyCode::Backquote,
        _ => return None,
    })
}

/// Calculate initial cell dimensions for window sizing
/// Uses Pango font metrics to get accurate measurements
fn calculate_initial_cell_dimensions(config: &Config) -> CellDimensions {
    use gtk4::pango;

    let font_family = &config.appearance.font.family;
    let font_size = config.appearance.font.size;

    // Get the default font map and create a context
    let font_map = pangocairo::FontMap::default();
    let context = font_map.create_context();

    // Try the requested font first, then fall back to generic monospace
    let fonts_to_try = [font_family.to_string(), "monospace".to_string()];

    for font_name in &fonts_to_try {
        let font_desc =
            pango::FontDescription::from_string(&format!("{} {}", font_name, font_size));

        if let Some(font) = font_map.load_font(&context, &font_desc) {
            let metrics = font.metrics(None);
            let char_width = metrics.approximate_char_width() as f64 / pango::SCALE as f64;
            let ascent = metrics.ascent() as f64 / pango::SCALE as f64;
            let descent = metrics.descent() as f64 / pango::SCALE as f64;
            let height = ascent + descent;

            if char_width > 0.0 && height > 0.0 {
                return CellDimensions {
                    width: char_width,
                    height: height * 1.1,
                };
            }
        }
    }

    // Last resort: use a Pango layout to measure a character directly
    let layout = pango::Layout::new(&context);
    let font_desc = pango::FontDescription::from_string(&format!("monospace {}", font_size));
    layout.set_font_description(Some(&font_desc));
    layout.set_text("M");

    let (width, height) = layout.pixel_size();
    if width > 0 && height > 0 {
        return CellDimensions {
            width: width as f64,
            height: height as f64 * 1.1,
        };
    }

    panic!(
        "Failed to load any font or measure text. \
         Please ensure fonts are installed (e.g., fonts-dejavu or similar)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_split_inherits_shell_arguments_and_cwd() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/fish".into());
        config.general.shell_args = vec!["--login".into()];

        let options =
            inherited_pane_session_options(&config, false, Some("/work/tree".into()), None, None);
        assert_eq!(options.shell.as_deref(), Some("/bin/fish"));
        assert_eq!(options.args, ["--login"]);
        assert_eq!(options.cwd.as_deref(), Some("/work/tree"));
    }

    #[test]
    fn remote_split_inherits_cwd_without_injecting_a_local_shell() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/local-only".into());
        config.general.shell_args = vec!["--must-not-cross-ssh".into()];

        let options =
            inherited_pane_session_options(&config, true, Some("/srv/remote".into()), None, None);
        assert!(options.shell.is_none());
        assert!(options.args.is_empty());
        assert_eq!(options.cwd.as_deref(), Some("/srv/remote"));
    }

    #[test]
    fn daemon_side_ssh_split_keeps_its_native_ssh_target() {
        let config = Config::default();
        let ssh = cterm_client::SshParams {
            host: "shell.example".into(),
            ..Default::default()
        };

        let options = inherited_pane_session_options(
            &config,
            false,
            Some("/srv/project".into()),
            Some(ssh),
            None,
        );
        assert_eq!(
            options.ssh.as_ref().map(|params| params.host.as_str()),
            Some("shell.example")
        );
        assert_eq!(options.cwd.as_deref(), Some("/srv/project"));
    }

    #[test]
    fn restored_launch_context_overrides_changed_local_defaults() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/new-default".into());
        config.general.shell_args = vec!["--new-default".into()];
        let original = cterm_client::CreateSessionOpts {
            shell: Some("/bin/fish".into()),
            args: vec!["--login".into()],
            env: vec![("PANE_TEST".into(), "preserved".into())],
            term: Some("xterm-256color".into()),
            ..Default::default()
        };
        let launch_context = cterm_app::upgrade::PaneLaunchContext::capture(&original);

        let options = inherited_pane_session_options(
            &config,
            false,
            Some("/work/current".into()),
            None,
            Some(&launch_context),
        );

        assert_eq!(options.shell.as_deref(), Some("/bin/fish"));
        assert_eq!(options.args, ["--login"]);
        assert_eq!(options.env, [("PANE_TEST".into(), "preserved".into())]);
        assert_eq!(options.term.as_deref(), Some("xterm-256color"));
        assert_eq!(options.cwd.as_deref(), Some("/work/current"));
    }

    #[test]
    fn shifted_split_keyvals_map_to_their_configured_physical_keys() {
        assert_eq!(keyval_to_keycode(gdk::Key::bar), Some(KeyCode::Backslash));
        assert_eq!(
            keyval_to_keycode(gdk::Key::underscore),
            Some(KeyCode::Minus)
        );
    }

    #[test]
    fn formerly_missing_shortcut_actions_map_to_native_gtk_actions() {
        for (action, name) in [
            (Action::SelectAll, "select-all"),
            (Action::ToggleFullscreen, "toggle-fullscreen"),
            (Action::OpenPreferences, "preferences"),
            (Action::FindText, "find"),
            (Action::ResetTerminal, "reset"),
        ] {
            assert_eq!(
                gtk_action_activation(&action),
                GtkActionActivation::simple(name)
            );
        }
    }

    #[test]
    fn parameterized_actions_preserve_their_native_parameters() {
        assert_eq!(
            gtk_action_activation(&Action::Tab(7)),
            GtkActionActivation::with_string("select-tab-index", "7")
        );
        assert_eq!(
            gtk_action_activation(&Action::SplitPane(SplitDirection::Horizontal)),
            GtkActionActivation::simple("split-pane-horizontal")
        );
        assert_eq!(
            gtk_action_activation(&Action::FocusPane(PaneDirection::Down)),
            GtkActionActivation::simple("focus-pane-down")
        );
    }

    #[test]
    fn managed_policy_blocks_secondary_session_and_configuration_actions() {
        for action in [
            Action::NewTab,
            Action::SplitPane(SplitDirection::Vertical),
            Action::ClosePane,
            Action::FocusPane(PaneDirection::Left),
            Action::ResizePane(PaneDirection::Right),
            Action::TogglePaneZoom,
            Action::NewWindow,
            Action::OpenPreferences,
            Action::QuickOpenTemplate,
        ] {
            assert!(is_managed_restricted_action(&action), "{action:?}");
        }

        for action in [
            Action::CloseTab,
            Action::CloseWindow,
            Action::Copy,
            Action::SelectAll,
            Action::ToggleFullscreen,
            Action::FindText,
            Action::ResetTerminal,
        ] {
            assert!(!is_managed_restricted_action(&action), "{action:?}");
        }
    }

    #[test]
    fn template_adapter_preserves_local_ssh_and_named_daemon_intent() {
        use cterm_app::config::{RemoteConfig, SshTabConfig, StickyTabConfig};

        let ssh_template = StickyTabConfig {
            name: "Production shell".into(),
            ssh: Some(SshTabConfig {
                host: "shell.example".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let local_plan = TemplateLaunchPlan::build(&ssh_template, &Config::default()).unwrap();
        let local_options = local_plan.session_options(80, 24);
        assert!(template_remote_details(&local_plan).is_none());
        assert_eq!(
            local_options.ssh.as_ref().map(|ssh| ssh.host.as_str()),
            Some("shell.example")
        );

        let mut config = Config::default();
        config.remotes.push(RemoteConfig {
            name: "build".into(),
            host: "dev@build.example".into(),
            ssh_compression: false,
        });
        let remote_template = StickyTabConfig {
            name: "Remote build".into(),
            command: Some("just".into()),
            args: vec!["test".into()],
            working_directory: Some("/srv/project".into()),
            remote: Some("build".into()),
            ..Default::default()
        };
        let remote_plan = TemplateLaunchPlan::build(&remote_template, &config).unwrap();
        assert_eq!(
            template_remote_details(&remote_plan),
            Some(("build", "dev@build.example", false))
        );
        assert!(remote_plan.local_workspace_preparation().is_none());
        let remote_options = remote_plan.session_options(80, 24);
        assert_eq!(remote_options.shell.as_deref(), Some("just"));
        assert_eq!(remote_options.args, ["test"]);
        assert_eq!(remote_options.cwd.as_deref(), Some("/srv/project"));
    }

    #[test]
    fn template_adapter_uses_docker_argv_and_plan_metadata() {
        use cterm_app::config::{DockerMode, DockerTabConfig, StickyTabConfig};

        let template = StickyTabConfig {
            name: "Alpine".into(),
            color: Some("#0db7ed".into()),
            keep_open: true,
            docker: Some(DockerTabConfig {
                mode: DockerMode::Run,
                image: Some("alpine:latest".into()),
                shell: Some("/bin/ash".into()),
                auto_remove: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &Config::default()).unwrap();
        let options = plan.session_options(80, 24);
        assert_eq!(options.shell.as_deref(), Some("docker"));
        assert_eq!(
            options.args,
            ["run", "-it", "--rm", "alpine:latest", "/bin/ash"]
        );
        assert_eq!(plan.appearance.tab_color.as_deref(), Some("#0db7ed"));
        assert!(plan.keep_open);
    }

    #[test]
    fn unique_template_policy_reuses_only_matching_template_identity() {
        let candidates = [
            (0, 11, Some("Editor")),
            (1, 22, Some("Logs")),
            (2, 33, None),
        ];
        assert_eq!(
            reusable_template_location(TemplateInstancePolicy::ReuseExisting, "Logs", candidates),
            Some((1, 22))
        );
        assert_eq!(
            reusable_template_location(TemplateInstancePolicy::AlwaysCreate, "Logs", candidates),
            None
        );
    }

    #[test]
    fn template_theme_override_is_scoped_and_unknown_names_fall_back() {
        let mut config = Config::default();
        config.appearance.custom_theme = Some(Theme::light());
        let window_theme = Theme::tokyo_night();

        let nord = resolve_template_theme(&config, &window_theme, Some("nord"));
        assert_eq!(nord.name, "Nord");
        let custom = resolve_template_theme(&config, &window_theme, Some("custom"));
        assert_eq!(custom.name, "Default Light");
        let unknown = resolve_template_theme(&config, &window_theme, Some("does-not-exist"));
        assert_eq!(unknown.name, "Tokyo Night");
        assert_eq!(window_theme.name, "Tokyo Night");
    }

    #[test]
    fn gtk_provides_the_conventional_fullscreen_shortcut() {
        let shortcuts = shortcut_manager(&Config::default());
        assert_eq!(
            shortcuts
                .match_event(KeyCode::F11, Modifiers::empty())
                .cloned(),
            Some(Action::ToggleFullscreen)
        );
    }
}
