//! HWND geometry, monitor, and cursor queries with Rust-owned output storage.
use std::mem::size_of;

use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, EnumDisplaySettingsW, GetMonitorInfoW, MapWindowPoints, MonitorFromWindow,
    ScreenToClient, DEVMODEW, ENUM_CURRENT_SETTINGS, MONITORINFO, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, ClipCursor, GetClientRect, GetCursorPos, GetWindowRect, IsWindow,
    SetCursorPos, WindowFromPoint, WINDOW_EX_STYLE, WINDOW_STYLE,
};

pub(crate) fn is_window(hwnd: HWND) -> bool {
    // SAFETY: IsWindow accepts stale or invalid handles.
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

pub(crate) fn client_rect(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid, aligned writable RECT.
    unsafe { GetClientRect(hwnd, &mut rect)? };
    Ok(rect)
}

pub(crate) fn window_rect(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid, aligned writable RECT.
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    Ok(rect)
}

pub(crate) fn client_to_screen(hwnd: HWND, point: &mut POINT) -> bool {
    // SAFETY: `point` is a valid mutable reference to a POINT.
    unsafe { ClientToScreen(hwnd, point).as_bool() }
}

pub(crate) fn screen_to_client(hwnd: HWND, point: &mut POINT) -> bool {
    // SAFETY: `point` is a valid mutable reference to a POINT.
    unsafe { ScreenToClient(hwnd, point).as_bool() }
}

pub(crate) fn map_rect(from: Option<HWND>, to: Option<HWND>, rect: RECT) -> RECT {
    // Passing exactly two corners preserves MapWindowPoints' mirrored-window RECT handling.
    let mut corners = [
        POINT {
            x: rect.left,
            y: rect.top,
        },
        POINT {
            x: rect.right,
            y: rect.bottom,
        },
    ];
    // SAFETY: `corners` is a valid 2-element POINT slice.
    unsafe { MapWindowPoints(from, to, &mut corners) };
    RECT {
        left: corners[0].x,
        top: corners[0].y,
        right: corners[1].x,
        bottom: corners[1].y,
    }
}

pub(crate) fn monitor_info(hwnd: HWND) -> Result<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info.cbSize` is initialized and `MonitorFromWindow` always returns a valid handle
    // with `MONITOR_DEFAULTTONEAREST` when any display exists.
    unsafe {
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).ok()?;
    }
    Ok(info)
}

pub(crate) fn adjust_window_rect_ex(
    rect: &mut RECT,
    style: WINDOW_STYLE,
    has_menu: bool,
    ex_style: WINDOW_EX_STYLE,
) {
    // SAFETY: `rect` is a valid mutable RECT reference.
    unsafe {
        let _ = AdjustWindowRectEx(rect, style, has_menu, ex_style);
    }
}

pub(crate) fn cursor_pos() -> Result<POINT> {
    let mut pt = POINT::default();
    // SAFETY: `pt` is a valid writable POINT.
    unsafe { GetCursorPos(&mut pt)? };
    Ok(pt)
}

pub(crate) fn set_cursor_pos(x: i32, y: i32) -> bool {
    // SAFETY: SetCursorPos accepts integer screen coordinates.
    unsafe { SetCursorPos(x, y).is_ok() }
}

pub(crate) fn clip_cursor(rect: Option<&RECT>) -> bool {
    // SAFETY: `rect` is either None or a valid shared reference to a RECT.
    unsafe { ClipCursor(rect.map(|r| r as *const RECT)).is_ok() }
}

pub(crate) fn window_from_point(pt: POINT) -> HWND {
    // SAFETY: WindowFromPoint takes a POINT by value and returns a handle or NULL.
    unsafe { WindowFromPoint(pt) }
}

pub(crate) fn get_capture() -> HWND {
    // SAFETY: GetCapture takes no arguments.
    unsafe { GetCapture() }
}

pub(crate) fn is_key_down(vk: i32) -> bool {
    // SAFETY: GetAsyncKeyState takes a virtual-key code and returns a bitmask.
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// Queries the current display refresh rate (Hz) of the monitor hosting `hwnd` (or primary monitor).
pub(crate) fn monitor_refresh_hz(hwnd: HWND) -> Option<u32> {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let mut monitor = MONITORINFOEXW::default();
    let device_name = if hwnd.is_invalid() {
        PCWSTR::null()
    } else {
        monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `cbSize` is initialized and `monitorInfo` is the leading field of `MONITORINFOEXW`.
        unsafe {
            GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                &mut monitor.monitorInfo,
            )
            .ok()
            .ok()?;
        }
        PCWSTR(monitor.szDevice.as_ptr())
    };
    // SAFETY: `device_name` is NULL or points into the NUL-terminated `monitor.szDevice`, and
    // `mode.dmSize` is initialized.
    let ok = unsafe { EnumDisplaySettingsW(device_name, ENUM_CURRENT_SETTINGS, &mut mode) };
    (ok.as_bool() && mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
}
