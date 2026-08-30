//! Upgrade receiver - handles receiving state during seamless upgrade
//!
//! When cterm is started with --upgrade-state, it reads the saved state from
//! a temp file, reconnects to running daemon sessions, and reconstructs windows.

use std::path::Path;

use cterm_app::config::load_config;
use cterm_app::upgrade::{PaneUpgradeState, TabUpgradeState, UpgradeState};
use cterm_ui::PaneLayout;
use gtk4::glib;
use gtk4::prelude::*;

/// Run the upgrade receiver
///
/// Reads upgrade state from the given file path, reconnects to daemon
/// sessions, and reconstructs the GTK application with restored windows.
pub fn run_receiver(state_path: &str) -> glib::ExitCode {
    #[cfg(feature = "adwaita")]
    let _ = libadwaita::init();

    match receive_and_reconstruct(state_path) {
        Ok(()) => glib::ExitCode::SUCCESS,
        Err(e) => {
            log::error!("Upgrade receiver failed: {}", e);
            glib::ExitCode::FAILURE
        }
    }
}

fn receive_and_reconstruct(state_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let state = cterm_app::upgrade::receive_upgrade(Path::new(state_path))?;

    log::info!(
        "Upgrade state: format_version={}, {} window(s)",
        state.format_version,
        state.windows.len()
    );

    // Store state for use during GTK activate
    UPGRADE_STATE.with(|s| {
        *s.borrow_mut() = Some(state);
    });

    // Start GTK and reconstruct windows
    let app = gtk4::Application::builder()
        .application_id("com.cterm.terminal")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(|app| {
        UPGRADE_STATE.with(|s| {
            if let Some(state) = s.borrow_mut().take() {
                reconstruct_windows(app, state);
            }
        });
    });

    app.run_with_args(&[] as &[&str]);
    Ok(())
}

thread_local! {
    static UPGRADE_STATE: std::cell::RefCell<Option<UpgradeState>> =
        const { std::cell::RefCell::new(None) };
}

fn pane_restore_state(tab: &TabUpgradeState) -> Option<(PaneLayout, Vec<PaneUpgradeState>)> {
    if let Some(layout) = tab.pane_layout.clone() {
        if !tab.panes.is_empty()
            && layout.pane_ids().len() == tab.panes.len()
            && tab.panes.iter().all(|pane| pane.session_id.is_some())
        {
            return Some((layout, tab.panes.clone()));
        }
        log::warn!(
            "Pane upgrade data for '{}' is incomplete; trying its legacy summary",
            tab.title
        );
    }

    let session_id = tab.session_id.clone()?;
    let mut pane = PaneUpgradeState::new(Some(session_id));
    pane.title = tab.title.clone();
    pane.title_locked = tab.custom_title.is_some();
    pane.template_name = tab.template_name.clone();
    pane.cwd = tab.cwd.clone();
    pane.keep_open = tab.keep_open;
    Some((PaneLayout::new(), vec![pane]))
}

/// Reconstruct windows by reconnecting to daemon sessions
fn reconstruct_windows(app: &gtk4::Application, state: UpgradeState) {
    log::info!(
        "Reconstructing {} window(s) from upgrade state",
        state.windows.len()
    );

    let config = load_config().unwrap_or_default();
    let theme = cterm_app::resolve_theme(&config);

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("Failed to create tokio runtime: {}", e);
            // Fall back to creating a fresh window
            crate::app::build_ui(app);
            return;
        }
    };

    let mut any_restored = false;

    for (window_idx, window_state) in state.windows.into_iter().enumerate() {
        // Each restored window owns its remote tunnels, matching newly-created
        // windows. Disconnecting a remote in one window must not tear down the
        // transport used by panes that belong to another window.
        let remote_manager = cterm_client::RemoteManager::new();
        log::info!(
            "Window {}: {}x{}, {} tab(s)",
            window_idx,
            window_state.width,
            window_state.height,
            window_state.tabs.len(),
        );

        let mut restored_tabs = Vec::new();
        for (tab_index, mut tab_state) in window_state.tabs.iter().cloned().enumerate() {
            let Some((layout, pane_states)) = pane_restore_state(&tab_state) else {
                log::warn!("Tab '{}' has no session IDs, skipping", tab_state.title);
                continue;
            };
            let mut reconnected = Vec::with_capacity(pane_states.len());
            let mut failed = false;
            for pane in &pane_states {
                let Some(session_id) = pane.session_id.as_deref() else {
                    log::warn!("A pane in tab '{}' has no session ID", tab_state.title);
                    failed = true;
                    break;
                };
                let configured_remote = match pane.remote_name.as_deref() {
                    Some(name) => match config.find_remote(name) {
                        Some(remote) => Some((
                            name.to_string(),
                            remote.host.clone(),
                            remote.ssh_compression,
                        )),
                        None => {
                            log::error!(
                                "Cannot restore remote pane session {session_id}: remote '{name}' is no longer configured"
                            );
                            failed = true;
                            break;
                        }
                    },
                    None => None,
                };
                let manager = remote_manager.clone();
                let result = rt.block_on(async {
                    let connection = if let Some((name, host, compress)) = configured_remote {
                        manager.get_or_connect(&name, &host, compress).await?
                    } else if let Some(path) = pane.daemon_socket.as_ref() {
                        cterm_client::DaemonConnection::connect_unix(path, false).await?
                    } else {
                        cterm_client::DaemonConnection::connect_local().await?
                    };
                    connection.attach_session(session_id, 80, 24).await
                });
                match result {
                    Ok((handle, screen)) => {
                        log::info!("Reconnected to pane session {session_id}");
                        reconnected.push(cterm_app::daemon_reconnect::ReconnectedSession {
                            handle,
                            title: pane.title.clone(),
                            custom_title: if pane.title_locked {
                                pane.title.clone()
                            } else {
                                String::new()
                            },
                            tab_color: tab_state.color.clone().unwrap_or_default(),
                            template_name: pane.template_name.clone().unwrap_or_default(),
                            screen,
                        });
                    }
                    Err(error) => {
                        log::error!("Failed to reconnect pane session {session_id}: {error}");
                        failed = true;
                        break;
                    }
                }
            }
            if failed || reconnected.len() != pane_states.len() {
                log::warn!(
                    "Skipping tab '{}' because its complete pane tree could not be restored",
                    tab_state.title
                );
                continue;
            }
            tab_state.pane_layout = Some(layout.clone());
            tab_state.panes = pane_states;
            restored_tabs.push((tab_index, tab_state, layout, reconnected));
        }

        if restored_tabs.is_empty() {
            log::warn!("No sessions restored for window {}, skipping", window_idx);
            continue;
        }

        // Create a window and add reconnected tabs
        let window = crate::window::CtermWindow::new_empty_with_remote_manager(
            app,
            &config,
            &theme,
            remote_manager.clone(),
        );

        // Restore window size before presenting
        if window_state.width > 0 && window_state.height > 0 {
            window
                .window
                .set_default_size(window_state.width, window_state.height);
        }

        let mut restored_active_tab = 0_u32;
        let mut added_tabs = 0_u32;
        for (original_index, tab_state, layout, sessions) in restored_tabs {
            if window.add_reconnected_pane_tab(tab_state, layout, sessions) {
                if original_index == window_state.active_tab {
                    restored_active_tab = added_tabs;
                }
                added_tabs += 1;
            }
        }
        if added_tabs == 0 {
            log::warn!("No pane tabs could be built for window {window_idx}");
            window.window.close();
            continue;
        }

        // Restore active tab
        if restored_active_tab > 0 {
            window.notebook.set_current_page(Some(restored_active_tab));
        }

        // Restore window geometry
        if window_state.maximized {
            window.window.maximize();
        }
        if window_state.fullscreen {
            window.window.fullscreen();
        }

        window.present();
        any_restored = true;
        log::info!("Window {} restored successfully", window_idx);
    }

    if !any_restored {
        log::warn!("No sessions could be restored, creating fresh window");
        crate::app::build_ui(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_ui::{SplitDirection, SplitPlacement, SplitRatio, SplitRequest};

    #[test]
    fn pane_restore_prefers_complete_v5_topology() {
        let mut tab = TabUpgradeState::new(1);
        tab.session_id = Some("legacy".into());
        let mut layout = PaneLayout::new();
        let first = layout.active();
        layout
            .split(
                first,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    placement: SplitPlacement::Second,
                    ratio: SplitRatio::HALF,
                },
            )
            .unwrap();
        tab.pane_layout = Some(layout.clone());
        tab.panes = vec![
            PaneUpgradeState::new(Some("left".into())),
            PaneUpgradeState::new(Some("right".into())),
        ];

        let (restored_layout, panes) = pane_restore_state(&tab).unwrap();
        assert_eq!(restored_layout, layout);
        assert_eq!(panes[0].session_id.as_deref(), Some("left"));
        assert_eq!(panes[1].session_id.as_deref(), Some("right"));
    }

    #[test]
    fn incomplete_v5_data_falls_back_to_legacy_summary() {
        let mut tab = TabUpgradeState::new(1);
        tab.title = "shell".into();
        tab.session_id = Some("legacy".into());
        tab.pane_layout = Some(PaneLayout::new());

        let (layout, panes) = pane_restore_state(&tab).unwrap();
        assert_eq!(layout.pane_ids().len(), 1);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session_id.as_deref(), Some("legacy"));
        assert_eq!(panes[0].title, "shell");
    }
}
