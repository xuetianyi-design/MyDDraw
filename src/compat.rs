//! Compatibility helpers: Win11-24H2-safe per-game-thread CPU0 affinity (`singlecpu`)
//! and optional opt-in diagnostic logging (`CNC_DDRAW_LOG` or `ddraw.ini` `log=true`).
#![forbid(unsafe_code)]
#![allow(non_snake_case)]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::config;
use crate::win32::process::{
    current_process_id, current_thread_id, module_path_at, pin_current_thread_to_cpu0,
    pin_thread_to_cpu0_if, prewarm_thread_query, process_thread_ids,
};

static SINGLECPU_ACTIVE: AtomicBool = AtomicBool::new(false);
static GAME_DIR_LOWER: OnceLock<Option<String>> = OnceLock::new();
static LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn game_dir_lower() -> Option<&'static str> {
    GAME_DIR_LOWER
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .map(|p| p.to_string_lossy().to_ascii_lowercase())
        })
        .as_deref()
}

fn pin_thread_by_id(tid: u32) {
    pin_thread_to_cpu0_if(tid, |start_query| {
        let Some(start_opt) = start_query else {
            return true;
        };
        let Some(game_dir) = game_dir_lower() else {
            return true;
        };
        let Some(start) = start_opt else {
            return false;
        };
        let Some(mod_dir) = module_path_at(start)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|p| p.to_string_lossy().to_ascii_lowercase())
        else {
            return false;
        };
        mod_dir.starts_with(game_dir)
    });
}

/// Pins the current game thread and all threads originating from modules in the game directory
/// to CPU0 while leaving GPU driver / wgpu worker threads free to run on all cores.
pub fn apply_single_cpu_affinity() {
    if !config::get().singlecpu {
        return;
    }
    // Always pin the calling (game UI/DirectDraw) thread to CPU0.
    pin_current_thread_to_cpu0();
    if SINGLECPU_ACTIVE.swap(true, Ordering::Relaxed) {
        return;
    }
    // Pre-initialize OnceLocks outside DllMain loader lock before enabling DLL_THREAD_ATTACH.
    let _ = game_dir_lower();
    prewarm_thread_query();

    let pid = current_process_id();
    for tid in process_thread_ids(pid) {
        pin_thread_by_id(tid);
    }
    log_msg("singlecpu affinity enabled for game threads");
}

/// Called from `DllMain(DLL_THREAD_ATTACH)` once single-CPU mode has been activated.
pub fn on_thread_attach() {
    if !SINGLECPU_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    pin_thread_by_id(current_thread_id());
}

fn open_log_file() -> Option<Mutex<std::fs::File>> {
    let path = if let Some(env_path) = std::env::var_os("CNC_DDRAW_LOG") {
        let p = PathBuf::from(env_path);
        if p.as_os_str().is_empty() {
            return None;
        }
        p
    } else if config::get().log {
        config::dll_or_exe_directory()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cnc-ddraw.log")
    } else {
        return None;
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
        .map(Mutex::new)
}

pub fn is_logging_enabled() -> bool {
    LOG_FILE.get_or_init(open_log_file).is_some()
}

pub fn log_msg(msg: &str) {
    let Some(file_mutex) = LOG_FILE.get_or_init(open_log_file).as_ref() else {
        return;
    };
    if let Ok(mut file) = file_mutex.lock() {
        let tid = current_thread_id();
        let _ = writeln!(file, "[tid={tid}] {msg}");
        let _ = file.flush();
    }
}
