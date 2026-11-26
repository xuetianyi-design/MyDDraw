//! Thread-safe display state, presentation scheduling, and wgpu renderer lifecycle.
#![forbid(unsafe_code)]

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use windows::core::{Error, Result, Weak as ComWeak, GUID};
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::HDC;

use crate::renderer::{PresentError, PresentOutcome, Renderer};
use crate::surface::{self, SurfaceSlot};
use crate::win32::hwnd::SendHwnd;
use crate::window::{self, HostWindow, PresentationMode};
use crate::{compat, config};

pub(super) const DEFAULT_MODE: (u32, u32, u32) = (640, 480, 32);
pub(super) const VIRTUAL_VIDMEM_BYTES: u32 = 64 * 1024 * 1024;
pub(super) const SURFACE_BUSY: windows::core::HRESULT =
    windows::core::HRESULT(0x8876_01ae_u32 as i32);

static FULLSCREEN_COUNT: AtomicUsize = AtomicUsize::new(0);
const FLUSH_MAX_DEFER: Duration = Duration::from_millis(16);

static ACTIVE_DISPLAY: Mutex<Weak<DisplayShared>> = Mutex::new(Weak::new());

pub(super) fn set_active_display(display: &Arc<DisplayShared>) {
    *ACTIVE_DISPLAY.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(display);
}

fn active_display() -> Option<Arc<DisplayShared>> {
    ACTIVE_DISPLAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()
}

pub fn is_fullscreen_active() -> bool {
    FULLSCREEN_COUNT.load(Ordering::Relaxed) > 0
}

pub fn flush_pending_present(from_timer: bool) {
    let Some(display) = active_display() else {
        return;
    };
    let min_ms = config::get().min_frame_interval_ms();
    let Some(d) = display.try_lock() else {
        window::arm_flush_timer(min_ms as u32);
        return;
    };
    display.flush_scheduled.store(false, Ordering::Release);
    if !display.dirty.load(Ordering::Acquire) {
        return;
    }
    if !from_timer
        && d.last_present
            .is_some_and(|t| t.elapsed() < Duration::from_millis(min_ms))
    {
        window::arm_flush_timer(min_ms as u32);
        return;
    }
    let _ = display.present_locked(d);
}

pub fn toggle_fullscreen_hotkey() {
    let Some(display) = active_display() else {
        return;
    };
    let Some(mut d) = display.try_lock() else {
        return;
    };
    let (lw, lh) = (d.width, d.height);
    let cfg = config::get();
    if let Some(host) = d.presentation.host.as_mut() {
        let was_fs = host.is_fullscreen();
        if let Ok(new_mode) = host.toggle_mode(lw, lh, cfg) {
            let now_fs = new_mode == PresentationMode::BorderlessFullscreen;
            if !was_fs && now_fs {
                FULLSCREEN_COUNT.fetch_add(1, Ordering::Relaxed);
            } else if was_fs && !now_fs {
                FULLSCREEN_COUNT.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(renderer) = d.presentation.renderer.as_mut() {
                renderer.sync_overlay();
            }
            let _ = display.present_locked(d);
            window::sync_cursor_lock();
        }
    }
}

pub fn on_host_window_changed() {
    let Some(display) = active_display() else {
        return;
    };
    let Some(mut d) = display.try_lock() else {
        return;
    };
    if let Some(renderer) = d.presentation.renderer.as_mut() {
        renderer.sync_overlay();
    }
    let _ = display.refresh_locked(d);
}

type RegisteredSurface = (ComWeak<IDirectDrawSurface>, Weak<SurfaceSlot>);

#[derive(Default)]
pub(super) struct SurfaceRegistry {
    pub(super) primary: Weak<SurfaceSlot>,
    pub(super) entries: Vec<RegisteredSurface>,
}

pub struct DisplayShared {
    registry: Mutex<SurfaceRegistry>,
    inner: Mutex<Display>,
    pub(super) dirty: AtomicBool,
    flush_scheduled: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
    bpp: AtomicU32,
}

impl DisplayShared {
    pub fn new(width: u32, height: u32, bpp: u32) -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(SurfaceRegistry::default()),
            inner: Mutex::new(Display {
                width,
                height,
                bpp,
                presentation: Presentation::default(),
                owner_draw: ComWeak::new(),
                last_present: None,
                consecutive_present_failures: 0,
            }),
            dirty: AtomicBool::new(false),
            flush_scheduled: AtomicBool::new(false),
            width: AtomicU32::new(width),
            height: AtomicU32::new(height),
            bpp: AtomicU32::new(bpp),
        })
    }

    /// Upgrade only while the owning DirectDraw object still has a strong reference.
    pub fn clone_owner_draw(&self) -> Option<IDirectDraw> {
        self.lock().owner_draw.upgrade()
    }

    pub fn lock(&self) -> MutexGuard<'_, Display> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, Display>> {
        self.inner.try_lock().ok()
    }

    pub fn mode(&self) -> (u32, u32, u32) {
        (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
            self.bpp.load(Ordering::Relaxed),
        )
    }

    pub(super) fn store_mode(&self, width: u32, height: u32, bpp: u32) {
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
        self.bpp.store(bpp, Ordering::Relaxed);
    }

    pub fn height(&self) -> u32 {
        self.height.load(Ordering::Relaxed).max(1)
    }

    pub(super) fn registry(&self) -> MutexGuard<'_, SurfaceRegistry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    // A final SurfaceData drop may release user-supplied COM private data.
    // Inspect and release upgraded references only after unlocking the registry.
    fn registry_snapshot(&self) -> Vec<RegisteredSurface> {
        self.registry().entries.clone()
    }

    // Presenters keep this reference alive until their display guard is dropped.
    // A final pixel-slot drop can call into application COM code.
    pub fn primary_slot(&self) -> Option<Arc<SurfaceSlot>> {
        self.registry().primary.upgrade()
    }

    pub fn register_surface(
        &self,
        interface: ComWeak<IDirectDrawSurface>,
        data: Weak<SurfaceSlot>,
        primary: bool,
    ) {
        let mut registry = self.registry();
        if primary {
            registry.primary = data.clone();
        }
        registry.entries.push((interface, data));
    }

    pub fn unregister_surface(&self, slot: &Arc<SurfaceSlot>) {
        let slot = Arc::downgrade(slot);
        let mut registry = self.registry();
        if registry.primary.ptr_eq(&slot) {
            registry.primary = Weak::new();
        }
        let removed = registry
            .entries
            .iter()
            .position(|(_, data)| data.ptr_eq(&slot))
            .map(|i| registry.entries.remove(i));
        drop(registry);
        drop(removed);
    }

    pub fn allocated_bytes(&self) -> usize {
        self.registry_snapshot()
            .into_iter()
            .filter_map(|(_, w)| w.upgrade())
            .map(|d| d.lock().byte_len())
            .sum()
    }

    pub fn live_surfaces(&self) -> Vec<IDirectDrawSurface> {
        self.registry_snapshot()
            .into_iter()
            .filter_map(|(interface, _)| interface.upgrade())
            .collect()
    }

    pub fn primary_surface(&self) -> Option<IDirectDrawSurface> {
        self.find_surface(|data| data.is_primary())
    }

    pub fn find_surface_by_dc(&self, hdc: HDC) -> Option<IDirectDrawSurface> {
        if hdc.is_invalid() {
            return None;
        }
        self.find_surface(|data| data.has_dc(hdc))
    }

    fn find_surface(
        &self,
        matches: impl Fn(&surface::SurfaceData) -> bool,
    ) -> Option<IDirectDrawSurface> {
        self.registry_snapshot()
            .into_iter()
            .find_map(|(interface, data)| {
                if matches(&data.upgrade()?.lock()) {
                    interface.upgrade()
                } else {
                    None
                }
            })
    }

    // The guard is consumed here. Release it before the primary slot, whose
    // destructor may release application COM objects and re-enter DirectDraw.
    pub(super) fn present_locked(&self, guard: MutexGuard<'_, Display>) -> Result<()> {
        let primary = self.primary_slot();
        let mut display = guard;
        display.present_now(self, primary.as_deref())
    }

    fn refresh_locked(&self, guard: MutexGuard<'_, Display>) -> Result<()> {
        let primary = self.primary_slot();
        let mut display = guard;
        display.refresh(self, primary.as_deref())
    }

    fn mark_dirty_and_schedule_flush(&self) {
        self.dirty.store(true, Ordering::Release);
        if !self.flush_scheduled.swap(true, Ordering::AcqRel) && !window::schedule_flush() {
            self.flush_scheduled.store(false, Ordering::Release);
        }
    }

    /// Requests a coalesced presentation. Safe to call from any thread: off-UI worker threads
    /// never block on `Display` or touch `Renderer`/`OverlayWindow`, instead posting `WM_APP_FLUSH`.
    pub fn request_refresh(&self) -> Result<()> {
        if !window::is_ui_thread_or_unbound() {
            self.mark_dirty_and_schedule_flush();
            return Ok(());
        }
        self.refresh_locked(self.lock())
    }

    /// Requests an immediate presentation on the UI thread, or schedules one when called off-UI.
    pub fn request_present_now(&self) -> Result<()> {
        if !window::is_ui_thread_or_unbound() {
            self.mark_dirty_and_schedule_flush();
            return Ok(());
        }
        self.present_locked(self.lock())
    }

    pub fn refresh_if_primary_uses_palette(&self, palette: &IDirectDrawPalette) -> Result<()> {
        let primary = self.primary_slot();
        let Some(primary) = primary else {
            return Ok(());
        };
        let attached = primary.lock().uses_palette(palette);
        if attached {
            self.request_refresh()?;
        }
        Ok(())
    }
}

pub struct Display {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub(super) presentation: Presentation,
    pub(super) owner_draw: ComWeak<IDirectDraw>,
    last_present: Option<Instant>,
    pub(super) consecutive_present_failures: u32,
}

#[derive(Default)]
pub(super) struct Presentation {
    pub(super) bound_hwnd: SendHwnd,
    pub(super) renderer: Option<Renderer>,
    pub(super) host: Option<HostWindow>,
}

impl Display {
    pub(super) fn set_host(&mut self, next: Option<HostWindow>) -> Option<HostWindow> {
        let prev = std::mem::replace(&mut self.presentation.host, next);
        let was_fs = prev.as_ref().is_some_and(HostWindow::is_fullscreen);
        let now_fs = self
            .presentation
            .host
            .as_ref()
            .is_some_and(HostWindow::is_fullscreen);
        if !was_fs && now_fs {
            FULLSCREEN_COUNT.fetch_add(1, Ordering::Relaxed);
        } else if was_fs && !now_fs {
            FULLSCREEN_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
        prev
    }

    fn present_now(&mut self, shared: &DisplayShared, primary: Option<&SurfaceSlot>) -> Result<()> {
        let Some(primary) = primary else {
            shared.dirty.store(false, Ordering::Release);
            shared.flush_scheduled.store(false, Ordering::Release);
            return Ok(());
        };
        if self.presentation.renderer.is_none() {
            let hwnd = self.presentation.bound_hwnd.get();
            if !window::is_window(hwnd) {
                shared.dirty.store(false, Ordering::Release);
                shared.flush_scheduled.store(false, Ordering::Release);
                return Ok(());
            }
            match Renderer::new(hwnd.0 as isize) {
                Ok(renderer) => {
                    self.presentation.renderer = Some(renderer);
                    self.consecutive_present_failures = 0;
                    compat::log_msg("recreated wgpu renderer after device/surface loss");
                }
                Err(err) => {
                    compat::log_msg(&format!("renderer recreate deferred: {err}"));
                    shared.dirty.store(true, Ordering::Release);
                    window::arm_flush_timer(50);
                    return Ok(());
                }
            }
        }

        let result = self
            .presentation
            .renderer
            .as_mut()
            .expect("renderer checked above")
            .present_surface(primary, || {
                // Clear dirty while still holding the primary surface mutex so any worker-thread
                // write that happens after upload sets dirty=true without being clobbered.
                shared.dirty.store(false, Ordering::Release);
            });
        match result {
            Ok(PresentOutcome::Presented) => {
                shared.flush_scheduled.store(false, Ordering::Release);
                self.consecutive_present_failures = 0;
                self.last_present = Some(Instant::now());
                window::disarm_flush_timer();
                Ok(())
            }
            Ok(PresentOutcome::Hidden) => {
                shared.dirty.store(false, Ordering::Release);
                shared.flush_scheduled.store(false, Ordering::Release);
                self.consecutive_present_failures = 0;
                self.last_present = Some(Instant::now());
                window::disarm_flush_timer();
                Ok(())
            }
            Ok(PresentOutcome::Timeout | PresentOutcome::Busy) => {
                shared.dirty.store(true, Ordering::Release);
                shared.flush_scheduled.store(false, Ordering::Release);
                window::arm_flush_timer(16);
                Ok(())
            }
            Err(PresentError::Recoverable(msg)) => {
                self.consecutive_present_failures =
                    self.consecutive_present_failures.saturating_add(1);
                compat::log_msg(&format!(
                    "recoverable present error (#{}): {msg}",
                    self.consecutive_present_failures
                ));
                if self.consecutive_present_failures >= 3 {
                    // Drop and recreate the swapchain/device on the next tick.
                    self.presentation.renderer = None;
                }
                shared.dirty.store(true, Ordering::Release);
                shared.flush_scheduled.store(false, Ordering::Release);
                window::arm_flush_timer(16);
                Ok(())
            }
            Err(PresentError::DeviceLost(msg)) => {
                compat::log_msg(&format!("device lost during present, rebuilding: {msg}"));
                self.presentation.renderer = None;
                shared.dirty.store(true, Ordering::Release);
                shared.flush_scheduled.store(false, Ordering::Release);
                window::arm_flush_timer(50);
                Ok(())
            }
            Err(PresentError::Fatal(msg)) => Err(Error::new(E_FAIL, msg)),
        }
    }

    fn refresh(&mut self, shared: &DisplayShared, primary: Option<&SurfaceSlot>) -> Result<()> {
        if (self.presentation.renderer.is_none() && self.presentation.bound_hwnd.get().is_invalid())
            || primary.is_none()
        {
            return Ok(());
        }
        shared.dirty.store(true, Ordering::Release);
        let force_now = self.last_present.is_none()
            || self
                .last_present
                .is_some_and(|t| t.elapsed() >= FLUSH_MAX_DEFER)
            || !window::can_schedule_flush();
        if force_now {
            return self.present_now(shared, primary);
        }
        if !shared.flush_scheduled.swap(true, Ordering::AcqRel) && !window::schedule_flush() {
            shared.flush_scheduled.store(false, Ordering::Release);
            return self.present_now(shared, primary);
        }
        Ok(())
    }
}

pub(super) struct WindowTransitionGuard<'a>(&'a AtomicBool);

impl<'a> WindowTransitionGuard<'a> {
    pub(super) fn acquire(flag: &'a AtomicBool) -> Result<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self(flag))
            .map_err(|_| Error::from(SURFACE_BUSY))
    }
}

impl Drop for WindowTransitionGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) fn mode_description(width: u32, height: u32, bpp: u32) -> Result<DDSURFACEDESC> {
    Ok(DDSURFACEDESC {
        dwSize: size_of::<DDSURFACEDESC>() as u32,
        dwFlags: (DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_PIXELFORMAT) as u32,
        dwWidth: width,
        dwHeight: height,
        Anonymous1: DDSURFACEDESC_0 {
            lPitch: ((width * (bpp / 8) + 3) & !3) as i32,
        },
        ddpfPixelFormat: surface::pixel_format(bpp)?,
        ..Default::default()
    })
}

pub(super) fn build_driver_caps(size: u32) -> DDCAPS_DX7 {
    let mut caps = DDCAPS_DX7 {
        dwSize: size,
        dwCaps: (DDCAPS_BLT
            | DDCAPS_BLTSTRETCH
            | DDCAPS_BLTCOLORFILL
            | DDCAPS_COLORKEY
            | DDCAPS_CANBLTSYSMEM
            | DDCAPS_CANCLIP
            | DDCAPS_CANCLIPSTRETCHED
            | DDCAPS_OVERLAY
            | DDCAPS_PALETTE) as u32,
        dwCaps2: (DDCAPS2_PRIMARYGAMMA | DDCAPS2_NOPAGELOCKREQUIRED | DDCAPS2_WIDESURFACES) as u32,
        dwCKeyCaps: (DDCKEYCAPS_SRCBLT | DDCKEYCAPS_SRCBLTCLRSPACE | DDCKEYCAPS_DESTBLT) as u32,
        dwFXCaps: (DDFXCAPS_BLTMIRRORLEFTRIGHT
            | DDFXCAPS_BLTMIRRORUPDOWN
            | DDFXCAPS_BLTSTRETCHX
            | DDFXCAPS_BLTSTRETCHY) as u32,
        dwPalCaps: (DDPCAPS_8BIT | DDPCAPS_ALLOW256 | DDPCAPS_PRIMARYSURFACE) as u32,
        dwVidMemTotal: VIRTUAL_VIDMEM_BYTES,
        dwVidMemFree: VIRTUAL_VIDMEM_BYTES,
        ..Default::default()
    };
    caps.ddsOldCaps.dwCaps = (DDSCAPS_PRIMARYSURFACE
        | DDSCAPS_OFFSCREENPLAIN
        | DDSCAPS_SYSTEMMEMORY
        | DDSCAPS_VIDEOMEMORY
        | DDSCAPS_LOCALVIDMEM
        | DDSCAPS_NONLOCALVIDMEM
        | DDSCAPS_OWNDC
        | DDSCAPS_FLIP
        | DDSCAPS_FRONTBUFFER
        | DDSCAPS_BACKBUFFER
        | DDSCAPS_OVERLAY
        | DDSCAPS_COMPLEX) as u32;
    caps.ddsCaps.dwCaps = caps.ddsOldCaps.dwCaps;
    caps
}

fn write_cstr(dst: &mut [i8], text: &str) {
    dst.fill(0);
    let max = dst.len().saturating_sub(1);
    for (slot, byte) in dst.iter_mut().zip(text.bytes()).take(max) {
        *slot = byte as i8;
    }
}

pub(super) fn fill_device_identifier() -> DDDEVICEIDENTIFIER {
    let mut id = DDDEVICEIDENTIFIER::default();
    write_cstr(&mut id.szDriver, "ddraw.dll");
    write_cstr(&mut id.szDescription, "cnc-ddraw wgpu");
    id.liDriverVersion = 0x0001_0000_0000_0000;
    id.guidDeviceIdentifier = GUID::from_u128(0x636e6364_6472_6177_7767_707530303031);
    id
}

pub(super) fn fill_device_identifier2() -> DDDEVICEIDENTIFIER2 {
    let base = fill_device_identifier();
    DDDEVICEIDENTIFIER2 {
        szDriver: base.szDriver,
        szDescription: base.szDescription,
        liDriverVersion: base.liDriverVersion,
        dwVendorId: base.dwVendorId,
        dwDeviceId: base.dwDeviceId,
        dwSubSysId: base.dwSubSysId,
        dwRevision: base.dwRevision,
        guidDeviceIdentifier: base.guidDeviceIdentifier,
        dwWHQLLevel: 0,
    }
}

pub(super) fn make_hwnd(hwnd: HWND) -> SendHwnd {
    SendHwnd(hwnd)
}
