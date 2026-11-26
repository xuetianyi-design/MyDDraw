//! Win32 windowing subsystem: host window mode management, owned presentation overlay,
//! logical<->physical coordinate mapping, and PE32 IAT / WndProc / WH_GETMESSAGE hooks.
#![deny(unsafe_code)]
#![allow(non_snake_case)]

mod dialog_template;
mod hook_logic;
#[allow(unsafe_code)]
pub mod hooks;
pub mod host;
pub mod input;
pub mod overlay;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use windows::Win32::Foundation::HWND;

pub(crate) use crate::win32::geometry::{client_rect, is_window};
use crate::win32::hwnd::{kill_timer, post_message_w, set_timer, window_thread_id};
use crate::win32::{current_thread_id, AtomicHwnd};
pub use host::{HostWindow, PresentationMode};
pub use overlay::OverlayWindow;

pub(crate) const WM_APP_FLUSH: u32 = 0x8000 + 0x4321;
pub(crate) const FLUSH_TIMER_ID: usize = 0x434E_4344;

static NEXT_BIND_ID: AtomicUsize = AtomicUsize::new(1);
static ACTIVE_BIND_ID: AtomicUsize = AtomicUsize::new(0);
static BOUND_HWND: AtomicHwnd = AtomicHwnd::new();
static PRESENT_HWND: AtomicHwnd = AtomicHwnd::new();
static BOUND_THREAD: AtomicU32 = AtomicU32::new(0);
static LOGICAL_WIDTH: AtomicU32 = AtomicU32::new(0);
static LOGICAL_HEIGHT: AtomicU32 = AtomicU32::new(0);
static LOGICAL_BPP: AtomicU32 = AtomicU32::new(0);
static MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static LOGICAL_SCALING_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn enable_dpi_awareness() {
    crate::win32::enable_dpi_awareness();
}

/// Computes the presentation viewport inside a `target_w x target_h` client area.
/// Shared by both `wgpu` rendering and mouse input mapping so clicks always align pixel-for-pixel.
pub fn compute_viewport(
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
    maintas: bool,
) -> (u32, u32, u32, u32) {
    if !maintas || width == 0 || height == 0 || target_w == 0 || target_h == 0 {
        return (0, 0, target_w.max(1), target_h.max(1));
    }
    let w = (target_h as u64 * width as u64 / height as u64).clamp(1, target_w as u64) as u32;
    let h = (target_w as u64 * height as u64 / width as u64).clamp(1, target_h as u64) as u32;
    ((target_w - w) / 2, (target_h - h) / 2, w, h)
}

pub(crate) fn logical_mode() -> Option<(HWND, u32, u32, u32)> {
    if !LOGICAL_SCALING_ACTIVE.load(Ordering::Relaxed) || !MODE_ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let hwnd = BOUND_HWND.load()?;
    let w = LOGICAL_WIDTH.load(Ordering::Relaxed);
    let h = LOGICAL_HEIGHT.load(Ordering::Relaxed);
    let bpp = LOGICAL_BPP.load(Ordering::Relaxed);
    if w == 0 || h == 0 {
        return None;
    }
    Some((hwnd, w, h, bpp))
}

pub(crate) fn bound_hwnd() -> Option<HWND> {
    BOUND_HWND.load()
}

pub(crate) fn bound_thread_id() -> u32 {
    BOUND_THREAD.load(Ordering::Relaxed)
}

/// Returns true when the caller is on the bound UI thread (or no window is bound yet).
pub fn is_ui_thread_or_unbound() -> bool {
    let bound = BOUND_THREAD.load(Ordering::Relaxed);
    bound == 0 || bound == current_thread_id()
}

pub(crate) fn present_hwnd() -> Option<HWND> {
    PRESENT_HWND.load()
}

pub(crate) fn is_scaling_active() -> bool {
    LOGICAL_SCALING_ACTIVE.load(Ordering::Relaxed)
}

pub fn set_present_hwnd(hwnd: HWND) {
    PRESENT_HWND.store(Some(hwnd));
}

pub fn clear_present_hwnd(hwnd: HWND) {
    PRESENT_HWND.clear_if(hwnd);
}

pub(crate) fn is_present_hwnd(hwnd: HWND) -> bool {
    PRESENT_HWND.matches(hwnd)
}

pub fn bind_window(hwnd: HWND, scaling_active: bool) -> usize {
    if hwnd.is_invalid() {
        return 0;
    }
    let id = NEXT_BIND_ID.fetch_add(1, Ordering::Relaxed);
    let tid = current_thread_id();
    let wnd_tid = window_thread_id(hwnd);
    if crate::compat::is_logging_enabled() {
        let cfg = crate::config::get();
        crate::compat::log_msg(&format!(
            "bind_window hwnd={:?} call_tid={} wnd_tid={} scaling={} devmode={} key1=0x{:X} key2=0x{:X}",
            hwnd.0, tid, wnd_tid, scaling_active, cfg.devmode, cfg.unlock_cursor1_vk, cfg.unlock_cursor2_vk
        ));
    }
    ACTIVE_BIND_ID.store(id, Ordering::Relaxed);
    BOUND_HWND.store(Some(hwnd));
    BOUND_THREAD.store(tid, Ordering::Relaxed);
    LOGICAL_SCALING_ACTIVE.store(scaling_active, Ordering::Relaxed);
    MODE_ACTIVE.store(false, Ordering::Relaxed);
    hooks::install_iat_hooks();
    hooks::install_subclass(hwnd);
    hooks::install_getmsg_hook(tid);
    id
}

pub fn set_mode(width: u32, height: u32, bpp: u32, active: bool) {
    LOGICAL_WIDTH.store(width, Ordering::Relaxed);
    LOGICAL_HEIGHT.store(height, Ordering::Relaxed);
    LOGICAL_BPP.store(bpp, Ordering::Relaxed);
    if BOUND_HWND.load().is_some() {
        MODE_ACTIVE.store(active, Ordering::Relaxed);
        if active {
            hook_logic::sync_cursor_lock();
        } else {
            hook_logic::unlock_cursor(false);
        }
    }
}

/// Keeps logical dimensions aligned with the exact primary surface dimensions presented by wgpu.
pub fn sync_presented_size(width: u32, height: u32) {
    if width > 0 && height > 0 && BOUND_HWND.load().is_some() {
        LOGICAL_WIDTH.store(width, Ordering::Relaxed);
        LOGICAL_HEIGHT.store(height, Ordering::Relaxed);
        if LOGICAL_SCALING_ACTIVE.load(Ordering::Relaxed) {
            MODE_ACTIVE.store(true, Ordering::Relaxed);
            hook_logic::sync_cursor_lock();
        }
    }
}

pub fn sync_cursor_lock() {
    hook_logic::sync_cursor_lock();
}

pub fn set_scaling_active(bind_id: usize, active: bool) {
    if bind_id == 0 || ACTIVE_BIND_ID.load(Ordering::Relaxed) != bind_id {
        return;
    }
    LOGICAL_SCALING_ACTIVE.store(active, Ordering::Relaxed);
    MODE_ACTIVE.store(false, Ordering::Relaxed);
}

pub fn is_active_owner(bind_id: usize) -> bool {
    bind_id != 0 && ACTIVE_BIND_ID.load(Ordering::Relaxed) == bind_id
}

pub fn unbind_window(bind_id: usize) {
    if bind_id == 0
        || ACTIVE_BIND_ID
            .compare_exchange(bind_id, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    MODE_ACTIVE.store(false, Ordering::Relaxed);
    LOGICAL_SCALING_ACTIVE.store(false, Ordering::Relaxed);
    hook_logic::reset_cursor_lock_state();
    BOUND_HWND.store(None);
    BOUND_THREAD.store(0, Ordering::Relaxed);
    hooks::uninstall_getmsg_hook();
    hooks::uninstall_subclass();
    hooks::uninstall_iat_hooks();
}

fn flush_window() -> Option<HWND> {
    hooks::subclassed_hwnd().filter(|&hwnd| is_window(hwnd))
}

pub fn can_schedule_flush() -> bool {
    flush_window().is_some()
}

pub fn schedule_flush() -> bool {
    let Some(hwnd) = flush_window() else {
        return false;
    };
    post_message_w(hwnd, WM_APP_FLUSH, Default::default(), Default::default())
}

pub fn arm_flush_timer(ms: u32) {
    if let Some(hwnd) = flush_window() {
        set_timer(hwnd, FLUSH_TIMER_ID, ms.max(1));
    }
}

pub fn disarm_flush_timer() {
    if let Some(hwnd) = flush_window() {
        kill_timer(hwnd, FLUSH_TIMER_ID);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use super::*;

    #[test]
    fn viewport_aspect_ratio_and_stretch() {
        assert_eq!(
            compute_viewport(640, 480, 1920, 1080, true),
            (240, 0, 1440, 1080)
        );
        assert_eq!(
            compute_viewport(640, 480, 1920, 1080, false),
            (0, 0, 1920, 1080)
        );
        assert_eq!(compute_viewport(640, 480, 640, 480, true), (0, 0, 640, 480));
        assert_eq!(compute_viewport(640, 480, 1, 99, true), (0, 49, 1, 1));
    }

    #[test]
    fn stale_unbind_and_present_clear_do_not_clobber_newer_binding() {
        let old_present = HWND(0x1001 as *mut c_void);
        let new_present = HWND(0x2002 as *mut c_void);
        set_present_hwnd(old_present);
        set_present_hwnd(new_present);
        clear_present_hwnd(old_present);
        assert_eq!(present_hwnd(), Some(new_present));
        clear_present_hwnd(new_present);
        assert_eq!(present_hwnd(), None);

        let old_id = NEXT_BIND_ID.fetch_add(1, Ordering::Relaxed);
        let new_id = NEXT_BIND_ID.fetch_add(1, Ordering::Relaxed);
        ACTIVE_BIND_ID.store(new_id, Ordering::Relaxed);
        assert!(!is_active_owner(old_id));
        assert!(is_active_owner(new_id));
        unbind_window(old_id);
        assert!(is_active_owner(new_id));
        ACTIVE_BIND_ID.store(0, Ordering::Relaxed);
    }
}
