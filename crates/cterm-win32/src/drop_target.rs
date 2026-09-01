//! Native Win32 file-drop adapter for Kitty's OSC 72 protocol.
//!
//! The COM target and `CF_HDROP` extraction are adapted from Tao's tested
//! Windows drop handler, with client-coordinate and retained-data behavior
//! informed by Baseview. Protocol negotiation remains in `cterm-app`.

// Copyright 2014-2021 The winit contributors
// Copyright 2021-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;

use cterm_app::kitty_dnd::MAX_LOCAL_DROP_BYTES;
use url::Url;
use windows::core::{implement, Ref};
use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWLP_USERDATA};

use crate::window::WindowState;

const MAX_DROPPED_PATHS: u32 = 4096;
const MAX_DROPPED_PATH_CODE_UNITS: usize = 32_768;
const MAX_DROPPED_PATH_CODE_UNITS_TOTAL: usize = MAX_LOCAL_DROP_BYTES / 6;

/// Owns OLE initialization and the COM target registered for one native window.
pub(crate) struct DropRegistration {
    hwnd: HWND,
    _target: IDropTarget,
}

impl DropRegistration {
    pub(crate) fn register(hwnd: HWND) -> Option<Self> {
        if let Err(error) = unsafe { OleInitialize(None) } {
            log::warn!("Kitty DND disabled: OLE initialization failed: {error}");
            return None;
        }

        let target: IDropTarget = CtermDropTarget::new(hwnd).into();
        if let Err(error) = unsafe { RegisterDragDrop(hwnd, &target) } {
            log::warn!("Kitty DND disabled: drop-target registration failed: {error}");
            unsafe { OleUninitialize() };
            return None;
        }

        Some(Self {
            hwnd,
            _target: target,
        })
    }
}

impl Drop for DropRegistration {
    fn drop(&mut self) {
        if let Err(error) = unsafe { RevokeDragDrop(self.hwnd) } {
            log::warn!("Failed to revoke Win32 drop target: {error}");
        }
        unsafe { OleUninitialize() };
    }
}

struct StgMediumGuard(STGMEDIUM);

impl Drop for StgMediumGuard {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

#[implement(IDropTarget)]
struct CtermDropTarget {
    hwnd: HWND,
    paths: RefCell<Vec<PathBuf>>,
    source_id: Cell<Option<u64>>,
    copy_allowed: Cell<bool>,
}

impl CtermDropTarget {
    fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            paths: RefCell::new(Vec::new()),
            source_id: Cell::new(None),
            copy_allowed: Cell::new(false),
        }
    }

    fn with_window_state<R>(&self, callback: impl FnOnce(&mut WindowState) -> R) -> Option<R> {
        let state = unsafe { GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) } as *mut WindowState;
        if state.is_null() {
            None
        } else {
            Some(callback(unsafe { &mut *state }))
        }
    }

    fn client_point(&self, point: &POINTL) -> Option<POINT> {
        let mut point = POINT {
            x: point.x,
            y: point.y,
        };
        unsafe { ScreenToClient(self.hwnd, &mut point) }
            .as_bool()
            .then_some(point)
    }

    fn leave(&self) {
        if let Some(source_id) = self.source_id.take() {
            let _ = self.with_window_state(|state| state.native_dnd_left(source_id));
        }
        self.paths.borrow_mut().clear();
    }

    fn update_hover(&self, point: &POINTL) -> bool {
        if !self.copy_allowed.get() || self.paths.borrow().is_empty() {
            self.leave();
            return false;
        }
        let Some(point) = self.client_point(point) else {
            self.leave();
            return false;
        };
        let previous = self.source_id.get();
        let Some((source_id, accepted)) =
            self.with_window_state(|state| state.native_dnd_moved(point.x, point.y, previous))
        else {
            self.source_id.set(None);
            return false;
        };
        self.source_id.set(source_id);
        accepted
    }

    fn source_allows_copy(effect: *mut DROPEFFECT) -> bool {
        !effect.is_null() && unsafe { effect.read() }.contains(DROPEFFECT_COPY)
    }

    fn set_effect(effect: *mut DROPEFFECT, accepted: bool) {
        if !effect.is_null() {
            unsafe {
                effect.write(if accepted {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                });
            }
        }
    }

    fn parse_paths(data: Ref<'_, IDataObject>) -> Vec<PathBuf> {
        let Some(data) = data.as_ref() else {
            return Vec::new();
        };
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let Ok(medium) = (unsafe { data.GetData(&format) }) else {
            return Vec::new();
        };
        let medium = StgMediumGuard(medium);
        let hdrop = HDROP(unsafe { medium.0.u.hGlobal.0 } as _);

        let mut empty = [];
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, Some(&mut empty)) };
        if count == 0 || count > MAX_DROPPED_PATHS {
            return Vec::new();
        }

        let mut paths = Vec::with_capacity(count as usize);
        let mut total_code_units = 0_usize;
        for index in 0..count {
            let code_units = unsafe { DragQueryFileW(hdrop, index, Some(&mut empty)) } as usize;
            if code_units == 0 || code_units > MAX_DROPPED_PATH_CODE_UNITS {
                return Vec::new();
            }
            let Some(next_total) = total_code_units.checked_add(code_units) else {
                return Vec::new();
            };
            if next_total > MAX_DROPPED_PATH_CODE_UNITS_TOTAL {
                return Vec::new();
            }
            total_code_units = next_total;
            let mut buffer = vec![0_u16; code_units + 1];
            let written = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) } as usize;
            if written == 0 || written > code_units {
                return Vec::new();
            }
            paths.push(OsString::from_wide(&buffer[..written]).into());
        }
        paths
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for CtermDropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.leave();
        self.copy_allowed
            .set(CtermDropTarget::source_allows_copy(effect));
        self.paths.replace(CtermDropTarget::parse_paths(data));
        CtermDropTarget::set_effect(effect, self.update_hover(point));
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        CtermDropTarget::set_effect(effect, self.update_hover(point));
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        self.leave();
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.paths.replace(CtermDropTarget::parse_paths(data));
        let accepted = self.update_hover(point);
        let source_id = self.source_id.get();
        let payload = accepted.then(|| uri_list(&self.paths.borrow())).flatten();
        let dropped = match (source_id, self.client_point(point), payload) {
            (Some(source_id), Some(point), Some(payload)) => self
                .with_window_state(|state| {
                    state.native_dnd_drop(source_id, point.x, point.y, payload)
                })
                .unwrap_or(false),
            _ => false,
        };
        if !dropped {
            self.leave();
        } else {
            self.source_id.set(None);
            self.paths.borrow_mut().clear();
        }
        CtermDropTarget::set_effect(effect, dropped);
        Ok(())
    }
}

fn uri_list(paths: &[PathBuf]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    for path in paths {
        let url = path_to_file_url(path)?;
        let extra = url.as_str().len() + usize::from(!payload.is_empty()) * 2;
        if payload.len().checked_add(extra)? > MAX_LOCAL_DROP_BYTES {
            return None;
        }
        if !payload.is_empty() {
            payload.extend_from_slice(b"\r\n");
        }
        payload.extend_from_slice(url.as_str().as_bytes());
    }
    (!payload.is_empty()).then_some(payload)
}

fn path_to_file_url(path: &Path) -> Option<Url> {
    if path.is_dir() {
        Url::from_directory_path(path).ok()
    } else {
        Url::from_file_path(path).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_use_file_urls_crlf_and_percent_encoding() {
        let payload = uri_list(&[
            PathBuf::from(r"C:\Program Files\cterm.txt"),
            PathBuf::from(r"D:\more.txt"),
        ])
        .unwrap();

        assert_eq!(
            String::from_utf8(payload).unwrap(),
            "file:///C:/Program%20Files/cterm.txt\r\nfile:///D:/more.txt"
        );
    }

    #[test]
    fn empty_uri_lists_are_rejected() {
        assert!(uri_list(&[]).is_none());
    }
}
