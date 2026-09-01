//! Native macOS consent for daemon-owned Kitty OSC 5113 send sessions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{MainThreadOnly, Message};
use objc2_app_kit::{NSAlert, NSAlertStyle, NSModalResponseCancel, NSWindow};
use objc2_foundation::{MainThreadMarker, NSString};
use tokio::sync::{oneshot, watch};

use cterm_proto::proto::{TtyFileTransferApprovalEvent, TtyFileTransferDirection};

const MAX_HOST_CHARS: usize = 255;
const MAX_TRANSFER_ID_CHARS: usize = 128;
static NEXT_PROMPT_TOKEN: AtomicU64 = AtomicU64::new(1);

struct ActivePrompt {
    alert: Retained<NSAlert>,
    parent: Retained<NSWindow>,
    cancelled: Arc<AtomicBool>,
}

thread_local! {
    /// AppKit objects never cross threads. Background cancellation dispatches a
    /// token to the main queue and this main-thread registry owns the sheet.
    static ACTIVE_PROMPTS: RefCell<HashMap<u64, ActivePrompt>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptResolution {
    pub approved: bool,
    /// The caller must close an outstanding AppKit sheet for timeout,
    /// lifecycle cancellation, or a lost completion callback.
    pub dismiss: bool,
}

/// Allocate a process-local token used only to address one owned AppKit sheet.
pub fn next_tty_file_transfer_prompt_token() -> u64 {
    let token = NEXT_PROMPT_TOKEN.fetch_add(1, Ordering::Relaxed);
    if token == 0 {
        NEXT_PROMPT_TOKEN.fetch_add(1, Ordering::Relaxed)
    } else {
        token
    }
}

/// Begin one fail-closed, main-thread-owned consent sheet without blocking the
/// AppKit event loop.
///
/// The completion sends `true` only for a valid daemon send policy followed by
/// an explicit click on the non-default "Allow Once" button. Unsupported
/// directions, malformed policy events, and pre-presentation cancellation are
/// denied without showing a misleading prompt.
#[allow(clippy::too_many_arguments)]
pub fn begin_tty_file_transfer_prompt(
    mtm: MainThreadMarker,
    token: u64,
    event: &TtyFileTransferApprovalEvent,
    daemon_host: &str,
    parent: &NSWindow,
    cancelled: Arc<AtomicBool>,
    result_tx: oneshot::Sender<bool>,
) {
    let Some(body) = format_tty_file_transfer_prompt(event, daemon_host) else {
        log::warn!(
            "Refusing invalid or unsupported OSC 5113 approval request {}",
            event.request_id
        );
        let _ = result_tx.send(false);
        return;
    };
    if cancelled.load(Ordering::Acquire) {
        let _ = result_tx.send(false);
        return;
    };

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str("Allow terminal file transfer?"));
    alert.setInformativeText(&NSString::from_str(&body));

    // The first NSAlert button is the Return-key default. Keep denial first so
    // pressing Return, closing the alert, or receiving an unexpected response
    // can never grant filesystem access.
    alert.addButtonWithTitle(&NSString::from_str("Deny"));
    alert.addButtonWithTitle(&NSString::from_str("Allow Once"));

    let completion_sender = RefCell::new(Some(result_tx));
    let completion_cancelled = Arc::clone(&cancelled);
    let completion_alert = alert.clone();
    let completion = RcBlock::new(move |response: objc2_app_kit::NSModalResponse| {
        // Match AppKit's ownership pattern used by established dialog crates:
        // retain the NSAlert through its completion callback even after the
        // registry releases its cancellation handle.
        let _keep_alert_alive = &completion_alert;
        ACTIVE_PROMPTS.with(|prompts| {
            prompts.borrow_mut().remove(&token);
        });
        let approved = response == objc2_app_kit::NSAlertSecondButtonReturn
            && !completion_cancelled.load(Ordering::Acquire);
        if let Some(sender) = completion_sender.borrow_mut().take() {
            let _ = sender.send(approved);
        }
    });

    ACTIVE_PROMPTS.with(|prompts| {
        prompts.borrow_mut().insert(
            token,
            ActivePrompt {
                alert: alert.clone(),
                parent: parent.retain(),
                cancelled,
            },
        );
    });
    alert.beginSheetModalForWindow_completionHandler(parent, Some(&completion));
}

/// End one pending sheet as a denial. Must be called on AppKit's main thread.
pub fn cancel_tty_file_transfer_prompt(_mtm: MainThreadMarker, token: u64) {
    let active = ACTIVE_PROMPTS.with(|prompts| prompts.borrow_mut().remove(&token));
    let Some(active) = active else {
        return;
    };

    active.cancelled.store(true, Ordering::Release);
    let sheet = active.alert.window();
    // `sheets` includes queued sheets, so this also removes a prompt that was
    // waiting behind another document-modal sheet and prevents stale display.
    active
        .parent
        .endSheet_returnCode(&sheet, NSModalResponseCancel);
    sheet.orderOut(None);
}

/// Await a sheet result, the daemon-advertised expiry, or pane/reader
/// cancellation. Cancellation and expiry are biased ahead of a simultaneous
/// positive completion so consent can never become valid after its deadline.
pub async fn await_tty_file_transfer_prompt(
    result_rx: oneshot::Receiver<bool>,
    mut lifecycle_cancel: watch::Receiver<bool>,
    expires_at: tokio::time::Instant,
) -> PromptResolution {
    if *lifecycle_cancel.borrow() {
        return PromptResolution {
            approved: false,
            dismiss: true,
        };
    }

    tokio::select! {
        biased;
        _ = lifecycle_cancel.changed() => PromptResolution {
            approved: false,
            dismiss: true,
        },
        _ = tokio::time::sleep_until(expires_at) => PromptResolution {
            approved: false,
            dismiss: true,
        },
        result = result_rx => match result {
            Ok(approved) => PromptResolution {
                approved,
                dismiss: false,
            },
            Err(_) => PromptResolution {
                approved: false,
                dismiss: true,
            },
        },
    }
}

fn format_tty_file_transfer_prompt(
    event: &TtyFileTransferApprovalEvent,
    daemon_host: &str,
) -> Option<String> {
    if event.request_id == 0
        || event.transfer_id.is_empty()
        || TtyFileTransferDirection::try_from(event.direction).ok()
            != Some(TtyFileTransferDirection::Send)
        || !event.paths.is_empty()
        || event.expires_in_ms == 0
        || event.max_files == 0
        || event.max_file_bytes == 0
        || event.max_session_bytes < event.max_file_bytes
    {
        return None;
    }

    let host = sanitize_display_text(daemon_host.trim(), MAX_HOST_CHARS);
    let host = if host.is_empty() {
        "unknown ctermd host".to_string()
    } else {
        host
    };
    let transfer_id = sanitize_display_text(&event.transfer_id, MAX_TRANSFER_ID_CHARS);

    Some(format!(
        "A program in this terminal wants to send files to ctermd on {host}.\n\n\
         File names and destinations arrive only after you approve. This one-time approval lets \
         the program create new files or replace existing writable files as the ctermd user.\n\n\
         Limits enforced by ctermd:\n\
         • Up to {} {}\n\
         • Up to {} per file\n\
         • Up to {} total\n\
         • This request expires in {}\n\n\
         Transfer: {transfer_id}",
        event.max_files,
        if event.max_files == 1 {
            "file"
        } else {
            "files"
        },
        format_byte_count(event.max_file_bytes),
        format_byte_count(event.max_session_bytes),
        format_expiry(event.expires_in_ms),
    ))
}

fn sanitize_display_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut output = String::with_capacity(value.len().min(max_chars));
    for character in chars.by_ref().take(max_chars) {
        if character.is_control() || is_directional_control(character) {
            output.push('�');
        } else {
            output.push(character);
        }
    }
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

fn is_directional_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn format_byte_count(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;

    for (unit_bytes, suffix) in [(TIB, "TiB"), (GIB, "GiB"), (MIB, "MiB"), (KIB, "KiB")] {
        if bytes >= unit_bytes {
            if bytes.is_multiple_of(unit_bytes) {
                return format!("{} {suffix}", bytes / unit_bytes);
            }
            return format!("{:.1} {suffix}", bytes as f64 / unit_bytes as f64);
        }
    }
    format!("{bytes} bytes")
}

fn format_expiry(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds} ms");
    }
    if milliseconds.is_multiple_of(1_000) {
        let seconds = milliseconds / 1_000;
        return format!(
            "{seconds} {}",
            if seconds == 1 { "second" } else { "seconds" }
        );
    }
    format!("{:.1} seconds", milliseconds as f64 / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send_event() -> TtyFileTransferApprovalEvent {
        TtyFileTransferApprovalEvent {
            request_id: 7,
            transfer_id: "upload-1".into(),
            direction: TtyFileTransferDirection::Send as i32,
            paths: Vec::new(),
            expires_in_ms: 60_000,
            max_files: 256,
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            max_session_bytes: 16 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn formatter_explains_remote_destination_authority_and_daemon_policy() {
        let body = format_tty_file_transfer_prompt(&send_event(), "build.example").unwrap();

        assert!(body.contains("ctermd on build.example"));
        assert!(body.contains("File names and destinations arrive only after you approve"));
        assert!(body.contains("create new files or replace existing writable files"));
        assert!(body.contains("256 files"));
        assert!(body.contains("4 GiB per file"));
        assert!(body.contains("16 GiB total"));
        assert!(body.contains("expires in 60 seconds"));
        assert!(body.contains("Transfer: upload-1"));
    }

    #[test]
    fn terminal_control_characters_are_neutralized_and_fields_are_bounded() {
        let mut event = send_event();
        event.transfer_id = format!("safe\n\u{202e}{}", "x".repeat(300));
        let body = format_tty_file_transfer_prompt(&event, "host\u{1b}]0;spoof\u{7}").unwrap();

        assert!(!body.contains('\u{1b}'));
        assert!(!body.contains('\u{7}'));
        assert!(!body.contains('\u{202e}'));
        assert!(body.contains("safe��"));
        assert!(body.contains('…'));
        assert!(body.len() < 1_500);

        assert_eq!(sanitize_display_text("line\n\u{202e}end", 64), "line��end");
        let bounded = sanitize_display_text(
            &"x".repeat(MAX_TRANSFER_ID_CHARS + 1),
            MAX_TRANSFER_ID_CHARS,
        );
        assert_eq!(bounded.chars().count(), MAX_TRANSFER_ID_CHARS + 1);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn unsupported_or_incomplete_policy_fails_closed_before_formatting() {
        let mut event = send_event();
        event.direction = TtyFileTransferDirection::Receive as i32;
        assert!(format_tty_file_transfer_prompt(&event, "host").is_none());

        event.direction = TtyFileTransferDirection::Send as i32;
        event.paths.push("/unexpected/path".into());
        assert!(format_tty_file_transfer_prompt(&event, "host").is_none());

        event.paths.clear();
        event.expires_in_ms = 0;
        assert!(format_tty_file_transfer_prompt(&event, "host").is_none());

        event.expires_in_ms = 1;
        event.max_session_bytes = event.max_file_bytes - 1;
        assert!(format_tty_file_transfer_prompt(&event, "host").is_none());
    }

    #[test]
    fn compact_policy_units_are_stable() {
        assert_eq!(format_byte_count(999), "999 bytes");
        assert_eq!(format_byte_count(1536), "1.5 KiB");
        assert_eq!(format_expiry(750), "750 ms");
        assert_eq!(format_expiry(1_000), "1 second");
        assert_eq!(format_expiry(1_500), "1.5 seconds");
    }

    #[tokio::test]
    async fn lifecycle_cancellation_denies_and_requests_sheet_dismissal() {
        let (_result_tx, result_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        cancel_tx.send_replace(true);

        assert_eq!(
            await_tty_file_transfer_prompt(
                result_rx,
                cancel_rx,
                tokio::time::Instant::now() + Duration::from_secs(60),
            )
            .await,
            PromptResolution {
                approved: false,
                dismiss: true,
            }
        );
    }

    #[tokio::test]
    async fn lost_reader_lifecycle_denies_and_requests_sheet_dismissal() {
        let (_result_tx, result_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        drop(cancel_tx);

        assert_eq!(
            await_tty_file_transfer_prompt(
                result_rx,
                cancel_rx,
                tokio::time::Instant::now() + Duration::from_secs(60),
            )
            .await,
            PromptResolution {
                approved: false,
                dismiss: true,
            }
        );
    }

    #[tokio::test]
    async fn explicit_completion_is_the_only_approval_path() {
        let (result_tx, result_rx) = oneshot::channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        result_tx.send(true).unwrap();

        assert_eq!(
            await_tty_file_transfer_prompt(
                result_rx,
                cancel_rx,
                tokio::time::Instant::now() + Duration::from_secs(60),
            )
            .await,
            PromptResolution {
                approved: true,
                dismiss: false,
            }
        );
    }

    #[tokio::test]
    async fn expiry_auto_denies_and_requests_sheet_dismissal() {
        let (_result_tx, result_rx) = oneshot::channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        assert_eq!(
            await_tty_file_transfer_prompt(result_rx, cancel_rx, tokio::time::Instant::now()).await,
            PromptResolution {
                approved: false,
                dismiss: true,
            }
        );
    }
}
