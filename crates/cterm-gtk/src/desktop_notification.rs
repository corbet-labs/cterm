//! Native desktop notification delivery through GApplication.

use gio::prelude::ApplicationExt;
use gio::NotificationPriority;

pub fn handle(action: &cterm_core::DesktopNotificationAction) {
    let Some(application) = gio::Application::default() else {
        log::warn!("Cannot deliver desktop notification before GApplication startup");
        return;
    };

    match action {
        cterm_core::DesktopNotificationAction::Show(notification) => {
            let native = gio::Notification::new(&notification.title);
            if !notification.body.is_empty() {
                native.set_body(Some(&notification.body));
            }
            native.set_priority(match notification.urgency {
                cterm_core::NotificationUrgency::Low => NotificationPriority::Low,
                cterm_core::NotificationUrgency::Normal => NotificationPriority::Normal,
                cterm_core::NotificationUrgency::Critical => NotificationPriority::Urgent,
            });
            if notification.focus {
                native.set_default_action("app.focus-terminal");
            }
            application.send_notification(notification.id.as_deref(), &native);
        }
        cterm_core::DesktopNotificationAction::Close(id) => {
            application.withdraw_notification(id);
        }
    }
}
