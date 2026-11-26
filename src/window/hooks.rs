//! Thin Win32 ABI hook stubs, host WndProc subclassing, and WH_GETMESSAGE installation.
//! All hook decision logic lives in `hook_logic` (`#![forbid(unsafe_code)]`) and PE32 IAT patching
//! lives in `crate::win32::iat`.
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::{BOOL, PCSTR};
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient, HDC};
use windows::Win32::System::LibraryLoader::{
    FindResourceA, LoadResource, LockResource, SizeofResource,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CallWindowProcA, CallWindowProcW, CreateDialogIndirectParamA,
    CreateDialogParamA, DialogBoxIndirectParamA, DialogBoxParamA, GetClientRect, GetClipCursor,
    GetWindowLongA, GetWindowLongW, GetWindowRect, SetWindowLongA, SetWindowLongW,
    SetWindowsHookExW, UnhookWindowsHookEx, DLGPROC, DLGTEMPLATE, GWLP_WNDPROC, HHOOK, MSG,
    RT_DIALOG, WH_GETMESSAGE, WNDPROC,
};

use crate::win32::geometry::is_window;
use crate::win32::hwnd::{def_window_proc_a, def_window_proc_w, is_unicode, kill_timer};
use crate::win32::{device_caps, iat, system_dpi, system_metrics, AtomicHwnd};

use super::dialog_template::FontPatch;
use super::hook_logic::{self, SubclassAction};
use super::{bound_hwnd, FLUSH_TIMER_ID};

static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);
static SUBCLASSED_HWND: AtomicHwnd = AtomicHwnd::new();
static SUBCLASSED_UNICODE: AtomicBool = AtomicBool::new(false);
static GETMSG_HOOK: AtomicIsize = AtomicIsize::new(0);
static ACTIVATE_PASSED: AtomicBool = AtomicBool::new(false);
static ACTIVATEAPP_PASSED: AtomicBool = AtomicBool::new(false);

pub(crate) fn subclassed_hwnd() -> Option<HWND> {
    SUBCLASSED_HWND.load()
}

unsafe extern "system" fn hook_GetClientRect(hwnd: HWND, rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some(value) = hook_logic::virtual_window_rect(hwnd, false) {
            // SAFETY: Caller passed a non-null writable RECT pointer per Win32 GetClientRect.
            unsafe { rect.write(value) };
            return BOOL(1);
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 GetClientRect.
    BOOL::from(unsafe { GetClientRect(hwnd, rect) }.is_ok())
}

unsafe extern "system" fn hook_GetWindowRect(hwnd: HWND, rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some(value) = hook_logic::virtual_window_rect(hwnd, true) {
            // SAFETY: Caller passed a non-null writable RECT pointer per Win32 GetWindowRect.
            unsafe { rect.write(value) };
            return BOOL(1);
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 GetWindowRect.
    BOOL::from(unsafe { GetWindowRect(hwnd, rect) }.is_ok())
}

unsafe extern "system" fn hook_GetSystemMetrics(index: i32) -> i32 {
    hook_logic::virtual_system_metrics(index).unwrap_or_else(|| system_metrics(index))
}

unsafe extern "system" fn hook_ClientToScreen(hwnd: HWND, pt: *mut POINT) -> BOOL {
    if !pt.is_null() {
        // SAFETY: Caller passed a valid, initialized mutable POINT per Win32 ClientToScreen.
        let point = unsafe { &mut *pt };
        if let Some(ok) = hook_logic::virtual_client_to_screen(hwnd, point) {
            return BOOL::from(ok);
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 ClientToScreen.
    unsafe { ClientToScreen(hwnd, pt) }
}

unsafe extern "system" fn hook_ScreenToClient(hwnd: HWND, pt: *mut POINT) -> BOOL {
    if !pt.is_null() {
        // SAFETY: Caller passed a valid, initialized mutable POINT per Win32 ScreenToClient.
        let point = unsafe { &mut *pt };
        if let Some(ok) = hook_logic::virtual_screen_to_client(hwnd, point) {
            return BOOL::from(ok);
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 ScreenToClient.
    unsafe { ScreenToClient(hwnd, pt) }
}

unsafe extern "system" fn hook_GetCursorPos(pt: *mut POINT) -> BOOL {
    if pt.is_null() {
        return BOOL(0);
    }
    let Some(pos) = hook_logic::virtual_cursor_pos() else {
        return BOOL(0);
    };
    // SAFETY: Caller passed a non-null writable POINT pointer per Win32 GetCursorPos.
    unsafe { pt.write(pos) };
    BOOL(1)
}

unsafe extern "system" fn hook_SetCursorPos(x: i32, y: i32) -> BOOL {
    BOOL::from(hook_logic::set_virtual_cursor_pos(x, y))
}

unsafe extern "system" fn hook_ClipCursor(rect: *const RECT) -> BOOL {
    // SAFETY: Caller passes either NULL or a valid readable RECT per Win32 ClipCursor.
    let rect_ref = unsafe { rect.as_ref() };
    BOOL::from(hook_logic::clip_virtual_cursor(rect_ref))
}

unsafe extern "system" fn hook_GetClipCursor(rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some(value) = hook_logic::virtual_clip_cursor() {
            // SAFETY: Caller passed a non-null writable RECT pointer per Win32 GetClipCursor.
            unsafe { rect.write(value) };
            return BOOL(1);
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 GetClipCursor.
    BOOL::from(unsafe { GetClipCursor(rect) }.is_ok())
}

unsafe extern "system" fn hook_WindowFromPoint(point: POINT) -> HWND {
    hook_logic::virtual_window_from_point(point)
}

unsafe extern "system" fn hook_SetWindowPos(
    hwnd: HWND,
    insert_after: HWND,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    flags: u32,
) -> BOOL {
    BOOL::from(hook_logic::handle_set_window_pos(
        hwnd,
        insert_after,
        x,
        y,
        cx,
        cy,
        flags,
    ))
}

unsafe extern "system" fn hook_BringWindowToTop(hwnd: HWND) -> BOOL {
    BOOL::from(hook_logic::handle_bring_window_to_top(hwnd))
}

unsafe extern "system" fn hook_MoveWindow(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    repaint: BOOL,
) -> BOOL {
    BOOL::from(hook_logic::handle_move_window(
        hwnd,
        x,
        y,
        width,
        height,
        repaint.as_bool(),
    ))
}

unsafe extern "system" fn hook_GetDeviceCaps(hdc: HDC, index: i32) -> i32 {
    hook_logic::virtual_device_caps(index).unwrap_or_else(|| device_caps(hdc, index))
}

// Win32 requires a DWORD-aligned template. The indirect caller may leave padding
// uninitialized, so copy it as raw storage and only read actual template fields.
type DialogStorage = Vec<MaybeUninit<u32>>;

/// # Safety
/// The source must be readable through `len` bytes and `patch` must describe that source.
unsafe fn copy_patched_template(
    src: *const u8,
    len: usize,
    patch: FontPatch,
) -> Option<DialogStorage> {
    let words = len.div_ceil(4);
    let mut out = DialogStorage::new();
    out.try_reserve_exact(words).ok()?;
    out.resize_with(words, MaybeUninit::uninit);
    let dst = out.as_mut_ptr().cast::<u8>();
    // SAFETY: The destination spans at least `len` bytes, is freshly allocated,
    // and the parser located a full point-size word within the source.
    unsafe {
        std::ptr::copy_nonoverlapping(src, dst, len);
        dst.add(patch.offset)
            .cast::<u16>()
            .write_unaligned(patch.point_size);
    }
    Some(out)
}

/// # Safety
/// The module and resource identifier must satisfy the `FindResourceA` contract.
unsafe fn load_and_patch_dialog_resource(
    hinst: HINSTANCE,
    template_name: PCSTR,
) -> Option<DialogStorage> {
    let dpi = system_dpi();
    if dpi <= 96 || bound_hwnd().is_none() {
        return None;
    }
    // SAFETY: Resource handles come from the caller's live module and `LockResource` returns
    // `SizeofResource` initialized bytes valid for the duration of this call.
    unsafe {
        let hmod = if hinst.0.is_null() {
            None
        } else {
            Some(HMODULE(hinst.0))
        };
        let hres = FindResourceA(hmod, template_name, PCSTR(RT_DIALOG.0 as *const u8)).ok()?;
        let size = SizeofResource(hmod, hres) as usize;
        if size == 0 || size > isize::MAX as usize {
            return None;
        }
        let hglob = LoadResource(hmod, hres).ok()?;
        let ptr = LockResource(hglob) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let patch = FontPatch::from_bytes(std::slice::from_raw_parts(ptr, size), dpi)?;
        copy_patched_template(ptr, size, patch)
    }
}

/// # Safety
/// Win32 passes no length. The indirect-dialog caller must supply one complete
/// `DLGTEMPLATE` or `DLGTEMPLATEEX` that remains readable and stable during the call.
unsafe fn patch_dialog_indirect_ptr(tpl: *const DLGTEMPLATE) -> Option<DialogStorage> {
    let dpi = system_dpi();
    if dpi <= 96 || bound_hwnd().is_none() || tpl.is_null() {
        return None;
    }
    let raw = tpl.cast::<u8>();
    // SAFETY: The parser requests only initialized structural words at their
    // documented offsets in the caller's valid template, never alignment padding.
    let patch = FontPatch::parse(None, dpi, |offset| unsafe {
        raw.add(offset).cast::<u16>().read_unaligned()
    })?;
    // SAFETY: `patch.len` was computed over the live template starting at `raw`.
    unsafe { copy_patched_template(raw, patch.len, patch) }
}

unsafe extern "system" fn hook_CreateDialogParamA(
    hinst: HINSTANCE,
    template_name: PCSTR,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    // SAFETY: Forwarding the caller's valid module and dialog resource identifier.
    if let Some(patched) = unsafe { load_and_patch_dialog_resource(hinst, template_name) } {
        // SAFETY: `patched` is a DWORD-aligned, valid DLGTEMPLATE clone alive during the call.
        if let Ok(hwnd) = unsafe {
            CreateDialogIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        } {
            return hwnd;
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 CreateDialogParamA.
    unsafe {
        CreateDialogParamA(
            Some(hinst),
            template_name,
            Some(parent),
            dlg_proc,
            init_param,
        )
    }
    .unwrap_or_default()
}

unsafe extern "system" fn hook_CreateDialogIndirectParamA(
    hinst: HINSTANCE,
    template: *const DLGTEMPLATE,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    // SAFETY: Caller supplies a valid indirect dialog template per Win32 contract.
    if let Some(patched) = unsafe { patch_dialog_indirect_ptr(template) } {
        // SAFETY: `patched` is a DWORD-aligned, valid DLGTEMPLATE clone alive during the call.
        if let Ok(hwnd) = unsafe {
            CreateDialogIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        } {
            return hwnd;
        }
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 CreateDialogIndirectParamA.
    unsafe { CreateDialogIndirectParamA(Some(hinst), template, Some(parent), dlg_proc, init_param) }
        .unwrap_or_default()
}

unsafe extern "system" fn hook_DialogBoxParamA(
    hinst: HINSTANCE,
    template_name: PCSTR,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> isize {
    // SAFETY: Forwarding the caller's valid module and dialog resource identifier.
    if let Some(patched) = unsafe { load_and_patch_dialog_resource(hinst, template_name) } {
        // SAFETY: `patched` remains alive for the full modal lifetime of DialogBoxIndirectParamA.
        return unsafe {
            DialogBoxIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        };
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 DialogBoxParamA.
    unsafe {
        DialogBoxParamA(
            Some(hinst),
            template_name,
            Some(parent),
            dlg_proc,
            init_param,
        )
    }
}

unsafe extern "system" fn hook_DialogBoxIndirectParamA(
    hinst: HINSTANCE,
    template: *const DLGTEMPLATE,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> isize {
    // SAFETY: Caller supplies a valid indirect dialog template per Win32 contract.
    if let Some(patched) = unsafe { patch_dialog_indirect_ptr(template) } {
        // SAFETY: `patched` remains alive for the full modal lifetime of DialogBoxIndirectParamA.
        return unsafe {
            DialogBoxIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        };
    }
    // SAFETY: Forwarding unchanged ABI arguments to Win32 DialogBoxIndirectParamA.
    unsafe { DialogBoxIndirectParamA(Some(hinst), template, Some(parent), dlg_proc, init_param) }
}

unsafe extern "system" fn getmsg_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        // SAFETY: For WH_GETMESSAGE with `code >= 0`, Win32 guarantees `lparam` points to a mutable MSG.
        let msg = unsafe { &mut *(lparam.0 as *mut MSG) };
        hook_logic::rewrite_getmsg(msg, wparam.0 != 0);
    }
    let hook = HHOOK(GETMSG_HOOK.load(Ordering::Relaxed) as *mut c_void);
    // SAFETY: Forwarding to the next hook in the chain per WH_GETMESSAGE contract.
    unsafe { CallNextHookEx(Some(hook), code, wparam, lparam) }
}

unsafe extern "system" fn subclass_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let forwarded_lparam = match hook_logic::handle_subclass_message(
        hwnd,
        msg,
        wparam,
        lparam,
        &ACTIVATE_PASSED,
        &ACTIVATEAPP_PASSED,
    ) {
        SubclassAction::Return(res) => return res,
        SubclassAction::DefWindowProc => {
            return if SUBCLASSED_UNICODE.load(Ordering::Relaxed) {
                def_window_proc_w(hwnd, msg, wparam, lparam)
            } else {
                def_window_proc_a(hwnd, msg, wparam, lparam)
            };
        }
        SubclassAction::Forward(lp) => lp,
    };
    let prev_raw = ORIG_WNDPROC.load(Ordering::Relaxed);
    // SAFETY: `prev_raw` was returned by `SetWindowLong(GWLP_WNDPROC)` for this window and
    // is invoked through the matching Unicode/ANSI `CallWindowProc`.
    let prev: WNDPROC = unsafe { std::mem::transmute(prev_raw) };
    if SUBCLASSED_UNICODE.load(Ordering::Relaxed) {
        // SAFETY: Invoking the original window procedure with validated arguments.
        unsafe { CallWindowProcW(prev, hwnd, msg, wparam, forwarded_lparam) }
    } else {
        // SAFETY: Invoking the original ANSI window procedure with validated arguments.
        unsafe { CallWindowProcA(prev, hwnd, msg, wparam, forwarded_lparam) }
    }
}

pub(crate) fn install_getmsg_hook(tid: u32) {
    uninstall_getmsg_hook();
    if tid == 0 {
        return;
    }
    // SAFETY: `getmsg_hook` matches `HOOKPROC` and runs on thread `tid` until unhooked.
    if let Ok(hook) = unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(getmsg_hook), None, tid) } {
        GETMSG_HOOK.store(hook.0 as isize, Ordering::Relaxed);
    }
}

pub(crate) fn uninstall_getmsg_hook() {
    let raw = GETMSG_HOOK.swap(0, Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: `raw` was returned by `SetWindowsHookExW` and swapped out exclusively.
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut c_void));
        }
    }
}

pub(crate) fn install_subclass(hwnd: HWND) {
    uninstall_subclass();
    if hwnd.is_invalid() || !is_window(hwnd) {
        return;
    }
    let unicode = is_unicode(hwnd);
    let new_proc = subclass_wndproc as *const () as usize as i32;
    // SAFETY: Replacing GWLP_WNDPROC with `subclass_wndproc` matching the window's encoding.
    let prev = unsafe {
        if unicode {
            let cur = GetWindowLongW(hwnd, GWLP_WNDPROC);
            if cur == new_proc || cur == 0 {
                return;
            }
            SetWindowLongW(hwnd, GWLP_WNDPROC, new_proc)
        } else {
            let cur = GetWindowLongA(hwnd, GWLP_WNDPROC);
            if cur == new_proc || cur == 0 {
                return;
            }
            SetWindowLongA(hwnd, GWLP_WNDPROC, new_proc)
        }
    };
    if prev != 0 {
        ORIG_WNDPROC.store(prev as isize, Ordering::Relaxed);
        SUBCLASSED_UNICODE.store(unicode, Ordering::Relaxed);
        SUBCLASSED_HWND.store(Some(hwnd));
        ACTIVATE_PASSED.store(false, Ordering::Relaxed);
        ACTIVATEAPP_PASSED.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn uninstall_subclass() {
    let Some(hwnd) = SUBCLASSED_HWND.swap(None) else {
        ORIG_WNDPROC.store(0, Ordering::Relaxed);
        return;
    };
    let prev = ORIG_WNDPROC.swap(0, Ordering::Relaxed);
    if prev == 0 || !is_window(hwnd) {
        return;
    }
    kill_timer(hwnd, FLUSH_TIMER_ID);
    let our_proc = subclass_wndproc as *const () as usize as i32;
    // SAFETY: Restores the original WNDPROC only if `subclass_wndproc` is still installed.
    unsafe {
        if SUBCLASSED_UNICODE.load(Ordering::Relaxed) {
            if GetWindowLongW(hwnd, GWLP_WNDPROC) == our_proc {
                let _ = SetWindowLongW(hwnd, GWLP_WNDPROC, prev as i32);
            }
        } else if GetWindowLongA(hwnd, GWLP_WNDPROC) == our_proc {
            let _ = SetWindowLongA(hwnd, GWLP_WNDPROC, prev as i32);
        }
    }
}

pub(crate) fn install_iat_hooks() {
    let targets: &[(&str, &str, usize)] = &[
        (
            "user32.dll",
            "GetClientRect",
            hook_GetClientRect as *const () as usize,
        ),
        (
            "user32.dll",
            "GetWindowRect",
            hook_GetWindowRect as *const () as usize,
        ),
        (
            "user32.dll",
            "GetSystemMetrics",
            hook_GetSystemMetrics as *const () as usize,
        ),
        (
            "user32.dll",
            "ClientToScreen",
            hook_ClientToScreen as *const () as usize,
        ),
        (
            "user32.dll",
            "ScreenToClient",
            hook_ScreenToClient as *const () as usize,
        ),
        (
            "user32.dll",
            "GetCursorPos",
            hook_GetCursorPos as *const () as usize,
        ),
        (
            "user32.dll",
            "SetCursorPos",
            hook_SetCursorPos as *const () as usize,
        ),
        (
            "user32.dll",
            "ClipCursor",
            hook_ClipCursor as *const () as usize,
        ),
        (
            "user32.dll",
            "GetClipCursor",
            hook_GetClipCursor as *const () as usize,
        ),
        (
            "user32.dll",
            "WindowFromPoint",
            hook_WindowFromPoint as *const () as usize,
        ),
        (
            "user32.dll",
            "SetWindowPos",
            hook_SetWindowPos as *const () as usize,
        ),
        (
            "user32.dll",
            "BringWindowToTop",
            hook_BringWindowToTop as *const () as usize,
        ),
        (
            "user32.dll",
            "MoveWindow",
            hook_MoveWindow as *const () as usize,
        ),
        (
            "user32.dll",
            "CreateDialogParamA",
            hook_CreateDialogParamA as *const () as usize,
        ),
        (
            "user32.dll",
            "CreateDialogIndirectParamA",
            hook_CreateDialogIndirectParamA as *const () as usize,
        ),
        (
            "user32.dll",
            "DialogBoxParamA",
            hook_DialogBoxParamA as *const () as usize,
        ),
        (
            "user32.dll",
            "DialogBoxIndirectParamA",
            hook_DialogBoxIndirectParamA as *const () as usize,
        ),
        (
            "gdi32.dll",
            "GetDeviceCaps",
            hook_GetDeviceCaps as *const () as usize,
        ),
    ];
    // SAFETY: Every hook function matches its Win32 import's exact stdcall ABI and remains
    // loaded in this DLL until `uninstall_iat_hooks` restores the IAT.
    unsafe { iat::install(targets) };
}

pub(crate) fn uninstall_iat_hooks() {
    iat::uninstall();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::hwnd::destroy_window;
    use crate::window::dialog_template::tests::{build_ex_template, build_standard_template};

    #[test]
    fn copy_patched_template_writes_scaled_point_size() {
        let std_tpl = build_standard_template(200, 100, 8, true);
        for (template, offset) in [(&std_tpl, 24), (&build_ex_template(8), 36)] {
            for (dpi, expected) in [(192, 4u16), (240, 3u16)] {
                let patch = FontPatch::from_bytes(template, dpi).unwrap();
                let mut expected_bytes = template.clone();
                expected_bytes[offset..offset + 2].copy_from_slice(&expected.to_le_bytes());
                // SAFETY: Both fixtures initialize every byte, including control padding.
                unsafe {
                    let copy =
                        copy_patched_template(template.as_ptr(), template.len(), patch).unwrap();
                    let bytes =
                        std::slice::from_raw_parts(copy.as_ptr().cast::<u8>(), template.len());
                    assert_eq!(bytes, expected_bytes);
                }
            }
        }
    }

    #[test]
    fn patched_dialog_keeps_96_dpi_pixel_dimensions() {
        crate::window::enable_dpi_awareness();
        let dpi = system_dpi();
        let raw = build_standard_template(200, 100, 8, false);
        let patched = FontPatch::from_bytes(&raw, dpi).map(|patch| {
            // SAFETY: `raw` is a complete initialized dialog template.
            unsafe { copy_patched_template(raw.as_ptr(), raw.len(), patch).unwrap() }
        });
        let template = patched
            .as_ref()
            .map_or(raw.as_ptr().cast(), |p| p.as_ptr().cast());
        // SAFETY: `template` points to a valid DWORD-aligned DLGTEMPLATE.
        let hwnd = unsafe { CreateDialogIndirectParamA(None, template, None, None, LPARAM(0)) }
            .expect("CreateDialogIndirectParamA should create patched dialog");
        let rc = super::super::client_rect(hwnd).unwrap();
        destroy_window(hwnd);
        // At 96 DPI, 200x100 DLU with 8pt MS Sans Serif is 300x163 px.
        assert!(
            (280..=320).contains(&rc.right) && (145..=180).contains(&rc.bottom),
            "unexpected patched dialog size {}x{} at {} DPI",
            rc.right,
            rc.bottom,
            dpi
        );
    }
}
