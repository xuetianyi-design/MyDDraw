# cnc-ddraw — Rust / wgpu experiment

This branch builds a 32-bit Windows `ddraw.dll` from Rust. A DirectDraw 1–7 client calls `DirectDrawCreate`, `DirectDrawCreateEx`, or COM `CoCreateInstance`, binds its HWND with `SetCooperativeLevel`, sets a logical display mode, and creates primary or offscreen surfaces. Updates to the primary surface produce frames through wgpu. The default Cargo target is `i686-pc-windows-msvc`.

## Build on Windows

Install Rust and the Visual Studio C++ x86 tools with the Windows SDK. From the **x86 Native Tools Command Prompt**, run

```bat
rustup target add i686-pc-windows-msvc
cargo build --locked --release
```

The DLL is written to `target\i686-pc-windows-msvc\release\ddraw.dll`. Place it beside a 32-bit DirectDraw executable. wgpu selects the adapter; set `WGPU_BACKEND=dx12` or `WGPU_BACKEND=vulkan` to select one backend explicitly.

To cross-compile on macOS or Linux, put the LLVM-MinGW UCRT toolchain's `bin` directory on PATH and run

```sh
rustup target add i686-pc-windows-gnullvm
cargo build --locked --release --target i686-pc-windows-gnullvm
```

The output is `target/i686-pc-windows-gnullvm/release/ddraw.dll`. To load the local build in Wine, use the native DLL override `WINEDLLOVERRIDES=ddraw=n`.

## Configuration (`ddraw.ini`)

Place an optional `ddraw.ini` next to `ddraw.dll` (or the game executable) using standard `[ddraw]` keys (with optional `[<exe_name>]` per-game overrides):

```ini
[ddraw]
windowed=false           ; start in windowed mode instead of borderless fullscreen
border=true              ; show window titlebar/frame in windowed mode
resizable=true           ; allow dragging window edges to resize
width=0                  ; windowed client width (0 = follow game logical resolution)
height=0                 ; windowed client height (0 = follow game logical resolution)
posX=-32000              ; windowed X position (-32000 = center on monitor)
posY=-32000              ; windowed Y position (-32000 = center on monitor)
maintas=true             ; preserve aspect ratio (false = stretch to fill window)
vsync=false              ; true = Fifo vsync, false = low-latency Mailbox/Immediate
maxfps=0                 ; 0 = coalesced ~250 Hz cap, >0 = explicit FPS cap
renderer=auto            ; auto, dx12, or vulkan (overridden by WGPU_BACKEND env var)
devmode=false            ; true = disable automatic cursor locking inside the window
keytogglefullscreen=0x0D ; Alt+key toggle between borderless fullscreen and windowed (0 = off)
keyunlockcursor1=0x09    ; Ctrl+key toggle cursor lock (default Tab, 0 = off)
keyunlockcursor2=0xA3    ; RightAlt+key toggle cursor lock (default Right Ctrl, 0 = off)
singlecpu=true           ; pin game-directory threads to CPU0 to prevent multi-core engine freezes
noactivateapp=false      ; shield focus deactivation messages (auto-enabled for RA2/TS presets)
log=false                ; write diagnostic events to cnc-ddraw.log (or set CNC_DDRAW_LOG=path)
```

Press **Alt+Enter** at any time to toggle between borderless fullscreen and windowed mode.

## Implementation

- `src/draw.rs` implements `IDirectDraw`, `IDirectDraw2`, `IDirectDraw4`, and `IDirectDraw7`, along with `DirectDrawCreate`, `DirectDrawCreateEx`, `DirectDrawCreateClipper`, device/mode/surface enumeration, vblank/scanline timing, and coalesced frame flushing.
- `src/window/` encapsulates all Win32 windowing concerns: `host.rs` (borderless fullscreen / windowed lifecycle and Alt+Enter toggle), `overlay.rs` (owned presentation overlay HWND and Z-order management), `input.rs` (viewport coordinate mapping, dialog hit-testing, and mouse routing), and `hooks.rs` (PE32 IAT hooks, WndProc subclassing, and `WH_GETMESSAGE`).
- `src/config.rs` parses `ddraw.ini` (`[ddraw]` + per-executable overrides).
- `src/surface.rs` implements `IDirectDrawSurface`, `IDirectDrawSurface2`, `IDirectDrawSurface3`, `IDirectDrawSurface4`, `IDirectDrawSurface7`, and `IDirectDrawGammaControl`. It owns CPU-backed pixels for primary, offscreen, multi-backbuffer flip chains, and overlay surfaces, supporting rectangular locks, color/depth fills, ROP fills, same- and cross-format blits, stretching, mirroring, source/destination color keys, clipper clipping, `BltBatch`, private data, uniqueness/priority/LOD state, gamma ramps, and GDI drawing through a DIB.
- `src/clipper.rs` implements `IDirectDrawClipper` backed by Win32 GDI regions (`SetClipList`/`GetClipList`/`SetHWnd`/`GetHWnd`).
- `src/palette.rs` stores the 256-entry `IDirectDrawPalette`. Changing an attached palette refreshes the primary image even when the pixel indices stay the same.
- `src/exports.rs`, `exports.def`, and `build.rs` provide standard DirectDraw export ordinals (`@1`..`@21`), reentrant `AcquireDDThreadLock`/`ReleaseDDThreadLock`, legacy stubs (`CompleteCreateSysmemSurface`, `D3DParseUnknownCommand`, `DDInternalLock`/`DDInternalUnlock`), helper exports (`DDIsWindowed`, `DDGetProcAddress`, `DDEnableZoom`), and `DllGetClassObject`/`IClassFactory` for `CLSID_DirectDraw`, `CLSID_DirectDraw7`, and `CLSID_DirectDrawClipper`.
- `src/renderer.rs` and `src/present.wgsl` share one presentation pipeline across Vulkan and DX12. Raw primary surface bytes (`R8Uint` indexed, `R16Uint` RGB555/RGB565, `R32Uint` XRGB8888) are uploaded directly without CPU conversion and unpacked in WGSL with a 256-entry palette/gamma LUT texture into a centered, aspect-preserving (or stretched) viewport.
- `src/simd_blit.rs` uses `fearless_simd` for runtime-dispatched (SSE2 / SSE4.2 / AVX2 / AVX-512) branchless CPU color-keyed blits and rectangular color fills on CPU-backed surfaces.

`src/descriptor.rs` checks DirectDraw structure sizes at the COM boundary and reads the fields selected by the caller's flags. Output descriptors can arrive with only `dwSize` initialized. Set `DDPF_RGB` when supplying true-color depths and masks. `EnumDisplayModes` treats a pixel format with no flags as a wildcard. Surface creation and resizing copy the visible pixels from each input row. `src/surface/dc.rs` owns the GDI bitmap and accounts for its scanline stride separately from the game's pitch.

The CPU blitter, SIMD kernels, configuration parser and dialog template parser use `#![forbid(unsafe_code)]`. The crate also enforces `unsafe_op_in_unsafe_fn`, so native entry points need an explicit block for each group of unsafe operations. For DPI scaling, indirect standard dialog templates with nonzero control creation data use the original Win32 template. Resource-backed templates have a known length and can be copied in full.

The implemented DirectDraw COM objects are single-threaded. Release the display and its surfaces, or detach the HWND with `SetCooperativeLevel(NULL, DDSCL_NORMAL)`, before destroying the window. Direct3D 3D game rendering remains outside this experiment's scope.

## Verify

The SDK smoke test is a separate C executable. It loads the DLL next to the executable by path, resolves exports by name and ordinal, and calls the interfaces using the SDK's x86 COM declarations.

```bat
cargo test --locked
cl /nologo tests\smoke.c user32.lib gdi32.lib /Fe:target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe --present
```

The default run checks 8/16/32-bit surfaces, odd-width pitches, palettes, GDI writes, overlapping blits, busy surfaces, flipping, DD2/4/7 and Surface2/3/4/7 interfaces, clippers, gamma control, multi-backbuffer chains, Blt FX/ROP/batching, overlays, vblank/scanline queries, export ordinals, thread lock, and COM class factories. `--present` also creates a real window, presents through wgpu, and checks fullscreen entry and restoration.
