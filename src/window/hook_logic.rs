//! Pure Rust decision logic for Win32 API hooks, `WH_GETMESSAGE`, and host WndProc subclassing.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BITSPIXEL, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_MENU, VK_RMENU};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_NOTOPMOST, HWND_TOPMOST, MSG, SET_WINDOW_POS_FLAGS, SM_CXSCREEN, SM_CYSCREEN, WM_ACTIVATE,
    WM_ACTIVATEAPP, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEWHEEL, WM_MOVE, WM_NCACTIVATE, WM_NULL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SIZE, WM_SYSCHAR, WM_SYSKEYDOWN, WM_TIMER, WM_WINDOWPOSCHANGED,
};

use crate::config;
use crate::win32::geometry::{
    client_rect, client_to_screen, clip_cursor, cursor_pos, get_capture, is_key_down, map_rect,
    screen_to_client, set_cursor_pos, window_from_point, window_rect,
};
use crate::win32::hwnd::{
    bring_window_to_top, desktop_window, foreground_window, is_iconic, kill_timer, move_window,
    set_window_pos,
};

use super::input::{
    client_origin_in_bound, client_to_logical, hit_test_logical_child, is_mouse_message,
    is_related_dialog_window, logical_to_client,
};
use super::overlay::ensure_present_on_top_if_active;
use super::{
    bound_hwnd, compute_viewport, is_present_hwnd, is_scaling_active, logical_mode, present_hwnd,
    FLUSH_TIMER_ID, WM_APP_FLUSH,
};

static CURSOR_LOCKED: AtomicBool = AtomicBool::new(false);
static CURSOR_MANUALLY_UNLOCKED: AtomicBool = AtomicBool::new(false);
static LAST_CLIP_RECT: [AtomicI32; 4] = [
    AtomicI32::new(0),
    AtomicI32::new(0),
    AtomicI32::new(0),
    AtomicI32::new(0),
];

fn same_last_clip_rect(rc: RECT) -> bool {
    LAST_CLIP_RECT[0].load(Ordering::Relaxed) == rc.left
        && LAST_CLIP_RECT[1].load(Ordering::Relaxed) == rc.top
        && LAST_CLIP_RECT[2].load(Ordering::Relaxed) == rc.right
        && LAST_CLIP_RECT[3].load(Ordering::Relaxed) == rc.bottom
}

fn store_last_clip_rect(rc: RECT) {
    LAST_CLIP_RECT[0].store(rc.left, Ordering::Relaxed);
    LAST_CLIP_RECT[1].store(rc.top, Ordering::Relaxed);
    LAST_CLIP_RECT[2].store(rc.right, Ordering::Relaxed);
    LAST_CLIP_RECT[3].store(rc.bottom, Ordering::Relaxed);
}

fn viewport_screen_rect(bound: HWND, lw: u32, lh: u32) -> Option<RECT> {
    let client = client_rect(bound).ok()?;
    if client.right <= 0 || client.bottom <= 0 {
        return None;
    }
    let (vx, vy, vw, vh) = compute_viewport(
        lw,
        lh,
        client.right as u32,
        client.bottom as u32,
        config::get().maintas,
    );
    let rc = RECT {
        left: vx as i32,
        top: vy as i32,
        right: (vx + vw) as i32,
        bottom: (vy + vh) as i32,
    };
    Some(map_rect(Some(bound), None, rc))
}

pub(super) fn sync_cursor_lock() {
    if config::get().devmode || CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed) {
        return;
    }
    let Some((bound, w, h, _)) = logical_mode() else {
        return;
    };
    let fg = foreground_window();
    if is_iconic(bound) || fg != bound {
        if crate::compat::is_logging_enabled() {
            crate::compat::log_msg(&format!(
                "sync_cursor_lock skipped: iconic={} fg={:?} bound={:?}",
                is_iconic(bound),
                fg.0,
                bound.0
            ));
        }
        return;
    }
    let Some(screen_rc) = viewport_screen_rect(bound, w, h) else {
        return;
    };
    if CURSOR_LOCKED.load(Ordering::Relaxed) && same_last_clip_rect(screen_rc) {
        return;
    }
    let ok = clip_cursor(Some(&screen_rc));
    if crate::compat::is_logging_enabled() {
        crate::compat::log_msg(&format!(
            "sync_cursor_lock clip_cursor({},{},{},{}) -> {}",
            screen_rc.left, screen_rc.top, screen_rc.right, screen_rc.bottom, ok
        ));
    }
    if ok {
        store_last_clip_rect(screen_rc);
        CURSOR_LOCKED.store(true, Ordering::Relaxed);
    }
}

pub(super) fn unlock_cursor(manual: bool) {
    if manual {
        CURSOR_MANUALLY_UNLOCKED.store(true, Ordering::Relaxed);
    }
    let was_locked = CURSOR_LOCKED.swap(false, Ordering::Relaxed);
    if was_locked || manual {
        let ok = clip_cursor(None);
        if crate::compat::is_logging_enabled() {
            crate::compat::log_msg(&format!(
                "unlock_cursor(manual={manual}) was_locked={was_locked} clip_cursor(None) -> {ok}"
            ));
        }
    }
}

pub(super) fn reset_cursor_lock_state() {
    CURSOR_MANUALLY_UNLOCKED.store(false, Ordering::Relaxed);
    if CURSOR_LOCKED.swap(false, Ordering::Relaxed) {
        let _ = clip_cursor(None);
    }
}

pub(super) fn toggle_cursor_lock() {
    if config::get().devmode {
        return;
    }
    let was_unlocked = CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed);
    if crate::compat::is_logging_enabled() {
        crate::compat::log_msg(&format!("toggle_cursor_lock was_unlocked={was_unlocked}"));
    }
    if was_unlocked {
        CURSOR_MANUALLY_UNLOCKED.store(false, Ordering::Relaxed);
        sync_cursor_lock();
    } else {
        unlock_cursor(true);
    }
}

fn maybe_relock_on_client_mouse_up() {
    if !config::get().devmode && CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed) {
        CURSOR_MANUALLY_UNLOCKED.store(false, Ordering::Relaxed);
        sync_cursor_lock();
    }
}

fn is_unlock_hotkey_pressed(vk: u32) -> bool {
    let cfg = config::get();
    if cfg.unlock_cursor1_vk != 0
        && (vk == cfg.unlock_cursor1_vk || vk == VK_CONTROL.0 as u32)
        && is_key_down(VK_CONTROL.0 as i32)
        && is_key_down(cfg.unlock_cursor1_vk as i32)
    {
        return true;
    }
    if cfg.unlock_cursor2_vk != 0
        && (vk == cfg.unlock_cursor2_vk || vk == VK_MENU.0 as u32)
        && is_key_down(VK_RMENU.0 as i32)
        && is_key_down(cfg.unlock_cursor2_vk as i32)
    {
        return true;
    }
    false
}

pub(super) fn virtual_window_rect(hwnd: HWND, include_dialogs: bool) -> Option<RECT> {
    let (bound, w, h, _) = logical_mode()?;
    if hwnd == bound || hwnd == desktop_window() {
        return Some(RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        });
    }
    if include_dialogs && is_related_dialog_window(bound, hwnd) {
        return Some(map_rect(None, Some(bound), window_rect(hwnd).ok()?));
    }
    None
}

pub(super) fn virtual_system_metrics(index: i32) -> Option<i32> {
    let (_, w, h, _) = logical_mode()?;
    if index == SM_CXSCREEN.0 {
        return Some(w as i32);
    }
    if index == SM_CYSCREEN.0 {
        return Some(h as i32);
    }
    None
}

pub(super) fn virtual_client_to_screen(hwnd: HWND, pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    if hwnd == bound {
        return Some(true);
    }
    if is_related_dialog_window(bound, hwnd) {
        return Some(client_to_screen(hwnd, pt) && screen_to_client(bound, pt));
    }
    None
}

pub(super) fn virtual_screen_to_client(hwnd: HWND, pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    if hwnd == bound {
        return Some(true);
    }
    if is_related_dialog_window(bound, hwnd) {
        return Some(client_to_screen(bound, pt) && screen_to_client(hwnd, pt));
    }
    None
}

pub(super) fn virtual_cursor_pos() -> Option<POINT> {
    let mut raw = cursor_pos().ok()?;
    if let Some((bound, w, h, _)) = logical_mode() {
        if screen_to_client(bound, &mut raw) {
            let (lx, ly) = client_to_logical(bound, raw.x, raw.y, w, h);
            raw = POINT { x: lx, y: ly };
        }
    }
    Some(raw)
}

pub(super) fn set_virtual_cursor_pos(x: i32, y: i32) -> bool {
    if let Some((bound, w, h, _)) = logical_mode() {
        let (cx, cy) = logical_to_client(bound, x, y, w, h);
        let mut pt = POINT { x: cx, y: cy };
        if client_to_screen(bound, &mut pt) {
            return set_cursor_pos(pt.x, pt.y);
        }
    }
    set_cursor_pos(x, y)
}

pub(super) fn clip_virtual_cursor(rect: Option<&RECT>) -> bool {
    let devmode = config::get().devmode;
    if !devmode && CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed) {
        return true;
    }
    if let Some((bound, w, h, _)) = logical_mode() {
        let Some(r) = rect else {
            if !devmode {
                sync_cursor_lock();
                return true;
            }
            return clip_cursor(None);
        };
        let (l, t) = logical_to_client(bound, r.left, r.top, w, h);
        let (ri, b) = logical_to_client(
            bound,
            r.right.saturating_sub(1),
            r.bottom.saturating_sub(1),
            w,
            h,
        );
        let mapped = map_rect(
            Some(bound),
            None,
            RECT {
                left: l,
                top: t,
                right: ri + 1,
                bottom: b + 1,
            },
        );
        let ok = clip_cursor(Some(&mapped));
        if ok {
            store_last_clip_rect(mapped);
            CURSOR_LOCKED.store(true, Ordering::Relaxed);
        }
        return ok;
    }
    let ok = clip_cursor(rect);
    if ok {
        CURSOR_LOCKED.store(rect.is_some(), Ordering::Relaxed);
    }
    ok
}

pub(super) fn virtual_clip_cursor() -> Option<RECT> {
    let (_, w, h, _) = logical_mode()?;
    Some(RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    })
}

pub(super) fn virtual_window_from_point(point: POINT) -> HWND {
    if let Some((bound, _, _, _)) = logical_mode() {
        return hit_test_logical_child(bound, point.x, point.y).unwrap_or(bound);
    }
    window_from_point(point)
}

pub(super) fn handle_set_window_pos(
    hwnd: HWND,
    insert_after: HWND,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    flags: u32,
) -> bool {
    if is_scaling_active() && bound_hwnd() == Some(hwnd) {
        return true;
    }
    let effective_insert_after = if insert_after == HWND_TOPMOST
        && bound_hwnd().is_some_and(|b| hwnd == b || is_related_dialog_window(b, hwnd))
    {
        HWND_NOTOPMOST
    } else {
        insert_after
    };
    let ok = set_window_pos(
        hwnd,
        Some(effective_insert_after),
        x,
        y,
        cx,
        cy,
        SET_WINDOW_POS_FLAGS(flags),
    )
    .is_ok();
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && is_related_dialog_window(bound, hwnd) {
            ensure_present_on_top_if_active(bound);
        }
    }
    ok
}

pub(super) fn handle_bring_window_to_top(hwnd: HWND) -> bool {
    let ok = bring_window_to_top(hwnd);
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && (hwnd == bound || is_related_dialog_window(bound, hwnd)) {
            ensure_present_on_top_if_active(bound);
        }
    }
    ok
}

pub(super) fn handle_move_window(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    repaint: bool,
) -> bool {
    if is_scaling_active() && bound_hwnd() == Some(hwnd) {
        return true;
    }
    move_window(hwnd, x, y, width, height, repaint)
}

pub(super) fn virtual_device_caps(index: i32) -> Option<i32> {
    if bound_hwnd().is_some() && (index == LOGPIXELSX.0 as i32 || index == LOGPIXELSY.0 as i32) {
        return Some(96);
    }
    let (_, w, h, bpp) = logical_mode()?;
    if index == BITSPIXEL.0 as i32 && bpp > 0 {
        return Some(bpp as i32);
    }
    if index == HORZRES.0 as i32 {
        return Some(w as i32);
    }
    if index == VERTRES.0 as i32 {
        return Some(h as i32);
    }
    None
}

pub(super) fn rewrite_getmsg(msg: &mut MSG, pm_remove: bool) {
    if pm_remove && matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN) {
        let was_down = (msg.lParam.0 as u32) & (1 << 30) != 0;
        let vk = msg.wParam.0 as u32;
        let cfg = config::get();
        if !was_down && !cfg.devmode && is_unlock_hotkey_pressed(vk) {
            if crate::compat::is_logging_enabled() {
                crate::compat::log_msg(&format!(
                    "getmsg unlock hotkey vk=0x{:X} ctrl={} rmenu={}",
                    vk,
                    is_key_down(VK_CONTROL.0 as i32),
                    is_key_down(VK_RMENU.0 as i32)
                ));
            }
            toggle_cursor_lock();
            msg.message = WM_NULL;
            return;
        }
    }
    let Some((bound, w, h, _)) = logical_mode() else {
        return;
    };
    let is_bound = msg.hwnd == bound;
    let is_dialog = is_related_dialog_window(bound, msg.hwnd);
    if !is_bound && !is_dialog {
        return;
    }
    let Ok(mut cursor) = cursor_pos() else {
        return;
    };
    if !screen_to_client(bound, &mut cursor) {
        return;
    }
    let (lx, ly) = client_to_logical(bound, cursor.x, cursor.y, w, h);
    msg.pt = POINT { x: lx, y: ly };
    if !is_mouse_message(msg.message) {
        return;
    }
    if matches!(msg.message, WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP) {
        maybe_relock_on_client_mouse_up();
    }
    if matches!(
        msg.message,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
    ) {
        ensure_present_on_top_if_active(bound);
    }
    let captured = get_capture();
    let target = if captured == msg.hwnd || is_bound {
        msg.hwnd
    } else {
        hit_test_logical_child(bound, lx, ly).unwrap_or(bound)
    };
    msg.hwnd = target;
    if msg.message == WM_MOUSEWHEEL || target == bound {
        msg.lParam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
    } else {
        let (tx, ty) = client_origin_in_bound(bound, target);
        let cx = lx - tx;
        let cy = ly - ty;
        msg.lParam = LPARAM(((cy as u16 as u32) << 16 | (cx as u16 as u32)) as isize);
    }
}

pub(super) enum SubclassAction {
    Return(LRESULT),
    DefWindowProc,
    Forward(LPARAM),
}

pub(super) fn handle_subclass_message(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    mut lparam: LPARAM,
    activate_passed: &AtomicBool,
    activateapp_passed: &AtomicBool,
) -> SubclassAction {
    if msg == WM_APP_FLUSH {
        crate::draw::flush_pending_present(false);
        return SubclassAction::Return(LRESULT(0));
    }
    if msg == WM_TIMER && wparam.0 == FLUSH_TIMER_ID {
        kill_timer(hwnd, FLUSH_TIMER_ID);
        crate::draw::flush_pending_present(true);
        return SubclassAction::Return(LRESULT(0));
    }
    let toggle_vk = config::get().toggle_fullscreen_vk;
    if toggle_vk != 0 {
        let alt_down = (lparam.0 as u32) & (1 << 29) != 0;
        let was_down = (lparam.0 as u32) & (1 << 30) != 0;
        if msg == WM_SYSKEYDOWN && wparam.0 as u32 == toggle_vk && alt_down {
            if !was_down {
                crate::draw::toggle_fullscreen_hotkey();
            }
            return SubclassAction::Return(LRESULT(0));
        }
        if msg == WM_SYSCHAR && wparam.0 as u32 == toggle_vk && alt_down {
            // Suppress default Windows Alt+Enter system beep.
            return SubclassAction::Return(LRESULT(0));
        }
    }
    let shield_focus = is_scaling_active()
        && (config::get().noactivateapp || !crate::draw::is_fullscreen_active());
    if shield_focus && matches!(msg, WM_KILLFOCUS | WM_NCACTIVATE) {
        return SubclassAction::DefWindowProc;
    }
    if msg == WM_ACTIVATE && shield_focus {
        let active = (wparam.0 & 0xffff) != 0;
        if !activate_passed.swap(true, Ordering::Relaxed) && active {
            // Allow the initial activation through once so the game initializes its focus flag.
        } else {
            return SubclassAction::Return(LRESULT(0));
        }
    }
    if msg == WM_ACTIVATEAPP {
        if wparam.0 == 0 {
            unlock_cursor(false);
        } else {
            crate::draw::on_host_window_changed();
            sync_cursor_lock();
        }
        if shield_focus {
            if !activateapp_passed.swap(true, Ordering::Relaxed) && wparam.0 != 0 {
                // Allow initial activation once.
            } else {
                return SubclassAction::Return(LRESULT(0));
            }
        }
    }
    if matches!(msg, WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGED) && bound_hwnd() == Some(hwnd) {
        crate::draw::on_host_window_changed();
        sync_cursor_lock();
    }
    // Only remap physical coordinates on `bound` when no owned presentation overlay exists;
    // when `PRESENT_HWND` is active, `present_wndproc` and `getmsg_hook` deliver logical coords.
    if is_mouse_message(msg) && present_hwnd().is_none() {
        if matches!(msg, WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP) {
            maybe_relock_on_client_mouse_up();
        }
        if let Some((bound, w, h, _)) = logical_mode() {
            if hwnd == bound {
                let raw = lparam.0 as u32;
                let mut px = (raw & 0xffff) as i16 as i32;
                let mut py = ((raw >> 16) & 0xffff) as i16 as i32;
                if msg == WM_MOUSEWHEEL {
                    let mut pt = POINT { x: px, y: py };
                    if screen_to_client(bound, &mut pt) {
                        px = pt.x;
                        py = pt.y;
                    }
                }
                let (lx, ly) = client_to_logical(bound, px, py, w, h);
                lparam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
            }
        }
    }
    SubclassAction::Forward(lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_cursor_lock_flips_manual_unlock_flag() {
        reset_cursor_lock_state();
        assert!(!CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed));

        toggle_cursor_lock();
        assert!(CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed));
        assert!(!CURSOR_LOCKED.load(Ordering::Relaxed));

        toggle_cursor_lock();
        assert!(!CURSOR_MANUALLY_UNLOCKED.load(Ordering::Relaxed));

        reset_cursor_lock_state();
    }
}
