//! Main window implementation
//!
//! Manages the main window, tabs, terminal rendering, and message handling.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, ScreenToClient, UpdateWindow, HBRUSH, PAINTSTRUCT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::Win32::UI::WindowsAndMessaging::*;

use cterm_app::config::Config;
use cterm_app::file_transfer::PendingFileManager;
use cterm_app::shortcuts::ShortcutManager;
use cterm_core::color::{ColorPalette, Rgb};
use cterm_core::mouse::{
    encode_mouse_event, MouseButton as ReportButton, MouseEvent as ReportMouseEvent,
    MouseModifiers, MousePosition,
};
use cterm_core::pty::{PtyConfig, PtySize};
use cterm_core::screen::{FileTransferOperation, MouseEncoding, MouseMode, ScreenConfig};
use cterm_core::term::{Key, Modifiers as CoreModifiers, Terminal, TerminalEvent};
use cterm_core::{KeyEventKind, KeyboardEnhancementFlags};
use cterm_ui::events::{Action, Modifiers};
use cterm_ui::pane::{
    PaneBranch, PaneDirection, PaneId, PaneLayout, PaneRect, PaneTree, SplitDirection, SplitRatio,
    SplitRequest,
};
use cterm_ui::theme::Theme;
use winapi::um::winuser;

use crate::clipboard;
use crate::dpi::{self, DpiInfo};
use crate::keycode;
use crate::menu::{self, MenuAction};
use crate::mouse::{self, MouseState};
use crate::notification_bar::{NotificationAction, NotificationBar};
use crate::tab_bar::TabBar;
use crate::terminal_canvas::TerminalRenderer;

/// Custom window messages
pub const WM_APP_PTY_DATA: u32 = WM_APP + 1;
pub const WM_APP_PTY_EXIT: u32 = WM_APP + 2;
pub const WM_APP_TITLE_CHANGED: u32 = WM_APP + 3;
pub const WM_APP_BELL: u32 = WM_APP + 4;
pub const WM_APP_DESKTOP_NOTIFICATION: u32 = WM_APP + 5;
pub const WM_APP_NATIVE_NOTIFICATION: u32 = WM_APP + 6;
pub const WM_APP_DAEMON_SESSION_READY: u32 = WM_APP + 7;

/// Commands sent to the daemon I/O thread
pub enum DaemonCmd {
    Write(Vec<u8>),
    Resize {
        cols: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
    },
    SetTitle(String),
    SetTabColor(String),
    SetTemplateName(String),
    SetFrontendState(cterm_core::FrontendState),
    ClearAlert,
    /// Detach this frontend while leaving the daemon-owned session alive.
    Close,
    /// Destroy the daemon-owned session as part of an explicit UI close.
    Destroy,
}

type RemoteDaemonEndpoint = (cterm_client::RemoteManager, String, String, bool);

#[derive(Clone)]
struct DaemonPaneContext {
    remote: Option<RemoteDaemonEndpoint>,
    remote_name: Option<String>,
    daemon_socket: Option<std::path::PathBuf>,
    shell: Option<String>,
    args: Vec<String>,
    env: Vec<(String, String)>,
    term: Option<String>,
    ssh: Option<cterm_client::SshParams>,
}

impl DaemonPaneContext {
    fn from_options(
        options: &cterm_client::CreateSessionOpts,
        remote: Option<RemoteDaemonEndpoint>,
    ) -> Self {
        Self {
            remote_name: remote.as_ref().map(|(_, name, _, _)| name.clone()),
            remote,
            daemon_socket: None,
            shell: options.shell.clone(),
            args: options.args.clone(),
            env: options.env.clone(),
            term: options.term.clone(),
            ssh: options.ssh.clone(),
        }
    }

    fn local_default() -> Self {
        Self {
            remote: None,
            remote_name: None,
            daemon_socket: None,
            shell: None,
            args: Vec::new(),
            env: Vec::new(),
            term: None,
            ssh: None,
        }
    }

    fn launch_context(&self) -> cterm_app::upgrade::PaneLaunchContext {
        cterm_app::upgrade::PaneLaunchContext::capture(&cterm_client::CreateSessionOpts {
            shell: self.shell.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            term: self.term.clone(),
            ssh: self.ssh.clone(),
            ..Default::default()
        })
    }

    fn apply_launch_context(&mut self, launch: &cterm_app::upgrade::PaneLaunchContext) {
        let mut options = cterm_client::CreateSessionOpts::default();
        launch.apply_to(&mut options);
        self.shell = options.shell;
        self.args = options.args;
        self.env = options.env;
        self.term = options.term;
        self.ssh = options.ssh;
    }
}

#[derive(Clone)]
enum PaneBackendContext {
    LocalPty,
    Daemon(Box<DaemonPaneContext>),
}

struct DaemonSessionReady {
    session_id: String,
    daemon_socket: Option<std::path::PathBuf>,
}

struct DaemonTabMetadata {
    title: String,
    color: Option<String>,
    background_color: Option<String>,
    title_locked: bool,
    template_name: Option<String>,
    keep_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneDivider {
    path: Vec<PaneBranch>,
    direction: SplitDirection,
    split_rect: PaneRect,
}

const PREVIOUS_KEY_STATE_BIT: usize = 1 << 30;
const EXTENDED_KEY_BIT: usize = 1 << 24;

/// Classify a Win32 key message without consulting mutable keyboard state.
fn key_event_kind(msg: u32, key_data: usize) -> Option<KeyEventKind> {
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN if key_data & PREVIOUS_KEY_STATE_BIT != 0 => {
            Some(KeyEventKind::Repeat)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => Some(KeyEventKind::Press),
        WM_KEYUP | WM_SYSKEYUP => Some(KeyEventKind::Release),
        _ => None,
    }
}

/// Map the standard PC-101 base-layout ASCII identity required by the kitty
/// protocol. Ordinary layout/IME text still stays on WM_CHAR.
fn ascii_key_for_vk(vk: u16) -> Option<char> {
    Some(match vk as i32 {
        value @ 0x30..=0x39 => value as u8 as char,
        value @ 0x41..=0x5a => (b'a' + (value as u8 - b'A')) as char,
        winuser::VK_SPACE => ' ',
        winuser::VK_OEM_MINUS => '-',
        winuser::VK_OEM_PLUS => '=',
        winuser::VK_OEM_4 => '[',
        winuser::VK_OEM_6 => ']',
        winuser::VK_OEM_1 => ';',
        winuser::VK_OEM_7 => '\'',
        winuser::VK_OEM_3 => '`',
        winuser::VK_OEM_5 => '\\',
        winuser::VK_OEM_COMMA => ',',
        winuser::VK_OEM_PERIOD => '.',
        winuser::VK_OEM_2 => '/',
        _ => return None,
    })
}

fn mapped_terminal_key(
    vk: u16,
    modifiers: Modifiers,
    enhanced_text: bool,
    extended: bool,
) -> Option<Key> {
    let functional = match vk as i32 {
        winuser::VK_UP => Key::Up,
        winuser::VK_DOWN => Key::Down,
        winuser::VK_LEFT => Key::Left,
        winuser::VK_RIGHT => Key::Right,
        winuser::VK_HOME => Key::Home,
        winuser::VK_END => Key::End,
        winuser::VK_PRIOR => Key::PageUp,
        winuser::VK_NEXT => Key::PageDown,
        winuser::VK_INSERT => Key::Insert,
        winuser::VK_DELETE => Key::Delete,
        winuser::VK_BACK => Key::Backspace,
        winuser::VK_RETURN if extended => Key::NumpadEnter,
        winuser::VK_RETURN => Key::Enter,
        winuser::VK_TAB => Key::Tab,
        winuser::VK_ESCAPE => Key::Escape,
        value if (winuser::VK_NUMPAD0..=winuser::VK_NUMPAD9).contains(&value) => {
            Key::NumpadDigit((value - winuser::VK_NUMPAD0) as u8)
        }
        winuser::VK_DECIMAL => Key::NumpadDecimal,
        winuser::VK_DIVIDE => Key::NumpadDivide,
        winuser::VK_MULTIPLY => Key::NumpadMultiply,
        winuser::VK_SUBTRACT => Key::NumpadSubtract,
        winuser::VK_ADD => Key::NumpadAdd,
        value if (winuser::VK_F1..=winuser::VK_F12).contains(&value) => {
            Key::F((value - winuser::VK_F1 + 1) as u8)
        }
        _ => {
            if enhanced_text {
                return ascii_key_for_vk(vk).map(Key::Char);
            }
            if modifiers.contains(Modifiers::CTRL)
                && !modifiers.intersects(Modifiers::ALT | Modifiers::SUPER)
            {
                return ascii_key_for_vk(vk)
                    .filter(char::is_ascii_alphabetic)
                    .map(Key::Char);
            }
            return None;
        }
    };
    Some(functional)
}

fn send_terminal_focus_event(terminal: &Arc<Mutex<Terminal>>, focused: bool) {
    let mut terminal = terminal.lock().unwrap();
    if terminal.screen().modes.focus_events {
        let sequence = if focused { b"\x1b[I" } else { b"\x1b[O" };
        if let Err(error) = terminal.write(sequence) {
            log::error!("Failed to send pane focus event: {error}");
        }
    }
}

/// One live terminal session displayed by a pane.
pub struct PaneEntry {
    pub source_id: u64,
    pub terminal: Arc<Mutex<Terminal>>,
    /// Last display title associated with this pane.
    pub title: String,
    #[allow(dead_code)]
    pub reader_handle: Option<thread::JoinHandle<()>>,
    /// Session ID for daemon-backed panes.
    pub session_id: Option<String>,
    /// Concrete daemon socket or named pipe used to reattach this pane.
    pub daemon_socket: Option<std::path::PathBuf>,
    /// Whether this pane's display title is locked against OSC updates.
    pub title_locked: bool,
    /// Template identity used to create this pane, when known.
    pub template_name: Option<String>,
    /// Keep this pane in the layout after its child exits.
    pub keep_open: bool,
    /// Whether this pane has an unacknowledged bell while it was unfocused.
    pub has_bell: bool,
    /// Command sender for daemon-backed panes.
    pub daemon_cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<DaemonCmd>>,
    backend: PaneBackendContext,
}

impl PaneEntry {
    fn shutdown(&mut self) {
        if let Some(sender) = self.daemon_cmd_tx.take() {
            let _ = sender.send(DaemonCmd::Close);
        }
        if let Ok(mut terminal) = self.terminal.lock() {
            // Dropping the owning PTY terminates a local child and wakes the
            // cloned reader handle. Daemon sessions deliberately survive UI
            // teardown and detach through DaemonCmd::Close above.
            drop(terminal.take_pty());
        }
    }

    fn destroy(&mut self) {
        if let Some(sender) = self.daemon_cmd_tx.take() {
            let _ = sender.send(DaemonCmd::Destroy);
        }
        if let Ok(mut terminal) = self.terminal.lock() {
            drop(terminal.take_pty());
        }
    }
}

impl Drop for PaneEntry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Tab entry.
pub struct TabEntry {
    pub id: u64,
    pub title: String,
    pub color: Option<String>,
    pub background_color: Option<String>,
    pub has_bell: bool,
    /// Whether title was explicitly set (locks out OSC updates)
    pub title_locked: bool,
    pub pane_layout: PaneLayout,
    pub panes: BTreeMap<PaneId, PaneEntry>,
}

impl TabEntry {
    fn new(
        id: u64,
        title: String,
        color: Option<String>,
        background_color: Option<String>,
        title_locked: bool,
        pane: PaneEntry,
    ) -> Self {
        let pane_layout = PaneLayout::new();
        let panes = BTreeMap::from([(pane_layout.active(), pane)]);
        Self {
            id,
            title,
            color,
            background_color,
            has_bell: false,
            title_locked,
            pane_layout,
            panes,
        }
    }

    fn active_pane(&self) -> Option<&PaneEntry> {
        self.panes.get(&self.pane_layout.active())
    }

    fn active_pane_mut(&mut self) -> Option<&mut PaneEntry> {
        self.panes.get_mut(&self.pane_layout.active())
    }

    fn active_terminal(&self) -> Option<Arc<Mutex<Terminal>>> {
        self.active_pane().map(|pane| Arc::clone(&pane.terminal))
    }

    fn pane_id_for_source(&self, source_id: u64) -> Option<PaneId> {
        self.panes
            .iter()
            .find_map(|(id, pane)| (pane.source_id == source_id).then_some(*id))
    }
}

/// Window state
pub struct WindowState {
    pub hwnd: HWND,
    pub config: Config,
    pub theme: Theme,
    pub shortcuts: ShortcutManager,
    pub tabs: Vec<TabEntry>,
    pub active_tab_index: usize,
    pub next_tab_id: AtomicU64,
    next_source_id: AtomicU64,
    pub renderer: Option<TerminalRenderer>,
    pub tab_bar: TabBar,
    pub notification_bar: NotificationBar,
    pub file_manager: PendingFileManager,
    pub dpi: DpiInfo,
    pub mouse_state: MouseState,
    /// Button currently forwarded to a mouse-tracking application (set on a
    /// forwarded press, used to emit the matching release and drag-motion reports).
    mouse_report_button: Option<ReportButton>,
    /// Last pointer position in client pixels (used by the wheel handler, whose
    /// message carries screen coordinates we'd otherwise have to convert).
    last_mouse_pos: (f32, f32),
    /// Last reported pointer position, used to coalesce cell-based motion while
    /// retaining every pixel transition in mode 1016.
    last_reported_mouse_position: Option<MousePosition>,
    pane_divider_drag: Option<PaneDivider>,
    /// Key releases paired with key-down events consumed by application
    /// shortcuts must not leak into enhanced keyboard reporting.
    suppressed_key_releases: HashSet<u16>,
    /// Physical keys whose presses were emitted as enhanced events.
    reported_keys: HashMap<u16, Key>,
    /// Modified text keys handled on WM_KEYDOWN; their generated WM_CHAR or
    /// WM_SYSCHAR messages must not be delivered a second time.
    enhanced_text_keys: HashSet<u16>,
    /// Visibility of the native window; only its active tab is actually visible.
    window_visibility: cterm_core::WindowVisibility,
    #[allow(dead_code)]
    menu_handle: winapi::shared::windef::HMENU,
    /// Skip close confirmation (set during relaunch)
    pub skip_close_confirm: bool,
    /// Remote host connection manager
    pub remote_manager: cterm_client::RemoteManager,
}

impl WindowState {
    /// Create a new window state
    pub fn new(hwnd: HWND, config: &Config, theme: &Theme) -> Self {
        let shortcuts = ShortcutManager::from_config(&config.shortcuts);
        let dpi = DpiInfo::for_window(hwnd);

        let mut tab_bar = TabBar::new(theme);
        tab_bar.set_dpi(dpi);

        let mut notification_bar = NotificationBar::new(theme);
        notification_bar.set_dpi(dpi);

        // Create menu
        let menu_handle = menu::create_menu_bar(
            false,
            crate::get_args().updater_enabled(),
            crate::get_args().managed,
        );
        menu::set_window_menu(hwnd.0 as *mut _, menu_handle);

        Self {
            hwnd,
            config: config.clone(),
            theme: theme.clone(),
            shortcuts,
            tabs: Vec::new(),
            active_tab_index: 0,
            next_tab_id: AtomicU64::new(0),
            next_source_id: AtomicU64::new(1),
            renderer: None,
            tab_bar,
            notification_bar,
            file_manager: PendingFileManager::new(),
            dpi,
            mouse_state: MouseState::new(),
            mouse_report_button: None,
            last_mouse_pos: (0.0, 0.0),
            last_reported_mouse_position: None,
            pane_divider_drag: None,
            suppressed_key_releases: HashSet::new(),
            reported_keys: HashMap::new(),
            enhanced_text_keys: HashSet::new(),
            window_visibility: cterm_core::WindowVisibility::Visible,
            menu_handle,
            skip_close_confirm: false,
            remote_manager: cterm_client::RemoteManager::new(),
        }
    }

    /// Initialize the renderer
    pub fn init_renderer(&mut self) -> windows::core::Result<()> {
        let font_family = &self.config.appearance.font.family;
        let font_size = self.config.appearance.font.size as f32;

        let renderer = TerminalRenderer::new(self.hwnd, &self.theme, font_family, font_size)?;
        self.renderer = Some(renderer);
        Ok(())
    }

    fn allocate_source_id(&self) -> u64 {
        self.next_source_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Create a new tab
    pub fn new_tab(&mut self) -> Result<u64, Box<dyn std::error::Error>> {
        if crate::get_args().managed {
            log::warn!("Ignoring new-tab request in managed mode");
            return Err(
                std::io::Error::other("secondary sessions are disabled in managed mode").into(),
            );
        }
        let shell = self
            .config
            .general
            .default_shell
            .clone()
            .unwrap_or_else(|| std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()));
        let initial_title = std::path::Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Terminal")
            .to_string();
        let opts = cterm_client::CreateSessionOpts {
            shell: self.config.general.default_shell.clone(),
            args: self.config.general.shell_args.clone(),
            cwd: self
                .config
                .general
                .working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            env: self
                .config
                .general
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            term: self.config.general.term.clone(),
            ..Default::default()
        };
        self.new_tab_with_options(opts, initial_title, false)
    }

    /// Create a daemon-backed tab from an argv-safe process specification.
    ///
    /// Normal desktop tabs deliberately use ctermd as well as managed tabs so
    /// every live Windows session can be handed to a replacement UI process.
    pub fn new_tab_with_options(
        &mut self,
        opts: cterm_client::CreateSessionOpts,
        initial_title: String,
        title_locked: bool,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(self.spawn_daemon_tab_configured(
            opts,
            DaemonTabMetadata {
                title: initial_title,
                color: None,
                background_color: None,
                title_locked,
                template_name: None,
                keep_open: false,
            },
            None,
        ))
    }

    /// Create a new tab from a template
    pub fn new_tab_from_template(
        &mut self,
        template: &cterm_app::config::StickyTabConfig,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        if crate::get_args().managed {
            log::warn!("Ignoring tab-template request in managed mode");
            return Err(
                std::io::Error::other("secondary sessions are disabled in managed mode").into(),
            );
        }
        #[cfg(not(unix))]
        if template.remote.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote daemon templates are not supported by the Windows transport",
            )
            .into());
        }
        let remote = if let Some(ref remote_name) = template.remote {
            let remote_config = self
                .config
                .remotes
                .iter()
                .find(|r| r.name == *remote_name)
                .cloned()
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("remote '{remote_name}' is not configured"),
                    )
                })?;

            Some((
                self.remote_manager.clone(),
                remote_config.name,
                remote_config.host,
                remote_config.ssh_compression,
            ))
        } else {
            None
        };
        let (cols, rows) = self.terminal_size();
        let options = template_session_options(template, &self.config, cols as u32, rows as u32);
        Ok(self.spawn_daemon_tab_configured(
            options,
            DaemonTabMetadata {
                title: template.name.clone(),
                color: template.color.clone(),
                background_color: template.background_color.clone(),
                title_locked: true,
                template_name: Some(template.name.clone()),
                keep_open: template.keep_open,
            },
            remote,
        ))
    }

    /// Create a new tab for Docker (exec into container or run image)
    pub fn new_docker_tab(
        &mut self,
        selection: crate::docker_dialog::DockerSelection,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        if crate::get_args().managed {
            log::warn!("Ignoring Docker-terminal request in managed mode");
            return Err(
                std::io::Error::other("secondary sessions are disabled in managed mode").into(),
            );
        }
        // Build the docker command based on selection
        let (shell, args, title) = match &selection {
            crate::docker_dialog::DockerSelection::ExecContainer(container) => (
                Some("docker".to_string()),
                vec![
                    "exec".to_string(),
                    "-it".to_string(),
                    container.name.clone(),
                    "/bin/sh".to_string(),
                ],
                format!("docker: {}", container.name),
            ),
            crate::docker_dialog::DockerSelection::RunImage(image) => {
                let image_name = if image.tag == "<none>" {
                    image.repository.clone()
                } else {
                    format!("{}:{}", image.repository, image.tag)
                };
                (
                    Some("docker".to_string()),
                    vec![
                        "run".to_string(),
                        "-it".to_string(),
                        "--rm".to_string(),
                        image_name.clone(),
                    ],
                    format!("docker: {}", image_name),
                )
            }
        };

        let opts = cterm_client::CreateSessionOpts {
            shell,
            args,
            cwd: self
                .config
                .general
                .working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            env: self
                .config
                .general
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            term: self.config.general.term.clone(),
            ..Default::default()
        };
        Ok(self.spawn_daemon_tab_configured(
            opts,
            DaemonTabMetadata {
                title,
                color: None,
                background_color: None,
                title_locked: true,
                template_name: None,
                keep_open: false,
            },
            None,
        ))
    }

    /// Create a new daemon-backed tab
    ///
    /// Connects to ctermd (local or remote), creates a session, and streams
    /// output. The tab is created immediately; the connection happens in
    /// a background thread.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_daemon_tab(
        &mut self,
        opts: cterm_client::CreateSessionOpts,
        title: String,
        color: Option<String>,
        background_color: Option<String>,
        keep_open: bool,
        remote: Option<(cterm_client::RemoteManager, String, String, bool)>,
    ) -> u64 {
        self.spawn_daemon_tab_configured(
            opts,
            DaemonTabMetadata {
                title,
                color,
                background_color,
                title_locked: true,
                template_name: None,
                keep_open,
            },
            remote,
        )
    }

    fn spawn_daemon_tab_configured(
        &mut self,
        mut opts: cterm_client::CreateSessionOpts,
        metadata: DaemonTabMetadata,
        remote: Option<(cterm_client::RemoteManager, String, String, bool)>,
    ) -> u64 {
        let DaemonTabMetadata {
            title,
            color,
            background_color,
            title_locked,
            template_name,
            keep_open,
        } = metadata;
        let tab_id = self.next_tab_id.fetch_add(1, Ordering::SeqCst);
        let (cols, rows) = self.terminal_size();
        let (pixel_width, pixel_height) = self.terminal_pixel_size();

        // Older call sites leave some or all geometry fields empty. Populate
        // them at the shared create boundary so every daemon-backed tab starts
        // with the real viewport geometry.
        if opts.cols == 0 {
            opts.cols = cols as u32;
        }
        if opts.rows == 0 {
            opts.rows = rows as u32;
        }
        if opts.pixel_width == 0 {
            opts.pixel_width = pixel_width;
        }
        if opts.pixel_height == 0 {
            opts.pixel_height = pixel_height;
        }
        opts.base_palette = Some(terminal_palette(&self.theme, background_color.as_deref()));
        opts.frontend_state.appearance = self.theme.appearance();
        opts.frontend_state.visibility = self.window_visibility;
        let backend = PaneBackendContext::Daemon(Box::new(DaemonPaneContext::from_options(
            &opts,
            remote.clone(),
        )));

        let screen_config = ScreenConfig {
            scrollback_lines: self.config.general.scrollback_lines,
        };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.set_base_palette(terminal_palette(&self.theme, background_color.as_deref()));
        terminal.set_frontend_state(opts.frontend_state);
        terminal.resize_with_pixels(
            cols,
            rows,
            pixel_width.min(u16::MAX as u32) as u16,
            pixel_height.min(u16::MAX as u32) as u16,
        );

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<DaemonCmd>();
        let write_tx = cmd_tx.clone();
        terminal.set_write_fn(Box::new(move |data: &[u8]| {
            let _ = write_tx.send(DaemonCmd::Write(data.to_vec()));
            Ok(())
        }));

        let terminal = Arc::new(Mutex::new(terminal));

        let source_id = self.allocate_source_id();
        let previous_focus = self
            .tabs
            .get(self.active_tab_index)
            .map(|tab| (self.active_tab_index, tab.pane_layout.active()));
        let had_focus = self.window_has_focus();
        let entry = TabEntry::new(
            tab_id,
            title.clone(),
            color.clone(),
            background_color.clone(),
            title_locked,
            PaneEntry {
                source_id,
                terminal: Arc::clone(&terminal),
                title: title.clone(),
                reader_handle: None,
                session_id: None,
                daemon_socket: None,
                title_locked,
                template_name: template_name.clone(),
                keep_open,
                has_bell: false,
                daemon_cmd_tx: Some(cmd_tx),
                backend,
            },
        );

        self.tabs.push(entry);
        self.active_tab_index = self.tabs.len() - 1;
        if had_focus {
            if let Some((tab_index, pane_id)) = previous_focus {
                self.send_pane_focus_event_in_tab(tab_index, pane_id, false);
            }
            let pane_id = self.tabs[self.active_tab_index].pane_layout.active();
            self.send_pane_focus_event(pane_id, true);
        }
        self.set_window_visibility(self.window_visibility);
        self.tab_bar.add_tab(tab_id, &title);
        self.tab_bar.set_active(tab_id);
        for index in 0..self.tabs.len() {
            self.resize_tab_panes(index);
        }

        if let Some(ref color_hex) = color {
            let rgb = parse_hex_color(color_hex);
            self.tab_bar.set_color(tab_id, rgb);
        }

        if let Some(ref bg) = background_color {
            if let Some(ref mut renderer) = self.renderer {
                renderer.set_background_override(Some(bg));
            }
        }

        let hwnd = self.hwnd.0 as usize;
        let reader_handle =
            start_daemon_create_thread(hwnd, source_id, terminal, opts, remote, None, cmd_rx);

        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            let pane = tab
                .active_pane_mut()
                .expect("a newly created tab has one active pane");
            pane.reader_handle = Some(reader_handle);
            // Send metadata to daemon (queued until session is created)
            if let Some(ref tx) = pane.daemon_cmd_tx {
                if title_locked && !title.is_empty() {
                    let _ = tx.send(DaemonCmd::SetTitle(title));
                }
                if let Some(template_name) = template_name {
                    let _ = tx.send(DaemonCmd::SetTemplateName(template_name));
                }
                if let Some(ref c) = color {
                    let _ = tx.send(DaemonCmd::SetTabColor(c.clone()));
                }
            }
        }

        self.invalidate();
        tab_id
    }

    /// Attach to an existing daemon session and create a tab for it.
    ///
    /// Used for reconnecting after upgrades and for the "Attach to Session" menu.
    #[allow(clippy::too_many_arguments)]
    pub fn attach_session_tab(
        &mut self,
        session_id: &str,
        title: String,
        custom_title: Option<String>,
        color: Option<String>,
        screen_snapshot: Option<cterm_proto::proto::GetScreenResponse>,
    ) -> u64 {
        let tab_id = self.next_tab_id.fetch_add(1, Ordering::SeqCst);
        let (cols, rows) = self.terminal_size();
        let (pixel_width, pixel_height) = self.terminal_pixel_size();

        let screen_config = ScreenConfig {
            scrollback_lines: self.config.general.scrollback_lines,
        };
        let base_palette = terminal_palette(&self.theme, None);
        let frontend_state = cterm_core::FrontendState {
            appearance: self.theme.appearance(),
            ..Default::default()
        };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.set_base_palette(base_palette.clone());
        terminal.set_frontend_state(frontend_state);

        // Apply screen snapshot if available
        if let Some(ref screen_data) = screen_snapshot {
            cterm_app::daemon_session::apply_screen_snapshot(&mut terminal, screen_data);
        }
        terminal.resize_with_pixels(
            cols,
            rows,
            pixel_width.min(u16::MAX as u32) as u16,
            pixel_height.min(u16::MAX as u32) as u16,
        );

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<DaemonCmd>();
        let _ = cmd_tx.send(DaemonCmd::Resize {
            cols: cols as u32,
            rows: rows as u32,
            pixel_width,
            pixel_height,
        });
        let write_tx = cmd_tx.clone();
        terminal.set_write_fn(Box::new(move |data: &[u8]| {
            let _ = write_tx.send(DaemonCmd::Write(data.to_vec()));
            Ok(())
        }));

        let terminal = Arc::new(Mutex::new(terminal));

        let (display_title, title_locked) = match custom_title {
            Some(ref ct) if !ct.is_empty() => (ct.clone(), true),
            _ => (title, false),
        };
        let attached_context = DaemonPaneContext::from_options(
            &cterm_client::CreateSessionOpts {
                shell: self.config.general.default_shell.clone(),
                args: self.config.general.shell_args.clone(),
                env: self
                    .config
                    .general
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
                term: self.config.general.term.clone(),
                ..Default::default()
            },
            None,
        );

        let source_id = self.allocate_source_id();
        let previous_focus = self
            .tabs
            .get(self.active_tab_index)
            .map(|tab| (self.active_tab_index, tab.pane_layout.active()));
        let had_focus = self.window_has_focus();
        let entry = TabEntry::new(
            tab_id,
            display_title.clone(),
            color.clone(),
            None,
            title_locked,
            PaneEntry {
                source_id,
                terminal: Arc::clone(&terminal),
                title: display_title.clone(),
                reader_handle: None,
                session_id: Some(session_id.to_string()),
                daemon_socket: None,
                title_locked,
                template_name: None,
                keep_open: false,
                has_bell: false,
                daemon_cmd_tx: Some(cmd_tx),
                backend: PaneBackendContext::Daemon(Box::new(attached_context)),
            },
        );

        self.tabs.push(entry);
        self.active_tab_index = self.tabs.len() - 1;
        if had_focus {
            if let Some((tab_index, pane_id)) = previous_focus {
                self.send_pane_focus_event_in_tab(tab_index, pane_id, false);
            }
            let pane_id = self.tabs[self.active_tab_index].pane_layout.active();
            self.send_pane_focus_event(pane_id, true);
            self.clear_active_pane_bell();
        }
        self.set_window_visibility(self.window_visibility);
        self.tab_bar.add_tab(tab_id, &display_title);
        self.tab_bar.set_active(tab_id);
        for index in 0..self.tabs.len() {
            self.resize_tab_panes(index);
        }

        if let Some(ref color_hex) = color {
            let rgb = parse_hex_color(color_hex);
            self.tab_bar.set_color(tab_id, rgb);
        }

        let hwnd = self.hwnd.0 as usize;
        let sid = session_id.to_string();
        let reader_handle = start_daemon_attach_thread(
            hwnd,
            source_id,
            terminal,
            sid,
            cols as u32,
            rows as u32,
            cmd_rx,
            None, // local sessions only for now
            base_palette,
            frontend_state,
        );

        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.active_pane_mut()
                .expect("a newly attached tab has one active pane")
                .reader_handle = Some(reader_handle);
        }

        self.invalidate();
        tab_id
    }

    fn restored_daemon_context(
        &self,
        pane_state: &cterm_app::upgrade::PaneUpgradeState,
        daemon_socket: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
    ) -> DaemonPaneContext {
        let remote = pane_state.remote_name.as_ref().and_then(|name| {
            self.config
                .remotes
                .iter()
                .find(|remote| remote.name == *name)
                .map(|remote| {
                    (
                        self.remote_manager.clone(),
                        remote.name.clone(),
                        remote.host.clone(),
                        remote.ssh_compression,
                    )
                })
        });
        let mut context = DaemonPaneContext::local_default();
        context.shell = self.config.general.default_shell.clone();
        context.args = self.config.general.shell_args.clone();
        context.env = self
            .config
            .general
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        context.term = self.config.general.term.clone();
        context.remote = remote;
        context.remote_name.clone_from(&pane_state.remote_name);
        context.daemon_socket = daemon_socket;
        if let Some(template) = pane_state.template_name.as_ref().and_then(|name| {
            self.config
                .sticky_tabs
                .iter()
                .find(|template| template.name == *name)
        }) {
            let options =
                template_session_options(template, &self.config, cols as u32, rows as u32);
            context.shell = options.shell;
            context.args = options.args;
            context.env = options.env;
            context.term = options.term;
            context.ssh = options.ssh;
        }
        if let Some(launch) = pane_state.launch_context.as_ref() {
            context.apply_launch_context(launch);
        }
        context
    }

    fn make_attached_pane(
        &self,
        pane_state: &cterm_app::upgrade::PaneUpgradeState,
        screen_snapshot: Option<cterm_proto::proto::GetScreenResponse>,
        daemon_socket: Option<std::path::PathBuf>,
        alerted: bool,
    ) -> Option<PaneEntry> {
        let session_id = pane_state.session_id.as_ref()?;
        let (cols, rows) = self.terminal_size();
        let (pixel_width, pixel_height) = self.terminal_pixel_size();
        let screen_config = ScreenConfig {
            scrollback_lines: self.config.general.scrollback_lines,
        };
        let base_palette = terminal_palette(&self.theme, None);
        let frontend_state = cterm_core::FrontendState {
            appearance: self.theme.appearance(),
            visibility: self.window_visibility,
        };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.set_base_palette(base_palette.clone());
        terminal.set_frontend_state(frontend_state);
        if let Some(ref screen) = screen_snapshot {
            cterm_app::daemon_session::apply_screen_snapshot(&mut terminal, screen);
        }
        terminal.resize_with_pixels(
            cols,
            rows,
            pixel_width.min(u16::MAX as u32) as u16,
            pixel_height.min(u16::MAX as u32) as u16,
        );

        let (command_sender, command_receiver) =
            tokio::sync::mpsc::unbounded_channel::<DaemonCmd>();
        let write_sender = command_sender.clone();
        terminal.set_write_fn(Box::new(move |data: &[u8]| {
            let _ = write_sender.send(DaemonCmd::Write(data.to_vec()));
            Ok(())
        }));
        let terminal = Arc::new(Mutex::new(terminal));
        let source_id = self.allocate_source_id();

        let context = self.restored_daemon_context(pane_state, daemon_socket.clone(), cols, rows);
        let reader_handle = start_daemon_attach_thread(
            self.hwnd.0 as usize,
            source_id,
            Arc::clone(&terminal),
            session_id.clone(),
            cols as u32,
            rows as u32,
            command_receiver,
            daemon_socket.clone(),
            base_palette,
            frontend_state,
        );
        Some(PaneEntry {
            source_id,
            terminal,
            title: pane_state.title.clone(),
            reader_handle: Some(reader_handle),
            session_id: Some(session_id.clone()),
            daemon_socket,
            title_locked: pane_state.title_locked,
            template_name: pane_state.template_name.clone(),
            keep_open: pane_state.keep_open,
            has_bell: alerted,
            daemon_cmd_tx: Some(command_sender),
            backend: PaneBackendContext::Daemon(Box::new(context)),
        })
    }

    fn make_unavailable_remote_pane(
        &self,
        pane_state: &cterm_app::upgrade::PaneUpgradeState,
        reason: &str,
    ) -> Option<PaneEntry> {
        let session_id = pane_state.session_id.clone()?;
        let (cols, rows) = self.terminal_size();
        let screen_config = ScreenConfig {
            scrollback_lines: self.config.general.scrollback_lines,
        };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.set_base_palette(terminal_palette(&self.theme, None));
        terminal.set_frontend_state(cterm_core::FrontendState {
            appearance: self.theme.appearance(),
            visibility: self.window_visibility,
        });
        let message = format!(
            "\r\ncterm preserved remote session {session_id}, but this Windows build cannot attach it:\r\n{reason}\r\n"
        );
        let _ = terminal.process(message.as_bytes());
        Some(PaneEntry {
            source_id: self.allocate_source_id(),
            terminal: Arc::new(Mutex::new(terminal)),
            title: pane_state.title.clone(),
            reader_handle: None,
            session_id: Some(session_id),
            daemon_socket: pane_state.daemon_socket.clone(),
            title_locked: pane_state.title_locked,
            template_name: pane_state.template_name.clone(),
            keep_open: true,
            has_bell: true,
            daemon_cmd_tx: None,
            backend: PaneBackendContext::Daemon(Box::new(
                self.restored_daemon_context(pane_state, None, cols, rows),
            )),
        })
    }

    /// Start the PTY reader thread
    fn start_pty_reader(
        &self,
        source_id: u64,
        terminal: Arc<Mutex<Terminal>>,
    ) -> thread::JoinHandle<()> {
        let hwnd = self.hwnd.0 as usize;

        // Clone the PTY reader handle so we can read without holding the terminal lock.
        // This is critical: pty.read() is blocking I/O, and holding the mutex during
        // the read would prevent the UI thread from rendering or handling input.
        let pty_reader = {
            let term = terminal.lock().unwrap();
            term.pty().and_then(|pty| pty.try_clone_reader().ok())
        };
        let sync_watchdog =
            spawn_synchronized_update_watchdog(hwnd, source_id, Arc::clone(&terminal));

        thread::spawn(move || {
            let Some(mut reader) = pty_reader else {
                log::error!("Failed to clone PTY reader for pane source {}", source_id);
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd as *mut _)),
                        WM_APP_PTY_EXIT,
                        WPARAM(source_id as usize),
                        LPARAM(0),
                    );
                }
                return;
            };

            let mut buffer = [0u8; 8192];

            loop {
                // Read from the cloned reader WITHOUT holding the terminal lock.
                // This allows the UI thread to render and handle input concurrently.
                let bytes_read = {
                    use std::io::Read;
                    match reader.read(&mut buffer) {
                        Ok(0) => break, // EOF
                        Ok(n) => n,
                        Err(_) => break,
                    }
                };

                // Process the data (briefly lock the terminal)
                let (content_changed, sync_deadline) = {
                    let mut term = terminal.lock().unwrap();
                    let events = term.process(&buffer[..bytes_read]);
                    let mut content_changed = false;

                    // Handle events
                    for event in events {
                        match event {
                            TerminalEvent::TitleChanged(_title) => {
                                // Post title change message
                                // Note: We'd need to pass the title somehow
                                unsafe {
                                    let _ = PostMessageW(
                                        Some(HWND(hwnd as *mut _)),
                                        WM_APP_TITLE_CHANGED,
                                        WPARAM(source_id as usize),
                                        LPARAM(0),
                                    );
                                }
                            }
                            TerminalEvent::Bell => unsafe {
                                let _ = PostMessageW(
                                    Some(HWND(hwnd as *mut _)),
                                    WM_APP_BELL,
                                    WPARAM(source_id as usize),
                                    LPARAM(0),
                                );
                            },
                            TerminalEvent::DesktopNotification(notification) => {
                                post_desktop_notification(hwnd, source_id, notification);
                            }
                            TerminalEvent::ProcessExited(_) => {
                                unsafe {
                                    let _ = PostMessageW(
                                        Some(HWND(hwnd as *mut _)),
                                        WM_APP_PTY_EXIT,
                                        WPARAM(source_id as usize),
                                        LPARAM(0),
                                    );
                                }
                                return;
                            }
                            TerminalEvent::ContentChanged => content_changed = true,
                            _ => {}
                        }
                    }
                    (content_changed, term.synchronized_update_deadline())
                };

                let _ = sync_watchdog.send(sync_deadline);

                if content_changed {
                    post_message(hwnd, WM_APP_PTY_DATA, source_id);
                }
            }

            // Process exited
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    WM_APP_PTY_EXIT,
                    WPARAM(source_id as usize),
                    LPARAM(0),
                );
            }
        })
    }

    /// Check if any pane has a running foreground process.
    pub fn has_running_process(&self) -> bool {
        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.values())
            .any(Self::pane_has_running_process)
    }

    fn pane_has_running_process(pane: &PaneEntry) -> bool {
        #[cfg(unix)]
        if pane
            .terminal
            .lock()
            .is_ok_and(|terminal| terminal.has_foreground_process())
        {
            return true;
        }
        let Some(session_id) = pane.session_id.clone() else {
            return false;
        };
        let daemon_socket = pane.daemon_socket.clone();
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return false;
        };
        runtime.block_on(async move {
            let connection = match daemon_socket {
                Some(path) => cterm_client::DaemonConnection::connect_unix(&path, false).await,
                None => cterm_client::DaemonConnection::connect_local().await,
            };
            let Ok(connection) = connection else {
                return false;
            };
            connection
                .get_session(&session_id)
                .await
                .is_ok_and(|session| session.has_foreground_process)
        })
    }

    fn confirm_close_panes<'a>(&self, panes: impl Iterator<Item = &'a PaneEntry>) -> bool {
        if self.skip_close_confirm || !self.config.general.confirm_close_with_running {
            return true;
        }
        if !panes.into_iter().any(Self::pane_has_running_process) {
            return true;
        }
        crate::dialogs::show_confirm(
            self.hwnd.0 as *mut _,
            "Close terminal?",
            "A process is still running. Are you sure you want to close it?",
        )
    }

    /// Check if we should confirm before closing
    /// Returns true if confirmation is needed
    pub fn should_confirm_close(&self) -> bool {
        if self.skip_close_confirm {
            return false;
        }
        if !self.config.general.confirm_close_with_running {
            return false;
        }
        self.has_running_process()
    }

    /// Capture every daemon-backed pane for seamless process replacement.
    pub fn collect_upgrade_state(&self) -> cterm_app::upgrade::WindowUpgradeState {
        let mut state = cterm_app::upgrade::WindowUpgradeState::new();
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
        }
        state.x = rect.left;
        state.y = rect.top;
        state.width = rect.right.saturating_sub(rect.left);
        state.height = rect.bottom.saturating_sub(rect.top);
        state.maximized = unsafe { IsZoomed(self.hwnd).as_bool() };
        let style = unsafe { GetWindowLongW(self.hwnd, GWL_STYLE) } as u32;
        state.fullscreen = style & (WS_CAPTION.0 | WS_THICKFRAME.0) == 0;
        state.active_tab = self.active_tab_index;

        for tab in &self.tabs {
            let mut tab_state = cterm_app::upgrade::TabUpgradeState::new(tab.id);
            tab_state.title = tab.title.clone();
            tab_state.custom_title = tab
                .active_pane()
                .is_some_and(|pane| pane.title_locked)
                .then(|| tab.title.clone());
            tab_state.color = tab.color.clone();
            tab_state.pane_layout = Some(tab.pane_layout.clone());
            for pane_id in tab.pane_layout.pane_ids() {
                let Some(pane) = tab.panes.get(&pane_id) else {
                    continue;
                };
                let terminal = pane.terminal.lock().unwrap();
                let mut pane_state =
                    cterm_app::upgrade::PaneUpgradeState::new(pane.session_id.clone());
                pane_state.title = if pane.title_locked && !pane.title.is_empty() {
                    pane.title.clone()
                } else {
                    let title = terminal.screen().title.clone();
                    if title.is_empty() {
                        pane.title.clone()
                    } else {
                        title
                    }
                };
                pane_state.title_locked = pane.title_locked;
                pane_state.template_name = pane.template_name.clone();
                pane_state.cwd = terminal
                    .foreground_cwd()
                    .map(|path| path.to_string_lossy().into_owned());
                pane_state.keep_open = pane.keep_open;
                pane_state.daemon_socket = pane.daemon_socket.clone();
                pane_state.remote_name = match &pane.backend {
                    PaneBackendContext::Daemon(context) => context.remote_name.clone(),
                    PaneBackendContext::LocalPty => None,
                };
                pane_state.launch_context = match &pane.backend {
                    PaneBackendContext::Daemon(context) => Some(context.launch_context()),
                    PaneBackendContext::LocalPty => None,
                };
                tab_state.panes.push(pane_state);
            }
            if let Some(active) = tab.active_pane().and_then(|pane| pane.session_id.as_ref()) {
                tab_state.session_id = Some(active.clone());
            }
            if let Some(active) = tab.active_pane() {
                tab_state.template_name = active.template_name.clone();
                tab_state.keep_open = active.keep_open;
                tab_state.cwd = active
                    .terminal
                    .lock()
                    .ok()
                    .and_then(|terminal| terminal.foreground_cwd())
                    .map(|path| path.to_string_lossy().into_owned());
            }
            state.tabs.push(tab_state);
        }
        state
    }

    fn create_default_pane(&self) -> Result<PaneEntry, Box<dyn std::error::Error>> {
        let cwd = self
            .active_terminal()
            .and_then(|terminal| terminal.lock().ok()?.foreground_cwd())
            .or_else(|| self.config.general.working_directory.clone());
        let backend = self
            .tabs
            .get(self.active_tab_index)
            .and_then(TabEntry::active_pane)
            .map(|pane| pane.backend.clone())
            .ok_or_else(|| std::io::Error::other("there is no active pane"))?;
        match backend {
            PaneBackendContext::LocalPty => self.create_local_pane(cwd),
            PaneBackendContext::Daemon(context) => {
                #[cfg(not(unix))]
                if context.remote.is_some() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "remote daemon panes are not supported by the Windows transport",
                    )
                    .into());
                }
                self.create_daemon_pane(*context, cwd)
            }
        }
    }

    fn create_local_pane(
        &self,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<PaneEntry, Box<dyn std::error::Error>> {
        let (cols, rows) = self.terminal_size();
        let (pixel_width, pixel_height) = self.terminal_pixel_size();
        let pty_config = PtyConfig {
            size: PtySize {
                cols: cols.min(u16::MAX as usize) as u16,
                rows: rows.min(u16::MAX as usize) as u16,
                pixel_width: pixel_width.min(u16::MAX as u32) as u16,
                pixel_height: pixel_height.min(u16::MAX as u32) as u16,
            },
            shell: self.config.general.default_shell.clone(),
            args: self.config.general.shell_args.clone(),
            cwd,
            env: self
                .config
                .general
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            term: self.config.general.term.clone(),
        };
        let screen_config = ScreenConfig {
            scrollback_lines: self.config.general.scrollback_lines,
        };
        let background = self
            .tabs
            .get(self.active_tab_index)
            .and_then(|tab| tab.background_color.as_deref());
        let mut terminal = Terminal::with_shell(cols, rows, screen_config, &pty_config)?;
        terminal.set_base_palette(terminal_palette(&self.theme, background));
        terminal.set_frontend_state(cterm_core::FrontendState {
            appearance: self.theme.appearance(),
            visibility: self.window_visibility,
        });
        let terminal = Arc::new(Mutex::new(terminal));
        let source_id = self.allocate_source_id();
        let reader_handle = self.start_pty_reader(source_id, Arc::clone(&terminal));
        Ok(PaneEntry {
            source_id,
            terminal,
            title: "Terminal".to_string(),
            reader_handle: Some(reader_handle),
            session_id: None,
            daemon_socket: None,
            title_locked: false,
            template_name: None,
            keep_open: false,
            has_bell: false,
            daemon_cmd_tx: None,
            backend: PaneBackendContext::LocalPty,
        })
    }

    fn create_daemon_pane(
        &self,
        context: DaemonPaneContext,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<PaneEntry, Box<dyn std::error::Error>> {
        let (cols, rows) = self.terminal_size();
        let (pixel_width, pixel_height) = self.terminal_pixel_size();
        let background = self
            .tabs
            .get(self.active_tab_index)
            .and_then(|tab| tab.background_color.as_deref());
        let frontend_state = cterm_core::FrontendState {
            appearance: self.theme.appearance(),
            visibility: self.window_visibility,
        };
        let options = cterm_client::CreateSessionOpts {
            cols: cols as u32,
            rows: rows as u32,
            pixel_width,
            pixel_height,
            shell: context.shell.clone(),
            args: context.args.clone(),
            cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
            env: context.env.clone(),
            term: context.term.clone(),
            ssh: context.ssh.clone(),
            base_palette: Some(terminal_palette(&self.theme, background)),
            frontend_state,
        };

        let screen_config = ScreenConfig {
            scrollback_lines: self.config.general.scrollback_lines,
        };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.set_base_palette(terminal_palette(&self.theme, background));
        terminal.set_frontend_state(frontend_state);
        terminal.resize_with_pixels(
            cols,
            rows,
            pixel_width.min(u16::MAX as u32) as u16,
            pixel_height.min(u16::MAX as u32) as u16,
        );
        let (command_sender, command_receiver) =
            tokio::sync::mpsc::unbounded_channel::<DaemonCmd>();
        let write_sender = command_sender.clone();
        terminal.set_write_fn(Box::new(move |data: &[u8]| {
            let _ = write_sender.send(DaemonCmd::Write(data.to_vec()));
            Ok(())
        }));
        let terminal = Arc::new(Mutex::new(terminal));
        let source_id = self.allocate_source_id();
        let reader_handle = start_daemon_create_thread(
            self.hwnd.0 as usize,
            source_id,
            Arc::clone(&terminal),
            options,
            context.remote.clone(),
            context.daemon_socket.clone(),
            command_receiver,
        );
        Ok(PaneEntry {
            source_id,
            terminal,
            title: "Terminal".to_string(),
            reader_handle: Some(reader_handle),
            session_id: None,
            daemon_socket: None,
            title_locked: false,
            template_name: None,
            keep_open: false,
            has_bell: false,
            daemon_cmd_tx: Some(command_sender),
            backend: PaneBackendContext::Daemon(Box::new(context)),
        })
    }

    pub fn split_active_pane(&mut self, direction: SplitDirection) {
        if crate::get_args().managed {
            log::warn!("Ignoring split-pane request in managed mode");
            return;
        }
        let previous = self
            .tabs
            .get(self.active_tab_index)
            .map(|tab| tab.pane_layout.active());
        let mut pane = match self.create_default_pane() {
            Ok(pane) => pane,
            Err(error) => {
                log::error!("Failed to create pane session: {error}");
                return;
            }
        };
        let source_id = pane.source_id;
        let Some(tab) = self.tabs.get_mut(self.active_tab_index) else {
            return;
        };
        let pane_id = match tab.pane_layout.split_active(SplitRequest {
            direction,
            ..SplitRequest::default()
        }) {
            Ok(pane_id) => pane_id,
            Err(error) => {
                pane.destroy();
                log::error!("Failed to split pane: {error}");
                return;
            }
        };
        tab.panes.insert(pane_id, pane);
        log::info!(
            "Split tab {} {:?}: pane {} source {}",
            tab.id,
            direction,
            pane_id,
            source_id
        );
        if self.window_has_focus() {
            if let Some(previous) = previous {
                self.send_pane_focus_event(previous, false);
            }
            self.send_pane_focus_event(pane_id, true);
            self.clear_active_pane_bell();
        }
        self.refresh_active_tab_title();
        self.resize_tab_panes(self.active_tab_index);
        self.set_window_visibility(self.window_visibility);
        self.invalidate();
    }

    pub fn close_active_pane(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab_index) else {
            return;
        };
        if tab.pane_layout.len() == 1 {
            let tab_id = tab.id;
            self.close_tab(tab_id);
            return;
        }
        if !self.confirm_close_panes(tab.active_pane().into_iter()) {
            return;
        }

        let had_focus = self.window_has_focus();
        let (tab_id, pane_id, next_pane_id, mut removed) = {
            let tab = &mut self.tabs[self.active_tab_index];
            let pane_id = tab.pane_layout.active();
            if let Err(error) = tab.pane_layout.close_active() {
                log::error!("Failed to close pane: {error}");
                return;
            }
            (
                tab.id,
                pane_id,
                tab.pane_layout.active(),
                tab.panes.remove(&pane_id),
            )
        };
        if had_focus {
            if let Some(pane) = removed.as_ref() {
                send_terminal_focus_event(&pane.terminal, false);
            }
            self.send_pane_focus_event(next_pane_id, true);
            self.clear_active_pane_bell();
        }
        if let Some(pane) = removed.as_mut() {
            pane.destroy();
        }
        drop(removed);
        log::info!("Closed pane {} in tab {}", pane_id, tab_id);
        self.refresh_active_tab_title();
        self.resize_tab_panes(self.active_tab_index);
        self.invalidate();
    }

    fn close_pane_source(&mut self, source_id: u64) {
        let Some((tab_index, pane_id)) = self.source_location(source_id) else {
            return;
        };
        if self.tabs[tab_index]
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.keep_open)
        {
            log::info!("Keeping exited pane {} open", pane_id);
            return;
        }
        if self.tabs[tab_index].pane_layout.len() == 1 {
            let tab_id = self.tabs[tab_index].id;
            self.close_tab(tab_id);
            return;
        }
        let was_focused = tab_index == self.active_tab_index
            && self.tabs[tab_index].pane_layout.active() == pane_id
            && self.window_has_focus();
        let (next_pane_id, mut removed) = {
            let tab = &mut self.tabs[tab_index];
            if let Err(error) = tab.pane_layout.close(pane_id) {
                log::error!("Failed to remove exited pane: {error}");
                return;
            }
            (tab.pane_layout.active(), tab.panes.remove(&pane_id))
        };
        if was_focused {
            if let Some(pane) = removed.as_ref() {
                send_terminal_focus_event(&pane.terminal, false);
            }
            self.send_pane_focus_event_in_tab(tab_index, next_pane_id, true);
            self.clear_pane_bell(tab_index, next_pane_id);
        } else {
            self.refresh_tab_bell(tab_index);
        }
        if let Some(pane) = removed.as_mut() {
            pane.destroy();
        }
        drop(removed);
        if tab_index == self.active_tab_index {
            self.refresh_active_tab_title();
        }
        self.resize_tab_panes(tab_index);
        self.invalidate();
    }

    pub fn focus_pane(&mut self, direction: PaneDirection) {
        let bounds = self.pane_bounds();
        let previous = self
            .tabs
            .get(self.active_tab_index)
            .map(|tab| tab.pane_layout.active());
        let moved = self
            .tabs
            .get_mut(self.active_tab_index)
            .and_then(|tab| tab.pane_layout.focus_direction(direction, bounds));
        if moved.is_none() {
            return;
        }
        if let Some(active) = moved {
            if self.window_has_focus() {
                if let Some(previous) = previous {
                    self.send_pane_focus_event(previous, false);
                }
                self.send_pane_focus_event(active, true);
                self.clear_active_pane_bell();
            }
            log::info!("Focused pane {} {:?}", active, direction);
        }
        self.refresh_active_tab_title();
        // Directional focus exits zoom mode, so all panes need their split sizes.
        self.resize_tab_panes(self.active_tab_index);
        self.invalidate();
    }

    pub fn resize_active_pane(&mut self, direction: PaneDirection) {
        let bounds = self.pane_bounds();
        let amount = self
            .renderer
            .as_ref()
            .map(|renderer| match direction {
                PaneDirection::Left | PaneDirection::Right => {
                    renderer.cell_dimensions().width.round().max(1.0) as u32
                }
                PaneDirection::Up | PaneDirection::Down => {
                    renderer.cell_dimensions().height.round().max(1.0) as u32
                }
            })
            .unwrap_or(8);
        let changed = self.tabs.get_mut(self.active_tab_index).is_some_and(|tab| {
            tab.pane_layout
                .adjust_active_size(direction, amount, bounds)
        });
        if changed {
            if let Some(tab) = self.tabs.get(self.active_tab_index) {
                log::info!(
                    "Resized pane {} {:?} in tab {}",
                    tab.pane_layout.active(),
                    direction,
                    tab.id
                );
            }
            self.resize_tab_panes(self.active_tab_index);
            self.invalidate();
        }
    }

    pub fn toggle_active_pane_zoom(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active_tab_index) else {
            return;
        };
        let zoomed = tab.pane_layout.toggle_zoom();
        log::info!("Pane zoom {} in tab {}", zoomed, tab.id);
        self.resize_tab_panes(self.active_tab_index);
        self.invalidate();
    }

    fn window_has_focus(&self) -> bool {
        unsafe { GetFocus() == self.hwnd }
    }

    fn send_pane_focus_event_in_tab(&self, tab_index: usize, pane_id: PaneId, focused: bool) {
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        let Some(pane) = tab.panes.get(&pane_id) else {
            return;
        };
        send_terminal_focus_event(&pane.terminal, focused);
    }

    fn send_pane_focus_event(&self, pane_id: PaneId, focused: bool) {
        self.send_pane_focus_event_in_tab(self.active_tab_index, pane_id, focused);
    }

    fn refresh_tab_bell(&mut self, tab_index: usize) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        tab.has_bell = tab.panes.values().any(|pane| pane.has_bell);
        self.tab_bar.set_bell(tab.id, tab.has_bell);
    }

    fn clear_pane_bell(&mut self, tab_index: usize, pane_id: PaneId) {
        let sender = self
            .tabs
            .get_mut(tab_index)
            .and_then(|tab| tab.panes.get_mut(&pane_id))
            .and_then(|pane| {
                pane.has_bell = false;
                pane.daemon_cmd_tx.clone()
            });
        if let Some(sender) = sender {
            let _ = sender.send(DaemonCmd::ClearAlert);
        }
        self.refresh_tab_bell(tab_index);
    }

    fn clear_active_pane_bell(&mut self) {
        let Some(pane_id) = self
            .tabs
            .get(self.active_tab_index)
            .map(|tab| tab.pane_layout.active())
        else {
            return;
        };
        self.clear_pane_bell(self.active_tab_index, pane_id);
    }

    fn refresh_active_tab_title(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active_tab_index) else {
            return;
        };
        let Some(pane) = tab.active_pane() else {
            return;
        };
        let title_locked = pane.title_locked;
        let title = if title_locked {
            pane.title.clone()
        } else {
            let screen_title = pane.terminal.lock().unwrap().screen().title.clone();
            if screen_title.is_empty() {
                pane.title.clone()
            } else {
                screen_title
            }
        };
        tab.title_locked = title_locked;
        if !title.is_empty() {
            tab.title = title.clone();
            if let Some(pane) = tab.active_pane_mut() {
                pane.title = title.clone();
            }
            self.tab_bar.set_title(tab.id, &title);
        }
    }

    /// Close a tab
    pub fn close_tab(&mut self, tab_id: u64) {
        if let Some(index) = self.tabs.iter().position(|t| t.id == tab_id) {
            if !self.confirm_close_panes(self.tabs[index].panes.values()) {
                return;
            }
            let old_active_index = self.active_tab_index;
            let was_active = index == old_active_index;
            let had_focus = was_active && self.window_has_focus();
            if had_focus {
                let pane_id = self.tabs[index].pane_layout.active();
                self.send_pane_focus_event_in_tab(index, pane_id, false);
            }
            let mut removed = self.tabs.remove(index);
            for pane in removed.panes.values_mut() {
                pane.destroy();
            }
            drop(removed);
            self.tab_bar.remove_tab(tab_id);
            for index in 0..self.tabs.len() {
                self.resize_tab_panes(index);
            }

            if self.tabs.is_empty() {
                // Close window
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                };
            } else {
                self.active_tab_index = if index < old_active_index {
                    old_active_index - 1
                } else if was_active {
                    old_active_index.min(self.tabs.len() - 1)
                } else {
                    old_active_index
                };
                let new_active_id = self.tabs[self.active_tab_index].id;
                self.tab_bar.set_active(new_active_id);
                self.set_window_visibility(self.window_visibility);
                if had_focus {
                    let pane_id = self.tabs[self.active_tab_index].pane_layout.active();
                    self.send_pane_focus_event(pane_id, true);
                    self.clear_active_pane_bell();
                }
                self.refresh_active_tab_title();
                if let Some(ref mut renderer) = self.renderer {
                    renderer.set_background_override(
                        self.tabs[self.active_tab_index].background_color.as_deref(),
                    );
                }
                self.invalidate();
            }
        }
    }

    /// Switch to tab
    pub fn switch_to_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            let previous_index = self.active_tab_index;
            let changed = index != previous_index;
            let had_focus = self.window_has_focus();
            if changed && had_focus {
                let pane_id = self.tabs[previous_index].pane_layout.active();
                self.send_pane_focus_event_in_tab(previous_index, pane_id, false);
            }
            self.active_tab_index = index;
            let tab_id = self.tabs[index].id;
            self.tab_bar.set_active(tab_id);
            self.set_window_visibility(self.window_visibility);
            if had_focus {
                let pane_id = self.tabs[index].pane_layout.active();
                if changed {
                    self.send_pane_focus_event(pane_id, true);
                }
                self.clear_active_pane_bell();
            }
            self.refresh_active_tab_title();

            // Apply per-tab background color override
            if let Some(ref mut renderer) = self.renderer {
                renderer.set_background_override(self.tabs[index].background_color.as_deref());
            }

            self.invalidate();
        }
    }

    /// Switch to next tab
    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            let next = (self.active_tab_index + 1) % self.tabs.len();
            self.switch_to_tab(next);
        }
    }

    /// Switch to previous tab
    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            let prev = if self.active_tab_index == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab_index - 1
            };
            self.switch_to_tab(prev);
        }
    }

    /// Switch to the next tab that has an active bell indicator
    pub fn next_alerted_tab(&mut self) {
        let count = self.tabs.len();
        if count == 0 {
            return;
        }
        for offset in 1..count {
            let idx = (self.active_tab_index + offset) % count;
            if self.tabs[idx].has_bell {
                self.switch_to_tab(idx);
                return;
            }
        }
        log::debug!("No alerted tabs found");
    }

    /// Get the active terminal
    pub fn active_terminal(&self) -> Option<Arc<Mutex<Terminal>>> {
        self.tabs
            .get(self.active_tab_index)
            .and_then(TabEntry::active_terminal)
    }

    fn source_location(&self, source_id: u64) -> Option<(usize, PaneId)> {
        self.tabs.iter().enumerate().find_map(|(tab_index, tab)| {
            tab.pane_id_for_source(source_id)
                .map(|pane_id| (tab_index, pane_id))
        })
    }

    fn pane_bounds(&self) -> PaneRect {
        let (width, height) = self.terminal_pixel_size();
        PaneRect::new(0, 0, width, height)
    }

    fn resize_tab_panes(&self, tab_index: usize) {
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        let bounds = self.pane_bounds();
        for positioned in tab.pane_layout.layout(bounds) {
            let Some(pane) = tab.panes.get(&positioned.id) else {
                continue;
            };
            let pixel_width = positioned.rect.width.max(1);
            let pixel_height = positioned.rect.height.max(1);
            let (cols, rows) = if let Some(renderer) = &self.renderer {
                renderer.terminal_size(pixel_width, pixel_height)
            } else {
                (80, 24)
            };
            pane.terminal.lock().unwrap().resize_with_pixels(
                cols,
                rows,
                pixel_width.min(u16::MAX as u32) as u16,
                pixel_height.min(u16::MAX as u32) as u16,
            );
            if let Some(sender) = &pane.daemon_cmd_tx {
                let _ = sender.send(DaemonCmd::Resize {
                    cols: cols as u32,
                    rows: rows as u32,
                    pixel_width,
                    pixel_height,
                });
            }
        }
    }

    /// Send focus event to the active terminal if focus events mode is enabled (DECSET 1004)
    /// `focused`: true for focus in (\x1b[I), false for focus out (\x1b[O)
    pub fn send_focus_event(&self, focused: bool) {
        if let Some(terminal) = self.active_terminal() {
            let mut term = terminal.lock().unwrap();
            if term.screen().modes.focus_events {
                let sequence = if focused { b"\x1b[I" } else { b"\x1b[O" };
                if let Err(e) = term.write(sequence) {
                    log::error!("Failed to send focus event: {}", e);
                }
            }
        }
    }

    /// Get terminal size in cells
    pub fn terminal_size(&self) -> (usize, usize) {
        let (width, height) = self.terminal_pixel_size();

        if let Some(ref renderer) = self.renderer {
            renderer.terminal_size(width, height)
        } else {
            (80, 24)
        }
    }

    /// Get the terminal viewport size in pixels, excluding window chrome.
    pub fn terminal_pixel_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut rect).ok() };

        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;

        // Subtract chrome heights
        let tab_bar_height = self.tab_bar.height() as u32;
        let notification_bar_height = self.notification_bar.height() as u32;
        let terminal_height = height.saturating_sub(tab_bar_height + notification_bar_height);

        (width.max(1), terminal_height.max(1))
    }

    /// Handle window resize
    pub fn on_resize(&mut self, width: u32, height: u32) {
        if let Some(ref mut renderer) = self.renderer {
            renderer.resize(width, height).ok();
        }

        for index in 0..self.tabs.len() {
            self.resize_tab_panes(index);
        }
    }

    /// Report Win32 show/minimize state to every terminal session in the window.
    pub fn set_window_visibility(&mut self, visibility: cterm_core::WindowVisibility) {
        self.window_visibility = visibility;
        for (index, tab) in self.tabs.iter().enumerate() {
            let visibility = if visibility == cterm_core::WindowVisibility::Visible
                && index == self.active_tab_index
            {
                cterm_core::WindowVisibility::Visible
            } else {
                cterm_core::WindowVisibility::Hidden
            };
            for pane in tab.panes.values() {
                let mut terminal = pane.terminal.lock().unwrap();
                let mut state = terminal.screen().frontend_state();
                if state.visibility == visibility {
                    continue;
                }
                state.visibility = visibility;
                if pane.daemon_cmd_tx.is_some() {
                    let _ = terminal.set_frontend_state_collecting(state);
                } else {
                    terminal.set_frontend_state(state);
                }
                drop(terminal);
                if let Some(sender) = &pane.daemon_cmd_tx {
                    let _ = sender.send(DaemonCmd::SetFrontendState(state));
                }
            }
        }
    }

    /// Handle DPI change
    pub fn on_dpi_changed(&mut self, dpi: u32) {
        self.dpi = DpiInfo::from_dpi(dpi);
        self.tab_bar.set_dpi(self.dpi);
        self.notification_bar.set_dpi(self.dpi);

        if let Some(ref mut renderer) = self.renderer {
            renderer.update_dpi(dpi).ok();
        }
        for index in 0..self.tabs.len() {
            self.resize_tab_panes(index);
        }
    }

    /// Invalidate and request redraw
    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
            // Force immediate repaint - without UpdateWindow, WM_PAINT may be
            // deferred until the message queue is empty, causing blank terminal
            let _ = UpdateWindow(self.hwnd);
        };
    }

    /// Render the window
    pub fn render(&mut self) -> windows::core::Result<()> {
        let Some(tab) = self.tabs.get(self.active_tab_index) else {
            return Ok(());
        };
        let y_offset = self.terminal_y_offset() as u32;
        let panes: Vec<_> = tab
            .pane_layout
            .layout(self.pane_bounds())
            .into_iter()
            .filter_map(|positioned| {
                tab.panes
                    .get(&positioned.id)
                    .map(|pane| (positioned, Arc::clone(&pane.terminal), pane.has_bell))
            })
            .collect();
        let Some((_, first_terminal, _)) = panes.first() else {
            return Ok(());
        };
        let chrome_width = self.pane_bounds().width as f32;
        let tab_bar_height = self.tab_bar.height() as f32;
        let Some(renderer) = self.renderer.as_mut() else {
            return Ok(());
        };
        let tab_bar = &mut self.tab_bar;
        let notification_bar = &mut self.notification_bar;

        {
            let terminal = first_terminal.lock().unwrap();
            renderer.begin_frame(terminal.screen());
        }
        let draw_result = (|| {
            for (positioned, terminal, alerted) in &panes {
                let terminal = terminal.lock().unwrap();
                let mut rect = positioned.rect;
                rect.y = rect.y.saturating_add(y_offset);
                renderer.render_pane(terminal.screen(), rect, positioned.is_active, *alerted)?;
            }
            if let Some((target, dwrite, text_format)) = renderer.chrome_resources() {
                tab_bar.render(&target, &dwrite, chrome_width, &text_format)?;
                notification_bar.render_at(
                    &target,
                    &dwrite,
                    chrome_width,
                    &text_format,
                    tab_bar_height,
                )?;
            }
            Ok::<(), windows::core::Error>(())
        })();
        let end_result = renderer.end_frame();
        draw_result.and(end_result)
    }

    /// Handle a physical keyboard event. Text-producing keys deliberately stay
    /// on WM_CHAR so Windows remains authoritative for layouts, dead keys, and
    /// IME composition.
    pub fn on_key_event(&mut self, vk: u16, kind: KeyEventKind, extended: bool) -> bool {
        let modifiers = keycode::get_modifiers();

        if kind == KeyEventKind::Release {
            self.enhanced_text_keys.remove(&vk);
            if self.suppressed_key_releases.remove(&vk) {
                return true;
            }
            if let Some(key) = self.reported_keys.remove(&vk) {
                if let Some(terminal) = self.active_terminal() {
                    let mut term = terminal.lock().unwrap();
                    let core_modifiers = CoreModifiers::from_bits_truncate(modifiers.bits());
                    if let Some(bytes) = term.handle_reported_key_release(key, core_modifiers) {
                        if let Err(e) = term.write(&bytes) {
                            log::error!("Failed to write key release to PTY: {}", e);
                        }
                        drop(term);
                        self.invalidate();
                    }
                }
                return true;
            }
            return false;
        }

        // Check for shortcuts before forwarding key-down/repeat events.
        if let Some(key) = keycode::vk_to_keycode(vk) {
            if let Some(action) = self.shortcuts.match_event(key, modifiers) {
                self.suppressed_key_releases.insert(vk);
                self.handle_action(action.clone());
                return true;
            }
        }

        let enhanced_text = modifiers
            .intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER)
            && !keycode::is_altgr_active()
            && self.active_terminal().is_some_and(|terminal| {
                terminal
                    .lock()
                    .unwrap()
                    .screen()
                    .keyboard_enhancement_flags()
                    .contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            });

        let Some(key) = mapped_terminal_key(vk, modifiers, enhanced_text, extended) else {
            return false;
        };
        let core_modifiers = CoreModifiers::from_bits_truncate(modifiers.bits());

        if let Some(terminal) = self.active_terminal() {
            let mut term = terminal.lock().unwrap();
            if let Some(bytes) = term.handle_key_event(key, core_modifiers, kind) {
                let track_release = kind == KeyEventKind::Press
                    && term
                        .handle_reported_key_release(key, core_modifiers)
                        .is_some();
                if let Err(e) = term.write(&bytes) {
                    log::error!("Failed to write key event to PTY: {}", e);
                }
                // Drop the lock before invalidate() — UpdateWindow dispatches WM_PAINT
                // synchronously, and render() needs to lock the terminal.
                drop(term);
                if enhanced_text && matches!(key, Key::Char(_)) {
                    self.enhanced_text_keys.insert(vk);
                }
                if track_release {
                    self.reported_keys.insert(vk, key);
                }
                self.invalidate();
                return true;
            }
        }

        false
    }

    fn suppress_generated_text_message(&self) -> bool {
        !self.enhanced_text_keys.is_empty()
    }

    /// Handle character input
    pub fn on_char(&mut self, c: char) {
        if let Some(terminal) = self.active_terminal() {
            let mut term = terminal.lock().unwrap();
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            term.write(s.as_bytes()).ok();
            // Drop the lock before invalidate() — UpdateWindow dispatches WM_PAINT
            // synchronously, and render() needs to lock the terminal.
            drop(term);
        }
        self.invalidate();
    }

    /// Handle an action
    fn handle_action(&mut self, action: Action) {
        match action {
            Action::NewTab => {
                self.new_tab().ok();
                self.invalidate();
            }
            Action::CloseTab => {
                if let Some(tab) = self.tabs.get(self.active_tab_index) {
                    let id = tab.id;
                    self.close_tab(id);
                }
            }
            Action::SplitPane(direction) => self.split_active_pane(direction),
            Action::ClosePane => self.close_active_pane(),
            Action::FocusPane(direction) => self.focus_pane(direction),
            Action::ResizePane(direction) => self.resize_active_pane(direction),
            Action::TogglePaneZoom => self.toggle_active_pane_zoom(),
            Action::NextTab => self.next_tab(),
            Action::PrevTab => self.prev_tab(),
            Action::NextAlertedTab => self.next_alerted_tab(),
            Action::Tab(n) => {
                let idx = (n as usize).saturating_sub(1);
                self.switch_to_tab(idx);
            }
            Action::Copy => self.copy_selection(),
            Action::Paste => self.paste(),
            Action::ZoomIn => self.zoom_in(),
            Action::ZoomOut => self.zoom_out(),
            Action::ZoomReset => self.zoom_reset(),
            Action::ScrollUp
            | Action::ScrollDown
            | Action::ScrollPageUp
            | Action::ScrollPageDown
            | Action::ScrollToTop
            | Action::ScrollToBottom
            | Action::PromptPrevious
            | Action::PromptNext => {
                if let Some(terminal) = self.active_terminal() {
                    let mut term = terminal.lock().unwrap();
                    let page = term.rows().max(1);
                    match action {
                        Action::ScrollUp => term.scroll_viewport_up(1),
                        Action::ScrollDown => term.scroll_viewport_down(1),
                        Action::ScrollPageUp => term.scroll_viewport_up(page),
                        Action::ScrollPageDown => term.scroll_viewport_down(page),
                        Action::ScrollToTop => term.scroll_viewport_up(usize::MAX),
                        Action::ScrollToBottom => term.scroll_viewport_to_bottom(),
                        Action::PromptPrevious => {
                            term.scroll_to_previous_prompt();
                        }
                        Action::PromptNext => {
                            term.scroll_to_next_prompt();
                        }
                        _ => unreachable!(),
                    }
                    drop(term);
                }
                self.invalidate();
            }
            Action::CloseWindow => {
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                };
            }
            Action::NewWindow => {
                // New window requires app-level handling, not implemented for shortcuts
                log::debug!("NewWindow action from shortcut not implemented");
            }
            Action::FindText => self.show_find_dialog(),
            Action::ResetTerminal => {
                if let Some(terminal) = self.active_terminal() {
                    let mut term = terminal.lock().unwrap();
                    term.screen_mut().reset();
                    drop(term);
                }
                self.invalidate();
            }
            _ => {}
        }
    }

    /// Handle menu command
    pub fn on_menu_command(&mut self, cmd: u16) {
        if let Some(action) = MenuAction::from_id(cmd) {
            if crate::get_args().managed
                && matches!(
                    action,
                    MenuAction::NewTab
                        | MenuAction::NewWindow
                        | MenuAction::QuickOpen
                        | MenuAction::DockerPicker
                        | MenuAction::Preferences
                        | MenuAction::TabTemplates
                        | MenuAction::AttachSession
                        | MenuAction::SSHConnect
                        | MenuAction::ManageRemotes
                )
            {
                log::warn!("Ignoring secondary-session action in managed mode");
                return;
            }
            match action {
                MenuAction::NewTab => {
                    self.new_tab().ok();
                }
                MenuAction::NewWindow => {
                    // Launch a new instance of the application
                    if let Ok(exe) = std::env::current_exe() {
                        std::process::Command::new(exe).spawn().ok();
                    }
                }
                MenuAction::CloseTab => {
                    if let Some(tab) = self.tabs.get(self.active_tab_index) {
                        let id = tab.id;
                        self.close_tab(id);
                    }
                }
                MenuAction::CloseOtherTabs => {
                    // Close all but active
                    let active_id = self.tabs.get(self.active_tab_index).map(|t| t.id);
                    if let Some(active_id) = active_id {
                        let ids: Vec<_> = self
                            .tabs
                            .iter()
                            .filter(|t| t.id != active_id)
                            .map(|t| t.id)
                            .collect();
                        for id in ids {
                            self.close_tab(id);
                        }
                    }
                }
                MenuAction::QuickOpen => {
                    // Show Quick Open dialog
                    let templates = cterm_app::load_sticky_tabs().unwrap_or_default();
                    if let Some(template) = crate::quick_open::show_quick_open(self.hwnd, templates)
                    {
                        // Create a new tab with the selected template
                        log::info!("Quick open selected: {}", template.name);
                        if let Err(error) = self.new_tab_from_template(&template) {
                            log::error!("Failed to open template '{}': {error}", template.name);
                            crate::dialogs::show_warning(
                                self.hwnd.0 as *mut _,
                                "Template unavailable",
                                &error.to_string(),
                            );
                        }
                    }
                }
                MenuAction::DockerPicker => {
                    // Show Docker picker dialog
                    if let Some(selection) =
                        crate::docker_dialog::show_docker_picker(self.hwnd.0 as *mut _)
                    {
                        // Create a new tab with the selected Docker configuration
                        if let Err(e) = self.new_docker_tab(selection) {
                            log::error!("Failed to create Docker tab: {}", e);
                            crate::dialogs::show_error(
                                self.hwnd.0 as *mut _,
                                "Docker Error",
                                &format!("Failed to create Docker tab: {}", e),
                            );
                        }
                    }
                }
                MenuAction::Quit => {
                    unsafe {
                        let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    };
                }
                MenuAction::Copy => self.copy_selection(),
                MenuAction::CopyHtml => self.copy_selection_as_html(),
                MenuAction::Paste => self.paste(),
                MenuAction::SelectAll => self.select_all(),
                MenuAction::ZoomIn => self.zoom_in(),
                MenuAction::ZoomOut => self.zoom_out(),
                MenuAction::ZoomReset => self.zoom_reset(),
                MenuAction::Fullscreen => self.toggle_fullscreen(),
                MenuAction::SetTitle => self.show_set_title_dialog(),
                MenuAction::SetColor => self.show_set_color_dialog(),
                MenuAction::Find => self.show_find_dialog(),
                MenuAction::Reset => {
                    if let Some(terminal) = self.active_terminal() {
                        let mut term = terminal.lock().unwrap();
                        term.screen_mut().reset();
                        drop(term);
                    }
                    self.invalidate();
                }
                MenuAction::ClearReset => {
                    if let Some(terminal) = self.active_terminal() {
                        let mut term = terminal.lock().unwrap();
                        term.screen_mut().reset();
                        drop(term);
                    }
                    self.invalidate();
                }
                MenuAction::SendSignalInt => self.send_signal(2), // SIGINT
                MenuAction::SendSignalKill => self.send_signal(9), // SIGKILL
                MenuAction::SendSignalHup => self.send_signal(1), // SIGHUP
                MenuAction::SendSignalTerm => self.send_signal(15), // SIGTERM
                MenuAction::SplitPaneHorizontal => {
                    self.split_active_pane(SplitDirection::Horizontal)
                }
                MenuAction::SplitPaneVertical => self.split_active_pane(SplitDirection::Vertical),
                MenuAction::ClosePane => self.close_active_pane(),
                MenuAction::FocusPaneLeft => self.focus_pane(PaneDirection::Left),
                MenuAction::FocusPaneRight => self.focus_pane(PaneDirection::Right),
                MenuAction::FocusPaneUp => self.focus_pane(PaneDirection::Up),
                MenuAction::FocusPaneDown => self.focus_pane(PaneDirection::Down),
                MenuAction::ResizePaneLeft => self.resize_active_pane(PaneDirection::Left),
                MenuAction::ResizePaneRight => self.resize_active_pane(PaneDirection::Right),
                MenuAction::ResizePaneUp => self.resize_active_pane(PaneDirection::Up),
                MenuAction::ResizePaneDown => self.resize_active_pane(PaneDirection::Down),
                MenuAction::TogglePaneZoom => self.toggle_active_pane_zoom(),
                MenuAction::PrevTab => self.prev_tab(),
                MenuAction::NextTab => self.next_tab(),
                MenuAction::NextAlertedTab => self.next_alerted_tab(),
                MenuAction::Tab1 => self.switch_to_tab(0),
                MenuAction::Tab2 => self.switch_to_tab(1),
                MenuAction::Tab3 => self.switch_to_tab(2),
                MenuAction::Tab4 => self.switch_to_tab(3),
                MenuAction::Tab5 => self.switch_to_tab(4),
                MenuAction::Tab6 => self.switch_to_tab(5),
                MenuAction::Tab7 => self.switch_to_tab(6),
                MenuAction::Tab8 => self.switch_to_tab(7),
                MenuAction::Tab9 => self.switch_to_tab(8),
                MenuAction::Preferences => {
                    if crate::preferences_dialog::show_preferences_dialog(self.hwnd.0 as *mut _) {
                        // Reload config and apply changes
                        if let Ok(config) = cterm_app::load_config() {
                            self.config = config;
                            // TODO: Apply theme and other changes without restart
                            log::info!("Preferences saved and reloaded");
                        }
                    }
                }
                MenuAction::TabTemplates => {
                    if crate::templates_dialog::show_templates_dialog(self.hwnd.0 as *mut _) {
                        log::info!("Tab templates saved");
                    }
                }
                MenuAction::CheckUpdates => {
                    if crate::get_args().updater_enabled() {
                        crate::dialogs::show_check_updates_dialog(self.hwnd.0 as *mut _);
                    } else {
                        log::warn!("Ignoring upstream update request in managed mode");
                    }
                }
                MenuAction::About => {
                    crate::dialogs::show_about_dialog(self.hwnd.0 as *mut _);
                }
                MenuAction::DebugRelaunch => {
                    if !crate::get_args().updater_enabled() {
                        log::warn!("Ignoring debug relaunch request in managed mode");
                        return;
                    }
                    if let Ok(exe) = std::env::current_exe() {
                        let window_state = self.collect_upgrade_state();
                        if !upgrade_window_is_handoff_ready(&window_state) {
                            log::warn!(
                                "Deferring debug relaunch until every pane has a daemon session ID"
                            );
                            crate::dialogs::show_warning(
                                self.hwnd.0 as *mut _,
                                "Relaunch not ready",
                                "A terminal session is still starting. Try relaunching again in a moment.",
                            );
                            return;
                        }
                        let mut upgrade_state = cterm_app::upgrade::UpgradeState::new();
                        upgrade_state.windows.push(window_state);
                        match cterm_app::upgrade::execute_upgrade(&exe, &upgrade_state) {
                            Ok(()) => {
                                self.skip_close_confirm = true;
                                unsafe {
                                    let _ = PostMessageW(
                                        Some(self.hwnd),
                                        WM_CLOSE,
                                        WPARAM(0),
                                        LPARAM(0),
                                    );
                                };
                            }
                            Err(error) => log::error!("Failed to relaunch cterm: {error}"),
                        }
                    }
                }
                MenuAction::DebugDumpState => {
                    log::info!("=== Debug State Dump ===");
                    log::info!("Tabs: {}", self.tabs.len());
                    log::info!("Active tab: {}", self.active_tab_index);
                    for (i, tab) in self.tabs.iter().enumerate() {
                        log::info!("  Tab {}: id={}, title={}", i, tab.id, tab.title);
                    }
                    log::info!("DPI: {:?}", self.dpi);
                    log::info!("========================");
                }
                MenuAction::DebugRelaunchDaemon => {
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
                }
                MenuAction::KillDaemon => {
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
                }
                MenuAction::ViewLogs => {
                    // Show the in-app log viewer
                    crate::log_viewer::show_log_viewer(self.hwnd.0 as *mut _);
                }
                MenuAction::AttachSession => {
                    if let Some(session_id) =
                        crate::session_dialog::show_session_picker(self.hwnd.0 as *mut _)
                    {
                        log::info!("Attaching to session: {}", session_id);
                        self.attach_session_tab(
                            &session_id,
                            "Terminal".to_string(),
                            None,
                            None,
                            None,
                        );
                    }
                }
                MenuAction::SSHConnect => {
                    #[cfg(not(unix))]
                    crate::dialogs::show_warning(
                        self.hwnd.0 as *mut _,
                        "Remote transport unavailable",
                        "Remote daemon connections are not supported by the current Windows transport.",
                    );
                    #[cfg(unix)]
                    {
                        if let Some(host) =
                            crate::session_dialog::show_ssh_dialog(self.hwnd.0 as *mut _)
                        {
                            log::info!("SSH connecting to: {}", host);
                            let (cols, rows) = self.terminal_size();
                            let opts = cterm_client::CreateSessionOpts {
                                cols: cols as u32,
                                rows: rows as u32,
                                ..Default::default()
                            };
                            let remote =
                                Some((self.remote_manager.clone(), host.clone(), host, true));
                            self.spawn_daemon_tab(
                                opts,
                                "SSH".to_string(),
                                None,
                                None,
                                false,
                                remote,
                            );
                        }
                    }
                }
                MenuAction::ManageRemotes => {
                    crate::remotes_dialog::show_remotes_dialog(self.hwnd.0 as *mut _);
                }
            }
        }
    }

    /// Show set title dialog
    fn show_set_title_dialog(&mut self) {
        if let Some(tab) = self.tabs.get(self.active_tab_index) {
            let current_title = tab.title.clone();
            if let Some(new_title) =
                crate::dialogs::show_set_title_dialog(self.hwnd.0 as *mut _, &current_title)
            {
                let tab_id = tab.id;
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.title = new_title.clone();
                    tab.title_locked = true;
                    if let Some(pane) = tab.active_pane_mut() {
                        pane.title_locked = true;
                        pane.title = new_title.clone();
                    }
                    // Persist to daemon
                    if let Some(tx) = tab
                        .active_pane()
                        .and_then(|pane| pane.daemon_cmd_tx.as_ref())
                    {
                        let _ = tx.send(DaemonCmd::SetTitle(new_title.clone()));
                    }
                    self.tab_bar.set_title(tab_id, &new_title);
                    self.invalidate();
                }
            }
        }
    }

    /// Show set color dialog
    fn show_set_color_dialog(&mut self) {
        if let Some(tab) = self.tabs.get(self.active_tab_index) {
            let tab_id = tab.id;
            if let Some(color_result) = crate::dialogs::show_set_color_dialog(self.hwnd.0 as *mut _)
            {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.color = color_result.clone();
                    // Persist to daemon
                    if let Some(tx) = tab
                        .active_pane()
                        .and_then(|pane| pane.daemon_cmd_tx.as_ref())
                    {
                        let _ = tx.send(DaemonCmd::SetTabColor(
                            color_result.as_deref().unwrap_or("").to_string(),
                        ));
                    }
                    // Parse color to Rgb
                    let rgb = color_result.and_then(|c| parse_hex_color(&c));
                    self.tab_bar.set_color(tab_id, rgb);
                    self.invalidate();
                }
            }
        }
    }

    /// Show find dialog
    fn show_find_dialog(&mut self) {
        if let Some(options) = crate::dialogs::show_find_dialog(self.hwnd.0 as *mut _) {
            // Perform search in terminal
            if let Some(terminal) = self.active_terminal() {
                let mut term = terminal.lock().unwrap();
                let results =
                    term.screen()
                        .find(&options.text, options.case_sensitive, options.regex);
                if !results.is_empty() {
                    log::info!("Found {} matches for: {}", results.len(), options.text);
                    // Scroll to first result
                    if let Some(first) = results.first() {
                        term.scroll_to_line(first.line);
                    }
                } else {
                    drop(term);
                    crate::dialogs::show_message(
                        self.hwnd.0 as *mut _,
                        "Find",
                        &format!("'{}' not found", options.text),
                        winapi::um::winuser::MB_OK | winapi::um::winuser::MB_ICONINFORMATION,
                    );
                    return;
                }
                drop(term);
            }
            self.invalidate();
        }
    }

    /// Select all text in the terminal
    fn select_all(&mut self) {
        if let Some(terminal) = self.active_terminal() {
            let mut term = terminal.lock().unwrap();
            let screen = term.screen_mut();
            let total_lines = screen.total_lines();
            if total_lines > 0 {
                // Select from first line to last line
                screen.start_selection(0, 0, cterm_core::screen::SelectionMode::Char);
                // Extend to end - use a large column value for last line
                screen.extend_selection(total_lines.saturating_sub(1), usize::MAX);
            }
            drop(term);
        }
        self.invalidate();
    }

    /// Copy selection as HTML
    fn copy_selection_as_html(&mut self) {
        if let Some(terminal) = self.active_terminal() {
            let term = terminal.lock().unwrap();
            if let Some(html) = term.screen().get_selected_html(&self.theme.colors) {
                // Copy HTML to clipboard
                clipboard::copy_to_clipboard(&html).ok();
                log::debug!("Copied {} chars as HTML to clipboard", html.len());
            }
        }
    }

    /// Send a signal to the active terminal's process
    fn send_signal(&mut self, _signal: i32) {
        // On Windows, signals work differently than Unix
        // For now, we'll send a Ctrl+C equivalent for SIGINT
        if let Some(terminal) = self.active_terminal() {
            let mut term = terminal.lock().unwrap();
            // Send Ctrl+C character
            term.write(&[0x03]).ok(); // ETX (Ctrl+C)
            drop(term);
        }
        self.invalidate();
    }

    /// Zoom in (increase font size)
    fn zoom_in(&mut self) {
        if let Some(ref mut renderer) = self.renderer {
            let new_size = renderer.font_size() + 1.0;
            if new_size <= 72.0 {
                renderer.set_font_size(new_size).ok();
                self.on_font_size_changed();
            }
        }
    }

    /// Zoom out (decrease font size)
    fn zoom_out(&mut self) {
        if let Some(ref mut renderer) = self.renderer {
            let new_size = renderer.font_size() - 1.0;
            if new_size >= 6.0 {
                renderer.set_font_size(new_size).ok();
                self.on_font_size_changed();
            }
        }
    }

    /// Reset zoom to default
    fn zoom_reset(&mut self) {
        if let Some(ref mut renderer) = self.renderer {
            let default_size = self.config.appearance.font.size as f32;
            renderer.set_font_size(default_size).ok();
            self.on_font_size_changed();
        }
    }

    /// Called when font size changes to resize terminals
    fn on_font_size_changed(&mut self) {
        for index in 0..self.tabs.len() {
            self.resize_tab_panes(index);
        }
        self.invalidate();
    }

    /// Toggle fullscreen mode
    fn toggle_fullscreen(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetWindowLongW, SetWindowPos, ShowWindow, GWL_STYLE, HWND_TOP,
            SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SW_MAXIMIZE, SW_RESTORE, WS_CAPTION,
            WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
        };

        unsafe {
            let style = GetWindowLongW(self.hwnd, GWL_STYLE) as u32;
            let windowed_style =
                WS_CAPTION.0 | WS_SYSMENU.0 | WS_THICKFRAME.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0;

            if (style & windowed_style) != 0 {
                // Enter fullscreen
                let new_style = style & !windowed_style;
                SetWindowLongW(self.hwnd, GWL_STYLE, new_style as i32);
                let _ = ShowWindow(self.hwnd, SW_MAXIMIZE);
            } else {
                // Exit fullscreen
                let new_style = style | windowed_style;
                SetWindowLongW(self.hwnd, GWL_STYLE, new_style as i32);
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED,
            );
        }
    }

    /// Copy selection to clipboard
    fn copy_selection(&mut self) {
        if let Some(terminal) = self.active_terminal() {
            let term = terminal.lock().unwrap();
            if let Some(text) = term.screen().get_selected_text() {
                clipboard::copy_to_clipboard(&text).ok();
            }
        }
    }

    /// Paste from clipboard
    fn paste(&mut self) {
        if let Ok(text) = clipboard::paste_from_clipboard() {
            if let Some(terminal) = self.active_terminal() {
                let mut term = terminal.lock().unwrap();
                term.write(text.as_bytes()).ok();
                drop(term);
            }
            self.invalidate();
        }
    }

    /// Handle PTY data received
    pub fn on_pty_data(&mut self, source_id: u64) {
        let notification_was_visible = self.notification_bar.is_visible();
        // Check for file transfers from the terminal
        if let Some((tab_index, pane_id)) = self.source_location(source_id) {
            if let Ok(mut terminal) = self.tabs[tab_index].panes[&pane_id].terminal.lock() {
                let transfers = terminal.screen_mut().take_file_transfers();
                for transfer in transfers {
                    match transfer {
                        FileTransferOperation::FileReceived { id, name, data } => {
                            log::info!(
                                "File received: id={}, name={:?}, size={}",
                                id,
                                name,
                                data.len()
                            );
                            let size = data.len();
                            self.file_manager.set_pending(id, name.clone(), data);
                            self.notification_bar.show_file(id, name.as_deref(), size);
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
                            self.file_manager
                                .set_pending_streaming(id, name.clone(), result.data);
                            self.notification_bar.show_file(id, name.as_deref(), size);
                        }
                    }
                }
            }
        }

        if self.notification_bar.is_visible() != notification_was_visible {
            for index in 0..self.tabs.len() {
                self.resize_tab_panes(index);
            }
        }

        // Invalidate to redraw
        self.invalidate();
    }

    /// Handle PTY exit
    pub fn on_pty_exit(&mut self, source_id: u64) {
        self.close_pane_source(source_id);
    }

    fn on_daemon_session_ready(&mut self, source_id: u64, ready: DaemonSessionReady) {
        let Some((tab_index, pane_id)) = self.source_location(source_id) else {
            return;
        };
        let Some(pane) = self.tabs[tab_index].panes.get_mut(&pane_id) else {
            return;
        };
        log::info!(
            "Pane source {} attached to daemon session {}",
            source_id,
            ready.session_id
        );
        pane.session_id = Some(ready.session_id);
        pane.daemon_socket = ready.daemon_socket.clone();
        if let PaneBackendContext::Daemon(context) = &mut pane.backend {
            context.daemon_socket = ready.daemon_socket;
        }
    }

    /// Handle bell
    pub fn on_bell(&mut self, source_id: u64) {
        let Some((tab_index, pane_id)) = self.source_location(source_id) else {
            return;
        };
        let is_focused_pane = tab_index == self.active_tab_index
            && self.tabs[tab_index].pane_layout.active() == pane_id
            && self.window_has_focus();
        if is_focused_pane {
            self.clear_pane_bell(tab_index, pane_id);
        } else {
            if let Some(pane) = self.tabs[tab_index].panes.get_mut(&pane_id) {
                pane.has_bell = true;
            }
            self.refresh_tab_bell(tab_index);
        }
        // Redraw both the tab marker and the alerted pane border.
        self.invalidate();
    }

    /// Handle title change from terminal
    pub fn on_title_changed(&mut self, source_id: u64) {
        let Some((tab_index, pane_id)) = self.source_location(source_id) else {
            return;
        };
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            // OSC titles never overwrite a title explicitly locked for this pane.
            if tab.panes[&pane_id].title_locked {
                return;
            }

            // Get title from terminal's screen
            let new_title = {
                let term = tab.panes[&pane_id].terminal.lock().unwrap();
                term.screen().title.clone()
            };

            if !new_title.is_empty() {
                tab.panes
                    .get_mut(&pane_id)
                    .expect("the source pane exists")
                    .title = new_title.clone();
                if tab.pane_layout.active() == pane_id {
                    tab.title = new_title.clone();
                    tab.title_locked = false;
                    self.tab_bar.set_title(tab.id, &new_title);
                }
            }
        }
    }

    /// Get the vertical offset from window top to terminal content area
    fn terminal_y_offset(&self) -> f32 {
        let tab_bar_height = self.tab_bar.height() as f32;
        let notification_height = self.notification_bar.height() as f32;
        tab_bar_height + notification_height
    }

    fn pane_at_client_point(&self, x: f32, y: f32) -> Option<(PaneId, PaneRect)> {
        let y_offset = self.terminal_y_offset();
        if x < 0.0 || y < y_offset {
            return None;
        }
        let tab = self.tabs.get(self.active_tab_index)?;
        pane_at_layout_point(
            &tab.pane_layout,
            self.pane_bounds(),
            x.floor() as u32,
            (y - y_offset).floor() as u32,
        )
    }

    fn divider_at_client_point(&self, x: f32, y: f32) -> Option<PaneDivider> {
        let offset = self.terminal_y_offset();
        if x < 0.0 || y < offset {
            return None;
        }
        let tab = self.tabs.get(self.active_tab_index)?;
        if tab.pane_layout.zoomed().is_some() {
            return None;
        }
        divider_at_tree_point(
            &tab.pane_layout.tree(),
            self.pane_bounds(),
            x.floor() as u32,
            (y - offset).floor() as u32,
        )
    }

    fn begin_divider_drag(&mut self, x: f32, y: f32) -> bool {
        let Some(divider) = self.divider_at_client_point(x, y) else {
            return false;
        };
        self.mouse_report_button = None;
        self.pane_divider_drag = Some(divider);
        unsafe {
            let _ = SetCapture(self.hwnd);
        }
        true
    }

    fn update_divider_drag(&mut self, x: f32, y: f32) -> bool {
        let Some(divider) = self.pane_divider_drag.clone() else {
            return false;
        };
        let local_y = y - self.terminal_y_offset();
        let basis_points = match divider.direction {
            SplitDirection::Horizontal => {
                ratio_at_coordinate(x, divider.split_rect.x as f32, divider.split_rect.width)
            }
            SplitDirection::Vertical => ratio_at_coordinate(
                local_y,
                divider.split_rect.y as f32,
                divider.split_rect.height,
            ),
        };
        let Ok(ratio) = SplitRatio::from_basis_points(basis_points) else {
            return true;
        };
        let changed = self
            .tabs
            .get_mut(self.active_tab_index)
            .and_then(|tab| tab.pane_layout.set_split_ratio(&divider.path, ratio).ok())
            .unwrap_or(false);
        if changed {
            self.resize_tab_panes(self.active_tab_index);
            self.invalidate();
        }
        true
    }

    fn end_divider_drag(&mut self) -> bool {
        if self.pane_divider_drag.take().is_none() {
            return false;
        }
        unsafe {
            let _ = ReleaseCapture();
        }
        true
    }

    fn focus_pane_at_client_point(&mut self, x: f32, y: f32) -> bool {
        let Some((pane_id, _)) = self.pane_at_client_point(x, y) else {
            return false;
        };
        let Some(tab) = self.tabs.get(self.active_tab_index) else {
            return false;
        };
        let previous = tab.pane_layout.active();
        if previous == pane_id {
            if self.window_has_focus() {
                self.clear_active_pane_bell();
            }
            return true;
        }
        let had_focus = self.window_has_focus();
        if had_focus {
            self.send_pane_focus_event(previous, false);
        }
        if self.tabs[self.active_tab_index]
            .pane_layout
            .set_active(pane_id)
            .is_err()
        {
            return false;
        }
        if had_focus {
            self.send_pane_focus_event(pane_id, true);
            self.clear_active_pane_bell();
        }
        self.refresh_active_tab_title();
        self.resize_tab_panes(self.active_tab_index);
        self.invalidate();
        true
    }

    /// Get the hyperlink URI at a window pixel position, if any
    fn hyperlink_at(&self, x: f32, y: f32) -> Option<String> {
        let (pane_id, rect) = self.pane_at_client_point(x, y)?;
        let renderer = self.renderer.as_ref()?;
        let cell_dims = renderer.cell_dimensions();
        let tab = self.tabs.get(self.active_tab_index)?;
        let term = tab.panes.get(&pane_id)?.terminal.lock().unwrap();
        let local_x = x - rect.x as f32;
        let local_y = y - self.terminal_y_offset() - rect.y as f32;
        let (col, row) = mouse::pixel_to_cell(local_x as i32, local_y as i32, &cell_dims, 0);
        term.screen()
            .get_cell(row, col)
            .and_then(|c| c.hyperlink.as_ref())
            .map(|h| h.uri.clone())
    }

    /// Open a URL using the system default handler
    fn open_url(&self, url: &str) {
        use crate::dialog_utils::to_wide;
        use std::ptr;
        use winapi::um::shellapi::ShellExecuteW;
        use winapi::um::winuser::SW_SHOWNORMAL;

        unsafe {
            let wide_url = to_wide(url);
            let open = to_wide("open");
            ShellExecuteW(
                ptr::null_mut(),
                open.as_ptr(),
                wide_url.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    /// Map client coordinates to both terminal-cell and terminal-local pixel
    /// coordinates. Pixel rows exclude tab and notification chrome.
    fn terminal_mouse_position(&self, x: f32, y: f32) -> Option<MousePosition> {
        let (pane_id, rect) = self.pane_at_client_point(x, y)?;
        let tab = self.tabs.get(self.active_tab_index)?;
        if pane_id != tab.pane_layout.active() {
            return None;
        }
        let cell_dims = self.renderer.as_ref()?.cell_dimensions();
        let pixel_x = (x - rect.x as f32).floor() as i32;
        let pixel_y = (y - self.terminal_y_offset() - rect.y as f32).floor() as i32;
        let (col, row) = mouse::pixel_to_cell(pixel_x, pixel_y, &cell_dims, 0);
        Some(MousePosition::new(col, row, pixel_x, pixel_y))
    }

    /// Whether the active terminal has enabled any mouse tracking mode.
    fn mouse_tracking_active(&self) -> bool {
        self.active_terminal()
            .map(|t| t.lock().unwrap().screen().modes.mouse_mode != MouseMode::None)
            .unwrap_or(false)
    }

    /// Forward a mouse event to a mouse-tracking application. Returns true if a
    /// report was sent (the event was consumed). Must not be called while holding
    /// the terminal lock.
    fn forward_mouse_event(&self, event: ReportMouseEvent, x: f32, y: f32) -> bool {
        let Some(position) = self.terminal_mouse_position(x, y) else {
            return false;
        };
        let Some(terminal) = self.active_terminal() else {
            return false;
        };
        let mut term = terminal.lock().unwrap();
        let mode = term.screen().modes.mouse_mode;
        if mode == MouseMode::None {
            return false;
        }
        let encoding = term.screen().modes.mouse_encoding;
        let consumed = if let Some(seq) =
            encode_mouse_event(mode, encoding, event, position, current_mouse_modifiers())
        {
            let _ = term.write(&seq);
            true
        } else {
            false
        };
        drop(term);
        if consumed {
            self.invalidate();
        }
        consumed
    }

    pub fn on_mouse_down(&mut self, x: f32, y: f32) {
        let tab_bar_height = self.tab_bar.height() as f32;
        let notification_height = self.notification_bar.height() as f32;

        if y < tab_bar_height && self.tab_bar.is_visible() {
            let (tab_id, close, new_tab) = self.tab_bar.hit_test(x, y);
            if new_tab {
                if let Err(error) = self.new_tab() {
                    log::error!("Failed to create tab: {error}");
                }
            } else if let Some(tab_id) = tab_id {
                if close {
                    self.close_tab(tab_id);
                } else if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
                    self.switch_to_tab(index);
                }
            }
            return;
        }

        // Notification bar is right below tab bar
        if y >= tab_bar_height && y < tab_bar_height + notification_height {
            // Adjust y coordinate relative to notification bar
            let rel_y = y - tab_bar_height;
            if let Some(action) = self.notification_bar.hit_test(x, rel_y) {
                self.handle_notification_action(action);
            }
            return;
        }

        if self.begin_divider_drag(x, y) {
            return;
        }

        if !self.focus_pane_at_client_point(x, y) {
            return;
        }

        // Ctrl+click to open hyperlinks in the terminal area
        let ctrl_pressed = unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(
                windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL.0 as i32,
            ) < 0
        };
        if ctrl_pressed {
            if let Some(uri) = self.hyperlink_at(x, y) {
                self.open_url(&uri);
                return;
            }
        }

        // Forward to a mouse-tracking application unless Shift is held (Shift is
        // reserved for local interaction, matching the xterm/VTE convention).
        if !shift_pressed()
            && self.mouse_tracking_active()
            && self.forward_mouse_event(ReportMouseEvent::Press(ReportButton::Left), x, y)
        {
            self.mouse_report_button = Some(ReportButton::Left);
            self.last_reported_mouse_position = self.terminal_mouse_position(x, y);
        }
    }

    /// Handle mouse button release.
    pub fn on_mouse_up(&mut self, x: f32, y: f32) {
        if self.end_divider_drag() {
            return;
        }
        // If a press was forwarded to a mouse-tracking app, report the release.
        if let Some(button) = self.mouse_report_button.take() {
            self.forward_mouse_event(ReportMouseEvent::Release(button), x, y);
        }
    }

    /// Handle middle-button press.
    pub fn on_middle_down(&mut self, x: f32, y: f32) {
        if !self.focus_pane_at_client_point(x, y) {
            return;
        }
        if !shift_pressed()
            && self.mouse_tracking_active()
            && self.forward_mouse_event(ReportMouseEvent::Press(ReportButton::Middle), x, y)
        {
            self.mouse_report_button = Some(ReportButton::Middle);
            self.last_reported_mouse_position = self.terminal_mouse_position(x, y);
        }
    }

    /// Handle mouse wheel: forward to a tracking app, translate to cursor keys on
    /// the alternate screen (alternate-scroll), or scroll the local scrollback.
    pub fn on_wheel(&mut self, delta: i32, x: f32, y: f32) {
        self.last_mouse_pos = (x, y);
        let up = delta > 0;
        let shift = shift_pressed();
        let Some((pane_id, rect)) = self.pane_at_client_point(x, y) else {
            return;
        };
        let Some(terminal) = self
            .tabs
            .get(self.active_tab_index)
            .and_then(|tab| tab.panes.get(&pane_id))
            .map(|pane| Arc::clone(&pane.terminal))
        else {
            return;
        };

        if !shift {
            // 1) Application is tracking the mouse: forward a wheel report.
            let tracking = terminal.lock().unwrap().screen().modes.mouse_mode != MouseMode::None;
            if tracking {
                let button = if up {
                    ReportButton::WheelUp
                } else {
                    ReportButton::WheelDown
                };
                let Some(cell_dims) = self
                    .renderer
                    .as_ref()
                    .map(|renderer| renderer.cell_dimensions())
                else {
                    return;
                };
                let pixel_x = (x - rect.x as f32).floor() as i32;
                let pixel_y = (y - self.terminal_y_offset() - rect.y as f32).floor() as i32;
                let (col, row) = mouse::pixel_to_cell(pixel_x, pixel_y, &cell_dims, 0);
                let position = MousePosition::new(col, row, pixel_x, pixel_y);
                let mut term = terminal.lock().unwrap();
                let mode = term.screen().modes.mouse_mode;
                let encoding = term.screen().modes.mouse_encoding;
                if let Some(sequence) = encode_mouse_event(
                    mode,
                    encoding,
                    ReportMouseEvent::Press(button),
                    position,
                    current_mouse_modifiers(),
                ) {
                    let _ = term.write(&sequence);
                }
                drop(term);
                self.invalidate();
                return;
            }

            // 2) Alternate screen + alternate-scroll: translate to cursor keys so
            //    pagers (less/man) scroll.
            let mut term = terminal.lock().unwrap();
            if term.screen().modes.alternate_screen && term.screen().modes.alternate_scroll {
                let key = if up {
                    cterm_core::term::Key::Up
                } else {
                    cterm_core::term::Key::Down
                };
                if let Some(bytes) = term.handle_key(key, cterm_core::term::Modifiers::empty()) {
                    for _ in 0..3 {
                        let _ = term.write(&bytes);
                    }
                }
                drop(term);
                self.invalidate();
                return;
            }
            drop(term);
        }

        // 3) Default: scroll the local scrollback viewport.
        let mut term = terminal.lock().unwrap();
        if up {
            term.scroll_viewport_up(3);
        } else {
            term.scroll_viewport_down(3);
        }
        drop(term);
        self.invalidate();
    }

    /// Handle mouse move for hyperlink hover and drag forwarding.
    pub fn on_mouse_move(&mut self, x: f32, y: f32) {
        self.last_mouse_pos = (x, y);

        if self.update_divider_drag(x, y) {
            return;
        }

        // Forward held-button or all-motion reporting. Cell encodings are
        // coalesced by cell; mode 1016 preserves every pixel transition.
        if !shift_pressed() {
            let reporting_modes = self.active_terminal().map(|terminal| {
                let term = terminal.lock().unwrap();
                (
                    term.screen().modes.mouse_mode,
                    term.screen().modes.mouse_encoding,
                )
            });
            let event = self
                .mouse_report_button
                .map(|button| ReportMouseEvent::Motion(Some(button)))
                .or_else(|| {
                    reporting_modes
                        .is_some_and(|(mode, _)| mode == MouseMode::AnyEvent)
                        .then_some(ReportMouseEvent::Motion(None))
                });

            if let (Some(event), Some((_, encoding)), Some(position)) =
                (event, reporting_modes, self.terminal_mouse_position(x, y))
            {
                if mouse_position_changed(encoding, self.last_reported_mouse_position, position) {
                    self.last_reported_mouse_position = Some(position);
                    self.forward_mouse_event(event, x, y);
                }
                return;
            }
        }
        self.last_reported_mouse_position = self.terminal_mouse_position(x, y);

        let divider = self.divider_at_client_point(x, y);
        let has_link = divider.is_none() && self.hyperlink_at(x, y).is_some();

        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{LoadCursorW, SetCursor};
            let cursor = if let Some(divider) = divider {
                LoadCursorW(
                    None,
                    if divider.direction == SplitDirection::Horizontal {
                        IDC_SIZEWE
                    } else {
                        IDC_SIZENS
                    },
                )
                .unwrap_or_default()
            } else if has_link {
                LoadCursorW(None, IDC_HAND).unwrap_or_default()
            } else {
                LoadCursorW(None, IDC_IBEAM).unwrap_or_default()
            };
            let _ = SetCursor(Some(cursor));
        }
    }

    /// Handle right-click for context menu
    pub fn on_right_click(&mut self, x: f32, y: f32) {
        // Check if click is in tab bar area
        let tab_bar_height = self.tab_bar.height() as f32;

        if y < tab_bar_height && self.tab_bar.is_visible() {
            // Hit test the tab bar
            let (tab_id, _is_close, _is_new) = self.tab_bar.hit_test(x, y);
            if let Some(tab_id) = tab_id {
                self.show_tab_context_menu(tab_id, x as i32, y as i32);
            }
            return;
        }

        if !self.focus_pane_at_client_point(x, y) {
            return;
        }

        // Forward to a mouse-tracking application unless Shift is held.
        if !shift_pressed()
            && self.mouse_tracking_active()
            && self.forward_mouse_event(ReportMouseEvent::Press(ReportButton::Right), x, y)
        {
            self.mouse_report_button = Some(ReportButton::Right);
            self.last_reported_mouse_position = self.terminal_mouse_position(x, y);
            return;
        }

        // Check for hyperlink under cursor in terminal area
        if let Some(uri) = self.hyperlink_at(x, y) {
            self.show_hyperlink_context_menu(x as i32, y as i32, &uri);
        }
    }

    /// Show context menu for a hyperlink
    fn show_hyperlink_context_menu(&mut self, x: i32, y: i32, uri: &str) {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreatePopupMenu, DestroyMenu, InsertMenuW, TrackPopupMenu, MF_STRING, TPM_LEFTALIGN,
            TPM_RETURNCMD, TPM_TOPALIGN,
        };

        const CMD_OPEN_URL: u32 = 11001;
        const CMD_COPY_URL: u32 = 11002;

        let uri = uri.to_string();

        unsafe {
            let menu = CreatePopupMenu().unwrap();

            let open_text: Vec<u16> = "Open URL\0".encode_utf16().collect();
            let _ = InsertMenuW(
                menu,
                0,
                MF_STRING,
                CMD_OPEN_URL as usize,
                PCWSTR(open_text.as_ptr()),
            );

            let copy_text: Vec<u16> = "Copy URL\0".encode_utf16().collect();
            let _ = InsertMenuW(
                menu,
                1,
                MF_STRING,
                CMD_COPY_URL as usize,
                PCWSTR(copy_text.as_ptr()),
            );

            // Get screen coordinates
            let mut pt = windows::Win32::Foundation::POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.hwnd, &mut pt);

            let cmd = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RETURNCMD,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );

            if cmd.as_bool() {
                match cmd.0 as u32 {
                    CMD_OPEN_URL => {
                        self.open_url(&uri);
                    }
                    CMD_COPY_URL => {
                        let _ = clipboard::copy_to_clipboard(&uri);
                    }
                    _ => {}
                }
            }

            let _ = DestroyMenu(menu);
        }
    }

    /// Show context menu for a tab
    fn show_tab_context_menu(&mut self, tab_id: u64, x: i32, y: i32) {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreatePopupMenu, InsertMenuW, TrackPopupMenu, MF_STRING, TPM_LEFTALIGN, TPM_TOPALIGN,
        };

        const CMD_RENAME: u32 = 10001;
        const CMD_SET_COLOR: u32 = 10002;

        unsafe {
            let menu = CreatePopupMenu().unwrap();

            // Add menu items
            let rename_text: Vec<u16> = "Rename Tab...\0".encode_utf16().collect();
            let _ = InsertMenuW(
                menu,
                0,
                MF_STRING,
                CMD_RENAME as usize,
                PCWSTR(rename_text.as_ptr()),
            );

            let color_text: Vec<u16> = "Set Tab Color...\0".encode_utf16().collect();
            let _ = InsertMenuW(
                menu,
                1,
                MF_STRING,
                CMD_SET_COLOR as usize,
                PCWSTR(color_text.as_ptr()),
            );

            // Get screen coordinates
            let mut pt = windows::Win32::Foundation::POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.hwnd, &mut pt);

            // Show the menu
            let cmd = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN
                    | TPM_TOPALIGN
                    | windows::Win32::UI::WindowsAndMessaging::TPM_RETURNCMD,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );

            // Handle the selected command
            if cmd.as_bool() {
                match cmd.0 as u32 {
                    CMD_RENAME => {
                        self.handle_tab_rename(tab_id);
                    }
                    CMD_SET_COLOR => {
                        self.handle_tab_set_color(tab_id);
                    }
                    _ => {}
                }
            }

            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(menu);
        }
    }

    /// Handle tab rename from context menu
    fn handle_tab_rename(&mut self, tab_id: u64) {
        // Get current title
        let current_title = self
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map(|t| t.title.clone())
            .unwrap_or_default();

        // Show input dialog
        if let Some(new_title) = crate::dialogs::show_input_dialog_win(
            self.hwnd,
            "Rename Tab",
            "Enter new tab name:",
            &current_title,
        ) {
            // Update tab title
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                // Persist to daemon
                if let Some(tx) = tab
                    .active_pane()
                    .and_then(|pane| pane.daemon_cmd_tx.as_ref())
                {
                    let _ = tx.send(DaemonCmd::SetTitle(new_title.clone()));
                }
                tab.title = new_title.clone();
                tab.title_locked = true;
                if let Some(pane) = tab.active_pane_mut() {
                    pane.title = new_title.clone();
                    pane.title_locked = true;
                }
            }
            self.tab_bar.set_title(tab_id, &new_title);
            self.invalidate();
        }
    }

    /// Handle tab set color from context menu
    fn handle_tab_set_color(&mut self, tab_id: u64) {
        // Show color picker dialog
        if let Some(color_opt) = crate::dialogs::show_set_color_dialog_win(self.hwnd) {
            // Update tab color
            let rgb = color_opt.as_ref().and_then(|hex| parse_hex_color(hex));
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                // Persist to daemon
                if let Some(tx) = tab
                    .active_pane()
                    .and_then(|pane| pane.daemon_cmd_tx.as_ref())
                {
                    let _ = tx.send(DaemonCmd::SetTabColor(
                        color_opt.as_deref().unwrap_or("").to_string(),
                    ));
                }
                tab.color = color_opt;
            }
            self.tab_bar.set_color(tab_id, rgb);
            self.invalidate();
        }
    }

    /// Handle notification bar action
    fn handle_notification_action(&mut self, action: NotificationAction) {
        if let Some(file_id) = self.notification_bar.pending_file_id() {
            match action {
                NotificationAction::Save => {
                    self.save_file(file_id, false);
                }
                NotificationAction::SaveAs => {
                    self.save_file(file_id, true);
                }
                NotificationAction::Discard => {
                    self.file_manager.discard(file_id);
                    self.notification_bar.hide();
                    for index in 0..self.tabs.len() {
                        self.resize_tab_panes(index);
                    }
                    self.invalidate();
                }
            }
        }
    }

    /// Save file (optionally with dialog)
    fn save_file(&mut self, file_id: u64, show_dialog: bool) {
        // Get default path from file manager
        let default_path = self.file_manager.default_save_path();

        let save_path = if show_dialog {
            // Show save dialog - need a path or empty path
            if let Some(ref path) = default_path {
                crate::dialogs::show_save_dialog(self.hwnd, path)
            } else {
                crate::dialogs::show_save_dialog(self.hwnd, std::path::Path::new("download"))
            }
        } else {
            default_path
        };

        if let Some(path) = save_path {
            match self.file_manager.save_to_path(file_id, &path) {
                Ok(_size) => {
                    log::info!("File saved to {:?}", path);
                }
                Err(e) => {
                    log::error!("Failed to save file: {}", e);
                    crate::dialogs::show_error_msg(
                        self.hwnd,
                        &format!("Failed to save file: {}", e),
                    );
                }
            }
        }

        self.notification_bar.hide();
        for index in 0..self.tabs.len() {
            self.resize_tab_panes(index);
        }
        self.invalidate();
    }
}

/// Start one deadline-driven synchronized-update watchdog for a terminal.
/// New deadlines replace old ones, so rapid application frames never create
/// an unbounded collection of sleeping threads.
fn spawn_synchronized_update_watchdog(
    hwnd: usize,
    tab_id: u64,
    terminal: Arc<Mutex<Terminal>>,
) -> std::sync::mpsc::Sender<Option<Instant>> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<Instant>>();
    thread::spawn(move || {
        let mut deadline: Option<Instant> = None;
        loop {
            let message = match deadline {
                Some(target) => rx.recv_timeout(target.saturating_duration_since(Instant::now())),
                None => match rx.recv() {
                    Ok(value) => {
                        deadline = value;
                        continue;
                    }
                    Err(_) => break,
                },
            };

            match message {
                Ok(value) => deadline = value,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let (redraw, next_deadline) = {
                        let mut term = terminal.lock().unwrap();
                        let redraw = term.expire_synchronized_update();
                        (redraw, term.synchronized_update_deadline())
                    };
                    deadline = next_deadline;
                    if redraw {
                        post_message(hwnd, WM_APP_PTY_DATA, tab_id);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    tx
}

/// Window class name
pub const WINDOW_CLASS: &str = "ctermWindow";

/// Register the window class
pub fn register_window_class() -> windows::core::Result<()> {
    let class_name: Vec<u16> = WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: std::mem::size_of::<*mut WindowState>() as i32,
        hInstance: unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)? }.into(),
        hIcon: HICON::default(),
        hCursor: unsafe { LoadCursorW(None, IDC_IBEAM)? },
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: HICON::default(),
    };

    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err(windows::core::Error::from_win32());
    }

    Ok(())
}

/// Create the main window
pub fn create_window(config: &Config, theme: &Theme) -> windows::core::Result<HWND> {
    let class_name: Vec<u16> = WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = "cterm".encode_utf16().chain(std::iter::once(0)).collect();

    let dpi = dpi::get_system_dpi();
    let width = dpi::scale_by_dpi(800, dpi);
    let height = dpi::scale_by_dpi(600, dpi);

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            None,
            None,
            None,
            None,
        )?
    };

    // Create window state
    let mut state = Box::new(WindowState::new(hwnd, config, theme));
    state.init_renderer()?;
    let args = crate::get_args();
    let opts = args.initial_session_options(config, 0, 0);
    if args.managed {
        state.spawn_daemon_tab(opts, args.initial_title(config), None, None, false, None);
    } else {
        state
            .new_tab_with_options(opts, args.initial_title(config), args.title.is_some())
            .map_err(|e| {
                log::error!("Failed to create initial tab: {}", e);
                windows::core::Error::from_win32()
            })?;
    }

    if let Some(ref title) = args.title {
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
        }
    }

    // Install the state before changing window geometry: ShowWindow and the
    // fullscreen transition can synchronously dispatch WM_SIZE.
    let state_ptr = Box::into_raw(state);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    if args.fullscreen {
        unsafe {
            (*state_ptr).toggle_fullscreen();
        }
    } else if args.maximized {
        unsafe {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }

    Ok(hwnd)
}

/// Create a window and restore tabs from upgrade state
///
/// Reconnects to daemon sessions and restores window geometry, tab colors,
/// and custom titles from the upgrade state.
pub fn create_window_from_upgrade(
    config: &Config,
    theme: &Theme,
    window_state: &cterm_app::upgrade::WindowUpgradeState,
) -> windows::core::Result<HWND> {
    let class_name: Vec<u16> = WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = "cterm".encode_utf16().chain(std::iter::once(0)).collect();

    // Use saved window geometry or defaults
    let dpi = dpi::get_system_dpi();
    let width = if window_state.width > 0 {
        window_state.width
    } else {
        dpi::scale_by_dpi(800, dpi)
    };
    let height = if window_state.height > 0 {
        window_state.height
    } else {
        dpi::scale_by_dpi(600, dpi)
    };
    let x = if window_state.x != 0 || window_state.y != 0 {
        window_state.x
    } else {
        CW_USEDEFAULT
    };
    let y = if window_state.x != 0 || window_state.y != 0 {
        window_state.y
    } else {
        CW_USEDEFAULT
    };

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            width,
            height,
            None,
            None,
            None,
            None,
        )?
    };

    let mut state = Box::new(WindowState::new(hwnd, config, theme));
    state.init_renderer()?;

    // Reconnect to daemon sessions
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            log::error!("Failed to create tokio runtime: {}", e);
            windows::core::Error::from_win32()
        })?;

    let mut any_restored = false;
    for tab_state in &window_state.tabs {
        let (layout, pane_states) = match upgrade_pane_records(tab_state) {
            Ok(records) => records,
            Err(error) => {
                log::error!("Cannot restore tab '{}': {error}", tab_state.title);
                continue;
            }
        };
        let pane_ids = layout.pane_ids();

        let mut restored = Vec::with_capacity(pane_states.len());
        for pane_state in &pane_states {
            let Some(session_id) = pane_state.session_id.as_ref() else {
                log::error!(
                    "Cannot restore a non-daemon pane in tab '{}'",
                    tab_state.title
                );
                restored.clear();
                break;
            };
            #[cfg(not(unix))]
            if let Some(remote_name) = pane_state.remote_name.as_deref() {
                let reason = format!(
                    "remote '{remote_name}' requires a transport that is currently unavailable on Windows"
                );
                log::error!("Failed to reconnect session {session_id}: {reason}");
                if let Some(pane) = state.make_unavailable_remote_pane(pane_state, &reason) {
                    restored.push(pane);
                    continue;
                }
                restored.clear();
                break;
            }
            let connection = rt.block_on(async {
                if let Some(remote_name) = pane_state.remote_name.as_ref() {
                    let remote = config
                        .remotes
                        .iter()
                        .find(|remote| remote.name == *remote_name)
                        .ok_or_else(|| format!("remote '{remote_name}' is not configured"))?;
                    state
                        .remote_manager
                        .get_or_connect(&remote.name, &remote.host, remote.ssh_compression)
                        .await
                        .map_err(|error| error.to_string())
                } else if let Some(path) = pane_state.daemon_socket.as_ref() {
                    cterm_client::DaemonConnection::connect_unix(path, false)
                        .await
                        .map_err(|error| error.to_string())
                } else {
                    cterm_client::DaemonConnection::connect_local()
                        .await
                        .map_err(|error| error.to_string())
                }
            });
            let connection = match connection {
                Ok(connection) => connection,
                Err(error) => {
                    log::error!("Failed to reconnect session {session_id}: {error}");
                    restored.clear();
                    break;
                }
            };
            let alerted = rt
                .block_on(connection.get_session(session_id))
                .map(|session| session.alerted)
                .unwrap_or(false);
            match rt.block_on(connection.attach_session(session_id, 80, 24)) {
                Ok((handle, screen)) => {
                    let socket = handle
                        .socket_path()
                        .map(std::path::Path::to_path_buf)
                        .or_else(|| pane_state.daemon_socket.clone());
                    if let Some(pane) =
                        state.make_attached_pane(pane_state, screen, socket, alerted)
                    {
                        restored.push(pane);
                    }
                    // attach_session fetched the snapshot and incremented the
                    // daemon's client count. The pane reader owns a distinct
                    // no-snapshot attachment, so release this temporary one.
                    if let Err(error) = rt.block_on(handle.detach()) {
                        log::warn!(
                            "Failed to detach snapshot handle for session {session_id}: {error}"
                        );
                    }
                }
                Err(error) => {
                    log::error!("Failed to reconnect session {session_id}: {error}");
                    restored.clear();
                    break;
                }
            }
        }
        if restored.len() != pane_ids.len() {
            continue;
        }

        let panes: BTreeMap<_, _> = pane_ids.into_iter().zip(restored).collect();
        let has_bell = panes.values().any(|pane| pane.has_bell);
        let title_locked = tab_state.custom_title.is_some();
        state.tabs.push(TabEntry {
            id: tab_state.id,
            title: tab_state.title.clone(),
            color: tab_state.color.clone(),
            background_color: None,
            has_bell,
            title_locked,
            pane_layout: layout,
            panes,
        });
        state.tab_bar.add_tab(tab_state.id, &tab_state.title);
        state.tab_bar.set_bell(tab_state.id, has_bell);
        if let Some(color) = tab_state.color.as_deref().and_then(parse_hex_color) {
            state.tab_bar.set_color(tab_state.id, Some(color));
        }
        any_restored = true;
    }

    if let Some(next_id) = state
        .tabs
        .iter()
        .map(|tab| tab.id)
        .max()
        .and_then(|id| id.checked_add(1))
    {
        state.next_tab_id.store(next_id, Ordering::SeqCst);
    }

    // If no sessions were restored, create a fresh tab
    if !any_restored {
        state.new_tab().map_err(|e| {
            log::error!("Failed to create initial tab: {}", e);
            windows::core::Error::from_win32()
        })?;
    }

    // Restore active tab
    if !state.tabs.is_empty() {
        state.switch_to_tab(window_state.active_tab.min(state.tabs.len() - 1));
        for index in 0..state.tabs.len() {
            state.resize_tab_panes(index);
        }
    }

    // Store state pointer in window
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    }

    // Restore fullscreen/maximized state
    if window_state.fullscreen {
        // Toggle fullscreen via the window state method
        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
        if !state_ptr.is_null() {
            let state = unsafe { &mut *state_ptr };
            state.toggle_fullscreen();
        }
    } else if window_state.maximized {
        unsafe {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }

    Ok(hwnd)
}

/// Start a background thread that connects to daemon, creates a session, and streams output.
fn start_daemon_create_thread(
    hwnd: usize,
    tab_id: u64,
    terminal: Arc<Mutex<Terminal>>,
    opts: cterm_client::CreateSessionOpts,
    remote: Option<(cterm_client::RemoteManager, String, String, bool)>,
    daemon_socket: Option<std::path::PathBuf>,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<DaemonCmd>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("Failed to create tokio runtime: {}", e);
                post_tab_exit(hwnd, tab_id);
                return;
            }
        };

        rt.block_on(async move {
            let conn = if let Some((ref mgr, ref name, ref host, compress)) = remote {
                match mgr.get_or_connect(name, host, compress).await {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("Failed to connect to remote: {}", e);
                        post_tab_exit(hwnd, tab_id);
                        return;
                    }
                }
            } else if let Some(ref path) = daemon_socket {
                match cterm_client::DaemonConnection::connect_unix(path, false).await {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("Failed to connect to daemon {}: {}", path.display(), e);
                        post_tab_exit(hwnd, tab_id);
                        return;
                    }
                }
            } else {
                match cterm_client::DaemonConnection::connect_local().await {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("Failed to connect to local daemon: {}", e);
                        post_tab_exit(hwnd, tab_id);
                        return;
                    }
                }
            };

            let session = match conn.create_session(opts).await {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to create daemon session: {}", e);
                    post_tab_exit(hwnd, tab_id);
                    return;
                }
            };

            post_daemon_session_ready(
                hwnd,
                tab_id,
                DaemonSessionReady {
                    session_id: session.session_id().to_string(),
                    daemon_socket: session.socket_path().map(std::path::Path::to_path_buf),
                },
            );

            run_daemon_io_loop(hwnd, tab_id, terminal, session, cmd_rx).await;
        });
    })
}

/// Start a background thread that connects to daemon, attaches to a session, and streams output.
///
/// `daemon_socket` specifies which socket to connect to. For remote (SSH-tunneled)
/// sessions this is the local forwarded socket; for local sessions it's None.
#[allow(clippy::too_many_arguments)]
fn start_daemon_attach_thread(
    hwnd: usize,
    tab_id: u64,
    terminal: Arc<Mutex<Terminal>>,
    session_id: String,
    cols: u32,
    rows: u32,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<DaemonCmd>,
    daemon_socket: Option<std::path::PathBuf>,
    base_palette: ColorPalette,
    frontend_state: cterm_core::FrontendState,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("Failed to create tokio runtime: {}", e);
                post_tab_exit(hwnd, tab_id);
                return;
            }
        };

        rt.block_on(async move {
            let conn = match if let Some(ref path) = daemon_socket {
                cterm_client::DaemonConnection::connect_unix(path, false).await
            } else {
                cterm_client::DaemonConnection::connect_local().await
            } {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Failed to connect to daemon: {}", e);
                    post_tab_exit(hwnd, tab_id);
                    return;
                }
            };

            // The tab already has its screen applied; this reader only needs the
            // PTY stream, so skip the snapshot to avoid re-transferring the full
            // scrollback a second time per session.
            let session = match conn
                .attach_session_no_snapshot(&session_id, cols, rows)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to attach to session {}: {}", session_id, e);
                    post_tab_exit(hwnd, tab_id);
                    return;
                }
            };

            if let Err(error) = session.set_base_palette(&base_palette).await {
                log::warn!("Failed to synchronize frontend palette with daemon: {error}");
            }
            if let Err(error) = session.set_frontend_state(frontend_state).await {
                log::warn!("Failed to synchronize frontend state with daemon: {error}");
            }

            run_daemon_io_loop(hwnd, tab_id, terminal, session, cmd_rx).await;
        });
    })
}

/// Run the daemon I/O loop: handles write/resize commands and streams output.
async fn run_daemon_io_loop(
    hwnd: usize,
    tab_id: u64,
    terminal: Arc<Mutex<Terminal>>,
    session: cterm_client::SessionHandle,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<DaemonCmd>,
) {
    // Shared cancellation for process exit and explicit frontend detach.
    let exit_notify = std::sync::Arc::new(tokio::sync::Notify::new());

    // Spawn command handler for write/resize
    let cmd_session = session.clone();
    let exit_notify_command = std::sync::Arc::clone(&exit_notify);
    tokio::spawn(async move {
        // Try to open a streaming-input RPC for low-latency keystroke
        // delivery. Falls back to batched fire-and-forget write_input
        // calls if the daemon doesn't support StreamInput.
        let input_stream = if cmd_session.supports_stream_input().await {
            match cmd_session.open_input_stream().await {
                Ok(tx) => {
                    log::debug!("Using StreamInput for low-latency writes");
                    Some(tx)
                }
                Err(e) => {
                    log::warn!("Failed to open input stream, falling back: {}", e);
                    None
                }
            }
        } else {
            log::debug!("Daemon does not support StreamInput, using batched write_input");
            None
        };

        let mut pushback: Option<DaemonCmd> = None;
        loop {
            let cmd = match pushback.take() {
                Some(c) => c,
                None => match cmd_rx.recv().await {
                    Some(c) => c,
                    None => break,
                },
            };

            match cmd {
                DaemonCmd::Write(data) => {
                    if let Some(ref tx) = input_stream {
                        if tx.send(data).is_err() {
                            log::error!("Input stream closed unexpectedly");
                            break;
                        }
                    } else {
                        // Fallback: batch consecutive writes, fire-and-forget the RPC
                        let mut batch = data;
                        while let Ok(next) = cmd_rx.try_recv() {
                            match next {
                                DaemonCmd::Write(more) => batch.extend_from_slice(&more),
                                other => {
                                    pushback = Some(other);
                                    break;
                                }
                            }
                        }
                        let s = cmd_session.clone();
                        tokio::spawn(async move {
                            if let Err(e) = s.write_input(&batch).await {
                                log::error!("Failed to write to daemon: {}", e);
                            }
                        });
                    }
                }
                DaemonCmd::Resize {
                    cols,
                    rows,
                    pixel_width,
                    pixel_height,
                } => {
                    if let Err(e) = cmd_session
                        .resize_with_pixels(cols, rows, pixel_width, pixel_height)
                        .await
                    {
                        log::error!("Failed to resize daemon session: {}", e);
                    }
                }
                DaemonCmd::SetTitle(title) => {
                    if let Err(e) = cmd_session.set_custom_title(&title).await {
                        log::error!("Failed to set custom title: {}", e);
                    }
                }
                DaemonCmd::SetTabColor(color) => {
                    if let Err(e) = cmd_session.set_metadata(None, Some(&color), None).await {
                        log::error!("Failed to set tab color: {}", e);
                    }
                }
                DaemonCmd::SetTemplateName(name) => {
                    if let Err(e) = cmd_session.set_metadata(None, None, Some(&name)).await {
                        log::error!("Failed to set template name: {}", e);
                    }
                }
                DaemonCmd::SetFrontendState(state) => {
                    if let Err(error) = cmd_session.set_frontend_state(state).await {
                        log::error!("Failed to update daemon frontend state: {error}");
                    }
                }
                DaemonCmd::ClearAlert => {
                    if let Err(error) = cmd_session.clear_alert().await {
                        log::warn!("Failed to clear daemon bell alert: {error}");
                    }
                }
                DaemonCmd::Close => {
                    if let Err(error) = cmd_session.detach().await {
                        log::warn!("Failed to detach daemon session: {error}");
                    }
                    exit_notify_command.notify_one();
                    break;
                }
                DaemonCmd::Destroy => {
                    if let Err(error) = cmd_session.destroy().await {
                        log::warn!("Failed to destroy daemon session: {error}");
                    }
                    exit_notify_command.notify_one();
                    break;
                }
            }
        }
    });

    // Subscribe to event stream (process exit, etc.)
    let event_session = session.clone();
    let exit_notify_event = std::sync::Arc::clone(&exit_notify);
    tokio::spawn(async move {
        match event_session.stream_events().await {
            Ok(mut stream) => {
                use futures::StreamExt;
                while let Some(result) = stream.next().await {
                    if let Ok(event) = result {
                        match event.event {
                            Some(cterm_proto::proto::terminal_event::Event::ProcessExited(_)) => {
                                log::info!("Daemon reports process exited");
                                exit_notify_event.notify_one();
                                break;
                            }
                            Some(cterm_proto::proto::terminal_event::Event::SessionPrompt(
                                prompt,
                            )) => {
                                // Show a native modal dialog (off the async worker), then send
                                // the user's reply back to the daemon.
                                let p = prompt.clone();
                                let (accept, secret) = tokio::task::spawn_blocking(move || {
                                    crate::ssh_prompt::show_ssh_prompt(&p)
                                })
                                .await
                                .unwrap_or((false, None));
                                let _ = event_session
                                    .respond_prompt(&prompt.prompt_id, accept, secret)
                                    .await;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to start daemon event stream: {}", e);
            }
        }
    });

    // Read output stream, cancellable by process exit notification
    tokio::select! {
        _ = exit_notify.notified() => {
            log::info!("Process exited, stopping daemon output stream");
        }
        _ = async {
            match session.stream_output().await {
                Ok(mut stream) => {
                    use futures::StreamExt;
                    let mut sync_watchdog =
                        tokio::time::interval(Duration::from_millis(16));
                    sync_watchdog.set_missed_tick_behavior(
                        tokio::time::MissedTickBehavior::Skip,
                    );
                    loop {
                        tokio::select! {
                            result = stream.next() => {
                                let Some(result) = result else { break };
                                match result {
                                    Ok(chunk) => {
                                    let mut term = terminal.lock().unwrap();
                                    let events = term.process_mirror(&chunk.data);
                                    let mut content_changed = false;
                                    for event in events {
                                        match event {
                                            TerminalEvent::TitleChanged(_) => {
                                                post_message(hwnd, WM_APP_TITLE_CHANGED, tab_id);
                                            }
                                            TerminalEvent::Bell => {
                                                post_message(hwnd, WM_APP_BELL, tab_id);
                                            }
                                            TerminalEvent::DesktopNotification(notification) => {
                                                post_desktop_notification(
                                                    hwnd,
                                                    tab_id,
                                                    notification,
                                                );
                                            }
                                            TerminalEvent::ContentChanged => content_changed = true,
                                            _ => {}
                                        }
                                    }
                                    drop(term);
                                    if content_changed {
                                        post_message(hwnd, WM_APP_PTY_DATA, tab_id);
                                    }
                                    }
                                    Err(e) => {
                                        log::error!("Daemon output stream error: {}", e);
                                        break;
                                    }
                                }
                            }
                            _ = sync_watchdog.tick() => {
                                if terminal.lock().unwrap().expire_synchronized_update() {
                                    post_message(hwnd, WM_APP_PTY_DATA, tab_id);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("Failed to start daemon output stream: {}", e);
                }
            }
        } => {}
    }

    post_tab_exit(hwnd, tab_id);
}

/// Post a WM_APP message to the window
fn post_message(hwnd: usize, msg: u32, tab_id: u64) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            msg,
            WPARAM(tab_id as usize),
            LPARAM(0),
        );
    }
}

fn post_desktop_notification(
    hwnd: usize,
    tab_id: u64,
    notification: cterm_core::DesktopNotificationAction,
) {
    let notification = Box::into_raw(Box::new(notification));
    let result = unsafe {
        PostMessageW(
            Some(HWND(hwnd as *mut _)),
            WM_APP_DESKTOP_NOTIFICATION,
            WPARAM(tab_id as usize),
            LPARAM(notification as isize),
        )
    };
    if result.is_err() {
        unsafe {
            drop(Box::from_raw(notification));
        }
    }
}

/// Post a PTY exit message to close the tab
fn post_tab_exit(hwnd: usize, tab_id: u64) {
    post_message(hwnd, WM_APP_PTY_EXIT, tab_id);
}

fn post_daemon_session_ready(hwnd: usize, source_id: u64, ready: DaemonSessionReady) {
    let ready = Box::into_raw(Box::new(ready));
    let posted = unsafe {
        PostMessageW(
            Some(HWND(hwnd as *mut _)),
            WM_APP_DAEMON_SESSION_READY,
            WPARAM(source_id as usize),
            LPARAM(ready as isize),
        )
    };
    if posted.is_err() {
        unsafe { drop(Box::from_raw(ready)) };
    }
}

/// Window procedure
/// Whether Shift is currently held (used to bypass mouse forwarding so local
/// interaction — hyperlink menu, scrollback — keeps working under a tracking app).
fn shift_pressed() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

/// Current Shift/Alt/Ctrl state for encoding into a mouse report.
fn current_mouse_modifiers() -> MouseModifiers {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
    unsafe {
        MouseModifiers {
            shift: GetKeyState(VK_SHIFT.0 as i32) < 0,
            alt: GetKeyState(VK_MENU.0 as i32) < 0,
            ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0,
        }
    }
}

fn mouse_position_changed(
    encoding: MouseEncoding,
    previous: Option<MousePosition>,
    current: MousePosition,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if encoding == MouseEncoding::SgrPixels {
        (previous.pixel_x, previous.pixel_y) != (current.pixel_x, current.pixel_y)
    } else {
        (previous.col, previous.row) != (current.col, current.row)
    }
}

fn signed_point_from_lparam(value: isize) -> (i32, i32) {
    (
        (value & 0xFFFF) as u16 as i16 as i32,
        ((value >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Get window state
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;

    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = unsafe { BeginPaint(hwnd, &mut ps) };
            state.render().ok();
            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }

        WM_SIZE => {
            state.set_window_visibility(if wparam.0 as u32 == SIZE_MINIMIZED {
                cterm_core::WindowVisibility::Hidden
            } else {
                cterm_core::WindowVisibility::Visible
            });
            let width = (lparam.0 & 0xFFFF) as u32;
            let height = ((lparam.0 >> 16) & 0xFFFF) as u32;
            state.on_resize(width, height);
            LRESULT(0)
        }

        WM_SHOWWINDOW => {
            state.set_window_visibility(if wparam.0 != 0 {
                cterm_core::WindowVisibility::Visible
            } else {
                cterm_core::WindowVisibility::Hidden
            });
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xFFFF) as u32;
            state.on_dpi_changed(dpi);
            // Resize window to suggested rect
            let rect = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            }
            .ok();
            LRESULT(0)
        }

        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let vk = (wparam.0 & 0xFFFF) as u16;
            let key_data = lparam.0 as usize;
            let kind = key_event_kind(msg, key_data)
                .expect("matched messages always have a key-event kind");
            if state.on_key_event(vk, kind, key_data & EXTENDED_KEY_BIT != 0) {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }

        WM_CHAR => {
            if !state.suppress_generated_text_message() {
                if let Some(c) = char::from_u32(wparam.0 as u32) {
                    // Only handle printable characters here. Control characters like
                    // Enter (\r), Tab (\t), Backspace (\x08), and Escape (\x1b) are
                    // already handled from their physical key messages.
                    // TranslateMessage generates WM_CHAR for them too, so we must
                    // skip them here to avoid double input.
                    if !c.is_control() {
                        state.on_char(c);
                    }
                }
            }
            LRESULT(0)
        }

        WM_SYSCHAR => {
            if state.suppress_generated_text_message() {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }

        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_mouse_down(x, y);
            LRESULT(0)
        }

        WM_RBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_right_click(x, y);
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_mouse_up(x, y);
            LRESULT(0)
        }

        WM_RBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_mouse_up(x, y);
            LRESULT(0)
        }

        WM_MBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_middle_down(x, y);
            LRESULT(0)
        }

        WM_MBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_mouse_up(x, y);
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.on_mouse_move(x, y);
            LRESULT(0)
        }

        WM_CAPTURECHANGED => {
            state.pane_divider_drag = None;
            LRESULT(0)
        }

        WM_MOUSEWHEEL => {
            // High word of wParam is the signed wheel delta (multiple of 120).
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            // Unlike button/move messages, WM_MOUSEWHEEL lParam is expressed in
            // screen coordinates. Convert before pane hit-testing.
            let (screen_x, screen_y) = signed_point_from_lparam(lparam.0);
            let mut point = POINT {
                x: screen_x,
                y: screen_y,
            };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut point);
            }
            state.on_wheel(delta, point.x as f32, point.y as f32);
            LRESULT(0)
        }

        WM_SETCURSOR => {
            // If cursor is in the client area, let our mouse-move handler control the cursor
            let hit_test = (lparam.0 & 0xFFFF) as u16;
            if hit_test == windows::Win32::UI::WindowsAndMessaging::HTCLIENT as u16 {
                // Return TRUE to prevent DefWindowProc from resetting the cursor
                LRESULT(1)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }

        WM_COMMAND => {
            let cmd = (wparam.0 & 0xFFFF) as u16;
            state.on_menu_command(cmd);
            LRESULT(0)
        }

        WM_APP_PTY_DATA => {
            let tab_id = wparam.0 as u64;
            state.on_pty_data(tab_id);
            LRESULT(0)
        }

        WM_APP_PTY_EXIT => {
            let tab_id = wparam.0 as u64;
            state.on_pty_exit(tab_id);
            LRESULT(0)
        }

        WM_APP_BELL => {
            let tab_id = wparam.0 as u64;
            state.on_bell(tab_id);
            LRESULT(0)
        }

        WM_APP_TITLE_CHANGED => {
            let tab_id = wparam.0 as u64;
            state.on_title_changed(tab_id);
            LRESULT(0)
        }

        WM_APP_DAEMON_SESSION_READY => {
            if lparam.0 != 0 {
                let ready = unsafe { Box::from_raw(lparam.0 as *mut DaemonSessionReady) };
                state.on_daemon_session_ready(wparam.0 as u64, *ready);
            }
            LRESULT(0)
        }

        WM_APP_DESKTOP_NOTIFICATION => {
            if lparam.0 != 0 {
                let notification = unsafe {
                    Box::from_raw(lparam.0 as *mut cterm_core::DesktopNotificationAction)
                };
                crate::desktop_notification::handle(hwnd, &notification);
            }
            LRESULT(0)
        }

        WM_APP_NATIVE_NOTIFICATION => {
            crate::desktop_notification::native_event(hwnd, wparam.0 as u32, lparam.0 as u32);
            LRESULT(0)
        }

        WM_SETFOCUS => {
            // Send focus in event to terminal if DECSET 1004 is enabled
            state.send_focus_event(true);
            state.clear_active_pane_bell();
            state.invalidate();
            LRESULT(0)
        }

        WM_KILLFOCUS => {
            // Send focus out event to terminal if DECSET 1004 is enabled
            state.send_focus_event(false);
            // Windows may not deliver matching key-up messages after focus moves.
            state.suppressed_key_releases.clear();
            state.reported_keys.clear();
            state.enhanced_text_keys.clear();
            LRESULT(0)
        }

        WM_CLOSE => {
            // Check if we should confirm before closing
            if state.should_confirm_close() {
                // Show confirmation dialog
                let confirmed = crate::dialogs::show_confirm(
                    hwnd.0 as *mut _,
                    "Close cterm?",
                    "A process is still running. Are you sure you want to close?",
                );
                if !confirmed {
                    return LRESULT(0); // User cancelled, don't close
                }
            }
            // Proceed with closing
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            // Clean up
            let mut state = unsafe { Box::from_raw(state_ptr) };
            if !state.skip_close_confirm {
                for tab in &mut state.tabs {
                    for pane in tab.panes.values_mut() {
                        pane.destroy();
                    }
                }
            }
            drop(state);
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn pane_at_layout_point(
    layout: &PaneLayout,
    bounds: PaneRect,
    x: u32,
    y: u32,
) -> Option<(PaneId, PaneRect)> {
    layout.layout(bounds).into_iter().find_map(|pane| {
        let right = pane.rect.x.saturating_add(pane.rect.width);
        let bottom = pane.rect.y.saturating_add(pane.rect.height);
        (x >= pane.rect.x && x < right && y >= pane.rect.y && y < bottom)
            .then_some((pane.id, pane.rect))
    })
}

fn upgrade_pane_records(
    tab: &cterm_app::upgrade::TabUpgradeState,
) -> Result<(PaneLayout, Vec<cterm_app::upgrade::PaneUpgradeState>), String> {
    match (&tab.pane_layout, tab.panes.is_empty()) {
        (Some(layout), false) if layout.pane_ids().len() == tab.panes.len() => {
            Ok((layout.clone(), tab.panes.clone()))
        }
        (Some(layout), false) => Err(format!(
            "layout has {} panes but state has {} records",
            layout.pane_ids().len(),
            tab.panes.len()
        )),
        (_, true) => {
            let mut pane = cterm_app::upgrade::PaneUpgradeState::new(tab.session_id.clone());
            pane.title = tab.title.clone();
            pane.title_locked = tab.custom_title.is_some();
            pane.template_name = tab.template_name.clone();
            pane.cwd = tab.cwd.clone();
            pane.keep_open = tab.keep_open;
            Ok((PaneLayout::new(), vec![pane]))
        }
        (None, false) => Err("pane records have no layout".to_string()),
    }
}

fn upgrade_window_is_handoff_ready(window: &cterm_app::upgrade::WindowUpgradeState) -> bool {
    !window.tabs.is_empty()
        && window.tabs.iter().all(|tab| {
            !tab.panes.is_empty()
                && tab
                    .panes
                    .iter()
                    .all(|pane| pane.session_id.as_ref().is_some_and(|id| !id.is_empty()))
        })
}

fn divider_at_tree_point(tree: &PaneTree, bounds: PaneRect, x: u32, y: u32) -> Option<PaneDivider> {
    fn visit(
        tree: &PaneTree,
        rect: PaneRect,
        x: u32,
        y: u32,
        path: &mut Vec<PaneBranch>,
    ) -> Option<PaneDivider> {
        let PaneTree::Split {
            direction,
            first_ratio,
            first,
            second,
        } = tree
        else {
            return None;
        };
        let (first_rect, second_rect, divider_coordinate) =
            split_pane_rect(rect, *direction, *first_ratio);

        path.push(PaneBranch::First);
        let first_hit = visit(first, first_rect, x, y, path);
        path.pop();
        if first_hit.is_some() {
            return first_hit;
        }
        path.push(PaneBranch::Second);
        let second_hit = visit(second, second_rect, x, y, path);
        path.pop();
        if second_hit.is_some() {
            return second_hit;
        }

        const HIT_RADIUS: u32 = 3;
        let inside = x >= rect.x
            && x < rect.x.saturating_add(rect.width)
            && y >= rect.y
            && y < rect.y.saturating_add(rect.height);
        let near = match direction {
            SplitDirection::Horizontal => x.abs_diff(divider_coordinate) <= HIT_RADIUS,
            SplitDirection::Vertical => y.abs_diff(divider_coordinate) <= HIT_RADIUS,
        };
        (inside && near).then(|| PaneDivider {
            path: path.clone(),
            direction: *direction,
            split_rect: rect,
        })
    }

    visit(tree, bounds, x, y, &mut Vec::new())
}

fn split_pane_rect(
    rect: PaneRect,
    direction: SplitDirection,
    ratio: SplitRatio,
) -> (PaneRect, PaneRect, u32) {
    let first_extent = |total: u32| match total {
        0 => 0,
        1 => 1,
        _ => ((u64::from(total) * u64::from(ratio.basis_points()) / 10_000) as u32)
            .clamp(1, total - 1),
    };
    match direction {
        SplitDirection::Horizontal => {
            let first_width = first_extent(rect.width);
            let divider = rect.x.saturating_add(first_width);
            (
                PaneRect::new(rect.x, rect.y, first_width, rect.height),
                PaneRect::new(
                    divider,
                    rect.y,
                    rect.width.saturating_sub(first_width),
                    rect.height,
                ),
                divider,
            )
        }
        SplitDirection::Vertical => {
            let first_height = first_extent(rect.height);
            let divider = rect.y.saturating_add(first_height);
            (
                PaneRect::new(rect.x, rect.y, rect.width, first_height),
                PaneRect::new(
                    rect.x,
                    divider,
                    rect.width,
                    rect.height.saturating_sub(first_height),
                ),
                divider,
            )
        }
    }
}

fn ratio_at_coordinate(coordinate: f32, origin: f32, extent: u32) -> u16 {
    if extent <= 1 {
        return SplitRatio::HALF.basis_points();
    }
    (((coordinate - origin) / extent as f32 * 10_000.0).round() as i32).clamp(
        i32::from(SplitRatio::MIN.basis_points()),
        i32::from(SplitRatio::MAX.basis_points()),
    ) as u16
}

/// Parse a hex color string (e.g., "#e74c3c") to Rgb
fn parse_hex_color(hex: &str) -> Option<Rgb> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;

    Some(Rgb::new(r, g, b))
}

fn terminal_palette(theme: &Theme, background: Option<&str>) -> ColorPalette {
    let mut palette = theme.colors.clone();
    palette.cursor = theme.cursor.color;
    if let Some(background) = background.and_then(Rgb::from_hex) {
        palette.background = background;
    }
    palette
}

fn template_session_options(
    template: &cterm_app::config::StickyTabConfig,
    config: &Config,
    cols: u32,
    rows: u32,
) -> cterm_client::CreateSessionOpts {
    let (configured_shell, configured_args) = template.get_command_args();
    let shell = configured_shell.or_else(|| config.general.default_shell.clone());
    let args = if template.docker.is_none() && template.command.is_none() {
        config.general.shell_args.clone()
    } else {
        configured_args
    };
    cterm_client::CreateSessionOpts {
        cols,
        rows,
        shell,
        args,
        cwd: template
            .working_directory
            .as_ref()
            .or(config.general.working_directory.as_ref())
            .map(|path| path.to_string_lossy().into_owned()),
        // Preserve the existing Win32 template environment order. The daemon
        // collects this into a map, so configured defaults replace duplicate
        // template entries exactly as the former local PTY path did.
        env: template
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .chain(
                config
                    .general
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            )
            .collect(),
        term: config.general.term.clone(),
        ssh: template.ssh.as_ref().map(|ssh| ssh.to_ssh_params()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_message_kind_distinguishes_press_repeat_and_release() {
        assert_eq!(key_event_kind(WM_KEYDOWN, 0), Some(KeyEventKind::Press));
        assert_eq!(
            key_event_kind(WM_SYSKEYDOWN, PREVIOUS_KEY_STATE_BIT),
            Some(KeyEventKind::Repeat)
        );
        assert_eq!(key_event_kind(WM_KEYUP, 0), Some(KeyEventKind::Release));
        assert_eq!(
            key_event_kind(WM_SYSKEYUP, PREVIOUS_KEY_STATE_BIT),
            Some(KeyEventKind::Release)
        );
        assert_eq!(key_event_kind(WM_CHAR, 0), None);
    }

    #[test]
    fn physical_mapping_keeps_layout_text_on_wm_char() {
        assert_eq!(
            mapped_terminal_key(winuser::VK_UP as u16, Modifiers::empty(), false, false),
            Some(Key::Up)
        );
        assert_eq!(
            mapped_terminal_key(winuser::VK_F12 as u16, Modifiers::empty(), false, false),
            Some(Key::F(12))
        );
        assert_eq!(
            mapped_terminal_key(0x41, Modifiers::empty(), false, false),
            None
        );
        assert_eq!(
            mapped_terminal_key(winuser::VK_OEM_1 as u16, Modifiers::CTRL, false, false,),
            None
        );
    }

    #[test]
    fn physical_mapping_preserves_numeric_keypad_identity() {
        assert_eq!(
            mapped_terminal_key(winuser::VK_NUMPAD7 as u16, Modifiers::empty(), false, false,),
            Some(Key::NumpadDigit(7))
        );
        assert_eq!(
            mapped_terminal_key(winuser::VK_ADD as u16, Modifiers::empty(), false, false,),
            Some(Key::NumpadAdd)
        );
        assert_eq!(
            mapped_terminal_key(winuser::VK_RETURN as u16, Modifiers::empty(), false, true,),
            Some(Key::NumpadEnter)
        );
    }

    #[test]
    fn legacy_ctrl_letters_and_enhanced_ascii_are_mapped_exactly() {
        assert_eq!(
            mapped_terminal_key(0x41, Modifiers::CTRL, false, false),
            Some(Key::Char('a'))
        );
        assert_eq!(
            mapped_terminal_key(0x5a, Modifiers::CTRL | Modifiers::SHIFT, false, false),
            Some(Key::Char('z'))
        );
        assert_eq!(
            mapped_terminal_key(0x41, Modifiers::CTRL | Modifiers::ALT, false, false),
            None
        );
        assert_eq!(
            mapped_terminal_key(
                winuser::VK_OEM_1 as u16,
                Modifiers::CTRL | Modifiers::SHIFT,
                true,
                false,
            ),
            Some(Key::Char(';'))
        );
        assert_eq!(
            mapped_terminal_key(0x32, Modifiers::ALT, true, false),
            Some(Key::Char('2'))
        );
    }

    #[test]
    fn pane_hit_testing_tracks_split_and_zoom_geometry() {
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout
            .split_active(SplitRequest {
                direction: SplitDirection::Horizontal,
                ..SplitRequest::default()
            })
            .unwrap();
        let bounds = PaneRect::new(0, 0, 100, 40);

        assert_eq!(
            pane_at_layout_point(&layout, bounds, 10, 10).unwrap().0,
            first
        );
        assert_eq!(
            pane_at_layout_point(&layout, bounds, 90, 10).unwrap().0,
            second
        );

        layout.zoom(first).unwrap();
        assert_eq!(
            pane_at_layout_point(&layout, bounds, 90, 10).unwrap().0,
            first
        );
    }

    #[test]
    fn divider_hit_testing_returns_stable_nested_split_paths() {
        let mut layout = PaneLayout::new();
        layout
            .split_active(SplitRequest {
                direction: SplitDirection::Horizontal,
                ..SplitRequest::default()
            })
            .unwrap();
        layout
            .split_active(SplitRequest {
                direction: SplitDirection::Vertical,
                ..SplitRequest::default()
            })
            .unwrap();
        let bounds = PaneRect::new(0, 0, 100, 40);

        let root = divider_at_tree_point(&layout.tree(), bounds, 50, 5).unwrap();
        assert_eq!(root.direction, SplitDirection::Horizontal);
        assert!(root.path.is_empty());

        let nested = divider_at_tree_point(&layout.tree(), bounds, 75, 20).unwrap();
        assert_eq!(nested.direction, SplitDirection::Vertical);
        assert_eq!(nested.path, vec![PaneBranch::Second]);
        assert_eq!(nested.split_rect, PaneRect::new(50, 0, 50, 40));
    }

    #[test]
    fn divider_drag_ratios_are_bounded_and_deterministic() {
        assert_eq!(ratio_at_coordinate(25.0, 0.0, 100), 2_500);
        assert_eq!(ratio_at_coordinate(-50.0, 0.0, 100), 500);
        assert_eq!(ratio_at_coordinate(150.0, 0.0, 100), 9_500);
        assert_eq!(ratio_at_coordinate(10.0, 10.0, 1), 5_000);
    }

    #[test]
    fn wheel_lparam_preserves_signed_screen_coordinates() {
        let x = -120_i16;
        let y = 340_i16;
        let packed = (u16::from_ne_bytes(x.to_ne_bytes()) as isize)
            | ((u16::from_ne_bytes(y.to_ne_bytes()) as isize) << 16);
        assert_eq!(signed_point_from_lparam(packed), (-120, 340));
    }

    #[test]
    fn upgrade_records_follow_layout_preorder_and_legacy_falls_back() {
        let mut layout = PaneLayout::new();
        layout
            .split_active(SplitRequest {
                direction: SplitDirection::Horizontal,
                ..SplitRequest::default()
            })
            .unwrap();
        let mut tab = cterm_app::upgrade::TabUpgradeState::new(9);
        tab.pane_layout = Some(layout.clone());
        tab.panes = vec![
            cterm_app::upgrade::PaneUpgradeState::new(Some("left".to_string())),
            cterm_app::upgrade::PaneUpgradeState::new(Some("right".to_string())),
        ];
        let (restored_layout, records) = upgrade_pane_records(&tab).unwrap();
        assert_eq!(restored_layout.pane_ids(), layout.pane_ids());
        assert_eq!(records[0].session_id.as_deref(), Some("left"));
        assert_eq!(records[1].session_id.as_deref(), Some("right"));

        let mut legacy = cterm_app::upgrade::TabUpgradeState::new(10);
        legacy.title = "legacy".to_string();
        legacy.session_id = Some("only".to_string());
        let (legacy_layout, records) = upgrade_pane_records(&legacy).unwrap();
        assert_eq!(legacy_layout.len(), 1);
        assert_eq!(records[0].session_id.as_deref(), Some("only"));
        assert_eq!(records[0].title, "legacy");
    }

    #[test]
    fn seamless_handoff_waits_for_every_daemon_session_id() {
        let mut window = cterm_app::upgrade::WindowUpgradeState::new();
        let mut tab = cterm_app::upgrade::TabUpgradeState::new(1);
        tab.pane_layout = Some(PaneLayout::new());
        tab.panes = vec![cterm_app::upgrade::PaneUpgradeState::new(None)];
        window.tabs.push(tab);
        assert!(!upgrade_window_is_handoff_ready(&window));

        window.tabs[0].panes[0].session_id = Some("daemon-session".to_string());
        assert!(upgrade_window_is_handoff_ready(&window));

        window.tabs[0]
            .panes
            .push(cterm_app::upgrade::PaneUpgradeState::new(None));
        assert!(!upgrade_window_is_handoff_ready(&window));
    }

    #[test]
    fn template_daemon_options_preserve_process_contract() {
        let mut config = Config::default();
        config.general.default_shell = Some("configured-shell.exe".to_string());
        config.general.shell_args = vec!["--configured".to_string()];
        config.general.working_directory = Some(std::path::PathBuf::from(r"C:\configured"));
        config
            .general
            .env
            .insert("FROM_CONFIG".to_string(), "yes".to_string());
        config.general.term = Some("xterm-direct".to_string());

        let template = cterm_app::config::StickyTabConfig {
            command: Some("program.exe".to_string()),
            args: vec!["two words".to_string(), "--literal".to_string()],
            working_directory: Some(std::path::PathBuf::from(r"C:\template")),
            env: std::collections::HashMap::from([(
                "FROM_TEMPLATE".to_string(),
                "yes".to_string(),
            )]),
            ..Default::default()
        };

        let options = template_session_options(&template, &config, 132, 43);
        assert_eq!(options.cols, 132);
        assert_eq!(options.rows, 43);
        assert_eq!(options.shell.as_deref(), Some("program.exe"));
        assert_eq!(options.args, ["two words", "--literal"]);
        assert_eq!(options.cwd.as_deref(), Some(r"C:\template"));
        assert!(options
            .env
            .contains(&("FROM_CONFIG".to_string(), "yes".to_string())));
        assert!(options
            .env
            .contains(&("FROM_TEMPLATE".to_string(), "yes".to_string())));
        assert_eq!(options.term.as_deref(), Some("xterm-direct"));
    }

    #[test]
    fn default_template_inherits_configured_shell_argv_and_cwd() {
        let mut config = Config::default();
        config.general.default_shell = Some("pwsh.exe".to_string());
        config.general.shell_args = vec!["-NoLogo".to_string()];
        config.general.working_directory = Some(std::path::PathBuf::from(r"C:\work"));

        let options = template_session_options(
            &cterm_app::config::StickyTabConfig::default(),
            &config,
            80,
            24,
        );
        assert_eq!(options.shell.as_deref(), Some("pwsh.exe"));
        assert_eq!(options.args, ["-NoLogo"]);
        assert_eq!(options.cwd.as_deref(), Some(r"C:\work"));
    }

    #[test]
    fn daemon_pane_launch_context_round_trips_for_post_upgrade_splits() {
        let options = cterm_client::CreateSessionOpts {
            shell: Some("pwsh.exe".to_string()),
            args: vec!["-NoLogo".to_string(), "-NoProfile".to_string()],
            env: vec![("CTERM_TEST".to_string(), "one".to_string())],
            term: Some("xterm-direct".to_string()),
            ssh: Some(cterm_client::SshParams {
                host: "server.example".to_string(),
                port: 2222,
                ..Default::default()
            }),
            ..Default::default()
        };
        let original = DaemonPaneContext::from_options(&options, None);
        let launch = original.launch_context();
        let mut restored = DaemonPaneContext::local_default();
        restored.apply_launch_context(&launch);

        assert_eq!(restored.shell, original.shell);
        assert_eq!(restored.args, original.args);
        assert_eq!(restored.env, original.env);
        assert_eq!(restored.term, original.term);
        assert_eq!(
            restored.ssh.as_ref().map(|ssh| ssh.host.as_str()),
            Some("server.example")
        );
        assert_eq!(restored.ssh.as_ref().map(|ssh| ssh.port), Some(2222));
    }
}
