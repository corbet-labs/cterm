//! Upgrade state types for seamless process upgrade
//!
//! These types capture the window/tab layout needed to reconstruct terminal
//! windows after a seamless upgrade. Terminal session state lives in the
//! ctermd daemon and is referenced by session ID.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use cterm_client::{CreateSessionOpts, SshParams};
use cterm_proto::proto::PortForward;
use cterm_ui::PaneLayout;

/// Complete upgrade state for all windows
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeState {
    /// Version of the serialization format
    pub format_version: u32,
    /// All windows to restore
    pub windows: Vec<WindowUpgradeState>,
}

impl UpgradeState {
    /// Current format version
    /// Increment this when making incompatible changes to serialized types
    pub const FORMAT_VERSION: u32 = 5;

    /// Create a new upgrade state
    pub fn new() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            windows: Vec::new(),
        }
    }
}

impl Default for UpgradeState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for a single window
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowUpgradeState {
    /// Window X position
    pub x: i32,
    /// Window Y position
    pub y: i32,
    /// Window width
    pub width: i32,
    /// Window height
    pub height: i32,
    /// Whether the window is maximized
    pub maximized: bool,
    /// Whether the window is fullscreen
    pub fullscreen: bool,
    /// All tabs in this window
    pub tabs: Vec<TabUpgradeState>,
    /// Index of the currently active tab
    pub active_tab: usize,
}

impl WindowUpgradeState {
    /// Create a new window upgrade state
    pub fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            maximized: false,
            fullscreen: false,
            tabs: Vec::new(),
            active_tab: 0,
        }
    }
}

impl Default for WindowUpgradeState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for a single tab
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabUpgradeState {
    /// Unique tab ID
    pub id: u64,
    /// Tab title
    pub title: String,
    /// Custom title set by user (locks out OSC title updates when Some)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    /// Tab color (if sticky tab)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Template name (for sticky/unique tabs)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,
    /// Daemon session ID for reconnecting to the running session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Working directory of the shell
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Keep the tab open after the process exits
    #[serde(default)]
    pub keep_open: bool,
    /// Complete split topology for this tab. The pane records below follow the
    /// layout's deterministic preorder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_layout: Option<PaneLayout>,
    /// Every terminal session owned by this tab, in `pane_layout.pane_ids()`
    /// order. Empty means the singular fields above describe a legacy
    /// one-pane tab.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneUpgradeState>,
}

impl TabUpgradeState {
    /// Create a new tab upgrade state
    pub fn new(id: u64) -> Self {
        Self {
            id,
            title: String::new(),
            custom_title: None,
            color: None,
            template_name: None,
            session_id: None,
            cwd: None,
            keep_open: false,
            pane_layout: None,
            panes: Vec::new(),
        }
    }
}

/// State for one terminal pane within a tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneUpgradeState {
    /// Daemon session ID used to reattach to the running terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Last display title reported by this pane.
    #[serde(default)]
    pub title: String,
    /// Whether the pane title is locked against OSC title updates.
    #[serde(default)]
    pub title_locked: bool,
    /// Template identity when the pane originated from a sticky template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,
    /// Last known foreground working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Keep the pane visible after its child exits.
    #[serde(default)]
    pub keep_open: bool,
    /// Last concrete daemon endpoint. Synthetic SSH sockets can expire during
    /// relaunch, so restore code prefers `remote_name` when it is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_socket: Option<PathBuf>,
    /// Configured remote-manager key used to recreate an SSH endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_name: Option<String>,
    /// Exact process/SSH launch context used when a new sibling pane must be
    /// created after the UI process has been replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_context: Option<PaneLaunchContext>,
}

impl PaneUpgradeState {
    pub fn new(session_id: Option<String>) -> Self {
        Self {
            session_id,
            title: String::new(),
            title_locked: false,
            template_name: None,
            cwd: None,
            keep_open: false,
            daemon_socket: None,
            remote_name: None,
            launch_context: None,
        }
    }
}

/// Serializable process context for creating sibling panes after a seamless
/// UI upgrade. Geometry, palette, and frontend state are intentionally omitted:
/// they are supplied from the receiving window at creation time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshLaunchState>,
}

impl PaneLaunchContext {
    /// Capture the stable launch fields from a daemon session request.
    pub fn capture(options: &CreateSessionOpts) -> Self {
        Self {
            shell: options.shell.clone(),
            args: options.args.clone(),
            env: options.env.clone(),
            term: options.term.clone(),
            ssh: options.ssh.as_ref().map(SshLaunchState::from),
        }
    }

    /// Apply the captured launch fields to a fresh session request while
    /// leaving window-dependent geometry and rendering state untouched.
    pub fn apply_to(&self, options: &mut CreateSessionOpts) {
        options.shell.clone_from(&self.shell);
        options.args.clone_from(&self.args);
        options.env.clone_from(&self.env);
        options.term.clone_from(&self.term);
        options.ssh = self.ssh.as_ref().map(SshParams::from);
    }
}

/// Serializable mirror of the protobuf-native SSH session parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshLaunchState {
    pub host: String,
    #[serde(default)]
    pub port: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_host: Option<String>,
    #[serde(default)]
    pub agent_forward: bool,
    #[serde(default)]
    pub x11_forward: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_forwards: Vec<PortForwardLaunchState>,
}

impl From<&SshParams> for SshLaunchState {
    fn from(ssh: &SshParams) -> Self {
        Self {
            host: ssh.host.clone(),
            port: ssh.port,
            username: ssh.username.clone(),
            identity_files: ssh.identity_files.clone(),
            remote_command: ssh.remote_command.clone(),
            jump_host: ssh.jump_host.clone(),
            agent_forward: ssh.agent_forward,
            x11_forward: ssh.x11_forward,
            local_forwards: ssh
                .local_forwards
                .iter()
                .map(PortForwardLaunchState::from)
                .collect(),
        }
    }
}

impl From<&SshLaunchState> for SshParams {
    fn from(ssh: &SshLaunchState) -> Self {
        Self {
            host: ssh.host.clone(),
            port: ssh.port,
            username: ssh.username.clone(),
            identity_files: ssh.identity_files.clone(),
            remote_command: ssh.remote_command.clone(),
            jump_host: ssh.jump_host.clone(),
            agent_forward: ssh.agent_forward,
            x11_forward: ssh.x11_forward,
            local_forwards: ssh.local_forwards.iter().map(PortForward::from).collect(),
        }
    }
}

/// Serializable `-L` entry used by [`SshLaunchState`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForwardLaunchState {
    #[serde(default)]
    pub local_port: u32,
    pub remote_host: String,
    #[serde(default)]
    pub remote_port: u32,
}

impl From<&PortForward> for PortForwardLaunchState {
    fn from(forward: &PortForward) -> Self {
        Self {
            local_port: forward.local_port,
            remote_host: forward.remote_host.clone(),
            remote_port: forward.remote_port,
        }
    }
}

impl From<&PortForwardLaunchState> for PortForward {
    fn from(forward: &PortForwardLaunchState) -> Self {
        Self {
            local_port: forward.local_port,
            remote_host: forward.remote_host.clone(),
            remote_port: forward.remote_port,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_ui::{SplitDirection, SplitRequest};

    #[test]
    fn test_upgrade_state_serialization() {
        let state = UpgradeState::new();

        let json = serde_json::to_vec(&state).expect("Failed to serialize");
        let restored: UpgradeState = serde_json::from_slice(&json).expect("Failed to deserialize");

        assert_eq!(restored.format_version, UpgradeState::FORMAT_VERSION);
        assert!(restored.windows.is_empty());
    }

    #[test]
    fn test_window_state_serialization() {
        let mut state = UpgradeState::new();

        let mut window = WindowUpgradeState::new();
        window.x = 100;
        window.y = 200;
        window.width = 1024;
        window.height = 768;
        window.maximized = true;

        state.windows.push(window);

        let json = serde_json::to_vec(&state).expect("Failed to serialize");
        let restored: UpgradeState = serde_json::from_slice(&json).expect("Failed to deserialize");

        assert_eq!(restored.windows.len(), 1);
        assert_eq!(restored.windows[0].x, 100);
        assert!(restored.windows[0].maximized);
    }

    #[test]
    fn pane_launch_context_captures_and_reapplies_session_options() {
        let original = CreateSessionOpts {
            shell: Some("nu".to_string()),
            args: vec!["--login".to_string()],
            cwd: Some("/work/one".to_string()),
            env: vec![("RUST_LOG".to_string(), "debug".to_string())],
            term: Some("xterm-256color".to_string()),
            ssh: Some(SshParams {
                host: "host.example".to_string(),
                port: 2222,
                username: Some("builder".to_string()),
                local_forwards: vec![PortForward {
                    local_port: 3000,
                    remote_host: "127.0.0.1".to_string(),
                    remote_port: 3001,
                }],
                ..SshParams::default()
            }),
            ..CreateSessionOpts::default()
        };

        let context = PaneLaunchContext::capture(&original);
        let mut restored = CreateSessionOpts {
            cwd: Some("/work/two".to_string()),
            cols: 120,
            rows: 40,
            ..CreateSessionOpts::default()
        };
        context.apply_to(&mut restored);

        assert_eq!(restored.shell, original.shell);
        assert_eq!(restored.args, original.args);
        assert_eq!(restored.env, original.env);
        assert_eq!(restored.term, original.term);
        assert_eq!(restored.ssh, original.ssh);
        assert_eq!(restored.cwd.as_deref(), Some("/work/two"));
        assert_eq!((restored.cols, restored.rows), (120, 40));
    }

    #[test]
    fn test_tab_state_serialization() {
        let mut tab = TabUpgradeState::new(42);
        tab.title = "My Tab".to_string();
        tab.session_id = Some("sess-abc123".to_string());
        tab.cwd = Some("/home/user".to_string());
        tab.keep_open = true;

        let json = serde_json::to_vec(&tab).expect("Failed to serialize");
        let restored: TabUpgradeState =
            serde_json::from_slice(&json).expect("Failed to deserialize");

        assert_eq!(restored.id, 42);
        assert_eq!(restored.title, "My Tab");
        assert_eq!(restored.session_id.as_deref(), Some("sess-abc123"));
        assert_eq!(restored.cwd.as_deref(), Some("/home/user"));
        assert!(restored.keep_open);
    }

    #[test]
    fn test_tab_state_optional_fields_omitted() {
        let tab = TabUpgradeState::new(1);

        let json = serde_json::to_string(&tab).expect("Failed to serialize");

        // Optional None fields should be omitted from JSON
        assert!(!json.contains("custom_title"));
        assert!(!json.contains("color"));
        assert!(!json.contains("template_name"));
        assert!(!json.contains("session_id"));
        assert!(!json.contains("cwd"));
        assert!(!json.contains("pane_layout"));
        assert!(!json.contains("panes"));
    }

    #[test]
    fn version_four_tab_deserializes_as_one_pane_summary() {
        let json = r#"{
            "format_version": 4,
            "windows": [{
                "x": 0,
                "y": 0,
                "width": 800,
                "height": 600,
                "maximized": false,
                "fullscreen": false,
                "tabs": [{
                    "id": 7,
                    "title": "legacy",
                    "session_id": "sess-legacy",
                    "cwd": "/tmp",
                    "keep_open": true
                }],
                "active_tab": 0
            }]
        }"#;

        let restored: UpgradeState = serde_json::from_str(json).expect("deserialize v4 state");
        let tab = &restored.windows[0].tabs[0];
        assert_eq!(restored.format_version, 4);
        assert_eq!(tab.session_id.as_deref(), Some("sess-legacy"));
        assert_eq!(tab.cwd.as_deref(), Some("/tmp"));
        assert!(tab.keep_open);
        assert!(tab.pane_layout.is_none());
        assert!(tab.panes.is_empty());
    }

    #[test]
    fn test_full_roundtrip() {
        let mut state = UpgradeState::new();

        let mut window = WindowUpgradeState::new();
        window.x = 50;
        window.y = 100;
        window.width = 1920;
        window.height = 1080;
        window.fullscreen = true;
        window.active_tab = 1;

        let mut tab0 = TabUpgradeState::new(1);
        tab0.title = "bash".to_string();
        tab0.session_id = Some("sess-001".to_string());

        let mut tab1 = TabUpgradeState::new(2);
        tab1.title = "vim".to_string();
        tab1.custom_title = Some("Editor".to_string());
        tab1.session_id = Some("sess-002".to_string());
        tab1.color = Some("#ff0000".to_string());
        tab1.template_name = Some("dev".to_string());
        tab1.keep_open = true;

        let mut pane_layout = PaneLayout::new();
        pane_layout
            .split_active(SplitRequest {
                direction: SplitDirection::Vertical,
                ..SplitRequest::default()
            })
            .expect("split pane");
        let pane_ids = pane_layout.pane_ids();
        tab1.pane_layout = Some(pane_layout.clone());

        let mut editor_pane = PaneUpgradeState::new(Some("sess-002".to_string()));
        editor_pane.title = "Editor".to_string();
        editor_pane.title_locked = true;
        editor_pane.template_name = Some("dev".to_string());
        editor_pane.cwd = Some("/home/user/project".to_string());
        editor_pane.keep_open = true;

        let mut shell_pane = PaneUpgradeState::new(Some("sess-003".to_string()));
        shell_pane.title = "Tests".to_string();
        shell_pane.cwd = Some("/home/user/project".to_string());
        shell_pane.daemon_socket = Some(PathBuf::from("/tmp/ctermd-test.sock"));
        shell_pane.remote_name = Some("build-box".to_string());
        shell_pane.launch_context = Some(PaneLaunchContext {
            shell: Some("/bin/zsh".to_string()),
            args: vec!["-l".to_string()],
            env: vec![("RUST_BACKTRACE".to_string(), "1".to_string())],
            term: Some("xterm-256color".to_string()),
            ssh: Some(SshLaunchState {
                host: "dev.example.test".to_string(),
                port: 2222,
                username: Some("dev".to_string()),
                local_forwards: vec![PortForwardLaunchState {
                    local_port: 8080,
                    remote_host: "127.0.0.1".to_string(),
                    remote_port: 80,
                }],
                ..SshLaunchState::default()
            }),
        });
        tab1.panes = vec![editor_pane.clone(), shell_pane.clone()];

        window.tabs.push(tab0);
        window.tabs.push(tab1);
        state.windows.push(window);

        let json = serde_json::to_vec_pretty(&state).expect("Failed to serialize");
        let restored: UpgradeState = serde_json::from_slice(&json).expect("Failed to deserialize");

        assert_eq!(restored.format_version, UpgradeState::FORMAT_VERSION);
        assert_eq!(restored.windows.len(), 1);

        let w = &restored.windows[0];
        assert_eq!(w.width, 1920);
        assert!(w.fullscreen);
        assert_eq!(w.active_tab, 1);
        assert_eq!(w.tabs.len(), 2);

        assert_eq!(w.tabs[0].title, "bash");
        assert_eq!(w.tabs[0].session_id.as_deref(), Some("sess-001"));
        assert!(!w.tabs[0].keep_open);

        assert_eq!(w.tabs[1].title, "vim");
        assert_eq!(w.tabs[1].custom_title.as_deref(), Some("Editor"));
        assert_eq!(w.tabs[1].color.as_deref(), Some("#ff0000"));
        assert!(w.tabs[1].keep_open);
        assert_eq!(w.tabs[1].pane_layout.as_ref(), Some(&pane_layout));
        assert_eq!(
            w.tabs[1]
                .pane_layout
                .as_ref()
                .expect("pane layout")
                .pane_ids(),
            pane_ids
        );
        assert_eq!(w.tabs[1].panes, vec![editor_pane, shell_pane]);
    }
}
