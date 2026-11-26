//! Game host HWND lifecycle: borderless fullscreen, windowed mode, live Alt+Enter toggle,
//! and lossless restoration of original window style and placement.
#![forbid(unsafe_code)]

use windows::core::Result;
use windows::Win32::Foundation::{E_INVALIDARG, HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    WINDOWPLACEMENT, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_TOPMOST, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

use super::overlay::clear_topmost;

use crate::config::Config;
use crate::win32::geometry::{adjust_window_rect_ex, is_window, monitor_info, window_rect};
use crate::win32::hwnd::{
    ex_style, set_window_long, set_window_placement, set_window_pos, style, window_placement,
    SendHwnd,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationMode {
    BorderlessFullscreen,
    Windowed,
}

#[derive(Clone, Copy)]
pub struct HostWindow {
    hwnd: SendHwnd,
    placement: WINDOWPLACEMENT,
    orig_style: i32,
    orig_ex_style: i32,
    mode: PresentationMode,
    saved_windowed_rect: Option<RECT>,
    has_custom_window_rect: bool,
}

impl HostWindow {
    pub fn enter(
        hwnd: HWND,
        mode: PresentationMode,
        logical_w: u32,
        logical_h: u32,
        cfg: &Config,
    ) -> Result<Self> {
        let orig_style = style(hwnd);
        if orig_style as u32 & WS_CHILD.0 != 0 {
            return Err(E_INVALIDARG.into());
        }
        let placement = window_placement(hwnd)?;
        let mut host = Self {
            hwnd: SendHwnd(hwnd),
            placement,
            orig_style,
            orig_ex_style: ex_style(hwnd),
            mode,
            saved_windowed_rect: None,
            has_custom_window_rect: false,
        };
        let result = match mode {
            PresentationMode::BorderlessFullscreen => host.apply_borderless_fullscreen(),
            PresentationMode::Windowed => host.apply_windowed(logical_w, logical_h, cfg),
        };
        if let Err(err) = result {
            let _ = host.restore();
            return Err(err);
        }
        Ok(host)
    }

    pub fn is_fullscreen(&self) -> bool {
        self.mode == PresentationMode::BorderlessFullscreen
    }

    pub fn toggle_mode(
        &mut self,
        logical_w: u32,
        logical_h: u32,
        cfg: &Config,
    ) -> Result<PresentationMode> {
        match self.mode {
            PresentationMode::BorderlessFullscreen => {
                self.apply_windowed(logical_w, logical_h, cfg)?;
                self.mode = PresentationMode::Windowed;
            }
            PresentationMode::Windowed => {
                if let Some(rc) = window_rect(self.hwnd.get())
                    .ok()
                    .filter(|r| r.right > r.left && r.bottom > r.top)
                {
                    self.saved_windowed_rect = Some(rc);
                    self.has_custom_window_rect = true;
                }
                self.apply_borderless_fullscreen()?;
                self.mode = PresentationMode::BorderlessFullscreen;
            }
        }
        Ok(self.mode)
    }

    /// Updates windowed size when the game switches logical resolution and the user hasn't
    /// configured a fixed size or manually resized/moved the window.
    pub fn on_logical_mode_changed(&mut self, logical_w: u32, logical_h: u32, cfg: &Config) {
        if self.mode == PresentationMode::Windowed
            && !self.has_custom_window_rect
            && cfg.width == 0
            && cfg.height == 0
        {
            let _ = self.apply_windowed(logical_w, logical_h, cfg);
        }
    }

    fn apply_borderless_fullscreen(&self) -> Result<()> {
        let hwnd = self.hwnd.get();
        let info = monitor_info(hwnd)?;
        let rect = info.rcMonitor;
        let strip_ex_styles = WS_EX_CLIENTEDGE.0
            | WS_EX_WINDOWEDGE.0
            | WS_EX_DLGMODALFRAME.0
            | WS_EX_STATICEDGE.0
            | WS_EX_TOPMOST.0;
        clear_topmost(hwnd);
        set_window_long(
            hwnd,
            GWL_STYLE,
            ((self.orig_style as u32 & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0) as i32,
        )?;
        set_window_long(
            hwnd,
            GWL_EXSTYLE,
            (self.orig_ex_style as u32 & !strip_ex_styles) as i32,
        )?;
        set_window_pos(
            hwnd,
            None,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }

    fn apply_windowed(&mut self, logical_w: u32, logical_h: u32, cfg: &Config) -> Result<()> {
        let hwnd = self.hwnd.get();
        let info = monitor_info(hwnd)?;
        let work = info.rcWork;
        let mut next_style = (self.orig_style as u32) & !(WS_POPUP.0 | WS_OVERLAPPEDWINDOW.0);
        if cfg.border {
            next_style |= WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0;
            if cfg.resizable {
                next_style |= WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0;
            }
        } else {
            next_style |= WS_POPUP.0;
        }
        clear_topmost(hwnd);
        let next_ex_style = self.orig_ex_style as u32 & !WS_EX_TOPMOST.0;
        set_window_long(hwnd, GWL_STYLE, next_style as i32)?;
        set_window_long(hwnd, GWL_EXSTYLE, next_ex_style as i32)?;

        let (x, y, win_w, win_h) = if let Some(rc) = self.saved_windowed_rect {
            (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top)
        } else {
            let client_w = if cfg.width > 0 {
                cfg.width
            } else {
                logical_w.max(320)
            } as i32;
            let client_h = if cfg.height > 0 {
                cfg.height
            } else {
                logical_h.max(200)
            } as i32;
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: client_w,
                bottom: client_h,
            };
            adjust_window_rect_ex(
                &mut rc,
                WINDOW_STYLE(next_style),
                false,
                WINDOW_EX_STYLE(next_ex_style),
            );
            let max_w = (work.right - work.left).max(320);
            let max_h = (work.bottom - work.top).max(200);
            let w = (rc.right - rc.left).min(max_w);
            let h = (rc.bottom - rc.top).min(max_h);
            let x = cfg.pos_x.unwrap_or_else(|| work.left + (max_w - w) / 2);
            let y = cfg.pos_y.unwrap_or_else(|| work.top + (max_h - h) / 2);
            (x, y, w, h)
        };

        set_window_pos(
            hwnd,
            None,
            x,
            y,
            win_w,
            win_h,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }

    pub fn restore(self) -> Result<()> {
        let hwnd = self.hwnd.get();
        if !is_window(hwnd) {
            return Ok(());
        }
        set_window_long(hwnd, GWL_STYLE, self.orig_style)?;
        set_window_long(hwnd, GWL_EXSTYLE, self.orig_ex_style)?;
        set_window_placement(hwnd, &self.placement)?;
        set_window_pos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::hwnd::{create_window_ex_w, destroy_window};
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::WS_VISIBLE;

    #[test]
    fn host_window_enters_toggles_and_restores_original_style_and_rect() {
        let hwnd = create_window_ex_w(
            Default::default(),
            w!("STATIC"),
            w!("cnc-ddraw-host-test"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            640,
            480,
            None,
            None,
        )
        .unwrap();
        let orig_style = style(hwnd);
        let orig_rect = window_rect(hwnd).unwrap();

        let cfg = Config {
            width: 800,
            height: 600,
            border: true,
            resizable: true,
            ..Config::default()
        };
        let mut host =
            HostWindow::enter(hwnd, PresentationMode::BorderlessFullscreen, 800, 600, &cfg)
                .unwrap();
        assert!(host.is_fullscreen());

        let next = host.toggle_mode(800, 600, &cfg).unwrap();
        assert_eq!(next, PresentationMode::Windowed);
        assert!(!host.is_fullscreen());
        let win_style = style(hwnd) as u32;
        assert!(win_style & WS_CAPTION.0 != 0);
        assert!(win_style & WS_THICKFRAME.0 != 0);

        let back = host.toggle_mode(800, 600, &cfg).unwrap();
        assert_eq!(back, PresentationMode::BorderlessFullscreen);

        host.restore().unwrap();
        assert_eq!(style(hwnd), orig_style);
        let restored = window_rect(hwnd).unwrap();
        assert_eq!(restored, orig_rect);

        destroy_window(hwnd);
    }

    #[test]
    fn host_window_strips_ws_ex_topmost_on_enter_and_toggle() {
        let hwnd = create_window_ex_w(
            WS_EX_TOPMOST,
            w!("STATIC"),
            w!("cnc-ddraw-topmost-test"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            640,
            480,
            None,
            None,
        )
        .unwrap();
        assert_ne!(ex_style(hwnd) as u32 & WS_EX_TOPMOST.0, 0);

        let cfg = Config::default();
        let mut host =
            HostWindow::enter(hwnd, PresentationMode::BorderlessFullscreen, 800, 600, &cfg)
                .unwrap();
        assert_eq!(ex_style(hwnd) as u32 & WS_EX_TOPMOST.0, 0);

        host.toggle_mode(800, 600, &cfg).unwrap();
        assert_eq!(ex_style(hwnd) as u32 & WS_EX_TOPMOST.0, 0);

        host.restore().unwrap();
        destroy_window(hwnd);
    }
}
