//! Owned Win32 overlay window used as the wgpu swapchain surface so underlying Win32 child
//! and popup game dialogs keep receiving WM_PAINT / WM_DRAWITEM unmodified.
#![forbid(unsafe_code)]

use std::num::NonZeroIsize;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_NOTOPMOST, HWND_TOP, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::win32::current_module_handle;
use crate::win32::geometry::{client_rect, client_to_screen, is_window};
use crate::win32::hwnd::{
    create_window_ex_w, destroy_window, ex_style, foreground_window, is_iconic, is_visible,
    prev_siblings, register_overlay_class, set_window_pos, SendHwnd,
};

use super::input::{is_related_dialog_window, is_same_process_window, present_wndproc};
use super::{clear_present_hwnd, present_hwnd, set_present_hwnd};

pub struct OverlayWindow {
    owner: SendHwnd,
    hwnd: SendHwnd,
    hinstance: NonZeroIsize,
    last_rect: Option<(i32, i32, u32, u32)>,
    visible: bool,
}

impl OverlayWindow {
    pub fn new(owner: HWND) -> Result<Self, String> {
        let module =
            current_module_handle().map_err(|err| format!("getting module handle: {err}"))?;
        let hinstance = HINSTANCE(module.0);
        let class_name = w!("CncDdrawWgpuPresent");
        register_overlay_class(class_name, hinstance, present_wndproc);

        let (x, y, w, h) = client_screen_rect(owner).unwrap_or((0, 0, 1, 1));
        let hwnd = create_window_ex_w(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!(""),
            WS_POPUP,
            x,
            y,
            w.max(1) as i32,
            h.max(1) as i32,
            Some(owner),
            Some(hinstance),
        )
        .map_err(|err| format!("creating presentation overlay window: {err}"))?;
        set_present_hwnd(hwnd);
        let hinst_nz = NonZeroIsize::new(module.0 as isize)
            .ok_or_else(|| "invalid module handle".to_string())?;
        Ok(Self {
            owner: SendHwnd(owner),
            hwnd: SendHwnd(hwnd),
            hinstance: hinst_nz,
            last_rect: None,
            visible: false,
        })
    }

    pub fn raw_handle(&self) -> Result<NonZeroIsize, String> {
        NonZeroIsize::new(self.hwnd.get().0 as isize)
            .ok_or_else(|| "invalid presentation window handle".to_string())
    }

    pub fn hinstance(&self) -> NonZeroIsize {
        self.hinstance
    }

    /// Synchronizes overlay bounds with the owner's client area.
    /// Returns `Some((width, height))` if visible and drawable, or `None` if hidden/minimized.
    pub fn sync_to_owner(&mut self) -> Result<Option<(u32, u32)>, String> {
        let owner = self.owner.get();
        let hwnd = self.hwnd.get();
        let (x, y, target_w, target_h) = client_screen_rect(owner)?;
        if is_iconic(owner) || !is_visible(owner) || target_w == 0 || target_h == 0 {
            self.hide();
            return Ok(None);
        }
        let rect = (x, y, target_w, target_h);
        if self.last_rect != Some(rect) || !self.visible {
            let _ = set_window_pos(
                hwnd,
                None,
                rect.0,
                rect.1,
                rect.2 as i32,
                rect.3 as i32,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            );
            self.last_rect = Some(rect);
            self.visible = true;
        }
        keep_present_on_top(owner, hwnd);
        Ok(Some((target_w, target_h)))
    }

    pub fn hide(&mut self) {
        if self.visible {
            let _ = set_window_pos(
                self.hwnd.get(),
                None,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
            );
            self.visible = false;
        }
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        let hwnd = self.hwnd.get();
        clear_present_hwnd(hwnd);
        if is_window(hwnd) {
            destroy_window(hwnd);
        }
    }
}

pub fn client_screen_rect(hwnd: HWND) -> Result<(i32, i32, u32, u32), String> {
    let client = client_rect(hwnd).map_err(|err| format!("getting client area: {err}"))?;
    let mut origin = POINT { x: 0, y: 0 };
    client_to_screen(hwnd, &mut origin);
    Ok((
        origin.x,
        origin.y,
        client.right.max(0) as u32,
        client.bottom.max(0) as u32,
    ))
}

/// Clears `WS_EX_TOPMOST` from `hwnd` if set, demoting it out of the topmost Z-order band.
pub(crate) fn clear_topmost(hwnd: HWND) {
    if !hwnd.is_invalid() && ex_style(hwnd) as u32 & WS_EX_TOPMOST.0 != 0 {
        let _ = set_window_pos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// Checks if any visible game window/dialog/dropdown/scrollbar is currently above `present`
/// in Z-order while the game owns foreground focus, strips `WS_EX_TOPMOST` if needed, and raises
/// `present` above it.
pub fn keep_present_on_top(owner: HWND, present: HWND) {
    if present.is_invalid() || owner.is_invalid() {
        return;
    }
    clear_topmost(owner);
    clear_topmost(present);
    let fg = foreground_window();
    if fg != owner && !is_related_dialog_window(owner, fg) && !is_same_process_window(fg) {
        return;
    }
    let mut need_raise = false;
    for prev in prev_siblings(present).take(512) {
        if prev != present
            && is_visible(prev)
            && (prev == owner
                || is_related_dialog_window(owner, prev)
                || is_same_process_window(prev))
        {
            clear_topmost(prev);
            need_raise = true;
        }
    }
    if need_raise {
        let _ = set_window_pos(
            present,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
    }
}

pub(crate) fn ensure_present_on_top_if_active(bound: HWND) {
    if let Some(present) = present_hwnd() {
        keep_present_on_top(bound, present);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOP, SWP_NOACTIVATE, SWP_SHOWWINDOW, WM_PAINT, WS_CHILD, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
    };

    use crate::win32::hwnd::test_support::{
        consume_paint, invalidate_window, pump_thread_messages, register_test_class,
    };
    use crate::win32::hwnd::{
        create_window_ex_w, def_window_proc_w, destroy_window, set_window_pos,
    };

    static PAINT_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn count_paint_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_PAINT {
            consume_paint(hwnd);
            PAINT_COUNT.fetch_add(1, Ordering::Relaxed);
            return LRESULT(0);
        }
        def_window_proc_w(hwnd, msg, wparam, lparam)
    }

    #[test]
    fn owned_overlay_above_dialogs_delivers_wm_paint_to_both_child_and_popup() {
        let class_name = w!("CncDdrawTestPaintClass");
        register_test_class(class_name, count_paint_wndproc);

        let parent = create_window_ex_w(
            Default::default(),
            w!("STATIC"),
            w!("parent"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            0,
            0,
            320,
            240,
            None,
            None,
        )
        .unwrap();
        let child = create_window_ex_w(
            Default::default(),
            class_name,
            w!("child"),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            160,
            120,
            Some(parent),
            None,
        )
        .unwrap();
        let popup = create_window_ex_w(
            Default::default(),
            class_name,
            w!("popup"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            160,
            120,
            Some(parent),
            None,
        )
        .unwrap();
        let overlay = create_window_ex_w(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("STATIC"),
            w!("overlay"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            320,
            240,
            Some(parent),
            None,
        )
        .unwrap();
        let _ = set_window_pos(
            overlay,
            Some(HWND_TOP),
            0,
            0,
            320,
            240,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        PAINT_COUNT.store(0, Ordering::Relaxed);
        invalidate_window(child);
        invalidate_window(popup);
        pump_thread_messages();
        assert!(
            PAINT_COUNT.load(Ordering::Relaxed) >= 2,
            "expected WM_PAINT on both child and popup behind overlay"
        );

        destroy_window(overlay);
        destroy_window(popup);
        destroy_window(parent);
    }

    #[test]
    fn keep_present_on_top_clears_topmost_on_owner_and_overlay() {
        use super::{ex_style, keep_present_on_top};
        use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST;

        let owner = create_window_ex_w(
            WS_EX_TOPMOST,
            w!("STATIC"),
            w!("topmost-owner"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            0,
            0,
            320,
            240,
            None,
            None,
        )
        .unwrap();
        let overlay = create_window_ex_w(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("STATIC"),
            w!("topmost-overlay"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            320,
            240,
            Some(owner),
            None,
        )
        .unwrap();

        keep_present_on_top(owner, overlay);
        assert_eq!(ex_style(owner) as u32 & WS_EX_TOPMOST.0, 0);
        assert_eq!(ex_style(overlay) as u32 & WS_EX_TOPMOST.0, 0);

        destroy_window(overlay);
        destroy_window(owner);
    }
}
