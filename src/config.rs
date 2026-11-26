//! Optional `ddraw.ini` configuration loader compatible with classic cnc-ddraw keys.
#![forbid(unsafe_code)]
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::win32::module_path_at;

const VK_RETURN_CODE: u32 = 0x0D;
const VK_TAB_CODE: u32 = 0x09;
const VK_RCONTROL_CODE: u32 = 0xA3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererBackend {
    Auto,
    Dx12,
    Vulkan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub windowed: bool,
    pub border: bool,
    pub resizable: bool,
    pub width: u32,
    pub height: u32,
    pub pos_x: Option<i32>,
    pub pos_y: Option<i32>,
    pub maintas: bool,
    pub vsync: bool,
    pub maxfps: i32,
    pub renderer: RendererBackend,
    pub devmode: bool,
    pub toggle_fullscreen_vk: u32,
    pub unlock_cursor1_vk: u32,
    pub unlock_cursor2_vk: u32,
    pub singlecpu: bool,
    pub noactivateapp: bool,
    pub log: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            windowed: false,
            border: true,
            resizable: true,
            width: 0,
            height: 0,
            pos_x: None,
            pos_y: None,
            maintas: true,
            vsync: false,
            maxfps: 0,
            renderer: RendererBackend::Auto,
            devmode: false,
            toggle_fullscreen_vk: VK_RETURN_CODE,
            unlock_cursor1_vk: VK_TAB_CODE,
            unlock_cursor2_vk: VK_RCONTROL_CODE,
            singlecpu: true,
            noactivateapp: false,
            log: false,
        }
    }
}

fn preset_noactivateapp(exe_stem: Option<&str>) -> bool {
    let Some(stem) = exe_stem else {
        return false;
    };
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "game"
            | "ra2"
            | "red alert 2"
            | "gamemd"
            | "ra2md"
            | "gamemd-spawn"
            | "yuri's revenge"
            | "sun"
            | "ts"
    )
}

impl Config {
    pub fn min_frame_interval_ms(&self) -> u64 {
        if self.maxfps > 0 {
            (1000 / self.maxfps as u64).max(1)
        } else {
            4
        }
    }

    pub fn for_executable(exe_stem: Option<&str>) -> Self {
        Self {
            noactivateapp: preset_noactivateapp(exe_stem),
            ..Self::default()
        }
    }

    pub fn parse_ini(text: &str, exe_section: Option<&str>) -> Self {
        let mut cfg = Self::for_executable(exe_section);
        let mut ddraw_pairs: Vec<(String, String)> = Vec::new();
        let mut game_pairs: Vec<(String, String)> = Vec::new();
        let mut section = String::new();

        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                continue;
            }
            let Some((raw_key, raw_val)) = line.split_once('=') else {
                continue;
            };
            let key = raw_key.trim().to_ascii_lowercase();
            // Strip inline comments preceded by ';' or '#' unless empty.
            let val = raw_val
                .split([';', '#'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if key.is_empty() {
                continue;
            }
            if section == "ddraw" || section.is_empty() {
                ddraw_pairs.push((key, val));
            } else if let Some(game) = exe_section {
                if section.eq_ignore_ascii_case(game) {
                    game_pairs.push((key, val));
                }
            }
        }

        for (k, v) in ddraw_pairs.into_iter().chain(game_pairs) {
            cfg.apply(&k, &v);
        }
        cfg
    }

    fn apply(&mut self, key: &str, value: &str) {
        match key {
            "windowed" => {
                if let Some(b) = parse_bool(value) {
                    self.windowed = b;
                }
            }
            "fullscreen" => {
                // Classic cnc-ddraw uses `fullscreen=true` for stretched fullscreen and
                // `windowed=true` for windowed mode. Only flip windowed off when explicitly true.
                if parse_bool(value) == Some(true) {
                    self.windowed = false;
                }
            }
            "border" => {
                if let Some(b) = parse_bool(value) {
                    self.border = b;
                }
            }
            "resizable" => {
                if let Some(b) = parse_bool(value) {
                    self.resizable = b;
                }
            }
            "width" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.width = v;
                }
            }
            "height" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.height = v;
                }
            }
            "posx" => {
                if let Ok(v) = value.parse::<i32>() {
                    self.pos_x = if v <= -32000 { None } else { Some(v) };
                }
            }
            "posy" => {
                if let Ok(v) = value.parse::<i32>() {
                    self.pos_y = if v <= -32000 { None } else { Some(v) };
                }
            }
            "maintas" => {
                if let Some(b) = parse_bool(value) {
                    self.maintas = b;
                }
            }
            "vsync" => {
                if let Some(b) = parse_bool(value) {
                    self.vsync = b;
                }
            }
            "maxfps" => {
                if let Ok(v) = value.parse::<i32>() {
                    self.maxfps = v;
                }
            }
            "renderer" => {
                self.renderer = match value.to_ascii_lowercase().as_str() {
                    "dx12" | "d3d12" | "direct3d12" => RendererBackend::Dx12,
                    "vulkan" | "vk" => RendererBackend::Vulkan,
                    _ => RendererBackend::Auto,
                };
            }
            "devmode" => {
                if let Some(b) = parse_bool(value) {
                    self.devmode = b;
                }
            }
            "keytogglefullscreen" => {
                if let Some(vk) = parse_u32_radix(value) {
                    self.toggle_fullscreen_vk = vk;
                }
            }
            "keyunlockcursor1" => {
                if let Some(vk) = parse_u32_radix(value) {
                    self.unlock_cursor1_vk = vk;
                }
            }
            "keyunlockcursor2" => {
                if let Some(vk) = parse_u32_radix(value) {
                    self.unlock_cursor2_vk = vk;
                }
            }
            "singlecpu" => {
                if let Some(b) = parse_bool(value) {
                    self.singlecpu = b;
                }
            }
            "noactivateapp" => {
                if let Some(b) = parse_bool(value) {
                    self.noactivateapp = b;
                }
            }
            "log" => {
                if let Some(b) = parse_bool(value) {
                    self.log = b;
                }
            }
            _ => {}
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}

fn parse_u32_radix(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u32>().ok()
    }
}

fn dll_directory() -> Option<PathBuf> {
    module_path_at(dll_directory as *const () as _).and_then(|p| p.parent().map(Path::to_path_buf))
}

pub fn dll_or_exe_directory() -> Option<PathBuf> {
    dll_directory().or_else(|| {
        std::env::current_exe()
            .ok()?
            .parent()
            .map(Path::to_path_buf)
    })
}

fn load_from_disk() -> Config {
    let exe_path = std::env::current_exe().ok();
    let exe_stem = exe_path
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .map(str::to_string);

    let mut candidates = Vec::new();
    if let Some(dir) = dll_directory() {
        candidates.push(dir.join("ddraw.ini"));
    }
    if let Some(dir) = exe_path.as_ref().and_then(|p| p.parent()) {
        let p = dir.join("ddraw.ini");
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }

    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Config::parse_ini(&text, exe_stem.as_deref());
        }
    }
    Config::for_executable(exe_stem.as_deref())
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get() -> &'static Config {
    CONFIG.get_or_init(load_from_disk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ddraw_section_and_game_override() {
        let ini = r#"
            ; global defaults
            [ddraw]
            windowed = true
            border = no
            resizable = true
            width = 1280
            height = 960
            posX = 100
            posY = -32000
            maintas = false
            vsync = yes
            maxfps = 60
            renderer = vulkan
            keytogglefullscreen = 0x0D

            [game]
            border = true
            renderer = dx12
            width = 1024
        "#;
        let cfg = Config::parse_ini(ini, Some("game"));
        assert!(cfg.windowed);
        assert!(cfg.border);
        assert!(cfg.resizable);
        assert_eq!(cfg.width, 1024);
        assert_eq!(cfg.height, 960);
        assert_eq!(cfg.pos_x, Some(100));
        assert_eq!(cfg.pos_y, None);
        assert!(!cfg.maintas);
        assert!(cfg.vsync);
        assert_eq!(cfg.maxfps, 60);
        assert_eq!(cfg.min_frame_interval_ms(), 16);
        assert_eq!(cfg.renderer, RendererBackend::Dx12);
        assert!(!cfg.devmode);
        assert_eq!(cfg.toggle_fullscreen_vk, 0x0D);
        assert_eq!(cfg.unlock_cursor1_vk, 0x09);
        assert_eq!(cfg.unlock_cursor2_vk, 0xA3);
        assert!(cfg.singlecpu);
        assert!(cfg.noactivateapp);
    }

    #[test]
    fn defaults_match_borderless_fullscreen() {
        let cfg = Config::default();
        assert!(!cfg.windowed);
        assert!(cfg.border);
        assert!(cfg.resizable);
        assert!(cfg.maintas);
        assert!(!cfg.vsync);
        assert!(cfg.singlecpu);
        assert!(!cfg.noactivateapp);
        assert_eq!(cfg.min_frame_interval_ms(), 4);
        assert_eq!(cfg.renderer, RendererBackend::Auto);
        assert!(!cfg.devmode);
        assert_eq!(cfg.toggle_fullscreen_vk, 0x0D);
        assert_eq!(cfg.unlock_cursor1_vk, 0x09);
        assert_eq!(cfg.unlock_cursor2_vk, 0xA3);
    }

    #[test]
    fn preset_enables_noactivateapp_for_ra2_and_allows_ini_override() {
        let preset = Config::for_executable(Some("game"));
        assert!(preset.noactivateapp);
        assert!(preset.singlecpu);

        let overridden = Config::parse_ini(
            "[ddraw]\nsinglecpu=false\nnoactivateapp=false\nlog=true\n",
            Some("game"),
        );
        assert!(!overridden.singlecpu);
        assert!(!overridden.noactivateapp);
        assert!(overridden.log);
    }
}
