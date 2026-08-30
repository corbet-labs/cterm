//! Native Windows notification balloons with Kitty replacement and close IDs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO,
    NIIF_NOSOUND, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT,
    NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, NOTIFYICONDATAW_0,
};
use windows::Win32::UI::WindowsAndMessaging::{
    LoadIconW, SetForegroundWindow, ShowWindow, IDI_APPLICATION, SW_RESTORE,
};

use crate::window::WM_APP_NATIVE_NOTIFICATION;

static NEXT_NOTIFICATION_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_NOTIFICATIONS: LazyLock<Mutex<HashMap<u32, ActiveNotification>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct ActiveNotification {
    hwnd: usize,
    protocol_id: Option<String>,
    generation: u64,
    focus: bool,
}

pub fn handle(hwnd: HWND, action: &cterm_core::DesktopNotificationAction) {
    match action {
        cterm_core::DesktopNotificationAction::Show(notification) => show(hwnd, notification),
        cterm_core::DesktopNotificationAction::Close(id) => close(hwnd, id),
    }
}

pub fn native_event(hwnd: HWND, native_id: u32, event: u32) {
    if !matches!(
        event,
        NIN_BALLOONUSERCLICK | NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT
    ) {
        return;
    }

    let notification = ACTIVE_NOTIFICATIONS
        .lock()
        .ok()
        .and_then(|mut active| active.remove(&native_id));
    let Some(notification) = notification else {
        return;
    };
    if notification.hwnd != hwnd.0 as usize {
        return;
    }

    if event == NIN_BALLOONUSERCLICK && notification.focus {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
    remove_native(hwnd, native_id);
}

fn show(hwnd: HWND, notification: &cterm_core::DesktopNotification) {
    let hwnd_value = hwnd.0 as usize;
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    let (native_id, replace) = {
        let mut active = ACTIVE_NOTIFICATIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let existing = notification.id.as_ref().and_then(|protocol_id| {
            active.iter().find_map(|(native_id, current)| {
                (current.hwnd == hwnd_value && current.protocol_id.as_ref() == Some(protocol_id))
                    .then_some(*native_id)
            })
        });
        let native_id = existing.unwrap_or_else(next_native_id);
        active.insert(
            native_id,
            ActiveNotification {
                hwnd: hwnd_value,
                protocol_id: notification.id.clone(),
                generation,
                focus: notification.focus,
            },
        );
        (native_id, existing.is_some())
    };

    let mut info_flags = match notification.urgency {
        cterm_core::NotificationUrgency::Low | cterm_core::NotificationUrgency::Normal => NIIF_INFO,
        cterm_core::NotificationUrgency::Critical => NIIF_ERROR,
    };
    if notification.muted {
        info_flags |= NIIF_NOSOUND;
    }

    let timeout = notification
        .expire_time
        .filter(|milliseconds| *milliseconds > 0)
        .map_or(12_000, |milliseconds| milliseconds as u32);
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: native_id,
        uFlags: NIF_ICON | NIF_TIP | NIF_INFO | NIF_MESSAGE,
        uCallbackMessage: WM_APP_NATIVE_NOTIFICATION,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
        Anonymous: NOTIFYICONDATAW_0 { uTimeout: timeout },
        dwInfoFlags: info_flags,
        ..Default::default()
    };
    copy_utf16("cterm", &mut data.szTip);
    copy_utf16(&notification.title, &mut data.szInfoTitle);
    copy_utf16(&notification.body, &mut data.szInfo);

    let message = if replace { NIM_MODIFY } else { NIM_ADD };
    if !unsafe { Shell_NotifyIconW(message, &data) }.as_bool() {
        log::warn!("Windows rejected a terminal desktop notification");
        remove_if_current(native_id, generation);
        return;
    }

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(timeout as u64));
        if remove_if_current(native_id, generation) {
            remove_native(HWND(hwnd_value as *mut _), native_id);
        }
    });
}

fn close(hwnd: HWND, protocol_id: &str) {
    let hwnd_value = hwnd.0 as usize;
    let native_ids = ACTIVE_NOTIFICATIONS
        .lock()
        .map(|mut active| {
            let ids: Vec<_> = active
                .iter()
                .filter_map(|(native_id, notification)| {
                    (notification.hwnd == hwnd_value
                        && notification.protocol_id.as_deref() == Some(protocol_id))
                    .then_some(*native_id)
                })
                .collect();
            for native_id in &ids {
                active.remove(native_id);
            }
            ids
        })
        .unwrap_or_default();
    for native_id in native_ids {
        remove_native(hwnd, native_id);
    }
}

fn remove_if_current(native_id: u32, generation: u64) -> bool {
    ACTIVE_NOTIFICATIONS
        .lock()
        .map(|mut active| {
            if active.get(&native_id).map(|entry| entry.generation) == Some(generation) {
                active.remove(&native_id);
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
}

fn remove_native(hwnd: HWND, native_id: u32) {
    let remove = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: native_id,
        ..Default::default()
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &remove);
    }
}

fn next_native_id() -> u32 {
    NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed).max(1)
}

fn copy_utf16<const N: usize>(value: &str, destination: &mut [u16; N]) {
    let max = destination.len().saturating_sub(1);
    for (target, unit) in destination.iter_mut().take(max).zip(value.encode_utf16()) {
        *target = unit;
    }
}
