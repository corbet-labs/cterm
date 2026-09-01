//! Terminal event conversion between cterm-core and proto

use crate::proto;
use cterm_core::screen::{
    ClipboardOperation as CoreClipboardOp, ClipboardSelection as CoreClipboardSel,
    DesktopNotificationAction as CoreNotificationAction,
    NotificationUrgency as CoreNotificationUrgency,
};
use cterm_core::term::TerminalEvent as CoreEvent;

/// Convert a wire-visible cterm_core event to its protocol representation.
pub fn event_to_proto(event: &CoreEvent) -> Option<proto::TerminalEvent> {
    use proto::terminal_event::Event;

    let event = match event {
        CoreEvent::TitleChanged(title) => Event::TitleChanged(proto::TitleChangedEvent {
            title: title.clone(),
        }),
        CoreEvent::Bell => Event::Bell(proto::BellEvent {}),
        CoreEvent::ProcessExited(code) => {
            Event::ProcessExited(proto::ProcessExitedEvent { exit_code: *code })
        }
        CoreEvent::ContentChanged => Event::ContentChanged(proto::ContentChangedEvent {}),
        CoreEvent::ClipboardRequest(op) => {
            let (operation, selection, data) = match op {
                CoreClipboardOp::Query { selection } => (
                    proto::ClipboardOperation::Read,
                    selection_to_proto(selection),
                    None,
                ),
                CoreClipboardOp::Set { selection, data } => (
                    proto::ClipboardOperation::Write,
                    selection_to_proto(selection),
                    Some(data.clone()),
                ),
            };
            Event::ClipboardRequest(proto::ClipboardRequestEvent {
                operation: operation as i32,
                selection: selection as i32,
                data,
            })
        }
        CoreEvent::DesktopNotification(notification) => {
            Event::DesktopNotification(desktop_notification_to_proto(notification))
        }
        // OSC 72 remains in the mirrored PTY byte stream and is parsed by the
        // native frontend. Sending it again on the event stream would apply a
        // drag command twice.
        CoreEvent::DndCommand(_) => return None,
    };

    Some(proto::TerminalEvent { event: Some(event) })
}

fn desktop_notification_to_proto(
    action: &CoreNotificationAction,
) -> proto::DesktopNotificationEvent {
    match action {
        CoreNotificationAction::Show(notification) => proto::DesktopNotificationEvent {
            title: notification.title.clone(),
            body: notification.body.clone(),
            action: proto::DesktopNotificationAction::Show as i32,
            id: notification.id.clone(),
            urgency: match notification.urgency {
                CoreNotificationUrgency::Low => proto::DesktopNotificationUrgency::Low,
                CoreNotificationUrgency::Normal => proto::DesktopNotificationUrgency::Normal,
                CoreNotificationUrgency::Critical => proto::DesktopNotificationUrgency::Critical,
            } as i32,
            expire_time: notification.expire_time,
            muted: notification.muted,
            focus: notification.focus,
        },
        CoreNotificationAction::Close(id) => proto::DesktopNotificationEvent {
            action: proto::DesktopNotificationAction::Close as i32,
            id: Some(id.clone()),
            ..Default::default()
        },
    }
}

/// Convert clipboard selection to proto
fn selection_to_proto(sel: &CoreClipboardSel) -> proto::ClipboardSelection {
    match sel {
        CoreClipboardSel::Clipboard => proto::ClipboardSelection::Clipboard,
        CoreClipboardSel::Primary => proto::ClipboardSelection::Primary,
        CoreClipboardSel::Select => proto::ClipboardSelection::Select,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_changed_event() {
        let event = CoreEvent::TitleChanged("test".to_string());
        let proto = event_to_proto(&event).unwrap();
        match proto.event {
            Some(proto::terminal_event::Event::TitleChanged(e)) => {
                assert_eq!(e.title, "test");
            }
            _ => panic!("Expected TitleChanged event"),
        }
    }

    #[test]
    fn test_bell_event() {
        let event = CoreEvent::Bell;
        let proto = event_to_proto(&event).unwrap();
        assert!(matches!(
            proto.event,
            Some(proto::terminal_event::Event::Bell(_))
        ));
    }

    #[test]
    fn test_process_exited_event() {
        let event = CoreEvent::ProcessExited(42);
        let proto = event_to_proto(&event).unwrap();
        match proto.event {
            Some(proto::terminal_event::Event::ProcessExited(e)) => {
                assert_eq!(e.exit_code, 42);
            }
            _ => panic!("Expected ProcessExited event"),
        }
    }

    #[test]
    fn test_desktop_notification_event() {
        let event = CoreEvent::DesktopNotification(CoreNotificationAction::Show(
            cterm_core::DesktopNotification {
                id: Some("build".into()),
                title: "Build complete".into(),
                body: "All checks passed".into(),
                urgency: CoreNotificationUrgency::Critical,
                expire_time: Some(5_000),
                muted: true,
                focus: true,
            },
        ));
        let proto = event_to_proto(&event).unwrap();
        match proto.event {
            Some(proto::terminal_event::Event::DesktopNotification(event)) => {
                assert_eq!(event.title, "Build complete");
                assert_eq!(event.body, "All checks passed");
                assert_eq!(event.id.as_deref(), Some("build"));
                assert_eq!(event.action, proto::DesktopNotificationAction::Show as i32);
                assert_eq!(
                    event.urgency,
                    proto::DesktopNotificationUrgency::Critical as i32
                );
                assert_eq!(event.expire_time, Some(5_000));
                assert!(event.muted);
                assert!(event.focus);
            }
            _ => panic!("Expected DesktopNotification event"),
        }
    }

    #[test]
    fn test_close_desktop_notification_event() {
        let event = CoreEvent::DesktopNotification(CoreNotificationAction::Close("build".into()));
        let proto = event_to_proto(&event).unwrap();
        match proto.event {
            Some(proto::terminal_event::Event::DesktopNotification(event)) => {
                assert_eq!(event.id.as_deref(), Some("build"));
                assert_eq!(event.action, proto::DesktopNotificationAction::Close as i32);
            }
            _ => panic!("Expected DesktopNotification event"),
        }
    }

    #[test]
    fn dnd_commands_stay_in_the_mirrored_pty_stream() {
        let event = CoreEvent::DndCommand(cterm_core::DndCommand {
            command_type: cterm_core::DndCommandType::AcceptDrops,
            more: false,
            client_id: 0,
            operation: 0,
            cell_x: 0,
            cell_y: 0,
            pixel_x: 0,
            pixel_y: 0,
            payload: b"text/uri-list".to_vec(),
        });

        assert!(event_to_proto(&event).is_none());
    }
}
