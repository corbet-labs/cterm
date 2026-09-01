//! Native consent prompt for daemon-owned Kitty OSC 5113 file transfers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr;

use cterm_proto::proto::{TtyFileTransferApprovalEvent, TtyFileTransferDirection};
use winapi::shared::basetsd::INT_PTR;
use winapi::shared::minwindef::{LPARAM, UINT, WPARAM};
use winapi::shared::windef::{HWND as WinapiHwnd, RECT};
use winapi::um::winuser::*;
use windows::Win32::Foundation::HWND as WindowsHwnd;

use crate::dialog_utils::{
    create_button, create_default_button, create_label, create_multiline_edit, set_edit_text,
};

const PROMPT_TITLE: &str = "Allow terminal file transfer?";
const MAX_DISPLAY_CHARS: usize = 160;
const MAX_DISPLAY_PATHS: usize = 8;
const IDC_PROMPT_DETAILS: i32 = 5113;
const DIALOG_TOKEN_INDEX: i32 = (std::mem::size_of::<isize>() * 2) as i32; // DWLP_USER
pub(crate) const WM_TTY_FILE_TRANSFER_DIALOG_CANCEL: u32 = WM_APP + 0x113;

thread_local! {
    static DIALOG_MESSAGES: RefCell<HashMap<usize, String>> = RefCell::new(HashMap::new());
}

/// Pane/daemon context added by the native frontend. Every displayed value is
/// still sanitized because pane titles and remote metadata may be untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TtyFileTransferPromptContext {
    pub pane_title: String,
    pub daemon_hostname: String,
    pub daemon_session_id: String,
}

/// Reject malformed or incomplete events before they can produce a consent UI.
pub(crate) fn valid_prompt_event(event: &TtyFileTransferApprovalEvent) -> bool {
    if event.request_id == 0
        || event.transfer_id.is_empty()
        || event.expires_in_ms == 0
        || event.max_files == 0
        || event.max_file_bytes == 0
        || event.max_session_bytes == 0
        || event.max_file_bytes > event.max_session_bytes
    {
        return false;
    }

    match TtyFileTransferDirection::try_from(event.direction)
        .unwrap_or(TtyFileTransferDirection::Unspecified)
    {
        TtyFileTransferDirection::Send => event.paths.is_empty(),
        TtyFileTransferDirection::Receive => true,
        TtyFileTransferDirection::Unspecified => false,
    }
}

/// Format the exact authorization scope shown by the Win32 dialog.
pub(crate) fn format_prompt(
    event: &TtyFileTransferApprovalEvent,
    context: &TtyFileTransferPromptContext,
) -> Option<String> {
    if !valid_prompt_event(event) {
        return None;
    }

    let pane_title = sanitize_display(&context.pane_title, MAX_DISPLAY_CHARS);
    let hostname = sanitize_display(&context.daemon_hostname, MAX_DISPLAY_CHARS);
    let daemon_session_id = sanitize_display(&context.daemon_session_id, MAX_DISPLAY_CHARS);
    let transfer_id = sanitize_display(&event.transfer_id, MAX_DISPLAY_CHARS);
    let subject = format!("Pane: {pane_title}\r\nDaemon host: {hostname}");
    let limits = format!(
        "Limits: up to {} files, {} per file, {} total.",
        event.max_files,
        format_bytes(event.max_file_bytes),
        format_bytes(event.max_session_bytes),
    );

    let request = match TtyFileTransferDirection::try_from(event.direction)
        .unwrap_or(TtyFileTransferDirection::Unspecified)
    {
        TtyFileTransferDirection::Send => format!(
            "A terminal program wants to send files to the daemon host.\r\n\r\n\
             {subject}\r\n\r\n\
             File names and destinations are supplied only after approval. This transfer may create or replace files anywhere the daemon account can write.\r\n\r\n\
             {limits}"
        ),
        TtyFileTransferDirection::Receive => {
            let mut paths = String::new();
            for path in event.paths.iter().take(MAX_DISPLAY_PATHS) {
                paths.push_str("\r\n  • ");
                paths.push_str(&sanitize_display(path, MAX_DISPLAY_CHARS));
            }
            if event.paths.len() > MAX_DISPLAY_PATHS {
                paths.push_str(&format!(
                    "\r\n  • … and {} more",
                    event.paths.len() - MAX_DISPLAY_PATHS
                ));
            }
            if paths.is_empty() {
                paths.push_str("\r\n  (no paths were supplied)");
            }
            format!(
                "A terminal program wants to read files from the daemon host.\r\n\r\n\
                 {subject}\r\n\r\n\
                 Requested paths:{paths}\r\n\r\n\
                 {limits}"
            )
        }
        TtyFileTransferDirection::Unspecified => return None,
    };

    Some(format!(
        "{request}\r\n\r\nDaemon session: {daemon_session_id}\r\nTransfer ID: {transfer_id}\r\n\r\nAllow this one transfer?"
    ))
}

/// Show a main-window-owned native prompt. Deny, close, an unexpected result,
/// invalid input, and dialog creation failure all return false. Deny is the
/// focused default, so pressing Enter cannot approve the transfer. The dialog
/// state is addressed only by the bounded registry token, never by a pointer.
pub(crate) fn show_prompt(
    parent: WindowsHwnd,
    token: usize,
    event: &TtyFileTransferApprovalEvent,
    context: &TtyFileTransferPromptContext,
) -> bool {
    let Some(message) = format_prompt(event, context) else {
        return false;
    };
    DIALOG_MESSAGES.with(|messages| {
        messages.borrow_mut().insert(token, message);
    });
    let template = build_dialog_template();
    let result = unsafe {
        DialogBoxIndirectParamW(
            ptr::null_mut(),
            template.as_ptr() as *const DLGTEMPLATE,
            parent.0 as WinapiHwnd,
            Some(dialog_proc),
            token as LPARAM,
        )
    };
    DIALOG_MESSAGES.with(|messages| {
        messages.borrow_mut().remove(&token);
    });
    result == IDYES as isize
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn build_dialog_template() -> Vec<u8> {
    let mut template = Vec::new();
    let style = DS_MODALFRAME | DS_CENTER | WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_SETFONT;
    template.extend_from_slice(&style.to_le_bytes());
    template.extend_from_slice(&0u32.to_le_bytes()); // extended style
    template.extend_from_slice(&0u16.to_le_bytes()); // controls are created in WM_INITDIALOG
    template.extend_from_slice(&0i16.to_le_bytes());
    template.extend_from_slice(&0i16.to_le_bytes());
    template.extend_from_slice(&420i16.to_le_bytes());
    template.extend_from_slice(&280i16.to_le_bytes());
    template.extend_from_slice(&0u16.to_le_bytes()); // no menu
    template.extend_from_slice(&0u16.to_le_bytes()); // default dialog class
    append_wide(&mut template, PROMPT_TITLE);
    while !template.len().is_multiple_of(2) {
        template.push(0);
    }
    template.extend_from_slice(&9u16.to_le_bytes());
    append_wide(&mut template, "Segoe UI");
    template
}

fn append_wide(buffer: &mut Vec<u8>, value: &str) {
    for code_unit in to_wide(value) {
        buffer.extend_from_slice(&code_unit.to_le_bytes());
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: WinapiHwnd,
    message: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> INT_PTR {
    match message {
        WM_INITDIALOG => initialize_dialog(hwnd, lparam as usize),
        WM_COMMAND => {
            match (wparam & 0xffff) as i32 {
                IDYES => EndDialog(hwnd, IDYES as isize),
                IDNO | IDCANCEL => EndDialog(hwnd, IDNO as isize),
                _ => return 0,
            };
            1
        }
        WM_CLOSE | WM_TTY_FILE_TRANSFER_DIALOG_CANCEL => {
            EndDialog(hwnd, IDNO as isize);
            1
        }
        WM_DESTROY => {
            let token = GetWindowLongPtrW(hwnd, DIALOG_TOKEN_INDEX) as usize;
            crate::window::tty_file_transfer_dialog_destroyed(token, hwnd as usize);
            0
        }
        _ => 0,
    }
}

unsafe fn initialize_dialog(hwnd: WinapiHwnd, token: usize) -> INT_PTR {
    SetWindowLongPtrW(hwnd, DIALOG_TOKEN_INDEX, token as isize);
    crate::window::tty_file_transfer_dialog_created(token, hwnd as usize);

    let Some(message) = DIALOG_MESSAGES.with(|messages| messages.borrow().get(&token).cloned())
    else {
        EndDialog(hwnd, IDNO as isize);
        return 0;
    };

    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    let margin = 16;
    let button_width = 90;
    let button_height = 28;
    let button_gap = 10;
    let button_y = height - margin - button_height;

    create_label(
        hwnd,
        0,
        "Review this terminal file transfer request",
        margin,
        margin,
        width - margin * 2,
        24,
    );
    let details = create_multiline_edit(
        hwnd,
        IDC_PROMPT_DETAILS,
        margin,
        margin + 30,
        width - margin * 2,
        button_y - margin * 2 - 30,
    );
    set_edit_text(details, &message);
    SendMessageW(details, EM_SETREADONLY as UINT, 1, 0);

    create_button(
        hwnd,
        IDYES,
        "Allow",
        width - margin - button_width * 2 - button_gap,
        button_y,
        button_width,
        button_height,
    );
    let deny = create_default_button(
        hwnd,
        IDNO,
        "Deny",
        width - margin - button_width,
        button_y,
        button_width,
        button_height,
    );
    SendMessageW(hwnd, DM_SETDEFID, IDNO as WPARAM, 0);
    SetFocus(deny);
    0
}

fn sanitize_display(value: &str, max_chars: usize) -> String {
    let mut sanitized = String::new();
    let mut truncated = false;
    for (index, character) in value.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        if character.is_control() || is_unsafe_format_control(character) {
            sanitized.push('�');
        } else {
            sanitized.push(character);
        }
    }
    if truncated {
        sanitized.push('…');
    }
    if sanitized.is_empty() {
        sanitized.push_str("(unnamed)");
    }
    sanitized
}

fn is_unsafe_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200b}'
            | '\u{200c}'
            | '\u{200d}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'
            | '\u{202b}'
            | '\u{202c}'
            | '\u{202d}'
            | '\u{202e}'
            | '\u{2060}'
            | '\u{2061}'
            | '\u{2062}'
            | '\u{2063}'
            | '\u{2064}'
            | '\u{2066}'
            | '\u{2067}'
            | '\u{2068}'
            | '\u{2069}'
            | '\u{feff}'
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    for (unit, label) in [(GIB, "GiB"), (MIB, "MiB"), (KIB, "KiB")] {
        if bytes >= unit && bytes % unit == 0 {
            return format!("{} {label}", bytes / unit);
        }
    }
    format!("{bytes} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(direction: TtyFileTransferDirection) -> TtyFileTransferApprovalEvent {
        TtyFileTransferApprovalEvent {
            request_id: 7,
            transfer_id: "transfer-7".to_string(),
            direction: direction as i32,
            paths: Vec::new(),
            expires_in_ms: 60_000,
            max_files: 256,
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            max_session_bytes: 16 * 1024 * 1024 * 1024,
        }
    }

    fn context() -> TtyFileTransferPromptContext {
        TtyFileTransferPromptContext {
            pane_title: "Build pane".to_string(),
            daemon_hostname: "workstation".to_string(),
            daemon_session_id: "daemon-session-1".to_string(),
        }
    }

    #[test]
    fn send_prompt_discloses_late_paths_and_replacement_scope() {
        let prompt = format_prompt(&event(TtyFileTransferDirection::Send), &context()).unwrap();

        assert!(prompt.contains("File names and destinations are supplied only after approval"));
        assert!(prompt.contains("create or replace files anywhere"));
        assert!(prompt.contains("up to 256 files, 4 GiB per file, 16 GiB total"));
        assert!(prompt.contains("Pane: Build pane\r\nDaemon host: workstation"));
        assert!(prompt.contains("Daemon session: daemon-session-1"));
    }

    #[test]
    fn unknown_and_incomplete_events_fail_closed() {
        assert!(format_prompt(&event(TtyFileTransferDirection::Unspecified), &context()).is_none());

        let mut missing_expiry = event(TtyFileTransferDirection::Send);
        missing_expiry.expires_in_ms = 0;
        assert!(format_prompt(&missing_expiry, &context()).is_none());

        let mut inconsistent_send = event(TtyFileTransferDirection::Send);
        inconsistent_send.paths.push("/C:/unexpected".to_string());
        assert!(format_prompt(&inconsistent_send, &context()).is_none());
    }

    #[test]
    fn receive_paths_are_sanitized_and_bounded() {
        let mut receive = event(TtyFileTransferDirection::Receive);
        receive.paths = (0..10)
            .map(|index| format!("/C:/safe/{index}\r\nspoof\u{202e}\u{200f}"))
            .collect();
        let prompt = format_prompt(&receive, &context()).unwrap();

        assert!(!prompt.contains("spoof\r\n"));
        assert!(!prompt.contains('\u{202e}'));
        assert!(!prompt.contains('\u{200f}'));
        assert!(prompt.contains("safe/0��spoof��"));
        assert!(prompt.contains("and 2 more"));
    }
}
