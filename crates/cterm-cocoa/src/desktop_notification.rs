//! Native macOS desktop notifications through UserNotifications.framework.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSRunningApplication};
use objc2_foundation::{MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationInterruptionLevel, UNNotificationPresentationOptions, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

static NEXT_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);
static NOTIFICATIONS_UNAVAILABLE: AtomicBool = AtomicBool::new(false);
static ACTIVE_NOTIFICATION_IDS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FOCUS_NOTIFICATION_IDS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct NotificationDelegateIvars;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CtermNotificationDelegate"]
    #[ivars = NotificationDelegateIvars]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &block2::DynBlock<dyn Fn()>,
        ) {
            let native_id = response.notification().request().identifier().to_string();
            let focus = FOCUS_NOTIFICATION_IDS
                .lock()
                .map(|mut ids| ids.remove(&native_id))
                .unwrap_or(false);
            if let Ok(mut active) = ACTIVE_NOTIFICATION_IDS.lock() {
                active.retain(|_, current| current != &native_id);
            }
            if focus {
                let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
                app.activateIgnoringOtherApps(true);
            }
            completion.call(());
        }
    }
);

impl NotificationDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>();
        let this = this.set_ivars(NotificationDelegateIvars);
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    static NOTIFICATION_DELEGATE: RefCell<Option<Retained<NotificationDelegate>>> =
        const { RefCell::new(None) };
}

fn is_app_bundle_executable(path: &Path) -> bool {
    let Some(macos_directory) = path.parent() else {
        return false;
    };
    if macos_directory.file_name().and_then(|name| name.to_str()) != Some("MacOS") {
        return false;
    }

    let Some(contents_directory) = macos_directory.parent() else {
        return false;
    };
    if contents_directory
        .file_name()
        .and_then(|name| name.to_str())
        != Some("Contents")
    {
        return false;
    }

    contents_directory
        .parent()
        .and_then(Path::extension)
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

/// UserNotifications requires a LaunchServices-backed application bundle.
/// Calling `currentNotificationCenter` from a raw executable raises an
/// Objective-C exception on recent macOS releases instead of returning an
/// error, so reject such processes before entering the framework.
fn notification_center() -> Option<Retained<UNUserNotificationCenter>> {
    if NOTIFICATIONS_UNAVAILABLE.load(Ordering::Relaxed) {
        return None;
    }

    let executable_is_bundled = std::env::current_exe()
        .ok()
        .as_deref()
        .is_some_and(is_app_bundle_executable);
    if !executable_is_bundled {
        NOTIFICATIONS_UNAVAILABLE.store(true, Ordering::Relaxed);
        log::debug!("macOS desktop notifications are unavailable outside a .app bundle");
        return None;
    }

    let running_application = NSRunningApplication::currentApplication();
    let launch_services_recognizes_bundle = running_application.bundleIdentifier().is_some()
        && running_application.bundleURL().is_some();
    if !launch_services_recognizes_bundle {
        NOTIFICATIONS_UNAVAILABLE.store(true, Ordering::Relaxed);
        log::debug!(
            "macOS desktop notifications are unavailable because LaunchServices did not register the .app bundle"
        );
        return None;
    }

    Some(UNUserNotificationCenter::currentNotificationCenter())
}

/// Ask once during application startup so the first terminal notification is
/// not silently dropped while macOS is still presenting its permission sheet.
pub fn request_authorization() {
    let Some(center) = notification_center() else {
        return;
    };
    NOTIFICATION_DELEGATE.with(|stored| {
        let mut stored = stored.borrow_mut();
        let delegate = stored.get_or_insert_with(|| {
            NotificationDelegate::new(MainThreadMarker::new().expect("main thread"))
        });
        center.setDelegate(Some(ProtocolObject::from_ref(&**delegate)));
    });
    let completion = RcBlock::new(
        |granted: objc2::runtime::Bool, _error: *mut objc2_foundation::NSError| {
            if !granted.as_bool() {
                log::info!("macOS desktop notification permission was not granted");
            }
        },
    );
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &completion,
    );
}

pub fn handle(action: &cterm_core::DesktopNotificationAction) {
    match action {
        cterm_core::DesktopNotificationAction::Show(notification) => show(notification),
        cterm_core::DesktopNotificationAction::Close(id) => close(id),
    }
}

fn show(notification: &cterm_core::DesktopNotification) {
    let Some(center) = notification_center() else {
        return;
    };

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notification.title));
    content.setBody(&NSString::from_str(&notification.body));
    content.setInterruptionLevel(match notification.urgency {
        cterm_core::NotificationUrgency::Low => UNNotificationInterruptionLevel::Passive,
        cterm_core::NotificationUrgency::Normal => UNNotificationInterruptionLevel::Active,
        cterm_core::NotificationUrgency::Critical => UNNotificationInterruptionLevel::TimeSensitive,
    });
    if !notification.muted {
        let sound = UNNotificationSound::defaultSound();
        content.setSound(Some(&sound));
    }

    let sequence = NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed);
    let native_id = format!("cterm-{}-{sequence}", std::process::id());
    if notification.focus {
        if let Ok(mut ids) = FOCUS_NOTIFICATION_IDS.lock() {
            if ids.len() >= 128 {
                if let Some(oldest) = ids.iter().next().cloned() {
                    ids.remove(&oldest);
                }
            }
            ids.insert(native_id.clone());
        }
    }
    if let Some(protocol_id) = notification.id.as_ref() {
        let obsolete = ACTIVE_NOTIFICATION_IDS
            .lock()
            .map(|mut active| {
                let mut obsolete = active
                    .insert(protocol_id.clone(), native_id.clone())
                    .into_iter()
                    .collect::<Vec<_>>();
                if active.len() > 64 {
                    if let Some(evicted) = active.keys().find(|id| *id != protocol_id).cloned() {
                        if let Some(native_id) = active.remove(&evicted) {
                            obsolete.push(native_id);
                        }
                    }
                }
                obsolete
            })
            .unwrap_or_default();
        for obsolete_id in obsolete {
            close_native(&obsolete_id);
        }
    }

    let identifier = NSString::from_str(&native_id);
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);
    center.addNotificationRequest_withCompletionHandler(&request, None);

    if let Some(milliseconds) = notification.expire_time.filter(|value| *value > 0) {
        let protocol_id = notification.id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(milliseconds as u64));
            if let Some(protocol_id) = protocol_id {
                let should_close = ACTIVE_NOTIFICATION_IDS
                    .lock()
                    .map(|mut active| {
                        if active.get(&protocol_id) == Some(&native_id) {
                            active.remove(&protocol_id);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !should_close {
                    return;
                }
            }
            close_native(&native_id);
        });
    }
}

fn close(protocol_id: &str) {
    let native_id = ACTIVE_NOTIFICATION_IDS
        .lock()
        .ok()
        .and_then(|mut active| active.remove(protocol_id));
    if let Some(native_id) = native_id {
        close_native(&native_id);
    }
}

fn close_native(native_id: &str) {
    if let Ok(mut ids) = FOCUS_NOTIFICATION_IDS.lock() {
        ids.remove(native_id);
    }
    let identifier = NSString::from_str(native_id);
    let identifiers = NSArray::from_slice(&[&*identifier]);
    let Some(center) = notification_center() else {
        return;
    };
    center.removePendingNotificationRequestsWithIdentifiers(&identifiers);
    center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
}

#[cfg(test)]
mod tests {
    use super::is_app_bundle_executable;
    use std::path::Path;

    #[test]
    fn recognizes_app_bundle_executable_paths() {
        assert!(is_app_bundle_executable(Path::new(
            "/Applications/cterm.app/Contents/MacOS/cterm"
        )));
        assert!(is_app_bundle_executable(Path::new(
            "/tmp/Test.APP/Contents/MacOS/cterm"
        )));
    }

    #[test]
    fn rejects_raw_and_malformed_executable_paths() {
        assert!(!is_app_bundle_executable(Path::new(
            "/tmp/cterm/target/debug/cterm"
        )));
        assert!(!is_app_bundle_executable(Path::new(
            "/tmp/cterm.app/MacOS/cterm"
        )));
        assert!(!is_app_bundle_executable(Path::new(
            "/tmp/cterm.app/Contents/cterm"
        )));
    }
}
