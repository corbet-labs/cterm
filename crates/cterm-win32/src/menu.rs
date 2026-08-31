//! Win32 menu creation and handling
//!
//! Creates the application menu bar with all menu items.

use std::ptr;

use cterm_ui::events::Action;
use cterm_ui::pane::{PaneDirection, SplitDirection};
use winapi::shared::windef::HMENU;
use winapi::um::winuser::{
    AppendMenuW, CreateMenu, CreatePopupMenu, SetMenu, MF_POPUP, MF_SEPARATOR, MF_STRING,
};

/// Menu action identifiers
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    // File menu
    NewTab = 1001,
    NewWindow = 1002,
    QuickOpen = 1007,
    CloseTab = 1003,
    CloseOtherTabs = 1004,
    DockerPicker = 1005,
    Quit = 1006,

    // Edit menu
    Copy = 2001,
    CopyHtml = 2002,
    Paste = 2003,
    SelectAll = 2004,

    // View menu
    ZoomIn = 2501,
    ZoomOut = 2502,
    ZoomReset = 2503,
    Fullscreen = 2504,

    // Terminal menu
    SetTitle = 3001,
    SetColor = 3002,
    Find = 3003,
    Reset = 3004,
    ClearReset = 3005,
    SendSignalInt = 3006,
    SendSignalKill = 3007,
    SendSignalHup = 3008,
    SendSignalTerm = 3009,
    SplitPaneHorizontal = 3101,
    SplitPaneVertical = 3102,
    ClosePane = 3103,
    FocusPaneLeft = 3111,
    FocusPaneRight = 3112,
    FocusPaneUp = 3113,
    FocusPaneDown = 3114,
    ResizePaneLeft = 3121,
    ResizePaneRight = 3122,
    ResizePaneUp = 3123,
    ResizePaneDown = 3124,
    TogglePaneZoom = 3131,

    // Tabs menu
    PrevTab = 4001,
    NextTab = 4002,
    NextAlertedTab = 4003,
    Tab1 = 4011,
    Tab2 = 4012,
    Tab3 = 4013,
    Tab4 = 4014,
    Tab5 = 4015,
    Tab6 = 4016,
    Tab7 = 4017,
    Tab8 = 4018,
    Tab9 = 4019,

    // Help menu
    Preferences = 5001,
    CheckUpdates = 5002,
    TabTemplates = 5003,
    About = 5004,

    // Sessions menu
    AttachSession = 7001,
    SSHConnect = 7002,
    ManageRemotes = 7003,

    // Debug menu (shown when Shift is held)
    DebugRelaunch = 6001,
    DebugDumpState = 6002,
    ViewLogs = 6003,
    DebugRelaunchDaemon = 6004,
    KillDaemon = 6005,
}

impl MenuAction {
    /// Convert from u16 ID
    pub fn from_id(id: u16) -> Option<Self> {
        match id {
            1001 => Some(Self::NewTab),
            1002 => Some(Self::NewWindow),
            1007 => Some(Self::QuickOpen),
            1003 => Some(Self::CloseTab),
            1004 => Some(Self::CloseOtherTabs),
            1005 => Some(Self::DockerPicker),
            1006 => Some(Self::Quit),
            2001 => Some(Self::Copy),
            2002 => Some(Self::CopyHtml),
            2003 => Some(Self::Paste),
            2004 => Some(Self::SelectAll),
            2501 => Some(Self::ZoomIn),
            2502 => Some(Self::ZoomOut),
            2503 => Some(Self::ZoomReset),
            2504 => Some(Self::Fullscreen),
            3001 => Some(Self::SetTitle),
            3002 => Some(Self::SetColor),
            3003 => Some(Self::Find),
            3004 => Some(Self::Reset),
            3005 => Some(Self::ClearReset),
            3006 => Some(Self::SendSignalInt),
            3007 => Some(Self::SendSignalKill),
            3008 => Some(Self::SendSignalHup),
            3009 => Some(Self::SendSignalTerm),
            3101 => Some(Self::SplitPaneHorizontal),
            3102 => Some(Self::SplitPaneVertical),
            3103 => Some(Self::ClosePane),
            3111 => Some(Self::FocusPaneLeft),
            3112 => Some(Self::FocusPaneRight),
            3113 => Some(Self::FocusPaneUp),
            3114 => Some(Self::FocusPaneDown),
            3121 => Some(Self::ResizePaneLeft),
            3122 => Some(Self::ResizePaneRight),
            3123 => Some(Self::ResizePaneUp),
            3124 => Some(Self::ResizePaneDown),
            3131 => Some(Self::TogglePaneZoom),
            4001 => Some(Self::PrevTab),
            4002 => Some(Self::NextTab),
            4003 => Some(Self::NextAlertedTab),
            4011 => Some(Self::Tab1),
            4012 => Some(Self::Tab2),
            4013 => Some(Self::Tab3),
            4014 => Some(Self::Tab4),
            4015 => Some(Self::Tab5),
            4016 => Some(Self::Tab6),
            4017 => Some(Self::Tab7),
            4018 => Some(Self::Tab8),
            4019 => Some(Self::Tab9),
            5001 => Some(Self::Preferences),
            5002 => Some(Self::CheckUpdates),
            5003 => Some(Self::TabTemplates),
            5004 => Some(Self::About),
            7001 => Some(Self::AttachSession),
            7002 => Some(Self::SSHConnect),
            7003 => Some(Self::ManageRemotes),
            6001 => Some(Self::DebugRelaunch),
            6002 => Some(Self::DebugDumpState),
            6003 => Some(Self::ViewLogs),
            6004 => Some(Self::DebugRelaunchDaemon),
            6005 => Some(Self::KillDaemon),
            _ => None,
        }
    }

    /// Get the ID for this action
    pub fn id(self) -> u16 {
        self as u16
    }

    /// Convert menu commands backed by the shared action model.
    ///
    /// Commands that expose Win32-only behavior remain in the native menu
    /// dispatcher and return `None`.
    pub fn shared_action(self) -> Option<Action> {
        match self {
            Self::NewTab => Some(Action::NewTab),
            Self::NewWindow => Some(Action::NewWindow),
            Self::QuickOpen => Some(Action::QuickOpenTemplate),
            Self::CloseTab => Some(Action::CloseTab),
            Self::Quit => Some(Action::CloseWindow),
            Self::Copy => Some(Action::Copy),
            Self::Paste => Some(Action::Paste),
            Self::SelectAll => Some(Action::SelectAll),
            Self::ZoomIn => Some(Action::ZoomIn),
            Self::ZoomOut => Some(Action::ZoomOut),
            Self::ZoomReset => Some(Action::ZoomReset),
            Self::Fullscreen => Some(Action::ToggleFullscreen),
            Self::Find => Some(Action::FindText),
            Self::Reset => Some(Action::ResetTerminal),
            Self::SplitPaneHorizontal => Some(Action::SplitPane(SplitDirection::Horizontal)),
            Self::SplitPaneVertical => Some(Action::SplitPane(SplitDirection::Vertical)),
            Self::ClosePane => Some(Action::ClosePane),
            Self::FocusPaneLeft => Some(Action::FocusPane(PaneDirection::Left)),
            Self::FocusPaneRight => Some(Action::FocusPane(PaneDirection::Right)),
            Self::FocusPaneUp => Some(Action::FocusPane(PaneDirection::Up)),
            Self::FocusPaneDown => Some(Action::FocusPane(PaneDirection::Down)),
            Self::ResizePaneLeft => Some(Action::ResizePane(PaneDirection::Left)),
            Self::ResizePaneRight => Some(Action::ResizePane(PaneDirection::Right)),
            Self::ResizePaneUp => Some(Action::ResizePane(PaneDirection::Up)),
            Self::ResizePaneDown => Some(Action::ResizePane(PaneDirection::Down)),
            Self::TogglePaneZoom => Some(Action::TogglePaneZoom),
            Self::PrevTab => Some(Action::PrevTab),
            Self::NextTab => Some(Action::NextTab),
            Self::NextAlertedTab => Some(Action::NextAlertedTab),
            Self::Tab1 => Some(Action::Tab(1)),
            Self::Tab2 => Some(Action::Tab(2)),
            Self::Tab3 => Some(Action::Tab(3)),
            Self::Tab4 => Some(Action::Tab(4)),
            Self::Tab5 => Some(Action::Tab(5)),
            Self::Tab6 => Some(Action::Tab(6)),
            Self::Tab7 => Some(Action::Tab(7)),
            Self::Tab8 => Some(Action::Tab(8)),
            Self::Tab9 => Some(Action::Tab(9)),
            Self::Preferences => Some(Action::OpenPreferences),
            Self::CloseOtherTabs
            | Self::DockerPicker
            | Self::CopyHtml
            | Self::SetTitle
            | Self::SetColor
            | Self::ClearReset
            | Self::SendSignalInt
            | Self::SendSignalKill
            | Self::SendSignalHup
            | Self::SendSignalTerm
            | Self::TabTemplates
            | Self::CheckUpdates
            | Self::About
            | Self::AttachSession
            | Self::SSHConnect
            | Self::ManageRemotes
            | Self::DebugRelaunch
            | Self::DebugDumpState
            | Self::ViewLogs
            | Self::DebugRelaunchDaemon
            | Self::KillDaemon => None,
        }
    }
}

/// Convert a Rust string to a null-terminated wide string
fn to_wide_string(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Create the main menu bar
pub fn create_menu_bar(show_debug: bool, updates_enabled: bool, managed: bool) -> HMENU {
    unsafe {
        let menu_bar = CreateMenu();

        // File menu
        let file_menu = CreatePopupMenu();
        if !managed {
            append_menu_item(file_menu, MenuAction::NewTab, "&New Tab\tCtrl+T");
            append_menu_item(file_menu, MenuAction::NewWindow, "New &Window\tCtrl+N");
            append_menu_item(file_menu, MenuAction::QuickOpen, "&Quick Open\tCtrl+G");
            append_separator(file_menu);
        }
        append_menu_item(file_menu, MenuAction::CloseTab, "&Close Tab\tCtrl+W");
        append_menu_item(file_menu, MenuAction::CloseOtherTabs, "Close &Other Tabs");
        if !managed {
            append_separator(file_menu);
            append_menu_item(file_menu, MenuAction::DockerPicker, "&Docker...");

            // Sessions submenu
            let sessions_menu = CreatePopupMenu();
            append_menu_item(
                sessions_menu,
                MenuAction::AttachSession,
                "&Attach to Session...",
            );
            append_menu_item(sessions_menu, MenuAction::SSHConnect, "&SSH Remote...");
            append_menu_item(
                sessions_menu,
                MenuAction::ManageRemotes,
                "&Manage Remotes...",
            );
            append_popup_menu(file_menu, sessions_menu, "S&essions");
        }

        append_separator(file_menu);
        append_menu_item(file_menu, MenuAction::Quit, "&Quit\tAlt+F4");
        append_popup_menu(menu_bar, file_menu, "&File");

        // Edit menu
        let edit_menu = CreatePopupMenu();
        append_menu_item(edit_menu, MenuAction::Copy, "&Copy\tCtrl+Shift+C");
        append_menu_item(edit_menu, MenuAction::CopyHtml, "Copy as &HTML");
        append_menu_item(edit_menu, MenuAction::Paste, "&Paste\tCtrl+Shift+V");
        append_separator(edit_menu);
        append_menu_item(
            edit_menu,
            MenuAction::SelectAll,
            "Select &All\tCtrl+Shift+A",
        );
        append_popup_menu(menu_bar, edit_menu, "&Edit");

        // View menu
        let view_menu = CreatePopupMenu();
        append_menu_item(view_menu, MenuAction::ZoomIn, "Zoom &In\tCtrl++");
        append_menu_item(view_menu, MenuAction::ZoomOut, "Zoom &Out\tCtrl+-");
        append_menu_item(view_menu, MenuAction::ZoomReset, "&Reset Zoom\tCtrl+0");
        append_separator(view_menu);
        append_menu_item(view_menu, MenuAction::Fullscreen, "&Fullscreen\tF11");
        append_popup_menu(menu_bar, view_menu, "&View");

        // Terminal menu
        let terminal_menu = CreatePopupMenu();
        append_menu_item(terminal_menu, MenuAction::SetTitle, "Set &Title...");
        append_menu_item(terminal_menu, MenuAction::SetColor, "Set &Color...");
        append_separator(terminal_menu);
        if !managed {
            let pane_menu = CreatePopupMenu();
            append_menu_item(
                pane_menu,
                MenuAction::SplitPaneHorizontal,
                "Split &Left/Right\tCtrl+Shift+\\",
            );
            append_menu_item(
                pane_menu,
                MenuAction::SplitPaneVertical,
                "Split &Top/Bottom\tCtrl+Shift+-",
            );
            append_menu_item(
                pane_menu,
                MenuAction::ClosePane,
                "&Close Pane\tCtrl+Shift+Delete",
            );
            append_separator(pane_menu);
            append_menu_item(pane_menu, MenuAction::FocusPaneLeft, "Focus &Left");
            append_menu_item(pane_menu, MenuAction::FocusPaneRight, "Focus &Right");
            append_menu_item(pane_menu, MenuAction::FocusPaneUp, "Focus &Up");
            append_menu_item(pane_menu, MenuAction::FocusPaneDown, "Focus &Down");
            append_separator(pane_menu);
            append_menu_item(pane_menu, MenuAction::ResizePaneLeft, "Resize Left");
            append_menu_item(pane_menu, MenuAction::ResizePaneRight, "Resize Right");
            append_menu_item(pane_menu, MenuAction::ResizePaneUp, "Resize Up");
            append_menu_item(pane_menu, MenuAction::ResizePaneDown, "Resize Down");
            append_separator(pane_menu);
            append_menu_item(
                pane_menu,
                MenuAction::TogglePaneZoom,
                "Toggle Pane &Zoom\tCtrl+Shift+Enter",
            );
            append_popup_menu(terminal_menu, pane_menu, "&Panes");
            append_separator(terminal_menu);
        }
        append_menu_item(terminal_menu, MenuAction::Find, "&Find...\tCtrl+Shift+F");
        append_separator(terminal_menu);

        // Signal submenu
        let signal_menu = CreatePopupMenu();
        append_menu_item(
            signal_menu,
            MenuAction::SendSignalInt,
            "&Interrupt (SIGINT)",
        );
        append_menu_item(signal_menu, MenuAction::SendSignalKill, "&Kill (SIGKILL)");
        append_menu_item(signal_menu, MenuAction::SendSignalHup, "&Hangup (SIGHUP)");
        append_menu_item(
            signal_menu,
            MenuAction::SendSignalTerm,
            "&Terminate (SIGTERM)",
        );
        append_popup_menu(terminal_menu, signal_menu, "Send &Signal");

        append_separator(terminal_menu);
        append_menu_item(terminal_menu, MenuAction::Reset, "&Reset Terminal");
        append_menu_item(terminal_menu, MenuAction::ClearReset, "Clear and R&eset");
        append_popup_menu(menu_bar, terminal_menu, "&Terminal");

        // Tabs menu
        let tabs_menu = CreatePopupMenu();
        append_menu_item(
            tabs_menu,
            MenuAction::PrevTab,
            "&Previous Tab\tCtrl+Shift+Tab",
        );
        append_menu_item(tabs_menu, MenuAction::NextTab, "&Next Tab\tCtrl+Tab");
        append_separator(tabs_menu);
        append_menu_item(
            tabs_menu,
            MenuAction::NextAlertedTab,
            "Next &Alerted Tab\tCtrl+Shift+B",
        );
        append_separator(tabs_menu);
        append_menu_item(tabs_menu, MenuAction::Tab1, "Tab &1\tAlt+1");
        append_menu_item(tabs_menu, MenuAction::Tab2, "Tab &2\tAlt+2");
        append_menu_item(tabs_menu, MenuAction::Tab3, "Tab &3\tAlt+3");
        append_menu_item(tabs_menu, MenuAction::Tab4, "Tab &4\tAlt+4");
        append_menu_item(tabs_menu, MenuAction::Tab5, "Tab &5\tAlt+5");
        append_menu_item(tabs_menu, MenuAction::Tab6, "Tab &6\tAlt+6");
        append_menu_item(tabs_menu, MenuAction::Tab7, "Tab &7\tAlt+7");
        append_menu_item(tabs_menu, MenuAction::Tab8, "Tab &8\tAlt+8");
        append_menu_item(tabs_menu, MenuAction::Tab9, "Tab &9\tAlt+9");
        append_popup_menu(menu_bar, tabs_menu, "T&abs");

        // Help menu
        let help_menu = CreatePopupMenu();
        if !managed {
            append_menu_item(help_menu, MenuAction::Preferences, "&Preferences...");
            append_menu_item(help_menu, MenuAction::TabTemplates, "&Tab Templates...");
        }
        if updates_enabled {
            append_separator(help_menu);
            append_menu_item(help_menu, MenuAction::CheckUpdates, "Check for &Updates...");
        }
        if !managed || updates_enabled {
            append_separator(help_menu);
        }
        append_menu_item(help_menu, MenuAction::About, "&About cterm");
        append_popup_menu(menu_bar, help_menu, "&Help");

        // Debug menu (only shown when Shift is held)
        if show_debug && !managed {
            let debug_menu = CreatePopupMenu();
            append_menu_item(debug_menu, MenuAction::ViewLogs, "&View Logs...");
            append_menu_item(debug_menu, MenuAction::DebugDumpState, "&Dump State");
            append_separator(debug_menu);
            append_menu_item(
                debug_menu,
                MenuAction::DebugRelaunch,
                "&Re-launch (Test Upgrade)",
            );
            append_menu_item(
                debug_menu,
                MenuAction::DebugRelaunchDaemon,
                "Re-launch ctermo&d",
            );
            append_menu_item(debug_menu, MenuAction::KillDaemon, "&Kill Local ctermd");
            append_popup_menu(menu_bar, debug_menu, "&Debug");
        }

        menu_bar
    }
}

/// Append a menu item to a menu
fn append_menu_item(menu: HMENU, action: MenuAction, text: &str) {
    let wide = to_wide_string(text);
    unsafe {
        AppendMenuW(menu, MF_STRING, action.id() as usize, wide.as_ptr());
    }
}

/// Append a separator to a menu
fn append_separator(menu: HMENU) {
    unsafe {
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
    }
}

/// Append a popup (submenu) to a menu
fn append_popup_menu(parent: HMENU, child: HMENU, text: &str) {
    let wide = to_wide_string(text);
    unsafe {
        AppendMenuW(parent, MF_POPUP, child as usize, wide.as_ptr());
    }
}

/// Set the menu bar for a window
pub fn set_window_menu(hwnd: winapi::shared::windef::HWND, menu: HMENU) {
    unsafe {
        SetMenu(hwnd, menu);
    }
}

/// Accelerator key definition
#[derive(Debug, Clone)]
pub struct Accelerator {
    pub action: MenuAction,
    pub key: u16,
    pub modifiers: AcceleratorModifiers,
}

bitflags::bitflags! {
    /// Accelerator key modifiers
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AcceleratorModifiers: u8 {
        const CTRL = 1 << 0;
        const SHIFT = 1 << 1;
        const ALT = 1 << 2;
    }
}

/// Get the default accelerator table
pub fn get_accelerators() -> Vec<Accelerator> {
    use winapi::um::winuser::*;

    vec![
        // File menu
        Accelerator {
            action: MenuAction::NewTab,
            key: 'T' as u16,
            modifiers: AcceleratorModifiers::CTRL,
        },
        Accelerator {
            action: MenuAction::NewWindow,
            key: 'N' as u16,
            modifiers: AcceleratorModifiers::CTRL,
        },
        Accelerator {
            action: MenuAction::QuickOpen,
            key: 'G' as u16,
            modifiers: AcceleratorModifiers::CTRL,
        },
        Accelerator {
            action: MenuAction::CloseTab,
            key: 'W' as u16,
            modifiers: AcceleratorModifiers::CTRL,
        },
        // Edit menu
        Accelerator {
            action: MenuAction::Copy,
            key: 'C' as u16,
            modifiers: AcceleratorModifiers::CTRL | AcceleratorModifiers::SHIFT,
        },
        Accelerator {
            action: MenuAction::Paste,
            key: 'V' as u16,
            modifiers: AcceleratorModifiers::CTRL | AcceleratorModifiers::SHIFT,
        },
        Accelerator {
            action: MenuAction::SelectAll,
            key: 'A' as u16,
            modifiers: AcceleratorModifiers::CTRL | AcceleratorModifiers::SHIFT,
        },
        // Terminal menu
        Accelerator {
            action: MenuAction::Find,
            key: 'F' as u16,
            modifiers: AcceleratorModifiers::CTRL | AcceleratorModifiers::SHIFT,
        },
        // Tabs menu
        Accelerator {
            action: MenuAction::PrevTab,
            key: VK_TAB as u16,
            modifiers: AcceleratorModifiers::CTRL | AcceleratorModifiers::SHIFT,
        },
        Accelerator {
            action: MenuAction::NextTab,
            key: VK_TAB as u16,
            modifiers: AcceleratorModifiers::CTRL,
        },
        Accelerator {
            action: MenuAction::Tab1,
            key: '1' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab2,
            key: '2' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab3,
            key: '3' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab4,
            key: '4' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab5,
            key: '5' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab6,
            key: '6' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab7,
            key: '7' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab8,
            key: '8' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
        Accelerator {
            action: MenuAction::Tab9,
            key: '9' as u16,
            modifiers: AcceleratorModifiers::ALT,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_ui::events::Action;
    use cterm_ui::pane::{PaneDirection, SplitDirection};

    #[test]
    fn test_menu_action_roundtrip() {
        let action = MenuAction::NewTab;
        assert_eq!(MenuAction::from_id(action.id()), Some(action));
    }

    #[test]
    fn pane_menu_actions_roundtrip() {
        for action in [
            MenuAction::SplitPaneHorizontal,
            MenuAction::SplitPaneVertical,
            MenuAction::ClosePane,
            MenuAction::FocusPaneLeft,
            MenuAction::FocusPaneRight,
            MenuAction::FocusPaneUp,
            MenuAction::FocusPaneDown,
            MenuAction::ResizePaneLeft,
            MenuAction::ResizePaneRight,
            MenuAction::ResizePaneUp,
            MenuAction::ResizePaneDown,
            MenuAction::TogglePaneZoom,
        ] {
            assert_eq!(MenuAction::from_id(action.id()), Some(action));
        }
    }

    #[test]
    fn shared_menu_actions_map_to_canonical_actions() {
        let cases = [
            (MenuAction::NewTab, Action::NewTab),
            (MenuAction::NewWindow, Action::NewWindow),
            (MenuAction::QuickOpen, Action::QuickOpenTemplate),
            (MenuAction::CloseTab, Action::CloseTab),
            (MenuAction::Quit, Action::CloseWindow),
            (MenuAction::Copy, Action::Copy),
            (MenuAction::Paste, Action::Paste),
            (MenuAction::SelectAll, Action::SelectAll),
            (MenuAction::ZoomIn, Action::ZoomIn),
            (MenuAction::ZoomOut, Action::ZoomOut),
            (MenuAction::ZoomReset, Action::ZoomReset),
            (MenuAction::Fullscreen, Action::ToggleFullscreen),
            (MenuAction::Find, Action::FindText),
            (MenuAction::Reset, Action::ResetTerminal),
            (
                MenuAction::SplitPaneHorizontal,
                Action::SplitPane(SplitDirection::Horizontal),
            ),
            (
                MenuAction::SplitPaneVertical,
                Action::SplitPane(SplitDirection::Vertical),
            ),
            (MenuAction::ClosePane, Action::ClosePane),
            (
                MenuAction::FocusPaneLeft,
                Action::FocusPane(PaneDirection::Left),
            ),
            (
                MenuAction::FocusPaneRight,
                Action::FocusPane(PaneDirection::Right),
            ),
            (
                MenuAction::FocusPaneUp,
                Action::FocusPane(PaneDirection::Up),
            ),
            (
                MenuAction::FocusPaneDown,
                Action::FocusPane(PaneDirection::Down),
            ),
            (
                MenuAction::ResizePaneLeft,
                Action::ResizePane(PaneDirection::Left),
            ),
            (
                MenuAction::ResizePaneRight,
                Action::ResizePane(PaneDirection::Right),
            ),
            (
                MenuAction::ResizePaneUp,
                Action::ResizePane(PaneDirection::Up),
            ),
            (
                MenuAction::ResizePaneDown,
                Action::ResizePane(PaneDirection::Down),
            ),
            (MenuAction::TogglePaneZoom, Action::TogglePaneZoom),
            (MenuAction::PrevTab, Action::PrevTab),
            (MenuAction::NextTab, Action::NextTab),
            (MenuAction::NextAlertedTab, Action::NextAlertedTab),
            (MenuAction::Tab1, Action::Tab(1)),
            (MenuAction::Tab2, Action::Tab(2)),
            (MenuAction::Tab3, Action::Tab(3)),
            (MenuAction::Tab4, Action::Tab(4)),
            (MenuAction::Tab5, Action::Tab(5)),
            (MenuAction::Tab6, Action::Tab(6)),
            (MenuAction::Tab7, Action::Tab(7)),
            (MenuAction::Tab8, Action::Tab(8)),
            (MenuAction::Tab9, Action::Tab(9)),
            (MenuAction::Preferences, Action::OpenPreferences),
        ];

        for (menu_action, expected) in cases {
            assert_eq!(menu_action.shared_action(), Some(expected));
        }
    }

    #[test]
    fn native_only_menu_actions_stay_outside_the_shared_dispatcher() {
        for action in [
            MenuAction::CloseOtherTabs,
            MenuAction::DockerPicker,
            MenuAction::CopyHtml,
            MenuAction::SetTitle,
            MenuAction::SetColor,
            MenuAction::ClearReset,
            MenuAction::SendSignalInt,
            MenuAction::SendSignalKill,
            MenuAction::SendSignalHup,
            MenuAction::SendSignalTerm,
            MenuAction::TabTemplates,
            MenuAction::CheckUpdates,
            MenuAction::About,
            MenuAction::AttachSession,
            MenuAction::SSHConnect,
            MenuAction::ManageRemotes,
            MenuAction::DebugRelaunch,
            MenuAction::DebugDumpState,
            MenuAction::ViewLogs,
            MenuAction::DebugRelaunchDaemon,
            MenuAction::KillDaemon,
        ] {
            assert_eq!(action.shared_action(), None);
        }
    }

    #[test]
    fn test_to_wide_string() {
        let wide = to_wide_string("Test");
        assert_eq!(wide.len(), 5); // "Test" + null terminator
        assert_eq!(wide[4], 0); // null terminator
    }
}
