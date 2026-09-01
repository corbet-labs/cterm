//! cterm-app: Application logic for cterm
//!
//! This crate contains the application logic that is independent of the UI,
//! including configuration management, session handling, sticky tabs,
//! seamless upgrade functionality, and daemon session management.

pub mod cli;
pub mod config;
pub mod daemon_reconnect;
pub mod daemon_session;
pub mod docker;
pub mod file_drop;
pub mod file_transfer;
pub mod git_sync;
pub mod kitty_dnd;
pub mod kitty_file_transfer;
pub mod kitty_file_transfer_fs;
mod kitty_file_transfer_receive;
mod kitty_rsync;
pub mod log_capture;
pub mod plugin_broker;
pub mod plugin_runtime;
pub mod plugins;
pub mod quick_open;
pub mod session;
pub mod shortcuts;
pub mod ssh_history;
pub mod template_launch;
pub mod upgrade;

pub use config::{
    background_sync, load_config, load_sticky_tabs, load_tool_shortcuts, save_config,
    save_config_with_sync, save_sticky_tabs, save_tool_shortcuts, set_config_dir_override, Config,
    ToolShortcutEntry,
};
pub use daemon_reconnect::{
    check_daemon_sessions, reconnect_all_sessions, ReconnectCheck, ReconnectedSession,
};
pub use daemon_session::{apply_screen_snapshot, DaemonTab, DaemonTabError};
pub use git_sync::{
    clone_repo, get_directory_remote_url, get_remote_url, get_sync_status, init_with_remote,
    is_git_repo, prepare_working_directory, pull_with_conflict_resolution, GitError, InitResult,
    PullResult, SyncStatus,
};
pub use kitty_file_transfer::{
    AuthorizedTtyTransferCommand, TtyTransferAction, TtyTransferApprovalRequest,
    TtyTransferDirection, TtyTransferManager,
};
pub use kitty_file_transfer_fs::{
    TtyTransferFilesystem, TtyTransferFilesystemConfigError, TtyTransferLimits,
    TtyTransferSendFilesystem,
};
pub use plugin_broker::{PluginBroker, PluginBrokerError, PluginBrokerOutput, PluginBrokerTimeout};
pub use plugin_runtime::{
    PluginApprovalPrompt, PluginAuthorization, PluginExecution, PluginInvocation, PluginRuntime,
    PluginRuntimeError,
};
pub use plugins::{
    PluginCatalog, PluginCatalogError, PluginCommandDescriptor, PluginDataPaths,
    PluginDiscoveryError, PluginDiscoveryFailure, PluginGrantFile, PluginStorageError,
};
pub use session::{Session, TabState, WindowState};
pub use shortcuts::ShortcutManager;
pub use template_launch::{
    TemplateAppearance, TemplateDaemonTarget, TemplateInstancePolicy, TemplateLaunchPlan,
    TemplateLaunchPlanError, TemplateLaunchTarget, TemplateNamedRemote, TemplateWorkspace,
    TemplateWorkspaceLocation, TemplateWorkspaceSource,
};
pub use upgrade::{execute_upgrade, receive_upgrade, UpgradeError};
pub use upgrade::{UpdateError, UpdateInfo, Updater, UpgradeState};

pub use config::resolve_theme;
pub use quick_open::{template_type_indicator, QuickOpenMatcher, TemplateMatch};
