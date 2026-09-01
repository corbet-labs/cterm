//! Native, fail-closed GTK consent for daemon-owned OSC 5113 transfers.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{ButtonsType, DialogFlags, DrawingArea, MessageDialog, MessageType, ResponseType};

use cterm_proto::proto::{TtyFileTransferApprovalEvent, TtyFileTransferDirection};

const MAX_DISPLAY_CHARS: usize = 160;

/// Queue one native prompt without blocking daemon event delivery. The expiry
/// is anchored when the event is received; a prompt waiting behind another one
/// can therefore expire without ever presenting stale authority.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_tty_file_transfer_prompt(
    event: TtyFileTransferApprovalEvent,
    daemon_hostname: String,
    owner: glib::SendWeakRef<DrawingArea>,
    active: Arc<AtomicBool>,
    mut lifecycle_cancel: tokio::sync::watch::Receiver<bool>,
    prompt_gate: Arc<tokio::sync::Semaphore>,
    expires_at: tokio::time::Instant,
) -> bool {
    if *lifecycle_cancel.borrow()
        || !active.load(Ordering::Acquire)
        || expires_at <= tokio::time::Instant::now()
    {
        return false;
    }

    let permit = tokio::select! {
        biased;
        _ = lifecycle_cancel.changed() => return false,
        _ = tokio::time::sleep_until(expires_at) => return false,
        permit = prompt_gate.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => return false,
        },
    };
    if *lifecycle_cancel.borrow()
        || !active.load(Ordering::Acquire)
        || expires_at <= tokio::time::Instant::now()
    {
        return false;
    }

    let expires_at = expires_at.into_std();
    let approved = tokio::task::spawn_blocking(move || {
        show_tty_file_transfer_prompt(&event, &daemon_hostname, owner, active, expires_at)
    })
    .await
    .unwrap_or(false);
    drop(permit);
    approved
}

pub fn show_tty_file_transfer_prompt(
    event: &TtyFileTransferApprovalEvent,
    daemon_hostname: &str,
    owner: glib::SendWeakRef<DrawingArea>,
    active: Arc<AtomicBool>,
    expires_at: Instant,
) -> bool {
    let Some(body) = format_prompt(event, daemon_hostname) else {
        return false;
    };
    if !active.load(Ordering::Acquire) || Instant::now() >= expires_at {
        return false;
    }

    run_on_main_blocking(move |tx| {
        if !active.load(Ordering::Acquire) || Instant::now() >= expires_at {
            let _ = tx.send(false);
            return;
        }
        let Some(owner) = owner.upgrade() else {
            let _ = tx.send(false);
            return;
        };
        let Some(parent) = owner
            .root()
            .and_then(|root| root.downcast::<gtk4::Window>().ok())
        else {
            let _ = tx.send(false);
            return;
        };
        let dialog = MessageDialog::new(
            Some(&parent),
            DialogFlags::MODAL,
            MessageType::Warning,
            ButtonsType::None,
            &body,
        );
        dialog.add_button("Deny", ResponseType::Cancel);
        dialog.add_button("Allow this transfer", ResponseType::Accept);
        dialog.set_default_response(ResponseType::Cancel);

        let sender = Rc::new(RefCell::new(Some(tx)));
        let response_sender = Rc::clone(&sender);
        dialog.connect_response(move |dialog, response| {
            if let Some(sender) = response_sender.borrow_mut().take() {
                let _ = sender.send(response == ResponseType::Accept);
            }
            dialog.close();
        });

        let weak_dialog = dialog.downgrade();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            if active.load(Ordering::Acquire) && Instant::now() < expires_at {
                return glib::ControlFlow::Continue;
            }
            if let Some(sender) = sender.borrow_mut().take() {
                let _ = sender.send(false);
            }
            if let Some(dialog) = weak_dialog.upgrade() {
                dialog.close();
            }
            glib::ControlFlow::Break
        });
        dialog.present();
    })
    .unwrap_or(false)
}

fn format_prompt(event: &TtyFileTransferApprovalEvent, daemon_hostname: &str) -> Option<String> {
    let direction = TtyFileTransferDirection::try_from(event.direction).ok()?;
    if event.request_id == 0
        || event.transfer_id.is_empty()
        || event.expires_in_ms == 0
        || event.max_files == 0
        || event.max_file_bytes == 0
        || event.max_session_bytes < event.max_file_bytes
        || (direction == TtyFileTransferDirection::Send && !event.paths.is_empty())
    {
        return None;
    }
    let host = sanitize(daemon_hostname);
    let transfer_id = sanitize(&event.transfer_id);
    let limits = format!(
        "Limits: up to {} files, {} per file and {} total. This request expires in {} seconds.",
        event.max_files,
        format_bytes(event.max_file_bytes),
        format_bytes(event.max_session_bytes),
        event.expires_in_ms / 1_000,
    );
    match direction {
        TtyFileTransferDirection::Send => Some(format!(
            "A program in terminal session ‘{transfer_id}’ wants to send files to the ctermd host ‘{host}’.\n\n\
             File names arrive only after approval. This transfer may create or replace files anywhere your daemon account can write. Each file is published atomically, but the whole transfer is not atomic.\n\n\
             {limits}\n\nAllow this one transfer?"
        )),
        TtyFileTransferDirection::Receive => {
            let paths = event
                .paths
                .iter()
                .take(8)
                .map(|path| format!("• {}", sanitize(path)))
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!(
                "A program in terminal session ‘{transfer_id}’ wants to read these files from the ctermd host ‘{host}’:\n\n{paths}\n\n{limits}\n\nAllow this one transfer?"
            ))
        }
        TtyFileTransferDirection::Unspecified => None,
    }
}

fn sanitize(value: &str) -> String {
    let mut sanitized = String::new();
    for character in value.chars().take(MAX_DISPLAY_CHARS) {
        sanitized.push(
            if character.is_control() || is_directional_control(character) {
                '�'
            } else {
                character
            },
        );
    }
    if value.chars().count() > MAX_DISPLAY_CHARS {
        sanitized.push('…');
    }
    sanitized
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

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn run_on_main_blocking<R, F>(f: F) -> Option<R>
where
    R: Send + 'static,
    F: FnOnce(std::sync::mpsc::Sender<R>) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    glib::idle_add_once(move || f(tx));
    rx.recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(direction: TtyFileTransferDirection) -> TtyFileTransferApprovalEvent {
        TtyFileTransferApprovalEvent {
            request_id: 1,
            transfer_id: "job\nname".into(),
            direction: direction as i32,
            paths: Vec::new(),
            expires_in_ms: 60_000,
            max_files: 256,
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            max_session_bytes: 16 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn send_warning_discloses_late_names_replacement_host_and_limits() {
        let body = format_prompt(&event(TtyFileTransferDirection::Send), "remote\nhost").unwrap();
        assert!(body.contains("names arrive only after approval"));
        assert!(body.contains("create or replace"));
        assert!(body.contains("ctermd host ‘remote�host’"));
        assert!(body.contains("4 GiB per file"));
        assert!(!body.contains("job\nname"));
    }

    #[test]
    fn unknown_direction_fails_closed() {
        assert!(format_prompt(&event(TtyFileTransferDirection::Unspecified), "host").is_none());
    }

    #[test]
    fn malformed_policy_and_directional_controls_fail_closed_or_are_neutralized() {
        let mut malformed = event(TtyFileTransferDirection::Send);
        malformed.max_session_bytes = malformed.max_file_bytes - 1;
        assert!(format_prompt(&malformed, "host").is_none());

        let body =
            format_prompt(&event(TtyFileTransferDirection::Send), "safe\u{202e}host").unwrap();
        assert!(!body.contains('\u{202e}'));
        assert!(body.contains("safe�host"));
    }
}
