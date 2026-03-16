#![allow(unreachable_patterns)]
use gpui::prelude::*;
use gpui::*;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

// Voice input is disabled for v1 ship. Set to true to re-enable.
const VOICE_ENABLED: bool = false;

fn list_subdirs(parent: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    dirs.push(entry.path().display().to_string());
                }
            }
        }
    }
    dirs.sort();
    dirs
}

fn list_home_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    list_subdirs(&home)
}

// ── OpenRouter API ──────────────────────────────────────────────

fn fetch_opencode_model_list() -> Vec<ModelOption> {
    let output = match Command::new("opencode").args(["models"]).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut models: Vec<ModelOption> = stdout.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let id = l.trim().to_string();
            let free = id.ends_with(":free");
            // Create a readable label from the id: "openrouter/anthropic/claude-sonnet-4.6" -> "claude-sonnet-4.6 (openrouter/anthropic)"
            let label = id.clone();
            ModelOption { id, label, free }
        })
        .collect();
    // Sort: provider groups, then alphabetically
    models.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    models
}

fn fetch_cursor_model_list() -> Vec<ModelOption> {
    let output = match Command::new("cursor").args(["agent", "models"]).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut models: Vec<ModelOption> = stdout.lines()
        .filter(|l| !l.trim().is_empty() && !l.contains("Available models") && !l.contains("Tip:") && !l.contains("Loading"))
        .filter_map(|l| {
            // Format: "model-id - Display Name  (current, default)"
            let clean = l.trim();
            // Strip ANSI escape codes
            let stripped: String = clean.chars().filter(|c| !c.is_control()).collect();
            let stripped = stripped.trim();
            if stripped.is_empty() { return None; }
            let parts: Vec<&str> = stripped.splitn(2, " - ").collect();
            let id = parts[0].trim().to_string();
            let label = if parts.len() > 1 {
                format!("{} ({})", parts[1].trim(), id)
            } else { id.clone() };
            if id.is_empty() { return None; }
            Some(ModelOption { id, label, free: false })
        })
        .collect();
    models.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    models
}

// ── Asset loading ──────────────────────────────────────────────
struct Assets;

impl Assets {
    fn resolve(path: &str) -> Option<PathBuf> {
        // 1. CARGO_MANIFEST_DIR (dev builds)
        if let Some(dir) = option_env!("CARGO_MANIFEST_DIR") {
            let p = PathBuf::from(dir).join(path);
            if p.exists() { return Some(p); }
        }
        if let Ok(exe) = std::env::current_exe() {
            // 2. Next to binary (flat install)
            if let Some(bin_dir) = exe.parent() {
                let p = bin_dir.join(path);
                if p.exists() { return Some(p); }
                // 3. ../Resources/ (.app bundle: Contents/MacOS/../Resources/)
                let p = bin_dir.join("../Resources").join(path);
                if p.exists() { return Some(p); }
            }
        }
        None
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match Self::resolve(path) {
            Some(full) => Ok(Some(std::fs::read(&full)?.into())),
            None => Ok(None),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        match Self::resolve(path) {
            Some(dir) if dir.is_dir() => {
                Ok(std::fs::read_dir(dir)?
                    .filter_map(|e| Some(SharedString::from(e.ok()?.path().to_string_lossy().into_owned())))
                    .collect())
            }
            _ => Ok(vec![]),
        }
    }
}

// ── Theme ───────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThemeColors {
    bg: u32,
    surface: u32,
    surface_raised: u32,
    border: u32,
    border_focus: u32,
    text: u32,
    text_muted: u32,
    text_faint: u32,
    green: u32,
    yellow: u32,
    red: u32,
    blue: u32,
    blue_muted: u32,
    user_input: u32,
    palette_bg: u32,
    palette_border: u32,
    selected_row: u32,
    // Visual polish
    shadow: u32,          // shadow color (semi-transparent)
    glow_focus: u32,      // glow around focused tile
    diff_add_bg: u32,     // background tint for + lines
    diff_remove_bg: u32,  // background tint for - lines
    diff_hunk_bg: u32,    // background tint for @@ lines
    tool_call_bg: u32,    // background for tool call cards
    tool_call_accent: u32, // left border accent on tool call cards
    header_gradient_start: u32,
    header_gradient_end: u32,
}

impl ThemeColors {
    fn c(&self, v: u32) -> Rgba { rgba(v) }
    fn bg(&self) -> Rgba { self.c(self.bg) }
    fn surface(&self) -> Rgba { self.c(self.surface) }
    fn surface_raised(&self) -> Rgba { self.c(self.surface_raised) }
    fn border(&self) -> Rgba { self.c(self.border) }
    fn border_focus(&self) -> Rgba { self.c(self.border_focus) }
    fn text(&self) -> Rgba { self.c(self.text) }
    fn text_muted(&self) -> Rgba { self.c(self.text_muted) }
    fn text_faint(&self) -> Rgba { self.c(self.text_faint) }
    fn green(&self) -> Rgba { self.c(self.green) }
    fn yellow(&self) -> Rgba { self.c(self.yellow) }
    fn red(&self) -> Rgba { self.c(self.red) }
    fn blue(&self) -> Rgba { self.c(self.blue) }
    fn blue_muted(&self) -> Rgba { self.c(self.blue_muted) }
    fn user_input(&self) -> Rgba { self.c(self.user_input) }
    fn palette_bg(&self) -> Rgba { self.c(self.palette_bg) }
    fn palette_border(&self) -> Rgba { self.c(self.palette_border) }
    fn selected_row(&self) -> Rgba { self.c(self.selected_row) }
    fn shadow(&self) -> Rgba { self.c(self.shadow) }
    fn glow_focus(&self) -> Rgba { self.c(self.glow_focus) }
    fn diff_add_bg(&self) -> Rgba { self.c(self.diff_add_bg) }
    fn diff_remove_bg(&self) -> Rgba { self.c(self.diff_remove_bg) }
    fn diff_hunk_bg(&self) -> Rgba { self.c(self.diff_hunk_bg) }
    fn header_gradient_start(&self) -> Rgba { self.c(self.header_gradient_start) }
    fn header_gradient_end(&self) -> Rgba { self.c(self.header_gradient_end) }

    /// Color accent for a runtime. Used for the tile border glow and sidebar dot.
    fn runtime_color(&self, runtime: &str) -> Rgba {
        match runtime {
            "claude"  => rgba(0xE8915Aff), // orange
            "codex"   => rgba(0x7CCCF0ff), // light blue
            "cursor"  => rgba(0x888888ff), // gray (dark theme friendly)
            "opencode" => rgba(0xDDDDDDff), // white-ish
            _         => self.text_muted(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThemeDef {
    name: String,
    colors: ThemeColors,
}

fn builtin_themes() -> Vec<ThemeDef> {
    vec![
        ThemeDef {
            name: "midnight".into(),
            colors: ThemeColors {
                bg: 0x0f1117ff, surface: 0x161922ff, surface_raised: 0x1c2030ff,
                border: 0x282d3eff, border_focus: 0x5b8aefff,
                text: 0xd4d7e0ff, text_muted: 0x7a8194ff, text_faint: 0x4a5068ff,
                green: 0x6dcb8aff, yellow: 0xe5c07bff, red: 0xe06c75ff,
                blue: 0x5b8aefff, blue_muted: 0x4a6bbfff, user_input: 0x7eb8e0ff,
                palette_bg: 0x1a1e2cff, palette_border: 0x3a4060ff, selected_row: 0x252a3cff,
                shadow: 0x00000060, glow_focus: 0x5b8aef30,
                diff_add_bg: 0x6dcb8a15, diff_remove_bg: 0xe06c7515, diff_hunk_bg: 0x5b8aef10,
                tool_call_bg: 0x1c203080, tool_call_accent: 0x5b8aefff,
                header_gradient_start: 0x161922ff, header_gradient_end: 0x1c2030ff,
            },
        },
        ThemeDef {
            name: "charcoal".into(),
            colors: ThemeColors {
                bg: 0x1a1a1aff, surface: 0x242424ff, surface_raised: 0x2e2e2eff,
                border: 0x3a3a3aff, border_focus: 0x7c9cf5ff,
                text: 0xe0e0e0ff, text_muted: 0x888888ff, text_faint: 0x555555ff,
                green: 0x7ec87eff, yellow: 0xd4aa4fff, red: 0xd46a6aff,
                blue: 0x7c9cf5ff, blue_muted: 0x5c7ccfff, user_input: 0x8fc4e8ff,
                palette_bg: 0x222222ff, palette_border: 0x444444ff, selected_row: 0x2c2c2cff,
                shadow: 0x00000060, glow_focus: 0x7c9cf530,
                diff_add_bg: 0x7ec87e15, diff_remove_bg: 0xd46a6a15, diff_hunk_bg: 0x7c9cf510,
                tool_call_bg: 0x2e2e2e80, tool_call_accent: 0x7c9cf5ff,
                header_gradient_start: 0x242424ff, header_gradient_end: 0x2e2e2eff,
            },
        },
        ThemeDef {
            name: "gruvbox".into(),
            colors: ThemeColors {
                bg: 0x282828ff, surface: 0x3c3836ff, surface_raised: 0x504945ff,
                border: 0x665c54ff, border_focus: 0x83a598ff,
                text: 0xebdbb2ff, text_muted: 0xa89984ff, text_faint: 0x7c6f64ff,
                green: 0xb8bb26ff, yellow: 0xfabd2fff, red: 0xfb4934ff,
                blue: 0x83a598ff, blue_muted: 0x458588ff, user_input: 0x8ec07cff,
                palette_bg: 0x32302fff, palette_border: 0x665c54ff, selected_row: 0x3c3836ff,
                shadow: 0x00000060, glow_focus: 0x83a59830,
                diff_add_bg: 0xb8bb2615, diff_remove_bg: 0xfb493415, diff_hunk_bg: 0x83a59810,
                tool_call_bg: 0x50494580, tool_call_accent: 0x83a598ff,
                header_gradient_start: 0x3c3836ff, header_gradient_end: 0x504945ff,
            },
        },
        ThemeDef {
            name: "solarized-dark".into(),
            colors: ThemeColors {
                bg: 0x002b36ff, surface: 0x073642ff, surface_raised: 0x0a4050ff,
                border: 0x586e75ff, border_focus: 0x268bd2ff,
                text: 0x839496ff, text_muted: 0x657b83ff, text_faint: 0x586e75ff,
                green: 0x859900ff, yellow: 0xb58900ff, red: 0xdc322fff,
                blue: 0x268bd2ff, blue_muted: 0x2176a8ff, user_input: 0x2aa198ff,
                palette_bg: 0x073642ff, palette_border: 0x586e75ff, selected_row: 0x0a4050ff,
                shadow: 0x00000060, glow_focus: 0x268bd230,
                diff_add_bg: 0x85990015, diff_remove_bg: 0xdc322f15, diff_hunk_bg: 0x268bd210,
                tool_call_bg: 0x0a405080, tool_call_accent: 0x268bd2ff,
                header_gradient_start: 0x073642ff, header_gradient_end: 0x0a4050ff,
            },
        },
        ThemeDef {
            name: "light".into(),
            colors: ThemeColors {
                bg: 0xf5f5f5ff, surface: 0xeaeaeaff, surface_raised: 0xe0e0e0ff,
                border: 0xccccccff, border_focus: 0x4078c0ff,
                text: 0x24292eff, text_muted: 0x586069ff, text_faint: 0x8b949eff,
                green: 0x22863aff, yellow: 0xb08800ff, red: 0xcb2431ff,
                blue: 0x4078c0ff, blue_muted: 0x6c9bd2ff, user_input: 0x0366d6ff,
                palette_bg: 0xf0f0f0ff, palette_border: 0xccccccff, selected_row: 0xe4e4e4ff,
                shadow: 0x00000020, glow_focus: 0x4078c020,
                diff_add_bg: 0x22863a12, diff_remove_bg: 0xcb243112, diff_hunk_bg: 0x4078c00c,
                tool_call_bg: 0xe0e0e080, tool_call_accent: 0x4078c0ff,
                header_gradient_start: 0xeaeaeaff, header_gradient_end: 0xe0e0e0ff,
            },
        },
        ThemeDef {
            name: "solarized-light".into(),
            colors: ThemeColors {
                bg: 0xfdf6e3ff, surface: 0xeee8d5ff, surface_raised: 0xe8e1cbff,
                border: 0xd3cbb7ff, border_focus: 0x268bd2ff,
                text: 0x657b83ff, text_muted: 0x839496ff, text_faint: 0x93a1a1ff,
                green: 0x859900ff, yellow: 0xb58900ff, red: 0xdc322fff,
                blue: 0x268bd2ff, blue_muted: 0x2176a8ff, user_input: 0x2aa198ff,
                palette_bg: 0xeee8d5ff, palette_border: 0xd3cbb7ff, selected_row: 0xe8e1cbff,
                shadow: 0x00000020, glow_focus: 0x268bd220,
                diff_add_bg: 0x85990012, diff_remove_bg: 0xdc322f12, diff_hunk_bg: 0x268bd20c,
                tool_call_bg: 0xe8e1cb80, tool_call_accent: 0x268bd2ff,
                header_gradient_start: 0xeee8d5ff, header_gradient_end: 0xe8e1cbff,
            },
        },
        ThemeDef {
            name: "ops".into(),
            colors: ThemeColors {
                bg: 0x08080cff, surface: 0x0e0e14ff, surface_raised: 0x14141cff,
                border: 0x1e1e2aff, border_focus: 0x44ff88ff,
                text: 0xc8ccd0ff, text_muted: 0x5a5e6aff, text_faint: 0x33364000,
                green: 0x44ff88ff, yellow: 0xffcc44ff, red: 0xff4466ff,
                blue: 0x44aaffff, blue_muted: 0x2277aaff, user_input: 0x66eeccff,
                palette_bg: 0x0c0c12ff, palette_border: 0x2a2a38ff, selected_row: 0x16162200,
                shadow: 0x00000080, glow_focus: 0x44ff8830,
                diff_add_bg: 0x44ff8812, diff_remove_bg: 0xff446612, diff_hunk_bg: 0x44aaff0c,
                tool_call_bg: 0x14141c80, tool_call_accent: 0x44aaffff,
                header_gradient_start: 0x0e0e14ff, header_gradient_end: 0x14141cff,
            },
        },
        ThemeDef {
            name: "monokai-pro".into(),
            colors: ThemeColors {
                bg: 0x2d2a2eff, surface: 0x221f22ff, surface_raised: 0x19181aff,
                border: 0x5b595cff, border_focus: 0xff6188ff,
                text: 0xfcfcfaff, text_muted: 0x939293ff, text_faint: 0x727072ff,
                green: 0xa9dc76ff, yellow: 0xffd866ff, red: 0xff6188ff,
                blue: 0x78dce8ff, blue_muted: 0x5ad4e6ff, user_input: 0xab9df2ff,
                palette_bg: 0x221f22ff, palette_border: 0x5b595cff, selected_row: 0x3a373cff,
                shadow: 0x00000070, glow_focus: 0xff618830,
                diff_add_bg: 0xa9dc7618, diff_remove_bg: 0xff618818, diff_hunk_bg: 0x78dce812,
                header_gradient_start: 0x221f22ff, header_gradient_end: 0x2d2a2eff,
                tool_call_bg: 0x19181a90, tool_call_accent: 0xab9df2ff,
            },
        },
    ]
}

// ── Starfield ───────────────────────────────────────────────────

struct Star {
    x: f32, // 0.0..1.0 normalized
    y: f32,
    size: f32,    // radius in px
    brightness: f32, // base brightness 0.0..1.0
    phase: f32,  // twinkle phase offset
    speed: f32,  // twinkle speed
}

fn generate_stars(count: usize, seed: u64) -> Vec<Star> {
    let mut stars = Vec::with_capacity(count);
    // Simple LCG pseudo-random for deterministic star positions
    let mut rng = seed;
    let next = |r: &mut u64| -> f32 {
        *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*r >> 33) as f32) / (u32::MAX as f32 / 2.0)
    };
    for _ in 0..count {
        let x = next(&mut rng);
        let y = next(&mut rng);
        let size = 0.4 + next(&mut rng) * 1.2; // 0.4 to 1.6px
        let brightness = 0.15 + next(&mut rng) * 0.85;
        let phase = next(&mut rng) * std::f32::consts::TAU;
        let speed = 0.3 + next(&mut rng) * 1.5;
        stars.push(Star { x, y, size, brightness, phase, speed });
    }
    stars
}


// ── Config (persisted to ~/.opensquirrel/config.toml) ───────────

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".opensquirrel").join("config.toml")
}

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".opensquirrel").join("state.json")
}

fn whisper_model_path(model_name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".opensquirrel")
        .join("models")
        .join(format!("ggml-{}.bin", model_name))
}

fn whisper_model_exists(model_name: &str) -> bool {
    whisper_model_path(model_name).exists()
}

fn list_audio_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for dev in devices {
            #[allow(deprecated)]
            if let Ok(name) = dev.name() {
                names.push(name);
            }
        }
    }
    names
}

fn find_audio_device_by_name(name: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if name.is_empty() {
        return host.default_input_device();
    }
    if let Ok(devices) = host.input_devices() {
        for dev in devices {
            #[allow(deprecated)]
            if let Ok(dev_name) = dev.name() {
                if dev_name == name {
                    return Some(dev);
                }
            }
        }
    }
    host.default_input_device()
}

// ── Saved state (persisted to ~/.opensquirrel/state.json) ──────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedAgentState {
    name: String,
    group: String,
    runtime_name: String,
    #[serde(default = "default_saved_target_machine")]
    target_machine: String,
    #[serde(default = "default_saved_agent_role")]
    role: String,
    #[serde(default)]
    parent_name: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    task_title: Option<String>,
    session_id: Option<String>,
    model: String,
    output_lines: Vec<String>,
    message_count: u32,
    scroll_offset: usize,
    favorite: bool,
    auto_scroll: bool,
    working_dir: String,
    #[serde(default)]
    remote_session_name: Option<String>,
    #[serde(default)]
    remote_line_cursor: usize,
    cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    #[serde(default)]
    pending_prompt: Option<String>,
    #[serde(default = "default_saved_turn_state")]
    turn_state: String,
    tool_edits: u32,
    tool_reads: u32,
    tool_bash: u32,
    tool_writes: u32,
    tool_other: u32,
}

fn default_saved_target_machine() -> String { "local".into() }
fn default_saved_agent_role() -> String { "coordinator".into() }
fn default_saved_turn_state() -> String { "ready".into() }

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedAppState {
    agents: Vec<SavedAgentState>,
    groups: Vec<String>,
    focused_group: usize,
    focused_agent: usize,
    view_mode: String,
    ui_scale: f32,
    sidebar_tab: String,
}

impl SavedAppState {
    fn save(&self) {
        let path = state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn load() -> Option<Self> {
        let path = state_path();
        if !path.exists() { return None; }
        let contents = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&contents).ok()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RuntimeDef {
    name: String,
    command: String,
    args: Vec<String>,
    env_remove: Vec<String>,
    #[serde(default)]
    env_set: Vec<(String, String)>, // env vars to inject (e.g. OPENAI_BASE_URL, OPENAI_API_KEY)
    description: String,
    models: Vec<ModelOption>,
    model_flag: String, // how to pass model to CLI, e.g. "--model" or "-c model="
    last_model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ModelOption {
    id: String,
    label: String,
    free: bool, // available without paid API key
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct McpDef {
    name: String,
    command: String,
    args: Vec<String>,
    description: String,
    global: bool, // true = always available, false = opt-in per agent
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MachineDef {
    name: String,
    kind: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    workdir: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AppConfig {
    runtimes: Vec<RuntimeDef>,
    mcps: Vec<McpDef>,
    #[serde(default)]
    machines: Vec<MachineDef>,
    last_runtime: String,
    #[serde(default = "default_saved_target_machine")]
    last_machine: String,
    #[serde(default)]
    cautious_enter: bool,
    #[serde(default)]
    terminal_text: bool,
    last_mcps: Vec<String>,
    groups: Vec<String>,
    theme: String,
    font_family: String,
    font_size: f32,
    #[serde(default = "default_whisper_model")]
    whisper_model: String,
    #[serde(default)]
    audio_device: String,
    #[serde(default = "default_bg_opacity")]
    bg_opacity: f32,
    #[serde(default = "default_bg_blur")]
    bg_blur: f32,
}

fn default_bg_opacity() -> f32 {
    1.0
}

fn default_bg_blur() -> f32 {
    0.0
}

fn default_whisper_model() -> String {
    "large-v3-turbo".into()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runtimes: vec![
                RuntimeDef {
                    name: "claude".into(),
                    command: "claude".into(),
                    args: vec!["-p".into(), "--output-format".into(), "stream-json".into(), "--verbose".into(), "--dangerously-skip-permissions".into()],
                    env_remove: vec!["CLAUDECODE".into()],
                    env_set: vec![],
                    description: "Claude Code (Anthropic)".into(),
                    models: vec![
                        ModelOption { id: "claude-opus-4-6".into(), label: "Opus 4.6".into(), free: false },
                        ModelOption { id: "claude-sonnet-4-6".into(), label: "Sonnet 4.6".into(), free: false },
                        ModelOption { id: "claude-haiku-4-5-20251001".into(), label: "Haiku 4.5".into(), free: false },
                    ],
                    model_flag: "--model".into(),
                    last_model: "claude-opus-4-6".into(),
                },
                RuntimeDef {
                    name: "codex".into(),
                    command: "codex".into(),
                    args: vec!["exec".into(), "--json".into(), "--dangerously-bypass-approvals-and-sandbox".into()],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "Codex (OpenAI)".into(),
                    models: vec![
                        ModelOption { id: "o3".into(), label: "o3".into(), free: false },
                        ModelOption { id: "o4-mini".into(), label: "o4-mini".into(), free: true },
                        ModelOption { id: "gpt-4.1".into(), label: "GPT-4.1".into(), free: true },
                        ModelOption { id: "gpt-4.1-mini".into(), label: "GPT-4.1 Mini".into(), free: true },
                        ModelOption { id: "gpt-5.3-codex".into(), label: "GPT-5.3 Codex".into(), free: false },
                        ModelOption { id: "codex-mini-latest".into(), label: "Codex Mini".into(), free: true },
                    ],
                    model_flag: "-c".into(), // passed as -c model="X"
                    last_model: "o4-mini".into(),
                },
                RuntimeDef {
                    name: "opencode".into(),
                    command: "opencode".into(),
                    args: vec!["run".into(), "--format".into(), "json".into()],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "OpenCode (BYOK -- any provider)".into(),
                    models: vec![], // populated dynamically from `opencode models`
                    model_flag: "-m".into(),
                    last_model: "anthropic/claude-sonnet-4-6".into(),
                },
                RuntimeDef {
                    name: "cursor".into(),
                    command: "cursor".into(),
                    args: vec!["agent".into(), "--print".into(), "--output-format".into(), "stream-json".into(), "--stream-partial-output".into(), "--yolo".into()],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "Cursor Agent (Cursor Pro subscription)".into(),
                    models: vec![], // populated dynamically from `cursor agent models`
                    model_flag: "--model".into(),
                    last_model: "sonnet-4.6".into(),
                },
            ],
            mcps: vec![
                McpDef {
                    name: "filesystem".into(),
                    command: "mcp-filesystem".into(),
                    args: vec![],
                    description: "Filesystem access".into(),
                    global: true,
                },
                McpDef {
                    name: "playwright".into(),
                    command: "npx".into(),
                    args: vec!["@playwright/mcp@latest".into()],
                    description: "Browser automation (Playwright)".into(),
                    global: false,
                },
                McpDef {
                    name: "browser-use".into(),
                    command: "uvx".into(),
                    args: vec!["browser-use".into(), "--mcp".into()],
                    description: "AI browser agent (browser-use)".into(),
                    global: false,
                },
                McpDef {
                    name: "github".into(),
                    command: "mcp-github".into(),
                    args: vec![],
                    description: "GitHub integration".into(),
                    global: false,
                },
            ],
            machines: vec![
                MachineDef {
                    name: "local".into(),
                    kind: "local".into(),
                    host: String::new(),
                    user: String::new(),
                    workdir: String::new(),
                },
                MachineDef {
                    name: "theodolos".into(),
                    kind: "ssh".into(),
                    host: "theodolos".into(),
                    user: String::new(),
                    workdir: String::new(),
                },
            ],
            last_runtime: "claude".into(),
            last_machine: "local".into(),
            cautious_enter: false,
            terminal_text: false,
            last_mcps: vec![],
            groups: vec!["default".into()],
            theme: "midnight".into(),
            font_family: "Menlo".into(),
            font_size: 15.0,
            whisper_model: default_whisper_model(),
            audio_device: String::new(),
            bg_opacity: default_bg_opacity(),
            bg_blur: default_bg_blur(),
        }
    }
}

impl AppConfig {
    fn load() -> Self {
        let path = config_path();
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(mut config) = toml::from_str::<Self>(&contents) {
                    // Merge in any new default runtimes not present in saved config
                    let defaults = Self::default();
                    for def_rt in &defaults.runtimes {
                        if !config.runtimes.iter().any(|r| r.name == def_rt.name) {
                            config.runtimes.push(def_rt.clone());
                        }
                    }
                    for def_machine in &defaults.machines {
                        if !config.machines.iter().any(|m| m.name == def_machine.name) {
                            config.machines.push(def_machine.clone());
                        }
                    }
                    return config;
                }
            }
        }
        Self::default()
    }

    fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, contents);
        }
    }
}

// ── Runtime config discovery ────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct RuntimeInfo {
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    thinking_enabled: bool,
    plugins: Vec<String>,
    mcps: Vec<String>,
    skills: Vec<String>,
    hooks: Vec<String>,
    extra: Vec<(String, String)>, // key-value pairs for misc settings
}

fn read_claude_config() -> RuntimeInfo {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut info = RuntimeInfo {
        model: "claude-opus-4-6".into(),
        context_window: 200_000,
        max_output_tokens: 32_000,
        ..Default::default()
    };

    // Read settings.json
    let settings_path = format!("{}/.claude/settings.json", home);
    if let Ok(contents) = std::fs::read_to_string(&settings_path) {
        if let Ok(v) = serde_json::from_str::<JsonValue>(&contents) {
            if let Some(true) = v.get("alwaysThinkingEnabled").and_then(|v| v.as_bool()) {
                info.thinking_enabled = true;
            }
            if let Some(env) = v.get("env").and_then(|v| v.as_object()) {
                if let Some(t) = env.get("MAX_THINKING_TOKENS").and_then(|v| v.as_str()) {
                    info.extra.push(("think_tokens".into(), t.into()));
                }
                if let Some(t) = env.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS").and_then(|v| v.as_str()) {
                    if let Ok(n) = t.parse::<u64>() { info.max_output_tokens = n; }
                }
            }
            if let Some(plugins) = v.get("enabledPlugins").and_then(|v| v.as_object()) {
                for (name, enabled) in plugins {
                    if enabled.as_bool().unwrap_or(false) {
                        info.plugins.push(name.split('@').next().unwrap_or(name).into());
                    }
                }
            }
            if let Some(hooks) = v.get("hooks").and_then(|v| v.as_object()) {
                for key in hooks.keys() {
                    info.hooks.push(key.clone());
                }
            }
        }
    }

    // Read MCP servers
    let mcp_path = format!("{}/.claude/mcp.json", home);
    if let Ok(contents) = std::fs::read_to_string(&mcp_path) {
        if let Ok(v) = serde_json::from_str::<JsonValue>(&contents) {
            if let Some(obj) = v.as_object() {
                for key in obj.keys() {
                    info.mcps.push(key.clone());
                }
            }
        }
    }

    // Check for memory
    let memory_glob = format!("{}/.claude/projects/*/memory/MEMORY.md", home);
    info.extra.push(("memory".into(), "yes".into()));

    // Check CLAUDE.md
    let claude_md = format!("{}/.claude/CLAUDE.md", home);
    if std::path::Path::new(&claude_md).exists() {
        info.extra.push(("instructions".into(), "CLAUDE.md".into()));
    }
    let _ = memory_glob; // used conceptually

    info
}

fn read_codex_config() -> RuntimeInfo {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut info = RuntimeInfo {
        model: "gpt-5.3-codex".into(),
        context_window: 200_000,
        max_output_tokens: 32_000,
        ..Default::default()
    };

    // Read config.toml
    let config_path = format!("{}/.codex/config.toml", home);
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        if let Ok(v) = contents.parse::<toml::Value>() {
            if let Some(m) = v.get("model").and_then(|v| v.as_str()) {
                info.model = m.into();
            }
            if let Some(effort) = v.get("model_reasoning_effort").and_then(|v| v.as_str()) {
                info.thinking_enabled = true;
                info.extra.push(("reasoning".into(), effort.into()));
            }
            if let Some(p) = v.get("personality").and_then(|v| v.as_str()) {
                info.extra.push(("personality".into(), p.into()));
            }
            if let Some(mcps) = v.get("mcp_servers").and_then(|v| v.as_table()) {
                for key in mcps.keys() {
                    info.mcps.push(key.clone());
                }
            }
        }
    }

    // Read skills
    let skills_dir = format!("{}/.codex/skills", home);
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    info.skills.push(name.into());
                }
            }
        }
    }

    // Check AGENTS.md
    let agents_md = format!("{}/.codex/AGENTS.md", home);
    if std::path::Path::new(&agents_md).exists() {
        info.extra.push(("instructions".into(), "AGENTS.md".into()));
    }

    info
}

fn read_opencode_config() -> RuntimeInfo {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut info = RuntimeInfo::default();

    let config_path = format!("{}/.config/opencode/opencode.json", home);
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        if let Ok(v) = serde_json::from_str::<JsonValue>(&contents) {
            if let Some(m) = v.get("model").and_then(|v| v.as_str()) {
                info.model = m.into();
            }
            if let Some(providers) = v.get("provider").and_then(|v| v.as_object()) {
                for (name, prov) in providers {
                    info.extra.push(("provider".into(), name.clone()));
                    if let Some(opts) = prov.get("options").and_then(|v| v.as_object()) {
                        if let Some(url) = opts.get("baseURL").and_then(|v| v.as_str()) {
                            info.extra.push(("endpoint".into(), url.into()));
                        }
                    }
                }
            }
        }
    }

    info
}

fn read_runtime_info(runtime_name: &str) -> RuntimeInfo {
    match runtime_name {
        "claude" => read_claude_config(),
        "codex" => read_codex_config(),
        "opencode" => read_opencode_config(),
        _ => RuntimeInfo::default(),
    }
}

// ── Token tracking ─────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct TokenStats {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    context_window: u64,
    max_output_tokens: u64,
    cost_usd: f64,
    model: String,
    thinking_enabled: bool,
    session_id: Option<String>,
}

impl TokenStats {
    fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens
    }

    fn context_usage_pct(&self) -> f32 {
        if self.context_window == 0 { return 0.0; }
        (self.total_tokens() as f32 / self.context_window as f32 * 100.0).min(100.0)
    }
}

// ── Modes ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Palette,
    Setup,
    Search,
}

// ── Actions ─────────────────────────────────────────────────────

actions!(
    opensquirrel,
    [
        EnterInsertMode,
        EnterCommandMode,
        NavUp,
        NavDown,
        PaneLeft,
        PaneRight,
        ScrollUp,
        ScrollDown,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToTop,
        ScrollToBottom,
        SpawnAgent,
        SubmitInput,
        DeleteChar,
        OpenPalette,
        ClosePalette,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        SetupNext, // Tab: next step in setup
        SetupPrev, // Shift-Tab: previous step
        SetupToggle, // Space: toggle selection
        CycleTheme,
        KillAgent,
        ToggleFavorite,
        ContinueTurn,
        ViewGrid,
        ViewPipeline,
        ViewFocus,
        SearchOpen,
        SearchClose,
        ChangeAgent,
        RestartAgent,
        ToggleAutoScroll,
        PipeToAgent,
        ToggleCustomEndpoint,
        CursorLeft,
        CursorRight,
        CursorWordLeft,
        CursorWordRight,
        CursorHome,
        CursorEnd,
        DeleteWordBack,
        DeleteToStart,
        InsertNewline,
        PasteClipboard,
        OpenTerminal,
        ShowStats,
        ToggleVoice,
        NextPane,
        PrevPane,
        NextGroup,
        PrevGroup,
        Quit,
    ]
);

// ── Agent ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode { Grid, Pipeline, Focus }

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentStatus {
    Working, Idle, Blocked, Starting, Interrupted,
}

impl AgentStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Blocked => "error",
            Self::Starting => "starting",
            Self::Interrupted => "interrupted",
        }
    }
    fn color(&self, t: &ThemeColors) -> Rgba {
        match self {
            Self::Working => t.green(),
            Self::Idle => t.text_muted(),
            Self::Blocked => t.red(),
            Self::Starting => t.yellow(),
            Self::Interrupted => t.yellow(),
        }
    }
    fn dot(&self) -> &'static str {
        match self {
            Self::Working => "●",
            Self::Idle => "○",
            Self::Blocked => "●",
            Self::Starting => "◌",
            Self::Interrupted => "↺",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentRole {
    Coordinator,
    Worker,
}

impl AgentRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Worker => "worker",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "worker" => Self::Worker,
            _ => Self::Coordinator,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Ready,
    Running,
    Interrupted,
}

impl TurnState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "interrupted" => Self::Interrupted,
            _ => Self::Ready,
        }
    }
}

#[derive(Clone)]
struct WorkerAssignment {
    parent_idx: usize,
    task_id: String,
    task_title: String,
}

#[derive(Clone)]
struct MachineTarget {
    name: String,
    ssh_destination: Option<String>,
    workdir: Option<String>,
}

fn make_tmux_session_name(agent_name: &str) -> String {
    let safe = agent_name.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("osq-{}-{}", if safe.is_empty() { "agent" } else { safe.as_str() }, suffix)
}

enum AgentMsg {
    OutputLine(String),
    StderrLine(String),
    Ready,
    Done { session_id: Option<String> },
    Error(String),
    TokenUpdate(TokenStats),
    RemoteCursor(usize),
    ToolCall(String), // tool name
}

#[derive(Default, Clone)]
struct ToolCallStats {
    edits: u32,
    reads: u32,
    bash: u32,
    writes: u32,
    other: u32,
}

impl ToolCallStats {
    fn total(&self) -> u32 { self.edits + self.reads + self.bash + self.writes + self.other }
    fn summary(&self) -> String {
        let total = self.total();
        if total == 0 { return String::new(); }
        format!("{} tools", total)
    }
}

struct AgentState {
    name: String,
    group: String,
    runtime_name: String,
    target_machine: String,
    role: AgentRole,
    status: AgentStatus,
    output_lines: Vec<String>,
    input_buffer: String,
    input_cursor: usize, // byte offset into input_buffer
    message_count: u32,
    scroll_offset: usize,
    session_id: Option<String>,
    prompt_tx: Option<mpsc::Sender<String>>,
    _reader_task: Option<Task<()>>,
    // Rich info
    tokens: TokenStats,
    runtime_info: RuntimeInfo,
    favorite: bool,
    pending_prompt: Option<String>,
    turn_state: TurnState,
    prompt_preamble: Option<String>,
    worker_assignment: Option<WorkerAssignment>,
    restore_notice: Option<String>,
    // New features
    working_dir: String,
    remote_session_name: Option<String>,
    remote_line_cursor: usize,
    turn_started: Option<Instant>,
    tool_calls: ToolCallStats,
    auto_scroll: bool,
    scroll_accum: f32,
    last_model: Option<String>,
    // Animation triggers
    status_changed_at: Instant,
    last_tool_call_at: Option<Instant>,
    spawn_time: Instant,
    delegate_buf: Option<String>,
}

impl AgentState {
    fn new(name: &str, group: &str, runtime: &str) -> Self {
        let runtime_info = read_runtime_info(runtime);
        let tokens = TokenStats {
            context_window: runtime_info.context_window,
            max_output_tokens: runtime_info.max_output_tokens,
            model: runtime_info.model.clone(),
            thinking_enabled: runtime_info.thinking_enabled,
            ..Default::default()
        };
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        Self {
            name: name.into(), group: group.into(), runtime_name: runtime.into(),
            target_machine: "local".into(),
            role: AgentRole::Coordinator,
            status: AgentStatus::Starting, output_lines: Vec::new(),
            input_buffer: String::new(), input_cursor: 0, message_count: 0, scroll_offset: 0,
            session_id: None, prompt_tx: None, _reader_task: None,
            tokens, runtime_info,
            favorite: false,
            pending_prompt: None,
            turn_state: TurnState::Ready,
            prompt_preamble: None,
            worker_assignment: None,
            restore_notice: None,
            working_dir: cwd,
            remote_session_name: None,
            remote_line_cursor: 0,
            turn_started: None,
            tool_calls: ToolCallStats::default(),
            auto_scroll: true,
            scroll_accum: 0.0,
            last_model: None,
            status_changed_at: Instant::now(),
            last_tool_call_at: None,
            spawn_time: Instant::now(),
            delegate_buf: None,
        }
    }
}

// ── Setup wizard state ──────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep { Machine, Directory, Runtime, Model, Mcps, Confirm }

struct SetupState {
    step: SetupStep,
    runtime_cursor: usize,
    selected_runtime: String,
    model_cursor: usize,
    selected_model: String,
    model_filter: String,
    machine_cursor: usize,
    selected_machine: String,
    // Directory selection
    dir_filter: String,
    dir_entries: Vec<String>,
    dir_cursor: usize,
    selected_dir: String,
    mcp_cursor: usize,
    selected_mcps: Vec<bool>,
    editing_agent: Option<usize>,
    custom_mode: bool,
    custom_field: usize,
    custom_base_url: String,
    custom_api_key: String,
    custom_model_id: String,
}

impl SetupState {
    /// Get filtered model list indices based on model_filter.
    fn filtered_models<'a>(&self, models: &'a [ModelOption]) -> Vec<(usize, &'a ModelOption)> {
        let q = self.model_filter.to_lowercase();
        if q.is_empty() {
            models.iter().enumerate().collect()
        } else {
            models.iter().enumerate()
                .filter(|(_, m)| {
                    m.label.to_lowercase().contains(&q) || m.id.to_lowercase().contains(&q)
                })
                .collect()
        }
    }
}

// ── Delegation ──────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
struct DelegateRequest {
    tasks: Vec<DelegateTask>,
}

#[derive(Clone, Debug, Deserialize)]
struct DelegateTask {
    id: String,
    title: String,
    runtime: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    target: Option<String>,
    prompt: String,
}

// ── Palette ─────────────────────────────────────────────────────

struct PaletteItem { label: String, action: PaletteAction }
enum PaletteAction { NewAgent, NewGroup, SetTheme(String), SetView(ViewMode), KillCurrent, CompactContext, ToggleSidebarTab, ToggleCautiousEnter, ToggleTerminalText, SetAudioDevice(String), SetBgOpacity(f32), SetBgBlur(f32), Quit }

// ── Search ─────────────────────────────────────────────────────

struct SearchResult {
    agent_idx: usize,
    agent_name: String,
    line_idx: usize,
    line: String,
}

// ── Diff classification (re-exported from lib.rs for tests) ─────
use opensquirrel::{
    build_persistent_runtime_args, classify_line, extract_latest_turn_output, LineKind,
    parse_bullet, parse_code_fence, parse_heading, parse_session_prompt, parse_spans,
    shell_escape, summarize_diff, DiffSummary, Span,
};

// ── Groups ──────────────────────────────────────────────────────

struct Group { name: String }

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab { Agents, Workers }

// ── Root ────────────────────────────────────────────────────────

struct OpenSquirrel {
    mode: Mode,
    agents: Vec<AgentState>,
    groups: Vec<Group>,
    focused_group: usize,
    focused_agent: usize,
    focus_handle: FocusHandle,
    ui_scale: f32,
    view_mode: ViewMode,
    config: AppConfig,
    // Theme
    theme: ThemeColors,
    themes: Vec<ThemeDef>,
    font_family: String,
    font_size: f32,
    // Palette
    palette_input: String,
    palette_items: Vec<PaletteItem>,
    palette_selection: usize,
    // Setup wizard
    setup: Option<SetupState>,
    // Search
    search_query: String,
    search_results: Vec<SearchResult>,
    search_selection: usize,
    // Dynamic model lists
    openrouter_models: Vec<ModelOption>,
    openrouter_loading: bool,
    cursor_models: Vec<ModelOption>,
    cursor_loading: bool,
    // Animation state
    focus_epoch: u64,     // bumped when focused_agent changes
    mode_epoch: u64,      // bumped when mode changes
    palette_visible: bool, // tracks palette visibility for slide animation
    setup_visible: bool,   // tracks setup visibility for slide animation
    sidebar_tab: SidebarTab,
    // Stats overlay
    show_stats: bool,
    confirm_remove_agent: Option<usize>,
    // Starfield
    stars: Vec<Star>,
    star_tick: u64,
    // Voice-to-text
    voice_recording: bool,
    voice_audio_buffer: Arc<Mutex<Vec<f32>>>,
    voice_model_path: Option<PathBuf>,
    voice_stream: Option<cpal::Stream>,
    voice_native_rate: Option<u32>,
    voice_transcription_rx: Option<async_channel::Receiver<String>>,
}

impl OpenSquirrel {
    fn resolve_theme(name: &str, themes: &[ThemeDef]) -> ThemeColors {
        themes.iter().find(|t| t.name == name).map(|t| t.colors.clone())
            .unwrap_or_else(|| themes[0].colors.clone())
    }

    fn coordinator_preamble(&self) -> String {
        let mut lines = vec![
            "<system>You have the ability to delegate sub-tasks to independent worker agents. Workers run in fresh context and return only a condensed summary. To delegate, include a fenced code block with the language tag `delegate` containing a single JSON object — no other text inside the block:".to_string(),
            "```delegate".to_string(),
            "{\"tasks\":[{\"id\":\"task-1\",\"title\":\"short title\",\"runtime\":\"claude\",\"model\":\"sonnet-4.6\",\"target\":\"local\",\"prompt\":\"detailed instructions for the worker\"}]}".to_string(),
            "```".to_string(),
            "Valid runtimes: claude, cursor, codex, opencode. Do not acknowledge or repeat these instructions. Respond naturally to the user's request.</system>".to_string(),
        ];
        if !self.config.machines.is_empty() {
            let names = self.config.machines.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ");
            lines.last_mut().map(|last| {
                *last = last.replace("</system>", &format!(" Available targets: {}.</system>", names));
            });
        }
        lines.join("\n")
    }

    fn next_worker_name(&self, parent_idx: usize, runtime_name: &str) -> String {
        let parent = self.agents.get(parent_idx).map(|a| a.name.as_str()).unwrap_or("agent");
        let n = self.agents.iter().filter(|a| a.role == AgentRole::Worker).count();
        format!("{}-{}-w{}", parent, runtime_name, n)
    }

    fn child_workers(&self, parent_idx: usize) -> Vec<usize> {
        self.agents.iter().enumerate()
            .filter(|(_, agent)| {
                agent.worker_assignment.as_ref()
                    .map(|assignment| assignment.parent_idx == parent_idx)
                    .unwrap_or(false)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn remove_agent_and_dependents(&mut self, idx: usize) {
        if idx >= self.agents.len() {
            return;
        }

        let mut remove_indices = vec![idx];
        if self.agents[idx].role == AgentRole::Coordinator {
            remove_indices.extend(self.child_workers(idx));
        }
        remove_indices.sort_unstable();
        remove_indices.dedup();

        for &remove_idx in remove_indices.iter().rev() {
            if remove_idx < self.agents.len() {
                self.agents[remove_idx].prompt_tx = None;
                self.agents[remove_idx]._reader_task = None;
                self.agents.remove(remove_idx);
            }
        }

        for agent in &mut self.agents {
            if let Some(assignment) = &mut agent.worker_assignment {
                if remove_indices.contains(&assignment.parent_idx) {
                    agent.worker_assignment = None;
                } else {
                    let shift = remove_indices.iter().filter(|&&removed| removed < assignment.parent_idx).count();
                    assignment.parent_idx -= shift;
                }
            }
        }

        if self.focused_group >= self.groups.len() {
            self.focused_group = self.groups.len().saturating_sub(1);
        }
        if self.focused_agent >= self.agents.len() && !self.agents.is_empty() {
            self.focused_agent = self.agents.len() - 1;
        }
        self.clamp_focus();
        self.save_state();
    }

    fn truncate_for_summary(text: &str, max_len: usize) -> String {
        if text.len() <= max_len {
            text.to_string()
        } else {
            format!("{}...\n[truncated, {} chars total]", &text[..max_len], text.len())
        }
    }

    fn resolve_machine_target(&self, target_name: &str) -> MachineTarget {
        if let Some(machine) = self.config.machines.iter().find(|machine| machine.name == target_name) {
            if machine.kind == "ssh" {
                let destination = if machine.user.is_empty() {
                    machine.host.clone()
                } else {
                    format!("{}@{}", machine.user, machine.host)
                };
                return MachineTarget {
                    name: machine.name.clone(),
                    ssh_destination: if destination.is_empty() { None } else { Some(destination) },
                    workdir: if machine.workdir.is_empty() { None } else { Some(machine.workdir.clone()) },
                };
            }
            return MachineTarget {
                name: machine.name.clone(),
                ssh_destination: None,
                workdir: if machine.workdir.is_empty() { None } else { Some(machine.workdir.clone()) },
            };
        }
        MachineTarget {
            name: "local".into(),
            ssh_destination: None,
            workdir: None,
        }
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let config = AppConfig::load();
        let groups: Vec<Group> = config.groups.iter().map(|n| Group { name: n.clone() }).collect();
        let themes = builtin_themes();
        let theme = Self::resolve_theme(&config.theme, &themes);

        let mut app = Self {
            mode: Mode::Normal,
            agents: Vec::new(),
            groups: if groups.is_empty() { vec![Group { name: "default".into() }] } else { groups },
            focused_group: 0, focused_agent: 0,
            focus_handle: cx.focus_handle(),
            ui_scale: 1.0, view_mode: ViewMode::Grid,
            config: config.clone(),
            theme, themes,
            font_family: config.font_family.clone(),
            font_size: config.font_size,
            palette_input: String::new(), palette_items: Vec::new(), palette_selection: 0,
            setup: None,
            search_query: String::new(), search_results: Vec::new(), search_selection: 0,
            openrouter_models: Vec::new(), openrouter_loading: false,
            cursor_models: Vec::new(), cursor_loading: false,
            focus_epoch: 0, mode_epoch: 0, palette_visible: false, setup_visible: false,
            sidebar_tab: SidebarTab::Agents,
            show_stats: false,
            confirm_remove_agent: None,
            stars: generate_stars(200, 0xDEADBEEF42),
            star_tick: 0,
            voice_recording: false,
            voice_audio_buffer: Arc::new(Mutex::new(Vec::new())),
            voice_model_path: {
                let p = whisper_model_path(&config.whisper_model);
                if p.exists() { Some(p) } else { None }
            },
            voice_stream: None,
            voice_native_rate: None,
            voice_transcription_rx: None,
        };

        // Restore from saved state, or create fresh agent
        if let Some(saved) = SavedAppState::load() {
            // Restore groups
            if !saved.groups.is_empty() {
                app.groups = saved.groups.iter().map(|n| Group { name: n.clone() }).collect();
            }
            app.view_mode = match saved.view_mode.as_str() {
                "pipeline" => ViewMode::Pipeline, "focus" => ViewMode::Focus, _ => ViewMode::Grid,
            };
            app.sidebar_tab = match saved.sidebar_tab.as_str() {
                "swarms" | "workers" => SidebarTab::Workers, _ => SidebarTab::Agents,
            };
            app.ui_scale = saved.ui_scale;

            // Restore each agent
            if !saved.agents.is_empty() {
                for sa in &saved.agents {
                    let model = if sa.model.is_empty() { None } else { Some(sa.model.as_str()) };
                    let role = AgentRole::from_str(&sa.role);
                    let prompt_preamble = if role == AgentRole::Coordinator {
                        Some(app.coordinator_preamble())
                    } else {
                        None
                    };
                    app.create_agent_with_role(
                        &sa.name,
                        &sa.group,
                        &sa.runtime_name,
                        model,
                        &sa.target_machine,
                        role,
                        prompt_preamble,
                        None,
                        sa.remote_session_name.clone(),
                        cx,
                    );
                    let idx = app.agents.len() - 1;
                    let a = &mut app.agents[idx];
                    // Restore visual state
                    a.output_lines = sa.output_lines.clone();
                    a.message_count = sa.message_count;
                    a.scroll_offset = sa.scroll_offset;
                    a.favorite = sa.favorite;
                    a.auto_scroll = sa.auto_scroll;
                    if !sa.working_dir.is_empty() { a.working_dir = sa.working_dir.clone(); }
                    a.remote_session_name = sa.remote_session_name.clone();
                    a.remote_line_cursor = sa.remote_line_cursor;
                    // Restore session for reconnection
                    a.session_id = sa.session_id.clone();
                    // Restore stats
                    a.tokens.cost_usd = sa.cost_usd;
                    a.tokens.input_tokens = sa.input_tokens;
                    a.tokens.output_tokens = sa.output_tokens;
                    a.tokens.cache_read_tokens = sa.cache_read_tokens;
                    a.tokens.cache_write_tokens = sa.cache_write_tokens;
                    a.pending_prompt = sa.pending_prompt.clone();
                    a.turn_state = TurnState::from_str(&sa.turn_state);
                    a.tool_calls = ToolCallStats {
                        edits: sa.tool_edits, reads: sa.tool_reads,
                        bash: sa.tool_bash, writes: sa.tool_writes, other: sa.tool_other,
                    };
                    if a.turn_state != TurnState::Ready || a.pending_prompt.is_some() {
                        a.status = AgentStatus::Interrupted;
                        let restore_msg = if a.target_machine == "local" {
                            "[restored interrupted turn -- press enter to continue]".into()
                        } else if a.remote_session_name.is_some() {
                            format!(
                                "[restored remote session on {} -- reattaching tmux and press enter to resend if needed]",
                                a.target_machine
                            )
                        } else {
                            format!(
                                "[restored interrupted remote turn on {} -- press enter to continue]",
                                a.target_machine
                            )
                        };
                        a.restore_notice = Some(restore_msg);
                    } else if sa.session_id.is_some() {
                        a.restore_notice = Some("[restored session -- send a message to reconnect]".into());
                    }
                }
                let name_to_idx: HashMap<String, usize> = app.agents.iter().enumerate()
                    .map(|(idx, agent)| (agent.name.clone(), idx))
                    .collect();
                for (idx, sa) in saved.agents.iter().enumerate() {
                    if let Some(parent_name) = &sa.parent_name {
                        if let Some(&parent_idx) = name_to_idx.get(parent_name) {
                            app.agents[idx].worker_assignment = Some(WorkerAssignment {
                                parent_idx,
                                task_id: sa.task_id.clone().unwrap_or_else(|| format!("restored-{}", idx)),
                                task_title: sa.task_title.clone().unwrap_or_else(|| "restored worker".into()),
                            });
                        }
                    }
                    if let Some(session_name) = saved.agents[idx].remote_session_name.clone() {
                        if !session_name.is_empty()
                            && app.agents[idx].target_machine != "local"
                            && app.agents[idx].turn_state != TurnState::Ready
                        {
                            if let Some(tx) = &app.agents[idx].prompt_tx {
                                let _ = tx.send(format!(
                                    "__OSQ_REATTACH__{}::{}",
                                    saved.agents[idx].remote_line_cursor,
                                    session_name
                                ));
                            }
                        }
                    }
                }
                app.focused_group = saved.focused_group.min(app.groups.len().saturating_sub(1));
                app.focused_agent = saved.focused_agent.min(app.agents.len().saturating_sub(1));
            } else {
                // No agents in saved state, create fresh
                let rt = app.config.last_runtime.clone();
                let model = app.config.runtimes.iter().find(|r| r.name == rt).map(|r| r.last_model.clone());
                app.create_agent_with_role(
                    "agent-0",
                    "default",
                    &rt,
                    model.as_deref(),
                    &app.config.last_machine.clone(),
                    AgentRole::Coordinator,
                    Some(app.coordinator_preamble()),
                    None,
                    None,
                    cx,
                );
            }
        } else {
            // No saved state, create fresh
            let rt = app.config.last_runtime.clone();
            let model = app.config.runtimes.iter().find(|r| r.name == rt).map(|r| r.last_model.clone());
            app.create_agent_with_role(
                "agent-0",
                "default",
                &rt,
                model.as_deref(),
                &app.config.last_machine.clone(),
                AgentRole::Coordinator,
                Some(app.coordinator_preamble()),
                None,
                None,
                cx,
            );
        }
        // Starfield twinkle timer -- ~30fps update
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(33)).await;
                let Ok(()) = this.update(cx, |view, cx| {
                    view.star_tick = view.star_tick.wrapping_add(1);
                    // Only notify if using ops theme (starfield visible)
                    if view.config.theme == "ops" { cx.notify(); }
                }) else { break };
            }
        }).detach();

        app
    }

    fn save_config(&mut self) {
        self.config.groups = self.groups.iter().map(|g| g.name.clone()).collect();
        self.config.save();
    }

    fn save_state(&self) {
        let agents: Vec<SavedAgentState> = self.agents.iter().map(|a| SavedAgentState {
            name: a.name.clone(),
            group: a.group.clone(),
            runtime_name: a.runtime_name.clone(),
            target_machine: a.target_machine.clone(),
            role: a.role.as_str().into(),
            parent_name: a.worker_assignment.as_ref()
                .and_then(|assignment| self.agents.get(assignment.parent_idx))
                .map(|parent| parent.name.clone()),
            task_id: a.worker_assignment.as_ref().map(|assignment| assignment.task_id.clone()),
            task_title: a.worker_assignment.as_ref().map(|assignment| assignment.task_title.clone()),
            session_id: a.session_id.clone(),
            model: a.tokens.model.clone(),
            output_lines: a.output_lines.clone(),
            message_count: a.message_count,
            scroll_offset: a.scroll_offset,
            favorite: a.favorite,
            auto_scroll: a.auto_scroll,
            working_dir: a.working_dir.clone(),
            remote_session_name: a.remote_session_name.clone(),
            remote_line_cursor: a.remote_line_cursor,
            cost_usd: a.tokens.cost_usd,
            input_tokens: a.tokens.input_tokens,
            output_tokens: a.tokens.output_tokens,
            cache_read_tokens: a.tokens.cache_read_tokens,
            cache_write_tokens: a.tokens.cache_write_tokens,
            pending_prompt: a.pending_prompt.clone(),
            turn_state: a.turn_state.as_str().into(),
            tool_edits: a.tool_calls.edits,
            tool_reads: a.tool_calls.reads,
            tool_bash: a.tool_calls.bash,
            tool_writes: a.tool_calls.writes,
            tool_other: a.tool_calls.other,
        }).collect();
        let state = SavedAppState {
            agents,
            groups: self.groups.iter().map(|g| g.name.clone()).collect(),
            focused_group: self.focused_group,
            focused_agent: self.focused_agent,
            view_mode: match self.view_mode {
                ViewMode::Grid => "grid", ViewMode::Pipeline => "pipeline", ViewMode::Focus => "focus",
            }.into(),
            ui_scale: self.ui_scale,
            sidebar_tab: match self.sidebar_tab {
                SidebarTab::Agents => "agents", SidebarTab::Workers => "workers",
            }.into(),
        };
        state.save();
    }

    fn get_models_for_runtime(&self, runtime_name: &str) -> Vec<ModelOption> {
        if runtime_name == "opencode" && !self.openrouter_models.is_empty() {
            return self.openrouter_models.clone();
        }
        if runtime_name == "cursor" && !self.cursor_models.is_empty() {
            return self.cursor_models.clone();
        }
        self.config.runtimes.iter()
            .find(|r| r.name == runtime_name)
            .map(|r| r.models.clone())
            .unwrap_or_default()
    }

    fn fetch_opencode_models(&mut self, cx: &mut Context<Self>) {
        if self.openrouter_loading || !self.openrouter_models.is_empty() { return; }
        self.openrouter_loading = true;
        let (tx, rx) = async_channel::bounded::<Vec<ModelOption>>(1);
        std::thread::spawn(move || {
            let models = fetch_opencode_model_list();
            let _ = tx.send_blocking(models);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(models) = rx.recv().await {
                cx.update(|cx| {
                    this.update(cx, |app, cx| {
                        app.openrouter_models = models;
                        app.openrouter_loading = false;
                        cx.notify();
                    }).ok();
                }).ok();
            }
        }).detach();
    }

    fn fetch_cursor_models(&mut self, cx: &mut Context<Self>) {
        if self.cursor_loading || !self.cursor_models.is_empty() { return; }
        self.cursor_loading = true;
        let (tx, rx) = async_channel::bounded::<Vec<ModelOption>>(1);
        std::thread::spawn(move || {
            let models = fetch_cursor_model_list();
            let _ = tx.send_blocking(models);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(models) = rx.recv().await {
                cx.update(|cx| {
                    this.update(cx, |app, cx| {
                        app.cursor_models = models;
                        app.cursor_loading = false;
                        cx.notify();
                    }).ok();
                }).ok();
            }
        }).detach();
    }

    // ── Agent lifecycle ─────────────────────────────────────────

    fn filter_dirs(entries: &[String], filter: &str) -> Vec<String> {
        if filter.is_empty() { return entries.to_vec(); }
        let q = filter.to_lowercase();
        entries.iter()
            .filter(|d| d.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }

    fn selected_mcp_defs(&self) -> Vec<McpDef> {
        self.config.mcps.iter()
            .filter(|m| m.global || self.config.last_mcps.contains(&m.name))
            .cloned()
            .collect()
    }

    fn create_agent_with_role(
        &mut self,
        name: &str,
        group: &str,
        runtime_name: &str,
        model: Option<&str>,
        target_machine: &str,
        role: AgentRole,
        prompt_preamble: Option<String>,
        worker_assignment: Option<WorkerAssignment>,
        remote_session_name_override: Option<String>,
        cx: &mut Context<Self>,
    ) -> usize {
        let agent_idx = self.agents.len();
        self.agents.push(AgentState::new(name, group, runtime_name));
        let machine_target = self.resolve_machine_target(target_machine);
        let remote_session_name = if machine_target.ssh_destination.is_some() {
            remote_session_name_override.or_else(|| Some(make_tmux_session_name(name)))
        } else {
            None
        };
        self.agents[agent_idx].target_machine = machine_target.name.clone();
        self.agents[agent_idx].role = role;
        self.agents[agent_idx].prompt_preamble = prompt_preamble;
        self.agents[agent_idx].worker_assignment = worker_assignment;
        self.agents[agent_idx].remote_session_name = remote_session_name.clone();

        let (msg_tx, msg_rx) = async_channel::unbounded::<AgentMsg>();
        let (prompt_tx, prompt_rx) = mpsc::channel::<String>();
        self.agents[agent_idx].prompt_tx = Some(prompt_tx);

        let runtime = self.config.runtimes.iter()
            .find(|r| r.name == runtime_name)
            .cloned()
            .unwrap_or_else(|| self.config.runtimes[0].clone());

        let model_override = model.map(String::from);
        let mcps = self.selected_mcp_defs();

        if let Some(ref m) = model_override {
            self.agents[agent_idx].tokens.model = m.clone();
        }
        self.agents[agent_idx].last_model = model_override.clone();

        let msg_tx_clone = msg_tx.clone();
        std::thread::spawn(move || {
            agent_thread(
                msg_tx_clone,
                prompt_rx,
                runtime,
                model_override,
                machine_target,
                role,
                remote_session_name,
                mcps,
            );
        });

        let task = cx.spawn(async move |this: WeakEntity<OpenSquirrel>, cx: &mut AsyncApp| {
            while let Ok(msg) = msg_rx.recv().await {
                let idx = agent_idx;
                let ok = this.update(cx, |view, cx| {
                    if idx >= view.agents.len() { return false; }

                    let mut pending_delegate: Option<DelegateRequest> = None;
                    let mut worker_done = false;
                    let mut turn_done = false;

                    let a = &mut view.agents[idx];
                    match msg {
                        AgentMsg::Ready => {
                            if a.turn_state == TurnState::Interrupted {
                                a.status = AgentStatus::Interrupted;
                            } else {
                                a.status = AgentStatus::Idle;
                                a.turn_state = TurnState::Ready;
                            }
                            a.status_changed_at = Instant::now();
                        }
                        AgentMsg::OutputLine(l) => {
                            a.restore_notice = None;
                            let trimmed = l.trim();
                            if a.role == AgentRole::Coordinator && trimmed == "```delegate" && a.delegate_buf.is_none() {
                                a.delegate_buf = Some(String::new());
                            } else if trimmed == "```" && a.delegate_buf.is_some() {
                                let json_str = a.delegate_buf.take().unwrap_or_default();
                                match serde_json::from_str::<DelegateRequest>(&json_str) {
                                    Ok(request) => pending_delegate = Some(request),
                                    Err(_) => a.output_lines.push("[delegate] failed to parse JSON".into()),
                                }
                            } else if let Some(ref mut buf) = a.delegate_buf {
                                buf.push_str(&l);
                                buf.push('\n');
                            }
                            a.output_lines.push(l);
                            if a.auto_scroll {
                                let len = a.output_lines.len();
                                if len > 40 { a.scroll_offset = len - 40; }
                            }
                        }
                        AgentMsg::StderrLine(l) => {
                            if !l.trim().is_empty() {
                                a.output_lines.push(format!("[!] {}", l));
                            }
                        }
                        AgentMsg::Done { session_id } => {
                            a.status = AgentStatus::Idle;
                            a.status_changed_at = Instant::now();
                            a.turn_started = None;
                            a.pending_prompt = None;
                            a.restore_notice = None;
                            a.turn_state = TurnState::Ready;
                            if let Some(id) = session_id { a.session_id = Some(id); }
                            a.output_lines.push(String::new());
                            turn_done = true;
                            if a.worker_assignment.is_some() {
                                worker_done = true;
                            }
                        }
                        AgentMsg::Error(e) => {
                            a.status = AgentStatus::Blocked;
                            a.status_changed_at = Instant::now();
                            a.turn_started = None;
                            a.turn_state = TurnState::Interrupted;
                            a.output_lines.push(format!("[!] {}", e));
                        }
                        AgentMsg::ToolCall(name) => {
                            a.last_tool_call_at = Some(Instant::now());
                            let lname = name.to_lowercase();
                            if lname.contains("edit") { a.tool_calls.edits += 1; }
                            else if lname.contains("bash") || lname.contains("shell") || lname.contains("command") { a.tool_calls.bash += 1; }
                            else if lname.contains("read") || lname.contains("glob") || lname.contains("grep") { a.tool_calls.reads += 1; }
                            else if lname.contains("write") { a.tool_calls.writes += 1; }
                            else { a.tool_calls.other += 1; }
                        }
                        AgentMsg::TokenUpdate(stats) => {
                            if stats.input_tokens > 0 || stats.output_tokens > 0 || stats.cost_usd > 0.0 {
                                a.tokens.input_tokens = stats.input_tokens;
                                a.tokens.output_tokens = stats.output_tokens;
                                a.tokens.cache_read_tokens = stats.cache_read_tokens;
                                a.tokens.cache_write_tokens = stats.cache_write_tokens;
                                a.tokens.cost_usd = stats.cost_usd;
                            }
                            if stats.context_window > 0 { a.tokens.context_window = stats.context_window; }
                            if stats.max_output_tokens > 0 { a.tokens.max_output_tokens = stats.max_output_tokens; }
                            if !stats.model.is_empty() { a.tokens.model = stats.model; }
                            a.tokens.thinking_enabled = stats.thinking_enabled;
                            if stats.session_id.is_some() {
                                a.session_id = stats.session_id.clone();
                                a.tokens.session_id = stats.session_id;
                            }
                        }
                        AgentMsg::RemoteCursor(cursor) => {
                            a.remote_line_cursor = cursor;
                        }
                    }

                    if let Some(request) = pending_delegate {
                        view.handle_delegate_request(idx, request, cx);
                    }
                    if worker_done {
                        view.handle_delegated_worker_done(idx, cx);
                    }
                    if turn_done {
                        view.save_state();
                    }
                    cx.notify();
                    true
                });
                if !matches!(ok, Ok(true)) { break; }
            }
        });
        self.agents[agent_idx]._reader_task = Some(task);
        self.save_state();
        agent_idx
    }

    fn handle_delegate_request(&mut self, coordinator_idx: usize, request: DelegateRequest, cx: &mut Context<Self>) {
        if coordinator_idx >= self.agents.len() || request.tasks.is_empty() {
            return;
        }

        let group = self.agents[coordinator_idx].group.clone();
        self.agents[coordinator_idx].output_lines.push(
            format!("[delegate] spawning {} worker(s)", request.tasks.len())
        );

        for task in request.tasks {
            let target_machine = task.target.clone().unwrap_or_else(|| "local".into());
            let worker_name = self.next_worker_name(coordinator_idx, &task.runtime);
            let assignment = WorkerAssignment {
                parent_idx: coordinator_idx,
                task_id: task.id.clone(),
                task_title: task.title.clone(),
            };
            let worker_idx = self.create_agent_with_role(
                &worker_name,
                &group,
                &task.runtime,
                task.model.as_deref(),
                &target_machine,
                AgentRole::Worker,
                None,
                Some(assignment),
                None,
                cx,
            );
            self.agents[coordinator_idx].output_lines.push(
                format!("[delegate] -> {} [{} @ {}] {}", worker_name, task.runtime, target_machine, task.title)
            );
            self.send_prompt(worker_idx, task.prompt, cx);
        }
        self.save_state();
    }

    fn build_worker_handoff_prompt(
        &self,
        worker_idx: usize,
        assignment: &WorkerAssignment,
        diff: &DiffSummary,
        output: &str,
    ) -> String {
        let worker = &self.agents[worker_idx];
        let diff_summary = if diff.files.is_empty() && diff.additions == 0 && diff.removals == 0 {
            "no diff activity detected".to_string()
        } else {
            let files = if diff.files.is_empty() {
                "unknown files".to_string()
            } else {
                diff.files.join(", ")
            };
            format!("files: {} | +{} -{}", files, diff.additions, diff.removals)
        };
        let tool_summary = if worker.tool_calls.summary().is_empty() {
            "none".to_string()
        } else {
            worker.tool_calls.summary()
        };

        format!(
            "Worker result received.\n\
             Task id: {}\n\
             Task title: {}\n\
             Worker: {}\n\
             Runtime: {}\n\
             Target: {}\n\
             Model: {}\n\
             Status: {}\n\
             Tokens: in={} out={} cost=${:.3}\n\
             Tools: {}\n\
             Diff summary: {}\n\n\
             Final worker output:\n{}",
            assignment.task_id,
            assignment.task_title,
            worker.name,
            worker.runtime_name,
            worker.target_machine,
            worker.tokens.model,
            if worker.status == AgentStatus::Blocked { "failed" } else { "success" },
            worker.tokens.input_tokens,
            worker.tokens.output_tokens,
            worker.tokens.cost_usd,
            tool_summary,
            diff_summary,
            output,
        )
    }

    fn handle_delegated_worker_done(&mut self, worker_idx: usize, cx: &mut Context<Self>) {
        let assignment = match self.agents.get(worker_idx).and_then(|agent| agent.worker_assignment.clone()) {
            Some(assignment) => assignment,
            None => return,
        };
        if assignment.parent_idx >= self.agents.len() {
            return;
        }

        let output = Self::truncate_for_summary(
            &extract_latest_turn_output(&self.agents[worker_idx].output_lines),
            6000,
        );
        let diff = summarize_diff(&self.agents[worker_idx].output_lines);
        let worker_name = self.agents[worker_idx].name.clone();
        let handoff = self.build_worker_handoff_prompt(worker_idx, &assignment, &diff, &output);

        self.agents[assignment.parent_idx].output_lines.push(
            format!("[delegate] <- {} completed {}", worker_name, assignment.task_id)
        );
        self.send_prompt(assignment.parent_idx, handoff, cx);
    }

    fn send_prompt(&mut self, idx: usize, prompt: String, cx: &mut Context<Self>) {
        if idx >= self.agents.len() { return; }
        let a = &mut self.agents[idx];
        a.status = AgentStatus::Working;
        a.turn_state = TurnState::Running;
        a.turn_started = Some(Instant::now());
        a.pending_prompt = Some(prompt.clone());
        a.message_count += 1;
        a.output_lines.push(format!("> {}", prompt));
        a.output_lines.push(String::new());
        cx.notify();
        let prompt = if let Some(ref preamble) = a.prompt_preamble {
            if a.message_count == 1 {
                format!("{}\n\n{}", preamble, prompt)
            } else {
                prompt
            }
        } else {
            prompt
        };
        let msg = if let Some(ref sid) = a.session_id {
            format!("SESSION:{}\n{}", sid, prompt)
        } else { prompt };
        if let Some(tx) = &a.prompt_tx { let _ = tx.send(msg); }
        self.save_state();
    }

    fn continue_pending_turn(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.agents.len() {
            return;
        }
        let prompt = self.agents[idx].pending_prompt.clone();
        if let Some(prompt) = prompt {
            self.send_prompt(idx, prompt, cx);
        }
    }

    // ── Helpers ─────────────────────────────────────────────────

    fn current_group_name(&self) -> &str {
        self.groups.get(self.focused_group).map(|g| g.name.as_str()).unwrap_or("default")
    }

    fn agents_in_current_group(&self) -> Vec<usize> {
        let gn = self.current_group_name().to_string();
        self.agents.iter().enumerate().filter(|(_, a)| a.group == gn).map(|(i, _)| i).collect()
    }

    fn set_focus(&mut self, idx: usize) {
        if self.focused_agent != idx {
            self.focused_agent = idx;
            self.focus_epoch += 1;
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.mode_epoch += 1;
            self.palette_visible = mode == Mode::Palette;
            self.setup_visible = mode == Mode::Setup;
        }
    }

    fn clamp_focus(&mut self) {
        let vis = self.agents_in_current_group();
        if vis.is_empty() { self.focused_agent = 0; }
        else if !vis.contains(&self.focused_agent) { self.set_focus(vis[0]); }
    }

    fn set_theme(&mut self, name: &str) {
        self.theme = Self::resolve_theme(name, &self.themes);
        self.config.theme = name.to_string();
        self.save_config();
    }

    fn cycle_theme(&mut self, _: &CycleTheme, _w: &mut Window, cx: &mut Context<Self>) {
        let current = &self.config.theme;
        let idx = self.themes.iter().position(|t| t.name == *current).unwrap_or(0);
        let next = (idx + 1) % self.themes.len();
        let name = self.themes[next].name.clone();
        self.set_theme(&name);
        cx.notify();
    }

    fn rebuild_palette(&mut self) {
        let q = self.palette_input.to_lowercase();
        let mut all = vec![
            PaletteItem { label: "New Agent".into(), action: PaletteAction::NewAgent },
            PaletteItem { label: "New Group".into(), action: PaletteAction::NewGroup },
        ];
        for t in &self.themes {
            let active = if t.name == self.config.theme { " (active)" } else { "" };
            all.push(PaletteItem {
                label: format!("Theme: {}{}", t.name, active),
                action: PaletteAction::SetTheme(t.name.clone()),
            });
        }
        all.push(PaletteItem { label: "View: Grid".into(), action: PaletteAction::SetView(ViewMode::Grid) });
        all.push(PaletteItem { label: "View: Pipeline".into(), action: PaletteAction::SetView(ViewMode::Pipeline) });
        all.push(PaletteItem { label: "View: Focus".into(), action: PaletteAction::SetView(ViewMode::Focus) });
        all.push(PaletteItem { label: "Toggle Sidebar: Agents/Workers".into(), action: PaletteAction::ToggleSidebarTab });
        all.push(PaletteItem {
            label: format!("Setting: Cautious Enter ({})", if self.config.cautious_enter { "on" } else { "off" }),
            action: PaletteAction::ToggleCautiousEnter,
        });
        all.push(PaletteItem {
            label: format!("Setting: Terminal Text ({})", if self.config.terminal_text { "on" } else { "off" }),
            action: PaletteAction::ToggleTerminalText,
        });
        for pct in [100, 90, 80, 70, 60, 50, 40, 30, 20, 10] {
            let val = pct as f32 / 100.0;
            let active = if (self.config.bg_opacity - val).abs() < 0.01 { " (active)" } else { "" };
            all.push(PaletteItem {
                label: format!("Background Opacity: {}%{}", pct, active),
                action: PaletteAction::SetBgOpacity(val),
            });
        }
        for pct in [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            let val = pct as f32 / 100.0;
            let active = if (self.config.bg_blur - val).abs() < 0.01 { " (active)" } else { "" };
            all.push(PaletteItem {
                label: format!("Background Blur: {}%{}", pct, active),
                action: PaletteAction::SetBgBlur(val),
            });
        }
        if VOICE_ENABLED {
            let model_status = if whisper_model_exists(&self.config.whisper_model) { "installed" } else { "not found" };
            all.push(PaletteItem {
                label: format!("Setting: Whisper Model ({}) [{}]", self.config.whisper_model, model_status),
                action: PaletteAction::Quit,
            });
            let devices = list_audio_input_devices();
            let current = if self.config.audio_device.is_empty() { "system default" } else { &self.config.audio_device };
            for dev_name in &devices {
                let active = if *dev_name == self.config.audio_device || (self.config.audio_device.is_empty() && dev_name == devices.first().unwrap_or(&String::new())) {
                    " (active)"
                } else {
                    ""
                };
                all.push(PaletteItem {
                    label: format!("Mic: {}{}", dev_name, active),
                    action: PaletteAction::SetAudioDevice(dev_name.clone()),
                });
            }
            if devices.is_empty() {
                all.push(PaletteItem {
                    label: format!("Mic: {} (no devices found)", current),
                    action: PaletteAction::Quit,
                });
            }
        }
        all.push(PaletteItem { label: "Compact Context".into(), action: PaletteAction::CompactContext });
        all.push(PaletteItem { label: "Kill Agent".into(), action: PaletteAction::KillCurrent });
        all.push(PaletteItem { label: "Quit".into(), action: PaletteAction::Quit });
        self.palette_items = all.into_iter().filter(|i| q.is_empty() || i.label.to_lowercase().contains(&q)).collect();
        self.palette_selection = 0;
    }

    fn start_setup(&mut self) {
        let last_rt = &self.config.last_runtime;
        let rt_cursor = self.config.runtimes.iter().position(|r| r.name == *last_rt).unwrap_or(0);
        let last_machine = &self.config.last_machine;
        let machine_cursor = self.config.machines.iter().position(|m| m.name == *last_machine).unwrap_or(0);
        let selected_mcps: Vec<bool> = self.config.mcps.iter()
            .map(|m| m.global || self.config.last_mcps.contains(&m.name))
            .collect();
        let rt = self.config.runtimes.get(rt_cursor);
        let last_model = rt.map(|r| r.last_model.clone()).unwrap_or_default();
        let model_cursor = rt.and_then(|r| r.models.iter().position(|m| m.id == last_model)).unwrap_or(0);
        self.setup = Some(SetupState {
            step: SetupStep::Machine,
            runtime_cursor: rt_cursor,
            selected_runtime: rt.map(|r| r.name.clone()).unwrap_or_default(),
            model_cursor,
            selected_model: last_model,
            machine_cursor,
            selected_machine: self.config.machines.get(machine_cursor).map(|m| m.name.clone()).unwrap_or_else(|| "local".into()),
            dir_filter: String::new(),
            dir_entries: list_home_dirs(),
            dir_cursor: 0,
            selected_dir: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            mcp_cursor: 0,
            selected_mcps,
            model_filter: String::new(),
            editing_agent: None,
            custom_mode: false,
            custom_field: 0,
            custom_base_url: String::new(),
            custom_api_key: String::new(),
            custom_model_id: String::new(),
        });
        self.set_mode(Mode::Setup);
    }

    fn start_change_agent(&mut self, cx: &mut Context<Self>) {
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx >= self.agents.len() { return; }
        if self.agents[idx].status == AgentStatus::Working { return; }

        // Extract values before borrowing self again
        let rt_name = self.agents[idx].runtime_name.clone();
        let model_id = self.agents[idx].tokens.model.clone();
        let machine_name = self.agents[idx].target_machine.clone();
        let rt_cursor = self.config.runtimes.iter().position(|r| r.name == rt_name).unwrap_or(0);
        let machine_cursor = self.config.machines.iter().position(|m| m.name == machine_name).unwrap_or(0);
        let models = self.get_models_for_runtime(&rt_name);
        let model_cursor = models.iter().position(|m| m.id == model_id).unwrap_or(0);
        let selected_mcps: Vec<bool> = self.config.mcps.iter()
            .map(|m| m.global || self.config.last_mcps.contains(&m.name))
            .collect();
        let need_fetch_oc = rt_name == "opencode";
        let need_fetch_cursor = rt_name == "cursor";

        self.setup = Some(SetupState {
            step: SetupStep::Machine,
            runtime_cursor: rt_cursor,
            selected_runtime: rt_name,
            model_cursor,
            selected_model: model_id,
            machine_cursor,
            selected_machine: self.config.machines.get(machine_cursor).map(|m| m.name.clone()).unwrap_or_else(|| "local".into()),
            dir_filter: String::new(),
            dir_entries: list_home_dirs(),
            dir_cursor: 0,
            selected_dir: self.agents[idx].working_dir.clone(),
            mcp_cursor: 0,
            selected_mcps,
            model_filter: String::new(),
            editing_agent: Some(idx),
            custom_mode: false,
            custom_field: 0,
            custom_base_url: String::new(),
            custom_api_key: String::new(),
            custom_model_id: String::new(),
        });
        self.set_mode(Mode::Setup);
        if need_fetch_oc { self.fetch_opencode_models(cx); }
        if need_fetch_cursor { self.fetch_cursor_models(cx); }
    }

    fn finish_setup(&mut self, cx: &mut Context<Self>) {
        if let Some(setup) = self.setup.take() {
            let model = if setup.selected_model.is_empty() { None } else { Some(setup.selected_model.clone()) };

            // Apply custom endpoint env vars to runtime config before creating agent
            if setup.custom_mode && !setup.custom_base_url.is_empty() {
                if let Some(rt) = self.config.runtimes.iter_mut().find(|r| r.name == setup.selected_runtime) {
                    rt.env_set.clear();
                    if setup.selected_runtime == "claude" {
                        // Claude Code: ANTHROPIC_BASE_URL + ANTHROPIC_AUTH_TOKEN, clear ANTHROPIC_API_KEY
                        rt.env_set.push(("ANTHROPIC_BASE_URL".into(), setup.custom_base_url.clone()));
                        if !setup.custom_api_key.is_empty() {
                            rt.env_set.push(("ANTHROPIC_AUTH_TOKEN".into(), setup.custom_api_key.clone()));
                        }
                        rt.env_set.push(("ANTHROPIC_API_KEY".into(), String::new()));
                    } else if setup.selected_runtime == "codex" {
                        // Codex: OPENAI_BASE_URL + OPENAI_API_KEY
                        rt.env_set.push(("OPENAI_BASE_URL".into(), setup.custom_base_url.clone()));
                        if !setup.custom_api_key.is_empty() {
                            rt.env_set.push(("OPENAI_API_KEY".into(), setup.custom_api_key.clone()));
                        }
                    } else if setup.selected_runtime == "opencode" {
                        // OpenCode: OPENROUTER_API_KEY (built-in OpenRouter support)
                        // For non-OpenRouter endpoints, use custom provider in opencode.json
                        if !setup.custom_api_key.is_empty() {
                            rt.env_set.push(("OPENROUTER_API_KEY".into(), setup.custom_api_key.clone()));
                        }
                    }
                }
            }

            if let Some(edit_idx) = setup.editing_agent {
                // Changing existing agent: kill old, create new in same slot
                if edit_idx < self.agents.len() {
                    let old = &self.agents[edit_idx];
                    let name = old.name.clone();
                    let group = old.group.clone();
                    // Kill old agent
                    self.agents[edit_idx].prompt_tx = None;
                    self.agents[edit_idx]._reader_task = None;
                    // Remove and insert new at same index
                    self.agents.remove(edit_idx);
                    let n = self.agents.len();
                    self.create_agent_with_role(
                        &name,
                        &group,
                        &setup.selected_runtime,
                        model.as_deref(),
                        &setup.selected_machine,
                        AgentRole::Coordinator,
                        Some(self.coordinator_preamble()),
                        None,
                        None,
                        cx,
                    );
                    if n > edit_idx {
                        let last = self.agents.pop().unwrap();
                        self.agents.insert(edit_idx, last);
                    }
                    if !setup.selected_dir.is_empty() {
                        self.agents[edit_idx].working_dir = setup.selected_dir.clone();
                    }
                    self.set_focus(edit_idx);
                }
            } else {
                let n = self.agents.len();
                let group = self.current_group_name().to_string();
                self.create_agent_with_role(
                    &format!("agent-{}", n),
                    &group,
                    &setup.selected_runtime,
                    model.as_deref(),
                    &setup.selected_machine,
                    AgentRole::Coordinator,
                    Some(self.coordinator_preamble()),
                    None,
                    None,
                    cx,
                );
                if !setup.selected_dir.is_empty() {
                    self.agents[n].working_dir = setup.selected_dir.clone();
                }
                self.set_focus(n);
            }

            // Save last-used options
            self.config.last_runtime = setup.selected_runtime.clone();
            self.config.last_machine = setup.selected_machine.clone();
            self.config.last_mcps = self.config.mcps.iter().enumerate()
                .filter(|(i, m)| !m.global && setup.selected_mcps.get(*i).copied().unwrap_or(false))
                .map(|(_, m)| m.name.clone())
                .collect();
            if let Some(rt) = self.config.runtimes.iter_mut().find(|r| r.name == setup.selected_runtime) {
                rt.last_model = setup.selected_model;
            }
            self.save_config();
            self.save_state();
        }
        self.set_mode(Mode::Normal);
    }

    // ── Actions ─────────────────────────────────────────────────

    fn enter_command_mode(&mut self, _: &EnterCommandMode, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Setup { self.setup = None; }
        if self.mode == Mode::Search { self.search_query.clear(); }
        self.set_mode(Mode::Normal);
        self.palette_input.clear();
        cx.notify();
    }

    fn open_palette(&mut self, _: &OpenPalette, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Palette);
        self.palette_input.clear();
        self.rebuild_palette();
        cx.notify();
    }

    fn close_palette(&mut self, _: &ClosePalette, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Normal);
        self.palette_input.clear();
        cx.notify();
    }

    fn submit_input(&mut self, _: &SubmitInput, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                self.clamp_focus();
                let idx = self.focused_agent;
                if idx >= self.agents.len() { return; }
                if self.agents[idx].status == AgentStatus::Working { return; }
                let prompt = self.agents[idx].input_buffer.trim().to_string();
                if prompt.is_empty() { return; }
                self.agents[idx].input_buffer.clear();
                self.agents[idx].input_cursor = 0;
                self.set_mode(Mode::Normal);
                self.send_prompt(idx, prompt, cx);
            }
            Mode::Palette => {
                if let Some(item) = self.palette_items.get(self.palette_selection) {
                    match &item.action {
                        PaletteAction::NewAgent => { self.start_setup(); }
                        PaletteAction::NewGroup => {
                            let n = self.groups.len();
                            self.groups.push(Group { name: format!("group-{}", n) });
                            self.focused_group = n;
                            self.save_config();
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::SetTheme(name) => {
                            let name = name.clone();
                            self.set_theme(&name);
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::SetView(vm) => {
                            self.view_mode = *vm;
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::ToggleSidebarTab => {
                            self.sidebar_tab = match self.sidebar_tab {
                                SidebarTab::Agents => SidebarTab::Workers,
                                SidebarTab::Workers => SidebarTab::Agents,
                            };
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::ToggleCautiousEnter => {
                            self.config.cautious_enter = !self.config.cautious_enter;
                            self.save_config();
                            self.rebuild_palette();
                        }
                        PaletteAction::ToggleTerminalText => {
                            self.config.terminal_text = !self.config.terminal_text;
                            self.save_config();
                            self.rebuild_palette();
                        }
                        PaletteAction::SetBgOpacity(val) => {
                            self.config.bg_opacity = *val;
                            self.save_config();
                            self.rebuild_palette();
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::SetBgBlur(val) => {
                            self.config.bg_blur = *val;
                            self.save_config();
                            self.rebuild_palette();
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::SetAudioDevice(name) => {
                            let name = name.clone();
                            self.config.audio_device = name.clone();
                            self.save_config();
                            if let Some(a) = self.agents.get_mut(self.focused_agent) {
                                a.output_lines.push(format!("[voice] mic set to: {}", name));
                            }
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::CompactContext => {
                            self.clamp_focus();
                            let idx = self.focused_agent;
                            if idx < self.agents.len() {
                                let compact_cmd = match self.agents[idx].runtime_name.as_str() {
                                    "claude" => "/compact",
                                    "cursor" => "/summarize",
                                    "codex" => "/compact",
                                    "opencode" => "/compact",
                                    _ => "/compact",
                                };
                                let prompt = compact_cmd.to_string();
                                self.set_mode(Mode::Normal);
                                self.send_prompt(idx, prompt, cx);
                            }
                        }
                        PaletteAction::KillCurrent => {
                            self.clamp_focus();
                            let idx = self.focused_agent;
                            if idx < self.agents.len() {
                                self.agents[idx].prompt_tx = None;
                                self.agents[idx]._reader_task = None;
                                self.agents[idx].status = AgentStatus::Idle;
                                self.agents[idx].output_lines.push("[killed]".into());
                            }
                            self.set_mode(Mode::Normal);
                        }
                        PaletteAction::Quit => { cx.quit(); return; }
                    }
                } else {
                    self.set_mode(Mode::Normal);
                }
                self.palette_input.clear();
                cx.notify();
            }
            Mode::Setup => {
                let is_dir_step = self.setup.as_ref().map(|s| s.step == SetupStep::Directory).unwrap_or(false);
                if is_dir_step {
                    // Enter drills into the selected directory
                    if let Some(ref mut s) = self.setup {
                        let filtered = Self::filter_dirs(&s.dir_entries, &s.dir_filter);
                        if let Some(dir) = filtered.get(s.dir_cursor).cloned() {
                            let path = std::path::Path::new(&dir);
                            if path.is_dir() {
                                s.selected_dir = dir.clone();
                                s.dir_entries = list_subdirs(&dir);
                                s.dir_filter.clear();
                                s.dir_cursor = 0;
                            }
                        }
                    }
                    cx.notify();
                    return;
                }
                self.setup_next(&SetupNext, _w, cx);
                return;
            }
            Mode::Search => {
                // Jump to selected search result
                let search_target = self.search_results.get(self.search_selection)
                    .map(|r| (r.agent_idx, r.line_idx));
                if let Some((agent_idx, line_idx)) = search_target {
                    self.set_focus(agent_idx);
                    if let Some(a) = self.agents.get(agent_idx) {
                        if let Some(gi) = self.groups.iter().position(|g| g.name == a.group) {
                            self.focused_group = gi;
                        }
                    }
                    if let Some(a) = self.agents.get_mut(agent_idx) {
                        a.scroll_offset = line_idx.saturating_sub(5);
                    }
                }
                self.set_mode(Mode::Normal);
                self.search_query.clear();
                cx.notify();
            }
            _ => {}
        }
    }

    fn delete_char(&mut self, _: &DeleteChar, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                if let Some(a) = self.agents.get_mut(self.focused_agent) {
                    if a.input_cursor > 0 {
                        // Find previous char boundary
                        let prev = a.input_buffer[..a.input_cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                        a.input_buffer.drain(prev..a.input_cursor);
                        a.input_cursor = prev;
                    }
                }
            }
            Mode::Palette => { self.palette_input.pop(); self.rebuild_palette(); }
            Mode::Search => { self.search_query.pop(); self.rebuild_search(); }
            Mode::Setup => {
                let mut did_delete = false;
                if let Some(ref mut s) = self.setup {
                    if s.step == SetupStep::Directory {
                        if !s.dir_filter.is_empty() {
                            s.dir_filter.pop();
                            s.dir_cursor = 0;
                            did_delete = true;
                        } else {
                            // Empty filter + backspace = go up to parent directory
                            let parent = std::path::Path::new(&s.selected_dir)
                                .parent()
                                .map(|p| p.display().to_string());
                            if let Some(parent_dir) = parent {
                                if !parent_dir.is_empty() {
                                    s.selected_dir = parent_dir.clone();
                                    s.dir_entries = list_subdirs(&parent_dir);
                                    s.dir_cursor = 0;
                                    did_delete = true;
                                }
                            }
                        }
                    } else if s.step == SetupStep::Model {
                        if s.custom_mode {
                            let has_text = match s.custom_field {
                                0 => !s.custom_base_url.is_empty(),
                                1 => !s.custom_api_key.is_empty(),
                                2 => !s.custom_model_id.is_empty(),
                                _ => false,
                            };
                            if has_text {
                                match s.custom_field {
                                    0 => { s.custom_base_url.pop(); }
                                    1 => { s.custom_api_key.pop(); }
                                    2 => { s.custom_model_id.pop(); }
                                    _ => {}
                                }
                                did_delete = true;
                            }
                        } else if !s.model_filter.is_empty() {
                            s.model_filter.pop();
                            s.model_cursor = 0;
                            did_delete = true;
                        }
                    }
                }
                // If no text was deleted, go back a step
                if !did_delete {
                    self.setup_prev(&SetupPrev, _w, cx);
                    return;
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let is_setup_typing = self.mode == Mode::Setup
            && self.setup.as_ref().map(|s| matches!(s.step, SetupStep::Model | SetupStep::Directory)).unwrap_or(false);
        if self.mode == Mode::Normal || self.mode == Mode::Palette || self.mode == Mode::Search || is_setup_typing {
            if event.keystroke.modifiers.platform || event.keystroke.modifiers.control || event.keystroke.modifiers.alt { return; }
            match event.keystroke.key.as_str() {
                "escape" | "enter" | "backspace" | "tab" | "left" | "right" | "up" | "down" => return,
                _ => {}
            }
            if let Some(ch) = &event.keystroke.key_char {
                match self.mode {
                    Mode::Normal => {
                        if let Some(a) = self.agents.get_mut(self.focused_agent) {
                            a.input_buffer.insert_str(a.input_cursor, ch);
                            a.input_cursor += ch.len();
                            cx.notify();
                        }
                    }
                    Mode::Palette => { self.palette_input.push_str(ch); self.rebuild_palette(); cx.notify(); }
                    Mode::Search => { self.search_query.push_str(ch); self.rebuild_search(); cx.notify(); }
                    Mode::Setup => {
                        if let Some(ref mut s) = self.setup {
                            match s.step {
                                SetupStep::Directory => {
                                    s.dir_filter.push_str(ch);
                                    s.dir_cursor = 0;
                                }
                                SetupStep::Model => {
                                    if s.custom_mode {
                                        match s.custom_field {
                                            0 => s.custom_base_url.push_str(ch),
                                            1 => s.custom_api_key.push_str(ch),
                                            2 => s.custom_model_id.push_str(ch),
                                            _ => {}
                                        }
                                    } else {
                                        s.model_filter.push_str(ch);
                                        s.model_cursor = 0;
                                    }
                                }
                                _ => {}
                            }
                        }
                        cx.notify();
                    }
                    _ => {}
                }
            }
        }
    }

    // Setup navigation
    fn setup_next(&mut self, _: &SetupNext, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(ref s) = self.setup else { return; };
        let step = s.step;
        let runtime_cursor = s.runtime_cursor;
        let selected_runtime = s.selected_runtime.clone();
        let model_cursor = s.model_cursor;
        let model_filter = s.model_filter.clone();

        match step {
            SetupStep::Machine => {
                let machine_name = self.config.machines.get(
                    self.setup.as_ref().map(|s| s.machine_cursor).unwrap_or(0)
                ).map(|m| m.name.clone()).unwrap_or_else(|| "local".into());
                if let Some(ref mut s) = self.setup {
                    s.selected_machine = machine_name.clone();
                    // Refresh dir listing for the selected machine
                    if machine_name == "local" {
                        s.dir_entries = list_home_dirs();
                    }
                    s.step = SetupStep::Directory;
                }
            }
            SetupStep::Directory => {
                // Tab confirms current selected_dir and advances
                if let Some(ref mut s) = self.setup {
                    s.step = SetupStep::Runtime;
                }
            }
            SetupStep::Runtime => {
                let rt_name = self.config.runtimes.get(runtime_cursor)
                    .map(|r| r.name.clone()).unwrap_or_default();
                let models = self.get_models_for_runtime(&rt_name);
                let last_model = self.config.runtimes.get(runtime_cursor)
                    .map(|r| r.last_model.clone()).unwrap_or_default();
                let mc = models.iter().position(|m| m.id == last_model).unwrap_or(0);
                let sel_model = models.get(mc).map(|m| m.id.clone()).unwrap_or_default();
                let need_fetch_oc = rt_name == "opencode";
                let need_fetch_cursor = rt_name == "cursor";

                if let Some(ref mut s) = self.setup {
                    s.selected_runtime = rt_name;
                    s.model_cursor = mc;
                    s.model_filter.clear();
                    s.selected_model = sel_model;
                    s.step = SetupStep::Model;
                }
                if need_fetch_oc { self.fetch_opencode_models(cx); }
                if need_fetch_cursor { self.fetch_cursor_models(cx); }
            }
            SetupStep::Model => {
                let custom_mode = self.setup.as_ref().map(|s| s.custom_mode).unwrap_or(false);
                if custom_mode {
                    let field = self.setup.as_ref().map(|s| s.custom_field).unwrap_or(0);
                    if field < 2 {
                        if let Some(ref mut s) = self.setup { s.custom_field = field + 1; }
                    } else {
                        let model_id = self.setup.as_ref().map(|s| s.custom_model_id.clone()).unwrap_or_default();
                        if let Some(ref mut s) = self.setup {
                            s.selected_model = model_id;
                            s.step = SetupStep::Mcps;
                        }
                    }
                } else {
                    let models = self.get_models_for_runtime(&selected_runtime);
                    let q = model_filter.to_lowercase();
                    let filtered: Vec<(usize, &ModelOption)> = if q.is_empty() {
                        models.iter().enumerate().collect()
                    } else {
                        models.iter().enumerate()
                            .filter(|(_, m)| m.label.to_lowercase().contains(&q) || m.id.to_lowercase().contains(&q))
                            .collect()
                    };
                    let sel_model = filtered.get(model_cursor)
                        .map(|(_, m)| m.id.clone())
                        .unwrap_or_default();
                    if let Some(ref mut s) = self.setup {
                        s.selected_model = sel_model;
                        s.step = SetupStep::Mcps;
                    }
                }
            }
            SetupStep::Mcps => {
                if let Some(ref mut s) = self.setup { s.step = SetupStep::Confirm; }
            }
            SetupStep::Confirm => { self.finish_setup(cx); return; }
        }
        cx.notify();
    }

    fn setup_prev(&mut self, _: &SetupPrev, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut s) = self.setup {
            match s.step {
                SetupStep::Machine => {} // can't go back
                SetupStep::Directory => { s.step = SetupStep::Machine; }
                SetupStep::Runtime => { s.step = SetupStep::Directory; }
                SetupStep::Model => { s.step = SetupStep::Runtime; }
                SetupStep::Mcps => { s.step = SetupStep::Model; }
                SetupStep::Confirm => { s.step = SetupStep::Mcps; }
            }
        }
        cx.notify();
    }

    fn setup_toggle(&mut self, _: &SetupToggle, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut s) = self.setup {
            match s.step {
                SetupStep::Mcps => {
                    let idx = s.mcp_cursor;
                    if idx < s.selected_mcps.len() {
                        // Don't allow deselecting global MCPs
                        if !self.config.mcps[idx].global {
                            s.selected_mcps[idx] = !s.selected_mcps[idx];
                        }
                    }
                }
                SetupStep::Machine => {
                    s.selected_machine = self.config.machines.get(s.machine_cursor)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| "local".into());
                }
                _ => {}
            }
        }
        cx.notify();
    }

    fn toggle_custom_endpoint(&mut self, _: &ToggleCustomEndpoint, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut s) = self.setup {
            if s.step == SetupStep::Model {
                let rt = &s.selected_runtime;
                // Only allow custom endpoints for runtimes that support them
                // (cursor is the only one that doesn't)
                if rt == "claude" || rt == "codex" || rt == "opencode" {
                    s.custom_mode = !s.custom_mode;
                    s.custom_field = 0;
                    if s.custom_mode && s.custom_base_url.is_empty() {
                        // Pre-fill base URL with sensible default per runtime
                        s.custom_base_url = if rt == "claude" {
                            "https://openrouter.ai/api".into()
                        } else {
                            "https://openrouter.ai/api/v1".into()
                        };
                    }
                }
            }
        }
        cx.notify();
    }

    // ── Text editing (Insert mode) ────────────────────────────────

    fn cursor_left(&mut self, _: &CursorLeft, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor > 0 {
                a.input_cursor = a.input_buffer[..a.input_cursor]
                    .char_indices().last().map(|(i, _)| i).unwrap_or(0);
            }
        }
        cx.notify();
    }

    fn cursor_right(&mut self, _: &CursorRight, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor < a.input_buffer.len() {
                a.input_cursor = a.input_buffer[a.input_cursor..]
                    .char_indices().nth(1).map(|(i, _)| a.input_cursor + i)
                    .unwrap_or(a.input_buffer.len());
            }
        }
        cx.notify();
    }

    fn cursor_word_left(&mut self, _: &CursorWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let s = &a.input_buffer[..a.input_cursor];
            // Skip trailing whitespace, then skip word chars
            let trimmed = s.trim_end();
            if trimmed.is_empty() { a.input_cursor = 0; }
            else {
                let last_space = trimmed.rfind(|c: char| c.is_whitespace()).map(|i| i + 1).unwrap_or(0);
                a.input_cursor = last_space;
            }
        }
        cx.notify();
    }

    fn cursor_word_right(&mut self, _: &CursorWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let s = &a.input_buffer[a.input_cursor..];
            // Skip current word chars, then skip whitespace
            let word_end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
            let after_ws = s[word_end..].find(|c: char| !c.is_whitespace()).map(|i| word_end + i).unwrap_or(s.len());
            a.input_cursor += after_ws;
        }
        cx.notify();
    }

    fn cursor_home(&mut self, _: &CursorHome, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            // Go to start of current line
            let before = &a.input_buffer[..a.input_cursor];
            a.input_cursor = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        cx.notify();
    }

    fn cursor_end(&mut self, _: &CursorEnd, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            // Go to end of current line
            let after = &a.input_buffer[a.input_cursor..];
            a.input_cursor += after.find('\n').unwrap_or(after.len());
        }
        cx.notify();
    }

    fn delete_word_back(&mut self, _: &DeleteWordBack, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor > 0 {
                let s = &a.input_buffer[..a.input_cursor];
                let trimmed = s.trim_end();
                let target = if trimmed.is_empty() { 0 }
                    else { trimmed.rfind(|c: char| c.is_whitespace()).map(|i| i + 1).unwrap_or(0) };
                a.input_buffer.drain(target..a.input_cursor);
                a.input_cursor = target;
            }
        }
        cx.notify();
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor > 0 {
                // Delete to start of current line
                let before = &a.input_buffer[..a.input_cursor];
                let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                a.input_buffer.drain(line_start..a.input_cursor);
                a.input_cursor = line_start;
            }
        }
        cx.notify();
    }

    fn insert_newline(&mut self, _: &InsertNewline, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if !self.config.cautious_enter {
            self.submit_input(&SubmitInput, _w, cx);
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.input_buffer.insert(a.input_cursor, '\n');
            a.input_cursor += 1;
        }
        cx.notify();
    }

    fn paste_clipboard(&mut self, _: &PasteClipboard, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        if let Some(clip) = cx.read_from_clipboard() {
            let text = clip.text().unwrap_or_default();
            if !text.is_empty() {
                if let Some(a) = self.agents.get_mut(self.focused_agent) {
                    a.input_buffer.insert_str(a.input_cursor, &text);
                    a.input_cursor += text.len();
                }
                cx.notify();
            }
        }
    }

    // Navigation
    fn nav_up(&mut self, _: &NavUp, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                self.focused_group = self.focused_group.saturating_sub(1);
                self.clamp_focus();
            }
            Mode::Setup => {
                if let Some(ref mut s) = self.setup {
                    match s.step {
                        SetupStep::Runtime => { s.runtime_cursor = s.runtime_cursor.saturating_sub(1); }
                        SetupStep::Model => {
                            if s.custom_mode {
                                s.custom_field = s.custom_field.saturating_sub(1);
                            } else {
                                s.model_cursor = s.model_cursor.saturating_sub(1);
                            }
                        }
                        SetupStep::Machine => { s.machine_cursor = s.machine_cursor.saturating_sub(1); }
                        SetupStep::Directory => { s.dir_cursor = s.dir_cursor.saturating_sub(1); }
                        SetupStep::Mcps => { s.mcp_cursor = s.mcp_cursor.saturating_sub(1); }
                        _ => {}
                    }
                }
            }
            Mode::Palette => {
                self.palette_selection = self.palette_selection.saturating_sub(1);
            }
            Mode::Search => {
                self.search_selection = self.search_selection.saturating_sub(1);
            }
            _ => {}
        }
        cx.notify();
    }

    fn nav_down(&mut self, _: &NavDown, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                if self.focused_group < self.groups.len().saturating_sub(1) { self.focused_group += 1; }
                self.clamp_focus();
            }
            Mode::Setup => {
                // Extract what we need before mutating
                let info = self.setup.as_ref().map(|s| (s.step, s.selected_runtime.clone(), s.model_filter.clone()));
                if let Some((step, rt_name, filter)) = info {
                    let max = match step {
                        SetupStep::Runtime => self.config.runtimes.len().saturating_sub(1),
                        SetupStep::Model => {
                            let models = self.get_models_for_runtime(&rt_name);
                            let q = filter.to_lowercase();
                            let count = if q.is_empty() { models.len() } else {
                                models.iter().filter(|m| m.label.to_lowercase().contains(&q) || m.id.to_lowercase().contains(&q)).count()
                            };
                            count.saturating_sub(1)
                        }
                        SetupStep::Machine => self.config.machines.len().saturating_sub(1),
                        SetupStep::Directory => {
                            let filter = self.setup.as_ref().map(|s| s.dir_filter.clone()).unwrap_or_default();
                            let entries = self.setup.as_ref().map(|s| s.dir_entries.clone()).unwrap_or_default();
                            Self::filter_dirs(&entries, &filter).len().saturating_sub(1)
                        }
                        SetupStep::Mcps => self.config.mcps.len().saturating_sub(1),
                        SetupStep::Confirm => 0,
                    };
                    if let Some(ref mut s) = self.setup {
                        match step {
                            SetupStep::Runtime => s.runtime_cursor = (s.runtime_cursor + 1).min(max),
                            SetupStep::Model => {
                                if s.custom_mode {
                                    s.custom_field = (s.custom_field + 1).min(2);
                                } else {
                                    s.model_cursor = (s.model_cursor + 1).min(max);
                                }
                            }
                            SetupStep::Machine => s.machine_cursor = (s.machine_cursor + 1).min(max),
                            SetupStep::Directory => s.dir_cursor = (s.dir_cursor + 1).min(max),
                            SetupStep::Mcps => s.mcp_cursor = (s.mcp_cursor + 1).min(max),
                            _ => {}
                        }
                    }
                }
            }
            Mode::Palette => {
                let max = self.palette_items.len().saturating_sub(1);
                self.palette_selection = (self.palette_selection + 1).min(max);
            }
            Mode::Search => {
                let max = self.search_results.len().saturating_sub(1);
                self.search_selection = (self.search_selection + 1).min(max);
            }
            _ => {}
        }
        cx.notify();
    }

    fn pane_left(&mut self, _: &PaneLeft, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        let vis = self.agents_in_current_group();
        if vis.is_empty() { return; }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            if pos > 0 { self.set_focus(vis[pos - 1]); }
        } else { self.set_focus(vis[0]); }
        cx.notify();
    }

    fn pane_right(&mut self, _: &PaneRight, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        let vis = self.agents_in_current_group();
        if vis.is_empty() { return; }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            if pos < vis.len() - 1 { self.set_focus(vis[pos + 1]); }
        } else { self.set_focus(vis[0]); }
        cx.notify();
    }

    fn next_pane(&mut self, _: &NextPane, _w: &mut Window, cx: &mut Context<Self>) {
        let vis = self.agents_in_current_group();
        if vis.is_empty() { return; }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            let next = (pos + 1) % vis.len();
            self.set_focus(vis[next]);
        } else { self.set_focus(vis[0]); }
        cx.notify();
    }

    fn prev_pane(&mut self, _: &PrevPane, _w: &mut Window, cx: &mut Context<Self>) {
        let vis = self.agents_in_current_group();
        if vis.is_empty() { return; }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            let prev = if pos == 0 { vis.len() - 1 } else { pos - 1 };
            self.set_focus(vis[prev]);
        } else { self.set_focus(vis[0]); }
        cx.notify();
    }

    fn next_group(&mut self, _: &NextGroup, _w: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() <= 1 { return; }
        self.focused_group = (self.focused_group + 1) % self.groups.len();
        self.clamp_focus();
        cx.notify();
    }

    fn prev_group(&mut self, _: &PrevGroup, _w: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() <= 1 { return; }
        self.focused_group = if self.focused_group == 0 { self.groups.len() - 1 } else { self.focused_group - 1 };
        self.clamp_focus();
        cx.notify();
    }

    fn scroll_up(&mut self, _: &ScrollUp, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) { a.scroll_offset = a.scroll_offset.saturating_sub(3); }
        cx.notify();
    }

    fn scroll_down(&mut self, _: &ScrollDown, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let max = a.output_lines.len().saturating_sub(1);
            a.scroll_offset = (a.scroll_offset + 3).min(max);
        }
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) { a.scroll_offset = a.scroll_offset.saturating_sub(20); }
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let max = a.output_lines.len().saturating_sub(1);
            a.scroll_offset = (a.scroll_offset + 20).min(max);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) { a.scroll_offset = 0; }
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let len = a.output_lines.len();
            a.scroll_offset = if len > 40 { len - 40 } else { 0 };
        }
        cx.notify();
    }

    fn toggle_voice(&mut self, _: &ToggleVoice, _w: &mut Window, cx: &mut Context<Self>) {
        if !VOICE_ENABLED { return; }
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();

        if self.voice_recording {
            self.voice_recording = false;
            self.voice_stream = None;

            let samples: Vec<f32> = {
                let mut buf = self.voice_audio_buffer.lock().unwrap();
                std::mem::take(&mut *buf)
            };

            if samples.is_empty() {
                if let Some(a) = self.agents.get_mut(self.focused_agent) {
                    a.output_lines.push("[voice] no audio captured".into());
                }
                cx.notify();
                return;
            }

            let native_rate = self.voice_native_rate.unwrap_or(16000);
            let samples = if native_rate != 16000 && native_rate > 0 {
                let ratio = native_rate as f64 / 16000.0;
                let new_len = (samples.len() as f64 / ratio) as usize;
                let mut resampled = Vec::with_capacity(new_len);
                for i in 0..new_len {
                    let src_idx = (i as f64 * ratio) as usize;
                    if src_idx < samples.len() {
                        resampled.push(samples[src_idx]);
                    }
                }
                resampled
            } else {
                samples
            };

            let peak = samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
            let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();

            if let Some(a) = self.agents.get_mut(self.focused_agent) {
                a.output_lines.push(format!(
                    "[voice] captured {:.1}s ({}Hz -> 16kHz) peak={:.4} rms={:.4}{}",
                    samples.len() as f64 / 16000.0,
                    native_rate,
                    peak,
                    rms,
                    if peak < 0.001 { " *** SILENCE - mic may not have permission ***" } else { "" }
                ));
            }

            let model_path = whisper_model_path(&self.config.whisper_model);
            let (tx, rx) = async_channel::bounded::<String>(1);
            self.voice_transcription_rx = Some(rx.clone());

            let focused_idx = self.focused_agent;
            std::thread::spawn(move || {
                let result = (|| -> Result<String, String> {
                    let ctx = WhisperContext::new_with_params(
                        model_path.to_str().unwrap_or(""),
                        WhisperContextParameters::default(),
                    ).map_err(|e| format!("failed to load whisper model: {}", e))?;

                    let mut state = ctx.create_state()
                        .map_err(|e| format!("failed to create whisper state: {}", e))?;

                    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                    params.set_n_threads(4);
                    params.set_language(Some("en"));
                    params.set_print_progress(false);
                    params.set_print_special(false);
                    params.set_print_realtime(false);
                    params.set_print_timestamps(false);

                    state.full(params, &samples)
                        .map_err(|e| format!("whisper inference failed: {}", e))?;

                    let n_segments = state.full_n_segments();

                    let mut text = String::new();
                    for i in 0..n_segments {
                        if let Some(seg) = state.get_segment(i) {
                            if let Ok(s) = seg.to_str() {
                                text.push_str(s);
                            }
                        }
                    }
                    Ok(text.trim().to_string())
                })();

                match result {
                    Ok(text) => { let _ = tx.send_blocking(text); }
                    Err(e) => { let _ = tx.send_blocking(format!("[error] {}", e)); }
                }
            });

            let rx = self.voice_transcription_rx.clone().unwrap();
            cx.spawn(async move |this, cx| {
                if let Ok(text) = rx.recv().await {
                    cx.update(|cx| {
                        this.update(cx, |this, cx| {
                            this.voice_transcription_rx = None;
                            if text.starts_with("[error]") {
                                if let Some(a) = this.agents.get_mut(focused_idx) {
                                    a.output_lines.push(format!("[voice] {}", text));
                                }
                            } else if !text.is_empty() {
                                if let Some(a) = this.agents.get_mut(focused_idx) {
                                    a.input_buffer.push_str(&text);
                                    a.input_cursor = a.input_buffer.len();
                                    a.output_lines.push(format!("[transcribed] {}", text));
                                }
                            } else {
                                if let Some(a) = this.agents.get_mut(focused_idx) {
                                    a.output_lines.push("[voice] no speech detected".into());
                                }
                            }
                            cx.notify();
                        }).ok();
                    }).ok();
                }
            }).detach();
        } else {
            if !whisper_model_exists(&self.config.whisper_model) {
                let expected = whisper_model_path(&self.config.whisper_model);
                if let Some(a) = self.agents.get_mut(self.focused_agent) {
                    a.output_lines.push(format!(
                        "[!] whisper model not found: {}",
                        expected.display()
                    ));
                    a.output_lines.push(format!(
                        "[!] download it: curl -L -o '{}' 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin'",
                        expected.display(), self.config.whisper_model
                    ));
                }
                cx.notify();
                return;
            }

            {
                let mut buf = self.voice_audio_buffer.lock().unwrap();
                buf.clear();
            }

            let buffer = Arc::clone(&self.voice_audio_buffer);
            let audio_device_name = self.config.audio_device.clone();
            let stream_result = (|| -> Result<(cpal::Stream, u32, u16, String), String> {
                let device = find_audio_device_by_name(&audio_device_name)
                    .ok_or_else(|| "no audio input device found".to_string())?;
                #[allow(deprecated)]
                let actual_name = device.name().unwrap_or_else(|_| "unknown".into());

                let default_config = device.default_input_config()
                    .map_err(|e| format!("failed to get default input config: {}", e))?;

                let native_rate = default_config.sample_rate();
                let native_channels = default_config.channels();

                let stream_config = cpal::StreamConfig {
                    channels: native_channels,
                    sample_rate: native_rate,
                    buffer_size: cpal::BufferSize::Default,
                };

                let ch = native_channels as usize;
                let buf_clone = Arc::clone(&buffer);
                let stream = device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut buf) = buf_clone.lock() {
                            if ch == 1 {
                                buf.extend_from_slice(data);
                            } else {
                                for chunk in data.chunks(ch) {
                                    buf.push(chunk[0]);
                                }
                            }
                        }
                    },
                    |err| {
                        eprintln!("[voice] audio stream error: {}", err);
                    },
                    None,
                ).map_err(|e| format!("failed to build audio stream: {}", e))?;

                stream.play().map_err(|e| format!("failed to start audio stream: {}", e))?;
                Ok((stream, native_rate, native_channels, actual_name))
            })();

            match stream_result {
                Ok((stream, native_rate, _channels, dev_name)) => {
                    self.voice_stream = Some(stream);
                    self.voice_recording = true;
                    self.voice_native_rate = Some(native_rate);
                    self.voice_model_path = Some(whisper_model_path(&self.config.whisper_model));
                    if let Some(a) = self.agents.get_mut(self.focused_agent) {
                        a.output_lines.push(format!(
                            "[recording on '{}' @ {}Hz...] press ` to stop",
                            dev_name, native_rate
                        ));
                    }
                }
                Err(e) => {
                    if let Some(a) = self.agents.get_mut(self.focused_agent) {
                        a.output_lines.push(format!("[!] voice: {}", e));
                    }
                }
            }
        }
        cx.notify();
    }

    fn spawn_agent(&mut self, _: &SpawnAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.start_setup();
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _w: &mut Window, cx: &mut Context<Self>) { self.ui_scale = (self.ui_scale + 0.1).min(2.0); cx.notify(); }
    fn zoom_out(&mut self, _: &ZoomOut, _w: &mut Window, cx: &mut Context<Self>) { self.ui_scale = (self.ui_scale - 0.1).max(0.5); cx.notify(); }
    fn zoom_reset(&mut self, _: &ZoomReset, _w: &mut Window, cx: &mut Context<Self>) { self.ui_scale = 1.0; cx.notify(); }
    fn kill_agent(&mut self, _: &KillAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx < self.agents.len() {
            // Drop the prompt sender to signal the thread to stop
            self.agents[idx].prompt_tx = None;
            self.agents[idx]._reader_task = None;
            self.agents[idx].status = AgentStatus::Idle;
            self.agents[idx].output_lines.push("[killed]".into());
        }
        cx.notify();
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.favorite = !a.favorite;
        }
        cx.notify();
    }

    fn change_agent(&mut self, _: &ChangeAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.start_change_agent(cx);
        cx.notify();
    }

    fn restart_agent(&mut self, _: &RestartAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx >= self.agents.len() { return; }
        if self.agents[idx].status == AgentStatus::Working { return; }
        let name = self.agents[idx].name.clone();
        let group = self.agents[idx].group.clone();
        let rt = self.agents[idx].runtime_name.clone();
        let model = self.agents[idx].last_model.clone();
        let target_machine = self.agents[idx].target_machine.clone();
        let role = self.agents[idx].role;
        let prompt_preamble = self.agents[idx].prompt_preamble.clone();
        let worker_assignment = self.agents[idx].worker_assignment.clone();
        // Kill old
        self.agents[idx].prompt_tx = None;
        self.agents[idx]._reader_task = None;
        self.agents.remove(idx);
        let n = self.agents.len();
        self.create_agent_with_role(
            &name,
            &group,
            &rt,
            model.as_deref(),
            &target_machine,
            role,
            prompt_preamble,
            worker_assignment,
            None,
            cx,
        );
        if n > idx {
            let last = self.agents.pop().unwrap();
            self.agents.insert(idx, last);
        }
        self.set_focus(idx);
        cx.notify();
    }

    fn toggle_auto_scroll(&mut self, _: &ToggleAutoScroll, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.auto_scroll = !a.auto_scroll;
            if a.auto_scroll {
                let len = a.output_lines.len();
                if len > 40 { a.scroll_offset = len - 40; }
            }
        }
        cx.notify();
    }

    fn show_stats(&mut self, _: &ShowStats, _w: &mut Window, cx: &mut Context<Self>) {
        self.show_stats = !self.show_stats;
        cx.notify();
    }

    fn render_starfield(&self, _cx: &Context<Self>) -> impl IntoElement {
        let stars_data: Vec<(f32, f32, f32, f32, f32, f32)> = self.stars.iter()
            .map(|s| (s.x, s.y, s.size, s.brightness, s.phase, s.speed))
            .collect();
        let tick = self.star_tick;
        let bg_hex = self.theme.bg;
        let bg_c = self.bg_alpha(rgba(bg_hex));
        canvas(
            move |_bounds, _window, _cx| {},
            move |bounds, _, window, _cx| {
                window.paint_quad(fill(bounds, bg_c));

                let time = tick as f32 * 0.033; // ~seconds
                let w: f32 = bounds.size.width.into();
                let h: f32 = bounds.size.height.into();

                for (sx, sy, size, brightness, phase, speed) in &stars_data {
                    let off_x: f32 = bounds.origin.x.into();
                    let off_y: f32 = bounds.origin.y.into();
                    let px_x = off_x + sx * w;
                    let px_y = off_y + sy * h;

                    // Twinkle: sinusoidal brightness modulation
                    let twinkle = 0.5 + 0.5 * (time * speed + phase).sin();
                    let alpha = brightness * (0.3 + 0.7 * twinkle);

                    let star_bounds = Bounds {
                        origin: point(px(px_x - size * 0.5), px(px_y - size * 0.5)),
                        size: Size { width: px(*size), height: px(*size) },
                    };

                    // Warm white with slight blue tint for some stars
                    let tint = if *brightness > 0.7 { 0.95 } else { 0.85 };
                    let star_color = Rgba { r: tint, g: tint, b: 1.0, a: alpha };
                    window.paint_quad(fill(star_bounds, star_color));
                }
            },
        ).size_full()
    }

    fn open_terminal(&mut self, _: &OpenTerminal, _w: &mut Window, _cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx < self.agents.len() {
            let dir = &self.agents[idx].working_dir;
            if !dir.is_empty() {
                let _ = Command::new("open").arg("-a").arg("Terminal").arg(dir).spawn();
            }
        }
    }

    fn pipe_to_agent(&mut self, _: &PipeToAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx >= self.agents.len() { return; }
        // Get last response from source agent
        let lines = &self.agents[idx].output_lines;
        if lines.is_empty() { return; }
        let last_prompt = lines.iter().rposition(|l| l.starts_with("> ")).unwrap_or(0);
        let start = (last_prompt + 1).min(lines.len());
        let response: String = lines[start..].join("\n").trim().to_string();
        if response.is_empty() { return; }
        // Find next agent in the same group
        let vis = self.agents_in_current_group();
        if vis.len() < 2 { return; }
        let cur_pos = vis.iter().position(|&i| i == idx).unwrap_or(0);
        let next_idx = vis[(cur_pos + 1) % vis.len()];
        if next_idx == idx || next_idx >= self.agents.len() { return; }
        if self.agents[next_idx].status == AgentStatus::Working { return; }
        // Pipe as a prompt
        let src_name = self.agents[idx].name.clone();
        let piped = format!("[piped from {}] {}", src_name, response);
        self.set_focus(next_idx);
        self.send_prompt(next_idx, piped, cx);
        cx.notify();
    }

    fn continue_turn(&mut self, _: &ContinueTurn, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.clamp_focus();
        let idx = self.focused_agent;
        self.continue_pending_turn(idx, cx);
        cx.notify();
    }

    fn view_grid(&mut self, _: &ViewGrid, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.view_mode = ViewMode::Grid; cx.notify();
    }
    fn view_pipeline(&mut self, _: &ViewPipeline, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.view_mode = ViewMode::Pipeline; cx.notify();
    }
    fn view_focus(&mut self, _: &ViewFocus, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.view_mode = ViewMode::Focus; cx.notify();
    }

    fn search_open(&mut self, _: &SearchOpen, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal { return; }
        self.set_mode(Mode::Search);
        self.search_query.clear();
        self.search_results.clear();
        self.search_selection = 0;
        cx.notify();
    }

    fn search_close(&mut self, _: &SearchClose, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Normal);
        self.search_query.clear();
        cx.notify();
    }

    fn rebuild_search(&mut self) {
        let q = self.search_query.to_lowercase();
        self.search_results.clear();
        if q.is_empty() { return; }
        for (ai, agent) in self.agents.iter().enumerate() {
            for (li, line) in agent.output_lines.iter().enumerate() {
                if line.to_lowercase().contains(&q) {
                    self.search_results.push(SearchResult {
                        agent_idx: ai,
                        agent_name: agent.name.clone(),
                        line_idx: li,
                        line: line.clone(),
                    });
                    if self.search_results.len() >= 50 { break; }
                }
            }
            if self.search_results.len() >= 50 { break; }
        }
        self.search_selection = 0;
    }

    fn quit_app(&mut self, _: &Quit, _w: &mut Window, cx: &mut Context<Self>) { self.save_config(); self.save_state(); cx.quit(); }

    // ── Render ──────────────────────────────────────────────────

    fn s(&self, base: f32) -> Pixels { px(base * self.ui_scale) }

    /// Apply background opacity to a color. Used for bg, surface, surface_raised so the desktop shows through.
    fn bg_alpha(&self, c: Rgba) -> Rgba {
        let o = self.config.bg_opacity;
        Rgba { r: c.r, g: c.g, b: c.b, a: c.a * o }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let surface = self.bg_alpha(t.surface());
        let surface_raised = self.bg_alpha(t.surface_raised());
        let mut sb = div()
            .w(self.s(220.0)).h_full().bg(surface)
            .border_r_1().border_color(t.border())
            .pt(self.s(16.0)).pb(self.s(16.0)).px(self.s(14.0)).flex().flex_col()
            .shadow(vec![
                BoxShadow {
                    color: t.shadow().into(),
                    offset: point(self.s(2.0), px(0.)),
                    blur_radius: self.s(8.0),
                    spread_radius: px(0.),
                },
            ]);

        // Tab switcher
        let agents_active = self.sidebar_tab == SidebarTab::Agents;
        sb = sb.child(
            div().flex().gap(self.s(2.0)).mb(self.s(8.0))
                .child(
                    div().id("tab-agents")
                        .px(self.s(8.0)).py(self.s(3.0)).rounded(self.s(4.0))
                        .bg(if agents_active { surface_raised } else { rgba(0x00000000) })
                        .text_size(self.s(11.0))
                        .text_color(if agents_active { t.text() } else { t.text_faint() })
                        .cursor_pointer()
                        .child("agents")
                        .on_click(cx.listener(|this, _, _, cx| { this.sidebar_tab = SidebarTab::Agents; cx.notify(); }))
                )
                .child(
                    div().id("tab-workers")
                        .px(self.s(8.0)).py(self.s(3.0)).rounded(self.s(4.0))
                        .bg(if !agents_active { surface_raised } else { rgba(0x00000000) })
                        .text_size(self.s(11.0))
                        .text_color(if !agents_active { t.text() } else { t.text_faint() })
                        .cursor_pointer()
                        .child("workers")
                        .on_click(cx.listener(|this, _, _, cx| { this.sidebar_tab = SidebarTab::Workers; cx.notify(); }))
                )
        );

        match self.sidebar_tab {
            SidebarTab::Agents => {
                for (gi, group) in self.groups.iter().enumerate() {
                    let focused = gi == self.focused_group;
                    let bg = if focused { surface_raised } else { rgba(0x00000000) };
                    let tc = if focused { t.text() } else { t.text_muted() };

                    sb = sb.child(
                        div().id(ElementId::Name(format!("grp-{}", gi).into()))
                            .w_full().px(self.s(8.0)).py(self.s(5.0)).rounded(self.s(6.0)).bg(bg)
                            .cursor_pointer()
                            .flex().items_center().gap(self.s(8.0))
                            .child(div().text_size(self.s(11.0)).text_color(if focused { t.blue() } else { t.text_faint() }).child(if focused { ">" } else { " " }))
                            .child(div().text_size(self.s(13.0)).text_color(tc).child(group.name.clone()))
                            .child(div().flex_grow())
                            .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child(
                                format!("{}", self.agents.iter().filter(|a| a.group == group.name).count())
                            ))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.focused_group = gi;
                                this.clamp_focus();
                                cx.notify();
                            })),
                    );

                    if focused {
                        for (i, agent) in self.agents.iter().enumerate() {
                            if agent.group != group.name { continue; }
                            let af = i == self.focused_agent;
                            let tc = if af { t.text() } else { t.text_muted() };
                            let fav_icon = if agent.favorite { "* " } else { "" };
                            let role_icon = if agent.role == AgentRole::Worker { "↳ " } else { "" };
                            let rt_c = t.runtime_color(&agent.runtime_name);
                            sb = sb.child(
                                div().id(ElementId::Name(format!("sa-{}", i).into()))
                                    .w_full().pl(self.s(20.0)).py(self.s(2.0)).flex().items_center().gap(self.s(6.0))
                                    .cursor_pointer()
                                    .child(div().w(self.s(3.0)).h(self.s(14.0)).rounded(self.s(2.0)).bg(rt_c))
                                    .child(div().text_size(self.s(10.0)).text_color(agent.status.color(t)).child(agent.status.dot()))
                                    .child(div().text_size(self.s(12.0)).text_color(tc).child(format!("{}{}{}", role_icon, fav_icon, agent.name)))
                                    .child(div().flex_grow())
                                    .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child(format!("{}@{}", agent.runtime_name, agent.target_machine)))
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.set_focus(i);
                                        cx.notify();
                                    })),
                            );
                        }
                    }
                }
                // New agent button
                sb = sb.child(
                    div().id("btn-new-agent")
                        .w_full().px(self.s(8.0)).py(self.s(5.0)).rounded(self.s(6.0))
                        .cursor_pointer()
                        .flex().items_center().gap(self.s(8.0))
                        .hover(move |s| s.bg(surface_raised))
                        .child(div().text_size(self.s(13.0)).text_color(t.text_faint()).child("+"))
                        .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("new agent"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.start_setup();
                            cx.notify();
                        }))
                );
            }
            SidebarTab::Workers => {
                sb = sb.child(
                    div().text_size(self.s(10.0)).text_color(t.text_faint()).mb(self.s(6.0)).child("FOCUSED AGENT WORKERS")
                );
                let workers = self.child_workers(self.focused_agent);
                if workers.is_empty() {
                    sb = sb.child(
                        div().text_size(self.s(11.0)).text_color(t.text_muted())
                            .child("No delegated workers yet.")
                    );
                } else {
                    for worker_idx in workers {
                        let worker = &self.agents[worker_idx];
                        let task_title = worker.worker_assignment.as_ref()
                            .map(|assignment| assignment.task_title.clone())
                            .unwrap_or_else(|| "worker".into());
                        sb = sb.child(
                            div().id(ElementId::Name(format!("worker-{}", worker_idx).into()))
                                .w_full().px(self.s(8.0)).py(self.s(5.0)).rounded(self.s(6.0))
                                .bg(if worker_idx == self.focused_agent { surface_raised } else { rgba(0x00000000) })
                                .cursor_pointer()
                                .flex().flex_col().gap(self.s(2.0))
                                .child(div().flex().items_center().gap(self.s(6.0))
                                    .child(div().text_size(self.s(10.0)).text_color(worker.status.color(t)).child(worker.status.dot()))
                                    .child(div().text_size(self.s(12.0)).text_color(t.text()).child(worker.name.clone()))
                                    .child(div().flex_grow())
                                    .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child(format!("{}@{}", worker.runtime_name, worker.target_machine)))
                                )
                                .child(div().text_size(self.s(10.0)).text_color(t.text_muted()).child(task_title))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_focus(worker_idx);
                                    cx.notify();
                                }))
                        );
                    }
                }
            }
        }

        sb = sb.child(div().flex_grow());

        // Aggregate stats
        let total_cost: f64 = self.agents.iter().map(|a| a.tokens.cost_usd).sum();
        let total_tokens: u64 = self.agents.iter().map(|a| a.tokens.total_tokens()).sum();
        let working_count = self.agents.iter().filter(|a| a.status == AgentStatus::Working).count();
        let idle_count = self.agents.iter().filter(|a| a.status == AgentStatus::Idle).count();
        let total_count = self.agents.len();

        sb = sb.child(
            div().border_t_1().border_color(t.border()).pt(self.s(8.0)).pb(self.s(4.0))
                .flex().flex_col().gap(self.s(3.0))
                .child(div().flex().justify_between()
                    .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child("total cost"))
                    .child(div().text_size(self.s(10.0)).text_color(t.green()).child(format!("${:.2}", total_cost)))
                )
                .child(div().flex().justify_between()
                    .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child("tokens"))
                    .child(div().text_size(self.s(10.0)).text_color(t.text_muted()).child(format!("{}k", total_tokens / 1000)))
                )
                .child(div().flex().justify_between()
                    .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child("agents"))
                    .child(div().text_size(self.s(10.0)).text_color(
                        if working_count > 0 { t.yellow() } else { t.text_muted() }
                    ).child(format!("{} total  {} working  {} idle", total_count, working_count, idle_count)))
                )
        );

        let (ml, mc) = match self.mode {
            Mode::Normal => ("", t.text_faint()),
            Mode::Palette => ("PALETTE", t.yellow()),
            Mode::Setup => ("SETUP", t.yellow()),
            Mode::Search => ("SEARCH", t.yellow()),
        };
        let vl = match self.view_mode {
            ViewMode::Grid => "grid", ViewMode::Pipeline => "pipe", ViewMode::Focus => "focus",
        };
        let mode_epoch = self.mode_epoch;
        let mode_badge = div().px(self.s(6.0)).py(self.s(2.0)).rounded(self.s(4.0))
            .bg(mc).text_size(self.s(10.0)).text_color(t.bg()).child(ml)
            .with_animation(
                ElementId::Name(format!("mode-{}", mode_epoch).into()),
                Animation::new(Duration::from_millis(200))
                    .with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            );
        sb = sb.child(div().mt(self.s(8.0)).flex().justify_between().items_center()
            .child(div().flex().gap(self.s(6.0)).items_center()
                .child(mode_badge)
                .child(div().px(self.s(5.0)).py(self.s(2.0)).rounded(self.s(4.0))
                    .bg(self.bg_alpha(t.surface_raised())).text_size(self.s(10.0)).text_color(t.text_muted()).child(vl)))
            .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child(format!("{}%", (self.ui_scale * 100.0) as u32))),
        );
        sb
    }


    /// Render a markdown span as a styled child element.
    fn render_span(&self, parent: Div, span: &Span, t: &ThemeColors) -> Div {
        match span {
            Span::Text(s) => parent.child(div().text_color(t.text()).child(s.clone())),
            Span::Code(s) => parent.child(
                div().px(self.s(4.0)).py(self.s(1.0)).mx(self.s(1.0))
                    .rounded(self.s(3.0)).bg(self.bg_alpha(t.surface_raised()))
                    .text_color(t.user_input())
                    .text_size(self.s(self.font_size - 1.0))
                    .font_family(SharedString::from("Menlo"))
                    .child(s.clone())
            ),
            Span::Bold(s) => parent.child(
                div().text_color(t.text()).font_weight(FontWeight::BOLD).child(s.clone())
            ),
            Span::Italic(s) => parent.child(
                div().text_color(t.text_muted()).child(s.clone())
            ),
            Span::BoldItalic(s) => parent.child(
                div().text_color(t.text()).font_weight(FontWeight::BOLD).child(s.clone())
            ),
        }
    }

    fn render_agent_tile(&self, idx: usize, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let a = &self.agents[idx];
        let t = &self.theme;
        let focused = idx == self.focused_agent;
        let rt_color = t.runtime_color(&a.runtime_name);
        let bc = if focused { rt_color } else { t.border() };

        // Shadow: runtime-colored glow for focused, subtle for unfocused
        let tile_shadow = if focused {
            vec![
                BoxShadow {
                    color: Rgba { r: rt_color.r, g: rt_color.g, b: rt_color.b, a: 0.35 }.into(),
                    offset: point(px(0.), px(0.)),
                    blur_radius: self.s(16.0),
                    spread_radius: self.s(2.0),
                },
                BoxShadow {
                    color: t.shadow().into(),
                    offset: point(px(0.), self.s(4.0)),
                    blur_radius: self.s(12.0),
                    spread_radius: px(0.),
                },
            ]
        } else {
            vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(px(0.), self.s(2.0)),
                blur_radius: self.s(8.0),
                spread_radius: px(0.),
            }]
        };

        let mut tile = div()
            .id(ElementId::Name(format!("tile-{}", idx).into()))
            .flex_grow().flex_shrink().flex_basis(px(0.)).min_w(px(0.)).h_full()
            .bg(self.bg_alpha(t.bg())).flex().flex_col().overflow_hidden()
            .cursor_pointer()
            .shadow(tile_shadow)
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.set_focus(idx);
                cx.notify();
            }));

        tile = if focused {
            tile.border_2().border_color(bc)
        } else {
            tile.border_1().border_color(t.border())
        };

        // Favorite indicator with bounce animation
        let fav_label = if a.favorite { " *" } else { "" };

        // Compact single-row header
        let status_color = a.status.color(t);
        let is_working = a.status == AgentStatus::Working;

        let elapsed_str = if let Some(started) = a.turn_started {
            let secs = started.elapsed().as_secs();
            if secs < 60 { format!("{}s", secs) } else { format!("{}m{}s", secs / 60, secs % 60) }
        } else { String::new() };

        let pct = a.tokens.context_usage_pct();
        let token_info = format!("{}k/{}k ${:.3}",
            a.tokens.total_tokens() / 1000,
            a.tokens.context_window / 1000,
            a.tokens.cost_usd,
        );

        let mut header = div().w_full().min_w(px(0.)).px(self.s(10.0)).pt(px(36.0)).pb(self.s(4.0))
            .bg(self.bg_alpha(t.surface()))
            .border_b_1().border_color(t.border())
            .flex().items_center().gap(self.s(6.0)).overflow_hidden();

        // Squirrel icon: spinning when working, static otherwise
        if is_working {
            header = header.child(
                svg().path("assets/squirrel_spin.svg")
                    .size(self.s(13.0))
                    .text_color(status_color)
                    .with_animation(
                        ElementId::Name(format!("spin-{}", idx).into()),
                        Animation::new(Duration::from_millis(1200))
                            .repeat()
                            .with_easing(ease_in_out),
                        |sv, delta| sv.with_transformation(Transformation::rotate(percentage(delta))),
                    )
            );
        } else {
            header = header.child(
                svg().path("assets/squirrel.svg")
                    .size(self.s(12.0))
                    .text_color(status_color)
            );
        }

        // Name
        header = header
            .child(div().text_size(self.s(12.0)).text_color(t.text())
                .child(format!("{}{}", a.name, fav_label)));

        // Status dot + label
        header = header
            .child(div().text_size(self.s(10.0)).text_color(status_color).child(a.status.label()));

        // Elapsed time
        if !elapsed_str.is_empty() {
            header = header.child(div().text_size(self.s(9.0)).text_color(t.yellow()).child(elapsed_str));
        }

        // Spacer
        header = header.child(div().flex_grow());

        // Model | tokens | cost — compact right side
        header = header
            .child(div().text_size(self.s(9.0)).text_color(t.text_muted()).child(a.tokens.model.clone()))
            .child(div().text_size(self.s(9.0)).text_color(t.text_faint()).child(token_info));

        // Context usage percentage
        header = header.child(div().text_size(self.s(9.0)).text_color(
            if pct > 80.0 { t.red() } else if pct > 50.0 { t.yellow() } else { t.text_faint() }
        ).child(format!("{:.0}%", pct)));

        // Inline action icons
        let btn_size = self.s(11.0);
        let btn_color = t.text_muted();
        let btn_hover = t.text();
        let red = t.red();
        let sr = t.surface_raised();

        if a.status == AgentStatus::Interrupted && a.pending_prompt.is_some() {
            header = header.child(
                div().id(ElementId::Name(format!("btn-continue-{}", idx).into()))
                    .px(self.s(4.0)).py(self.s(2.0)).rounded(self.s(4.0))
                    .cursor_pointer()
                    .text_size(btn_size).text_color(t.yellow())
                    .hover(|s| s.bg(sr).text_color(t.text()))
                    .child("↺")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.continue_pending_turn(idx, cx);
                        cx.notify();
                    }))
            );
        }

        let wd = a.working_dir.clone();
        if !wd.is_empty() {
            header = header.child(
                div().id(ElementId::Name(format!("btn-term-{}", idx).into()))
                    .px(self.s(4.0)).py(self.s(2.0)).rounded(self.s(4.0))
                    .cursor_pointer()
                    .text_size(btn_size).text_color(btn_color)
                    .hover(|s| s.bg(sr).text_color(btn_hover))
                    .child("↗")
                    .on_click(cx.listener(move |this, _, _, _cx| {
                        let dir = if idx < this.agents.len() {
                            this.agents[idx].working_dir.clone()
                        } else { String::new() };
                        if !dir.is_empty() {
                            let _ = Command::new("open").arg("-a").arg("Terminal").arg(&dir).spawn();
                        }
                    }))
            );
        }

        header = header.child(
            div().id(ElementId::Name(format!("btn-restart-{}", idx).into()))
                .px(self.s(4.0)).py(self.s(2.0)).rounded(self.s(4.0))
                .cursor_pointer()
                .text_size(btn_size).text_color(btn_color)
                .hover(|s| s.bg(sr).text_color(btn_hover))
                .child("↻")
                .on_click(cx.listener(move |this, _, _w, cx| {
                    this.set_focus(idx);
                    this.restart_agent(&RestartAgent, _w, cx);
                }))
        );

        header = header.child(
            div().id(ElementId::Name(format!("btn-kill-{}", idx).into()))
                .px(self.s(4.0)).py(self.s(2.0)).rounded(self.s(4.0))
                .cursor_pointer()
                .text_size(btn_size).text_color(btn_color)
                .hover(|s| s.bg(sr).text_color(red))
                .child("✕")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.confirm_remove_agent = Some(idx);
                    cx.notify();
                }))
        );

        tile = tile.child(header);

        // Compact badges row (only shown when there are badges to display)
        let tc_summary = a.tool_calls.summary();
        let has_badges = a.tokens.thinking_enabled
            || !a.runtime_info.plugins.is_empty()
            || !a.runtime_info.mcps.is_empty()
            || !tc_summary.is_empty()
            || !a.auto_scroll;

        if has_badges {
            let mut badges = div().w_full().px(self.s(10.0)).py(self.s(3.0))
                .bg(self.bg_alpha(t.surface())).border_b_1().border_color(t.border())
                .flex().items_center().gap(self.s(4.0)).overflow_hidden();

            if a.tokens.thinking_enabled {
                badges = badges.child(
                    div().px(self.s(4.0)).py(self.s(1.0)).rounded(self.s(3.0)).bg(self.bg_alpha(t.surface_raised()))
                        .text_size(self.s(8.0)).text_color(t.yellow()).child("think")
                );
            }
            if !a.runtime_info.plugins.is_empty() {
                let n = a.runtime_info.plugins.len();
                badges = badges.child(
                    div().px(self.s(4.0)).py(self.s(1.0)).rounded(self.s(3.0)).bg(self.bg_alpha(t.surface_raised()))
                        .text_size(self.s(8.0)).text_color(t.blue_muted())
                        .child(if n == 1 { "1 plugin".into() } else { format!("{} plugins", n) })
                );
            }
            if !a.runtime_info.mcps.is_empty() {
                let n = a.runtime_info.mcps.len();
                badges = badges.child(
                    div().px(self.s(4.0)).py(self.s(1.0)).rounded(self.s(3.0)).bg(self.bg_alpha(t.surface_raised()))
                        .text_size(self.s(8.0)).text_color(t.green())
                        .child(if n == 1 { "1 mcp".into() } else { format!("{} mcps", n) })
                );
            }
            if !tc_summary.is_empty() {
                let tc_recent = a.last_tool_call_at
                    .map(|t| t.elapsed() < Duration::from_millis(600))
                    .unwrap_or(false);
                let tc_badge = div().px(self.s(4.0)).py(self.s(1.0)).rounded(self.s(3.0)).bg(self.bg_alpha(t.surface_raised()))
                    .text_size(self.s(8.0)).text_color(if tc_recent { t.green() } else { t.text_muted() })
                    .child(tc_summary);
                if tc_recent {
                    let tc_count = a.tool_calls.total();
                    badges = badges.child(
                        tc_badge.with_animation(
                            ElementId::Name(format!("tc-bump-{}-{}", idx, tc_count).into()),
                            Animation::new(Duration::from_millis(400))
                                .with_easing(ease_in_out),
                            |el, delta| el.opacity(0.5 + 0.5 * delta),
                        )
                    );
                } else {
                    badges = badges.child(tc_badge);
                }
            }
            if !a.auto_scroll {
                badges = badges.child(
                    div().px(self.s(4.0)).py(self.s(1.0)).rounded(self.s(3.0)).bg(self.bg_alpha(t.surface_raised()))
                        .text_size(self.s(8.0)).text_color(t.yellow())
                        .child("pinned")
                        .with_animation(
                            ElementId::Name(format!("pin-{}", idx).into()),
                            Animation::new(Duration::from_secs(2))
                                .repeat()
                                .with_easing(pulsating_between(0.6, 1.0)),
                            |el, delta| el.opacity(delta),
                        )
                );
            }

            tile = tile.child(badges);
        }

        if a.role == AgentRole::Coordinator {
            let workers = self.child_workers(idx);
            if !workers.is_empty() {
                let mut worker_strip = div().w_full().px(self.s(14.0)).py(self.s(6.0))
                    .bg(self.bg_alpha(t.surface()))
                    .border_b_1().border_color(t.border())
                    .flex().flex_col().gap(self.s(4.0))
                    .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child("workers"));
                for worker_idx in workers {
                    let worker = &self.agents[worker_idx];
                    let task_title = worker.worker_assignment.as_ref()
                        .map(|assignment| assignment.task_title.clone())
                        .unwrap_or_else(|| "worker".into());
                    worker_strip = worker_strip.child(
                        div().flex().items_center().gap(self.s(6.0))
                            .child(div().text_size(self.s(10.0)).text_color(worker.status.color(t)).child(worker.status.dot()))
                            .child(div().text_size(self.s(11.0)).text_color(t.text()).child(worker.name.clone()))
                            .child(div().flex_grow())
                            .child(div().text_size(self.s(10.0)).text_color(t.text_muted()).child(task_title))
                    );
                }
                tile = tile.child(worker_strip);
            }
        }

        if let Some(notice) = &a.restore_notice {
            tile = tile.child(
                div().w_full().px(self.s(14.0)).py(self.s(10.0))
                    .bg(self.bg_alpha(t.surface()))
                    .border_b_1().border_color(t.border())
                    .child(
                        div().w_full().px(self.s(10.0)).py(self.s(8.0))
                            .rounded(self.s(8.0))
                            .bg(self.bg_alpha(t.surface_raised()))
                            .text_size(self.s(12.0)).text_color(t.text_muted())
                            .child(notice.clone())
                    )
            );
        }

        // Output area with markdown rendering
        let fs = self.font_size;
        let transcript_font = self.transcript_font();
        let mut out = div()
            .id(ElementId::Name(format!("output-{}", idx).into()))
            .flex_grow().flex_shrink().w_full().min_w(px(0.)).max_w_full()
            .px(self.s(14.0)).py(self.s(8.0)).flex().flex_col()
            .overflow_hidden().font_family(transcript_font.clone()).text_size(self.s(fs)).line_height(self.s(fs + 8.0))
            .gap(self.s(8.0))
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                if idx >= this.agents.len() { return; }
                let a = &mut this.agents[idx];
                let raw_delta: f32 = match event.delta {
                    ScrollDelta::Lines(lines) => -lines.y * 3.0,
                    ScrollDelta::Pixels(px) => {
                        let y: f32 = px.y.into();
                        -y / 8.0
                    }
                };
                a.scroll_accum += raw_delta;
                let lines_to_scroll = a.scroll_accum as isize;
                if lines_to_scroll != 0 {
                    a.scroll_accum -= lines_to_scroll as f32;
                    let max = a.output_lines.len().saturating_sub(1);
                    if lines_to_scroll < 0 {
                        a.scroll_offset = a.scroll_offset.saturating_sub((-lines_to_scroll) as usize);
                    } else {
                        a.scroll_offset = (a.scroll_offset + lines_to_scroll as usize).min(max);
                    }
                    a.auto_scroll = false;
                    cx.notify();
                }
            }));

        // Empty area is clickable to enter insert mode
        if a.output_lines.is_empty() {
            out = out.child(
                div().id(ElementId::Name(format!("empty-click-{}", idx).into()))
                    .flex_grow().w_full().cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_focus(idx);
                        this.set_mode(Mode::Normal);
                        cx.notify();
                    }))
            );
        }
        let start = a.scroll_offset.min(a.output_lines.len());
        let lines = &a.output_lines[start..];

        // Pre-pass: split lines into turns (user input vs agent response blocks)
        struct Turn { is_user: bool, start: usize, end: usize }
        let mut turns: Vec<Turn> = Vec::new();
        {
            let mut i = 0;
            while i < lines.len() {
                if classify_line(&lines[i]) == LineKind::UserInput {
                    turns.push(Turn { is_user: true, start: i, end: i + 1 });
                    i += 1;
                    // Skip blank lines after user input
                    while i < lines.len() && lines[i].trim().is_empty() { i += 1; }
                } else {
                    // Agent response: collect until next user input
                    let block_start = i;
                    while i < lines.len() && classify_line(&lines[i]) != LineKind::UserInput {
                        i += 1;
                    }
                    if block_start < i {
                        turns.push(Turn { is_user: false, start: block_start, end: i });
                    }
                }
            }
        }

        for turn in &turns {
            if turn.is_user {
                let line = &lines[turn.start];
                let display_text = line.strip_prefix("> ").unwrap_or(line);
                out = out.child(
                    div().w_full().px(self.s(12.0)).py(self.s(10.0))
                        .rounded(self.s(10.0))
                        .bg(self.bg_alpha(t.surface()))
                        .border_1().border_color(t.blue())
                        .child(div().text_color(t.blue()).text_size(self.s(fs)).child(display_text.to_string()))
                );
                continue;
            }

            // Agent response: render as ONE card
            let block_lines = &lines[turn.start..turn.end];
            let full_text: String = block_lines.iter()
                .filter(|l| !l.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");

            if full_text.is_empty() { continue; }

            let mut card = div().flex_grow().min_w(px(0.)).flex().flex_col().gap(self.s(3.0));
            let mut in_code_block = false;
            let mut j = 0;

            while j < block_lines.len() {
                let bline = &block_lines[j];

                // Skip empty lines (just add spacing)
                if bline.trim().is_empty() {
                    card = card.child(div().h(self.s(4.0)));
                    j += 1;
                    continue;
                }

                // Code fence handling
                if let Some(lang) = parse_code_fence(bline) {
                    if in_code_block {
                        in_code_block = false;
                        j += 1;
                        continue;
                    } else {
                        in_code_block = true;
                        if !lang.is_empty() {
                            card = card.child(
                                div().w_full().px(self.s(8.0)).pt(self.s(6.0)).pb(self.s(2.0))
                                    .bg(self.bg_alpha(t.surface_raised())).rounded_t(self.s(4.0))
                                    .text_size(self.s(10.0)).text_color(t.text_faint())
                                    .child(lang)
                            );
                        }
                        j += 1;
                        continue;
                    }
                }

                // Inside code block
                if in_code_block {
                    card = card.child(
                        div().w_full().px(self.s(8.0)).py(self.s(1.0))
                            .bg(self.bg_alpha(t.surface_raised()))
                            .text_size(self.s(fs - 1.0)).text_color(t.text())
                            .font_family(SharedString::from("Menlo"))
                            .child(bline.clone())
                    );
                    j += 1;
                    continue;
                }

                // Heading
                if let Some((level, content)) = parse_heading(bline) {
                    let size = match level {
                        1 => fs + 6.0,
                        2 => fs + 4.0,
                        3 => fs + 2.0,
                        _ => fs + 1.0,
                    };
                    card = card.child(
                        div().flex_grow().pt(self.s(6.0)).pb(self.s(2.0))
                            .text_size(self.s(size)).text_color(t.text())
                            .font_weight(FontWeight::BOLD)
                            .child(content.to_string())
                    );
                    j += 1;
                    continue;
                }

                // Bullet point
                if let Some((indent, content)) = parse_bullet(bline) {
                    let indent_px = self.s(indent as f32 * 16.0 + 4.0);
                    let spans = parse_spans(content);
                    let mut row = div().w_full().min_w(px(0.)).overflow_hidden()
                        .pl(indent_px).flex().items_start().gap(self.s(6.0))
                        .child(div().flex_shrink_0().text_color(t.text_muted()).child("  -"));
                    let mut span_row = div().flex_shrink().min_w(px(0.)).flex().flex_wrap();
                    for span in &spans {
                        span_row = self.render_span(span_row, span, t);
                    }
                    row = row.child(span_row);
                    card = card.child(row);
                    j += 1;
                    continue;
                }

                // Diff/special lines
                let kind = classify_line(bline);
                if kind != LineKind::Normal {
                    let (text_color, bg_color) = match kind {
                        LineKind::Error => (t.red(), None),
                        LineKind::Thinking => (t.text_faint(), None),
                        LineKind::System => (t.yellow(), None),
                        LineKind::DiffAdd => (t.green(), Some(t.diff_add_bg())),
                        LineKind::DiffRemove => (t.red(), Some(t.diff_remove_bg())),
                        LineKind::DiffHunk => (t.blue_muted(), Some(t.diff_hunk_bg())),
                        LineKind::DiffMeta => (t.text_muted(), None),
                        _ => (t.text(), None),
                    };
                    let mut line_div = div().text_color(text_color).w_full().min_w(px(0.))
                        .px(self.s(4.0)).rounded(self.s(2.0));
                    if let Some(bg) = bg_color {
                        line_div = line_div.bg(bg);
                    }
                    if kind == LineKind::Thinking {
                        line_div = line_div.opacity(0.6);
                    }
                    card = card.child(line_div.child(bline.clone()));
                    j += 1;
                    continue;
                }

                // Normal text with inline markdown
                let spans = parse_spans(bline);
                if spans.len() == 1 {
                    if let Span::Text(ref s) = spans[0] {
                        card = card.child(
                            div().w_full().min_w(px(0.)).text_color(t.text()).child(s.clone())
                        );
                        j += 1;
                        continue;
                    }
                }
                let mut row = div().w_full().min_w(px(0.)).overflow_hidden().flex().flex_wrap();
                for span in &spans {
                    row = self.render_span(row, span, t);
                }
                card = card.child(row);
                j += 1;
            }

            out = out.child(
                div().w_full().min_w(px(0.)).px(self.s(8.0)).py(self.s(6.0))
                    .flex().items_start().gap(self.s(8.0))
                    .child(card)
                    .child(self.render_copy_icon(full_text, format!("copy-{}-{}", idx, turn.start), cx))
            );
        }
        tile = tile.child(out);

        // Input bar — always active
        if focused {
            let tc = t.text();
            let pc = t.blue();

            let mut input_area = div()
                .id(ElementId::Name(format!("input-click-{}", idx).into()))
                .w_full().px(self.s(14.0)).py(self.s(10.0)).bg(self.bg_alpha(t.surface()))
                .border_t_1().border_color(t.border_focus())
                .flex().flex_col().gap(self.s(4.0))
                .font_family(self.font_family.clone()).text_size(self.s(fs))
                .min_h(self.s(44.0))
                .cursor_text()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_focus(idx);
                    cx.notify();
                }));

            {
                // Show text with cursor indicator, supporting multiline
                let lines: Vec<&str> = a.input_buffer.split('\n').collect();
                let line_count = lines.len();

                // Build display with cursor
                if a.input_buffer.is_empty() {
                    let prompt_hint = if self.config.cautious_enter {
                        "type a prompt... (Cmd+Enter to send)"
                    } else {
                        "type a prompt... (Enter to send, Cmd+Enter for quick send)"
                    };
                    input_area = input_area.child(
                        div().flex().items_start().gap(self.s(6.0))
                            .child(div().text_color(pc).text_size(self.s(14.0)).child(">"))
                            .child(div().text_color(t.text_faint()).child(prompt_hint))
                    );
                } else {
                    // Render each line, inserting cursor at the right position
                    let mut byte_offset = 0usize;
                    for (li, line) in lines.iter().enumerate() {
                        let line_start = byte_offset;
                        let line_end = line_start + line.len();
                        let prompt_char = if li == 0 { ">" } else { " " };

                        let row = if a.input_cursor >= line_start && a.input_cursor <= line_end {
                            let pos_in_line = a.input_cursor - line_start;
                            let before_cursor = &line[..pos_in_line];
                            let after_cursor = &line[pos_in_line..];
                            div().flex().items_start().w_full()
                                .child(div().text_color(pc).text_size(self.s(14.0)).pr(self.s(6.0)).child(prompt_char))
                                .child(div().text_color(tc).child(before_cursor.to_string()))
                                .child(div().text_color(t.blue()).child("│"))
                                .child(div().text_color(tc).child(after_cursor.to_string()))
                        } else {
                            div().flex().items_start().w_full()
                                .child(div().text_color(pc).text_size(self.s(14.0)).pr(self.s(6.0)).child(prompt_char))
                                .child(div().text_color(tc).child(line.to_string()))
                        };
                        input_area = input_area.child(row);
                        byte_offset = line_end + 1; // +1 for the \n
                    }
                }

                // Line count indicator for multiline
                if line_count > 1 {
                    input_area = input_area.child(
                        div().text_size(self.s(10.0)).text_color(t.text_faint())
                            .child(format!("{} lines", line_count))
                    );
                }
            }
            tile = tile.child(input_area);
        }
        // Spawn fade-in animation (within first 500ms of agent creation)
        let spawn_age = a.spawn_time.elapsed();
        if spawn_age < Duration::from_millis(500) {
            return tile.with_animation(
                ElementId::Name(format!("spawn-{}-{}", idx, a.spawn_time.elapsed().as_millis()).into()),
                Animation::new(Duration::from_millis(400))
                    .with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            ).into_any_element();
        }
        // Focus glow transition
        if focused {
            let fe = self.focus_epoch;
            return tile.with_animation(
                ElementId::Name(format!("focus-{}-{}", idx, fe).into()),
                Animation::new(Duration::from_millis(200))
                    .with_easing(ease_out_quint()),
                |el, delta| el.opacity(0.85 + 0.15 * delta),
            ).into_any_element();
        }
        tile.into_any_element()
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let mut p = div()
            .absolute().top(self.s(100.0)).left(self.s(400.0)).w(self.s(500.0))
            .bg(t.palette_bg()).border_1().border_color(t.palette_border())
            .rounded(self.s(12.0)).flex().flex_col().overflow_hidden()
            .shadow(vec![
                BoxShadow {
                    color: t.shadow().into(),
                    offset: point(px(0.), self.s(8.0)),
                    blur_radius: self.s(24.0),
                    spread_radius: self.s(4.0),
                },
            ]);

        p = p.child(
            div().w_full().px(self.s(14.0)).py(self.s(10.0)).border_b_1().border_color(t.palette_border())
                .flex().items_center().gap(self.s(8.0)).font_family(self.font_family.clone()).text_size(self.s(14.0))
                .child(div().text_color(t.blue()).child(">"))
                .child(div().text_color(t.text()).child(
                    if self.palette_input.is_empty() { "type a command...".into() }
                    else { format!("{}|", self.palette_input) }
                )),
        );

        for (i, item) in self.palette_items.iter().enumerate() {
            let sel = i == self.palette_selection;
            p = p.child(
                div().id(ElementId::Name(format!("pal-{}", i).into()))
                    .w_full().px(self.s(14.0)).py(self.s(8.0))
                    .cursor_pointer()
                    .bg(if sel { t.selected_row() } else { rgba(0x00000000) })
                    .text_size(self.s(13.0)).text_color(if sel { t.text() } else { t.text_muted() })
                    .child(item.label.clone())
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.palette_selection = i;
                        this.submit_input(&SubmitInput, window, cx);
                    })),
            );
        }
        // Slide-in + fade animation
        let epoch = self.mode_epoch;
        p.with_animation(
            ElementId::Name(format!("palette-slide-{}", epoch).into()),
            Animation::new(Duration::from_millis(200))
                .with_easing(ease_out_quint()),
            |el, delta| el.opacity(delta),
        )
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let setup = self.setup.as_ref().unwrap();
        let t = &self.theme;

        let mut w = div()
            .absolute().top(self.s(80.0)).left(self.s(350.0)).w(self.s(600.0))
            .bg(t.palette_bg()).border_1().border_color(t.palette_border())
            .rounded(self.s(12.0)).flex().flex_col().overflow_hidden()
            .shadow(vec![
                BoxShadow {
                    color: t.shadow().into(),
                    offset: point(px(0.), self.s(8.0)),
                    blur_radius: self.s(24.0),
                    spread_radius: self.s(4.0),
                },
            ]);

        // Step indicator
        let steps = [
            ("Machine", SetupStep::Machine),
            ("Directory", SetupStep::Directory),
            ("Runtime", SetupStep::Runtime),
            ("Model", SetupStep::Model),
            ("MCPs", SetupStep::Mcps),
            ("Confirm", SetupStep::Confirm),
        ];
        let mut step_row = div().w_full().px(self.s(14.0)).py(self.s(10.0)).border_b_1().border_color(t.palette_border())
            .flex().items_center().gap(self.s(16.0));
        for (label, step) in &steps {
            let active = setup.step == *step;
            let c = if active { t.blue() } else { t.text_faint() };
            step_row = step_row.child(div().text_size(self.s(12.0)).text_color(c).child(
                if active { format!("> {}", label) } else { label.to_string() }
            ));
        }
        w = w.child(step_row);

        // Step content
        match setup.step {
            SetupStep::Runtime => {
                w = w.child(div().px(self.s(14.0)).py(self.s(8.0)).text_size(self.s(11.0)).text_color(t.text_muted())
                    .child("Select runtime (arrows to move, Tab to continue)"));
                for (i, rt) in self.config.runtimes.iter().enumerate() {
                    let sel = i == setup.runtime_cursor;
                    w = w.child(
                        div().id(ElementId::Name(format!("srt-{}", i).into()))
                            .w_full().px(self.s(14.0)).py(self.s(6.0))
                            .cursor_pointer()
                            .bg(if sel { t.selected_row() } else { rgba(0x00000000) })
                            .flex().items_center().gap(self.s(10.0))
                            .child(div().text_size(self.s(13.0)).text_color(if sel { t.blue() } else { t.text_faint() })
                                .child(if sel { ">" } else { " " }))
                            .child(div().text_size(self.s(13.0)).text_color(if sel { t.text() } else { t.text_muted() })
                                .child(rt.name.clone()))
                            .child(div().flex_grow())
                            .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child(rt.description.clone()))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                if let Some(ref mut s) = this.setup {
                                    s.runtime_cursor = i;
                                }
                                cx.notify();
                            })),
                    );
                }
            }
            SetupStep::Model => {
                let supports_custom = setup.selected_runtime != "cursor";

                if setup.custom_mode && supports_custom {
                    // Custom endpoint form
                    w = w.child(div().px(self.s(14.0)).py(self.s(8.0)).text_size(self.s(11.0)).text_color(t.text_muted())
                        .child("Custom endpoint (Ctrl-E to switch back, arrows to navigate fields)"));

                    let fields = [
                        ("Base URL", &setup.custom_base_url, "https://openrouter.ai/api/v1"),
                        ("API Key", &setup.custom_api_key, "sk-or-v1-..."),
                        ("Model ID", &setup.custom_model_id, "anthropic/claude-sonnet-4.6"),
                    ];

                    for (i, (label, value, placeholder)) in fields.iter().enumerate() {
                        let active = i == setup.custom_field;
                        let display = if value.is_empty() {
                            placeholder.to_string()
                        } else if i == 1 && !value.is_empty() {
                            // Mask API key except last 4 chars
                            let len = value.len();
                            if len > 4 {
                                format!("{}...{}", &value[..4], &value[len-4..])
                            } else {
                                value.to_string()
                            }
                        } else {
                            format!("{}|", value)
                        };
                        let text_c = if value.is_empty() { t.text_faint() } else { t.text() };

                        w = w.child(
                            div().w_full().px(self.s(14.0)).py(self.s(6.0))
                                .bg(if active { t.selected_row() } else { rgba(0x00000000) })
                                .flex().items_center().gap(self.s(10.0))
                                .child(div().text_size(self.s(12.0)).text_color(if active { t.blue() } else { t.text_faint() })
                                    .child(if active { ">" } else { " " }))
                                .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0))
                                    .child(label.to_string()))
                                .child(div().flex_grow().text_size(self.s(12.0)).text_color(text_c)
                                    .child(display)),
                        );
                    }

                    // Hint about which env vars will be set
                    let env_hint = if setup.selected_runtime == "claude" {
                        "Sets ANTHROPIC_BASE_URL + ANTHROPIC_AUTH_TOKEN"
                    } else if setup.selected_runtime == "codex" {
                        "Sets OPENAI_BASE_URL + OPENAI_API_KEY"
                    } else {
                        "Sets OPENROUTER_API_KEY"
                    };
                    w = w.child(div().px(self.s(14.0)).py(self.s(8.0)).text_size(self.s(10.0)).text_color(t.text_faint())
                        .child(format!("Tab on last field to continue. {}", env_hint)));
                } else {
                    // Normal model list
                    let models = self.get_models_for_runtime(&setup.selected_runtime);
                    let filtered = setup.filtered_models(&models);
                    let is_dynamic_rt = setup.selected_runtime == "opencode" || setup.selected_runtime == "cursor";

                    // Filter input bar
                    let mut filter_row = div().w_full().px(self.s(14.0)).py(self.s(8.0)).border_b_1().border_color(t.palette_border())
                        .flex().items_center().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.blue()).child("search:"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(
                            if setup.model_filter.is_empty() {
                                "type to filter models...".to_string()
                            } else {
                                format!("{}|", setup.model_filter)
                            }
                        ))
                        .child(div().flex_grow())
                        .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child(
                            format!("{} models", filtered.len())
                        ));
                    if supports_custom {
                        filter_row = filter_row.child(
                            div().text_size(self.s(10.0)).text_color(t.yellow()).child("Ctrl-E: custom")
                        );
                    }
                    w = w.child(filter_row);

                    let is_loading = (self.openrouter_loading && setup.selected_runtime == "opencode")
                        || (self.cursor_loading && setup.selected_runtime == "cursor");
                    if is_loading && is_dynamic_rt {
                        let src = if setup.selected_runtime == "cursor" { "cursor" } else { "opencode" };
                        w = w.child(div().px(self.s(14.0)).py(self.s(12.0)).text_size(self.s(13.0)).text_color(t.yellow())
                            .child(format!("Fetching models from {}...", src)));
                    } else if filtered.is_empty() {
                        w = w.child(div().px(self.s(14.0)).py(self.s(6.0)).text_size(self.s(13.0)).text_color(t.text_faint())
                            .child(if models.is_empty() { "No models available" } else { "No matches" }));
                    }

                    // Show up to 20 visible models (scrollable window around cursor)
                    let visible_count = 20;
                    let start = if setup.model_cursor >= visible_count {
                        setup.model_cursor - visible_count + 1
                    } else { 0 };
                    let end = (start + visible_count).min(filtered.len());

                    for (vi, &(_, ref model)) in filtered[start..end].iter().enumerate() {
                        let list_idx = start + vi;
                        let sel = list_idx == setup.model_cursor;
                        let free_tag = if model.free { " (free)" } else { "" };
                        w = w.child(
                            div().id(ElementId::Name(format!("smd-{}", list_idx).into()))
                                .w_full().px(self.s(14.0)).py(self.s(4.0))
                                .cursor_pointer()
                                .bg(if sel { t.selected_row() } else { rgba(0x00000000) })
                                .flex().items_center().gap(self.s(10.0)).overflow_hidden()
                                .child(div().text_size(self.s(12.0)).text_color(if sel { t.blue() } else { t.text_faint() })
                                    .child(if sel { ">" } else { " " }))
                                .child(div().flex_shrink().min_w(px(0.)).text_size(self.s(12.0))
                                    .text_color(if sel { t.text() } else { t.text_muted() })
                                    .child(model.label.clone()))
                                .child(div().flex_grow())
                                .child(div().text_size(self.s(9.0)).text_color(t.text_faint()).child(format!("{}{}", model.id, free_tag)))
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    if let Some(ref mut s) = this.setup {
                                        s.model_cursor = list_idx;
                                    }
                                    cx.notify();
                                })),
                        );
                    }

                    // Scroll indicator
                    if filtered.len() > visible_count {
                        w = w.child(div().px(self.s(14.0)).py(self.s(4.0)).text_size(self.s(10.0)).text_color(t.text_faint())
                            .child(format!("showing {}-{} of {}", start + 1, end, filtered.len())));
                    }
                }
            }
            SetupStep::Machine => {
                w = w.child(div().px(self.s(14.0)).py(self.s(8.0)).text_size(self.s(11.0)).text_color(t.text_muted())
                    .child("Select machine target (arrows to move, Tab to continue)"));
                for (i, machine) in self.config.machines.iter().enumerate() {
                    let sel = i == setup.machine_cursor;
                    let detail = if machine.kind == "ssh" {
                        if machine.host.is_empty() { "ssh".to_string() } else { format!("ssh {}", machine.host) }
                    } else {
                        "local".to_string()
                    };
                    w = w.child(
                        div().w_full().px(self.s(14.0)).py(self.s(6.0))
                            .cursor_pointer()
                            .bg(if sel { t.selected_row() } else { rgba(0x00000000) })
                            .flex().items_center().gap(self.s(10.0))
                            .child(div().text_size(self.s(13.0)).text_color(if sel { t.blue() } else { t.text_faint() })
                                .child(if sel { ">" } else { " " }))
                            .child(div().text_size(self.s(13.0)).text_color(if sel { t.text() } else { t.text_muted() })
                                .child(machine.name.clone()))
                            .child(div().flex_grow())
                            .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child(detail)),
                    );
                }
            }
            SetupStep::Directory => {
                let filtered = Self::filter_dirs(&setup.dir_entries, &setup.dir_filter);
                let home = std::env::var("HOME").unwrap_or_default();
                let current_display = if !home.is_empty() && setup.selected_dir.starts_with(&home) {
                    format!("~{}", &setup.selected_dir[home.len()..])
                } else { setup.selected_dir.clone() };

                // Current directory + hint
                w = w.child(
                    div().w_full().px(self.s(14.0)).py(self.s(6.0))
                        .flex().items_center().gap(self.s(8.0))
                        .child(div().text_size(self.s(11.0)).text_color(t.text_muted()).child("cwd:"))
                        .child(div().text_size(self.s(12.0)).text_color(t.text()).child(current_display))
                        .child(div().flex_grow())
                        .child(div().text_size(self.s(9.0)).text_color(t.text_faint()).child("Enter=open  ⌫=parent  Tab=confirm"))
                );
                // Filter bar
                w = w.child(
                    div().w_full().px(self.s(14.0)).py(self.s(6.0)).border_b_1().border_color(t.palette_border())
                        .flex().items_center().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.blue()).child("filter:"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(
                            if setup.dir_filter.is_empty() {
                                "type to search...".to_string()
                            } else {
                                format!("{}│", setup.dir_filter)
                            }
                        ))
                        .child(div().flex_grow())
                        .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child(
                            format!("{} dirs", filtered.len())
                        ))
                );
                let visible_count = 15;
                let start = if setup.dir_cursor >= visible_count { setup.dir_cursor - visible_count + 1 } else { 0 };
                let end = (start + visible_count).min(filtered.len());
                for (vi, dir) in filtered[start..end].iter().enumerate() {
                    let list_idx = start + vi;
                    let sel = list_idx == setup.dir_cursor;
                    let home = std::env::var("HOME").unwrap_or_default();
                    let display = if !home.is_empty() && dir.starts_with(&home) {
                        format!("~{}", &dir[home.len()..])
                    } else { dir.clone() };
                    w = w.child(
                        div().w_full().px(self.s(14.0)).py(self.s(4.0))
                            .bg(if sel { t.selected_row() } else { rgba(0x00000000) })
                            .flex().items_center().gap(self.s(10.0)).overflow_hidden()
                            .child(div().text_size(self.s(12.0)).text_color(if sel { t.blue() } else { t.text_faint() })
                                .child(if sel { ">" } else { " " }))
                            .child(div().flex_shrink().min_w(px(0.)).text_size(self.s(12.0))
                                .text_color(if sel { t.text() } else { t.text_muted() })
                                .child(display)),
                    );
                }
                if filtered.len() > visible_count {
                    w = w.child(div().px(self.s(14.0)).py(self.s(4.0)).text_size(self.s(10.0)).text_color(t.text_faint())
                        .child(format!("showing {}-{} of {}", start + 1, end, filtered.len())));
                }
            }
            SetupStep::Mcps => {
                w = w.child(div().px(self.s(14.0)).py(self.s(8.0)).text_size(self.s(11.0)).text_color(t.text_muted())
                    .child("Toggle MCPs (Space to toggle, Tab to continue)"));
                for (i, mcp) in self.config.mcps.iter().enumerate() {
                    let cursor = i == setup.mcp_cursor;
                    let checked = setup.selected_mcps.get(i).copied().unwrap_or(false);
                    let checkbox = if checked { "[x]" } else { "[ ]" };
                    let locked = mcp.global;
                    w = w.child(
                        div().w_full().px(self.s(14.0)).py(self.s(6.0))
                            .bg(if cursor { t.selected_row() } else { rgba(0x00000000) })
                            .flex().items_center().gap(self.s(10.0))
                            .child(div().text_size(self.s(13.0)).text_color(
                                if checked { t.green() } else { t.text_faint() }
                            ).child(checkbox))
                            .child(div().text_size(self.s(13.0)).text_color(
                                if cursor { t.text() } else { t.text_muted() }
                            ).child(mcp.name.clone()))
                            .child(div().flex_grow())
                            .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child(
                                if locked { format!("{} (global)", mcp.description) } else { mcp.description.clone() }
                            )),
                    );
                }
            }
            SetupStep::Confirm => {
                let rt_name = &setup.selected_runtime;
                let model_name = if setup.selected_model.is_empty() { "default".to_string() } else { setup.selected_model.clone() };
                let machine_name = if setup.selected_machine.is_empty() { "local".to_string() } else { setup.selected_machine.clone() };
                let mcps: Vec<&str> = self.config.mcps.iter().enumerate()
                    .filter(|(i, _)| setup.selected_mcps.get(*i).copied().unwrap_or(false))
                    .map(|(_, m)| m.name.as_str())
                    .collect();
                let has_custom = !setup.custom_base_url.is_empty();

                let mut confirm_col = div().px(self.s(14.0)).py(self.s(12.0)).flex().flex_col().gap(self.s(8.0))
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("Runtime"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(rt_name.clone())))
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("Model"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(model_name.clone())));

                let home = std::env::var("HOME").unwrap_or_default();
                let dir_display = if !home.is_empty() && setup.selected_dir.starts_with(&home) {
                    format!("~{}", &setup.selected_dir[home.len()..])
                } else { setup.selected_dir.clone() };

                confirm_col = confirm_col
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("Machine"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(machine_name)))
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("Directory"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(dir_display)));

                if has_custom {
                    confirm_col = confirm_col.child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("Endpoint"))
                        .child(div().text_size(self.s(13.0)).text_color(t.yellow()).child(setup.custom_base_url.clone())));
                }

                confirm_col = confirm_col
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("MCPs"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(
                            if mcps.is_empty() { "none".into() } else { mcps.join(", ") }
                        )))
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).w(self.s(80.0)).child("Group"))
                        .child(div().text_size(self.s(13.0)).text_color(t.text()).child(self.current_group_name().to_string())))
                    .child(div().mt(self.s(8.0)).text_size(self.s(12.0)).text_color(t.green()).child(
                        if setup.editing_agent.is_some() { "Press Enter to apply, Esc to cancel" }
                        else { "Press Enter to create, Esc to cancel" }
                    ));
                w = w.child(confirm_col);
            }
        }

        // Bottom hint
        w = w.child(
            div().w_full().px(self.s(14.0)).py(self.s(8.0)).border_t_1().border_color(t.palette_border())
                .flex().gap(self.s(16.0))
                .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("Tab: next"))
                .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("Shift-Tab: back"))
                .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("Esc: cancel")),
        );
        let epoch = self.mode_epoch;
        w.with_animation(
            ElementId::Name(format!("setup-slide-{}", epoch).into()),
            Animation::new(Duration::from_millis(250))
                .with_easing(ease_out_quint()),
            |el, delta| el.opacity(delta),
        )
    }

    fn render_stats(&self) -> Div {
        let t = &self.theme;
        let mut panel = div()
            .absolute().top(self.s(0.0)).right(self.s(0.0))
            .w(self.s(500.0)).h_full()
            .bg(t.palette_bg()).border_l_1().border_color(t.palette_border())
            .py(self.s(16.0)).px(self.s(20.0))
            .flex().flex_col().gap(self.s(8.0))
            .overflow_hidden()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(self.s(-4.0), px(0.)),
                blur_radius: self.s(16.0),
                spread_radius: px(0.),
            }]);

        panel = panel.child(
            div().flex().items_center().justify_between()
                .child(div().text_size(self.s(16.0)).text_color(t.text()).font_weight(FontWeight::BOLD).child("Usage Stats"))
                .child(div().text_size(self.s(12.0)).text_color(t.text_faint()).child("[?] to close"))
        );

        // Aggregate totals
        let total_cost: f64 = self.agents.iter().map(|a| a.tokens.cost_usd).sum();
        let total_input: u64 = self.agents.iter().map(|a| a.tokens.input_tokens).sum();
        let total_output: u64 = self.agents.iter().map(|a| a.tokens.output_tokens).sum();
        let total_cache_read: u64 = self.agents.iter().map(|a| a.tokens.cache_read_tokens).sum();
        let total_cache_write: u64 = self.agents.iter().map(|a| a.tokens.cache_write_tokens).sum();
        let total_msgs: u32 = self.agents.iter().map(|a| a.message_count).sum();
        let total_tools: u32 = self.agents.iter().map(|a| a.tool_calls.total()).sum();

        panel = panel.child(
            div().w_full().p(self.s(12.0)).bg(self.bg_alpha(t.surface_raised())).rounded(self.s(8.0))
                .flex().flex_col().gap(self.s(6.0))
                .child(div().text_size(self.s(11.0)).text_color(t.text_muted()).font_weight(FontWeight::BOLD).child("TOTALS"))
                .child(self.stat_row("Total Cost", &format!("${:.4}", total_cost), t.green()))
                .child(self.stat_row("Input Tokens", &format!("{}", total_input), t.text()))
                .child(self.stat_row("Output Tokens", &format!("{}", total_output), t.text()))
                .child(self.stat_row("Cache Read", &format!("{}", total_cache_read), t.blue()))
                .child(self.stat_row("Cache Write", &format!("{}", total_cache_write), t.blue_muted()))
                .child(self.stat_row("Messages", &format!("{}", total_msgs), t.text()))
                .child(self.stat_row("Tool Calls", &format!("{}", total_tools), t.yellow()))
        );

        // Per-agent breakdown
        panel = panel.child(
            div().text_size(self.s(11.0)).text_color(t.text_muted()).font_weight(FontWeight::BOLD).mt(self.s(8.0)).child("PER AGENT")
        );

        for a in &self.agents {
            let rt_color = t.runtime_color(&a.runtime_name);
            let pct = a.tokens.context_usage_pct();
            panel = panel.child(
                div().w_full().p(self.s(10.0)).bg(self.bg_alpha(t.surface())).rounded(self.s(6.0))
                    .border_l_2().border_color(rt_color)
                    .flex().flex_col().gap(self.s(4.0))
                    .child(div().flex().items_center().gap(self.s(8.0))
                        .child(div().text_size(self.s(12.0)).text_color(t.text()).font_weight(FontWeight::BOLD).child(a.name.clone()))
                        .child(div().text_size(self.s(10.0)).text_color(rt_color).child(a.runtime_name.clone()))
                        .child(div().flex_grow())
                        .child(div().text_size(self.s(10.0)).text_color(a.status.color(t)).child(a.status.label()))
                        .child(div().text_size(self.s(11.0)).text_color(t.green()).child(format!("${:.4}", a.tokens.cost_usd)))
                    )
                    .child(div().flex().gap(self.s(12.0)).flex_wrap()
                        .child(self.mini_stat("in", a.tokens.input_tokens, t.text_muted()))
                        .child(self.mini_stat("out", a.tokens.output_tokens, t.text_muted()))
                        .child(self.mini_stat("cache-r", a.tokens.cache_read_tokens, t.blue()))
                        .child(self.mini_stat("cache-w", a.tokens.cache_write_tokens, t.blue_muted()))
                        .child(div().text_size(self.s(10.0)).text_color(t.text_faint())
                            .child(format!("ctx {:.0}%", pct)))
                        .child(div().text_size(self.s(10.0)).text_color(t.text_faint())
                            .child(format!("{}", a.tokens.model)))
                    )
                    .child(div().flex().gap(self.s(8.0))
                        .child(div().text_size(self.s(10.0)).text_color(t.text_faint())
                            .child(format!("{}ed {}sh {}rd {}wr", a.tool_calls.edits, a.tool_calls.bash, a.tool_calls.reads, a.tool_calls.writes)))
                        .child(div().flex_grow())
                        .child(div().text_size(self.s(10.0)).text_color(t.text_faint())
                            .child(format!("{} msgs", a.message_count)))
                        .child(div().text_size(self.s(10.0)).text_color(
                            if a.tokens.thinking_enabled { t.yellow() } else { t.text_faint() }
                        ).child(if a.tokens.thinking_enabled { "thinking ON" } else { "" }))
                    )
            );
        }

        // Keyboard shortcuts
        panel = panel.child(
            div().text_size(self.s(11.0)).text_color(t.text_muted()).font_weight(FontWeight::BOLD).mt(self.s(12.0)).child("SHORTCUTS")
        );
        panel = panel.child(
            div().w_full().p(self.s(10.0)).bg(self.bg_alpha(t.surface())).rounded(self.s(6.0))
                .flex().flex_col().gap(self.s(3.0))
                .child(self.shortcut_row("i / click", "enter insert mode"))
                .child(self.shortcut_row("esc", "command mode"))
                .child(self.shortcut_row(
                    if self.config.cautious_enter { "Cmd+Enter" } else { "Enter" },
                    if self.config.cautious_enter { "send prompt" } else { "send prompt (default)" }
                ))
                .child(self.shortcut_row(
                    if self.config.cautious_enter { "Enter" } else { "Cmd+Enter" },
                    if self.config.cautious_enter { "insert newline" } else { "alternate send" }
                ))
                .child(self.shortcut_row("Cmd-k", "command palette"))
                .child(self.shortcut_row("w/s", "switch groups"))
                .child(self.shortcut_row("a/d", "switch panes"))
                .child(self.shortcut_row("j/k", "scroll"))
                .child(self.shortcut_row("n", "new agent"))
                .child(self.shortcut_row("c", "change runtime or machine"))
                .child(self.shortcut_row("r", "relaunch agent"))
                .child(self.shortcut_row("x", "stop agent"))
                .child(self.shortcut_row("Enter", "continue interrupted turn"))
                .child(self.shortcut_row("f", "favorite"))
                .child(self.shortcut_row("p", "toggle auto-scroll"))
                .child(self.shortcut_row("|", "pipe to next agent"))
                .child(self.shortcut_row("g t", "open working dir"))
                .child(self.shortcut_row("1/2/3", "grid/pipeline/focus"))
                .child(self.shortcut_row("/", "search"))
                .child(self.shortcut_row("t", "cycle theme"))
                .child(self.shortcut_row("?", "this panel"))
        );

        panel
    }

    fn stat_row(&self, label: &str, value: &str, color: Rgba) -> Div {
        div().flex().justify_between()
            .child(div().text_size(self.s(11.0)).text_color(self.theme.text_faint()).child(label.to_string()))
            .child(div().text_size(self.s(11.0)).text_color(color).child(value.to_string()))
    }

    fn mini_stat(&self, label: &str, value: u64, color: Rgba) -> Div {
        div().flex().gap(self.s(4.0))
            .child(div().text_size(self.s(10.0)).text_color(self.theme.text_faint()).child(label.to_string()))
            .child(div().text_size(self.s(10.0)).text_color(color).child(format!("{}", value)))
    }

    fn shortcut_row(&self, key: &str, desc: &str) -> Div {
        div().flex().gap(self.s(8.0))
            .child(div().text_size(self.s(11.0)).text_color(self.theme.blue_muted()).w(self.s(80.0)).child(key.to_string()))
            .child(div().text_size(self.s(11.0)).text_color(self.theme.text_faint()).child(desc.to_string()))
    }

    fn render_search(&self) -> Div {
        let t = &self.theme;
        let mut panel = div()
            .absolute().top(self.s(0.0)).right(self.s(0.0))
            .w(self.s(500.0)).h_full()
            .bg(t.palette_bg()).border_l_1().border_color(t.palette_border())
            .flex().flex_col()
            .shadow(vec![
                BoxShadow {
                    color: t.shadow().into(),
                    offset: point(self.s(-4.0), px(0.)),
                    blur_radius: self.s(20.0),
                    spread_radius: px(0.),
                },
            ]);

        // Search input
        let query_display = if self.search_query.is_empty() {
            "Search across agents...".to_string()
        } else {
            self.search_query.clone()
        };
        let query_color = if self.search_query.is_empty() { t.text_faint() } else { t.text() };

        panel = panel.child(
            div().w_full().px(self.s(14.0)).py(self.s(10.0))
                .border_b_1().border_color(t.palette_border())
                .flex().items_center().gap(self.s(8.0))
                .child(div().text_size(self.s(14.0)).text_color(t.text_muted()).child("/"))
                .child(div().text_size(self.s(13.0)).text_color(query_color).child(query_display))
        );

        // Result count
        panel = panel.child(
            div().w_full().px(self.s(14.0)).py(self.s(4.0))
                .text_size(self.s(11.0)).text_color(t.text_faint())
                .child(format!("{} results", self.search_results.len()))
        );

        // Results list
        let mut results = div().flex_grow().w_full().overflow_hidden().flex().flex_col();
        for (i, sr) in self.search_results.iter().enumerate() {
            let selected = i == self.search_selection;
            let bg = if selected { t.selected_row() } else { rgba(0x00000000) };
            let line_preview: String = sr.line.chars().take(80).collect();
            results = results.child(
                div().w_full().px(self.s(14.0)).py(self.s(4.0)).bg(bg)
                    .flex().flex_col().gap(self.s(2.0))
                    .child(
                        div().flex().gap(self.s(8.0))
                            .child(div().text_size(self.s(11.0)).text_color(t.blue()).child(sr.agent_name.clone()))
                            .child(div().text_size(self.s(10.0)).text_color(t.text_faint()).child(format!("line {}", sr.line_idx + 1)))
                    )
                    .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).child(line_preview))
            );
        }
        panel = panel.child(results);

        // Bottom hint
        panel = panel.child(
            div().w_full().px(self.s(14.0)).py(self.s(6.0)).border_t_1().border_color(t.palette_border())
                .flex().gap(self.s(16.0))
                .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("W/S: navigate"))
                .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("Enter: jump"))
                .child(div().text_size(self.s(11.0)).text_color(t.text_faint()).child("Esc: close"))
        );

        panel
    }

    fn render_remove_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let idx = self.confirm_remove_agent.unwrap_or(0);
        let label = self.agents.get(idx)
            .map(|agent| {
                if agent.role == AgentRole::Coordinator {
                    format!("Remove `{}` and its delegated workers from view?", agent.name)
                } else {
                    format!("Remove `{}` from view?", agent.name)
                }
            })
            .unwrap_or_else(|| "Remove this agent from view?".into());

        div().absolute().top(px(0.)).left(px(0.)).size_full()
            .bg(rgba(0x00000066))
            .flex().items_center().justify_center()
            .child(
                div().w(self.s(420.0))
                    .bg(t.palette_bg())
                    .border_1().border_color(t.palette_border())
                    .rounded(self.s(12.0))
                    .shadow(vec![BoxShadow {
                        color: t.shadow().into(),
                        offset: point(px(0.), self.s(10.0)),
                        blur_radius: self.s(24.0),
                        spread_radius: px(0.),
                    }])
                    .p(self.s(16.0))
                    .flex().flex_col().gap(self.s(12.0))
                    .child(div().text_size(self.s(15.0)).text_color(t.text()).font_weight(FontWeight::BOLD).child("Are you sure?"))
                    .child(div().text_size(self.s(12.0)).text_color(t.text_muted()).child(label))
                    .child(
                        div().flex().justify_end().gap(self.s(8.0))
                            .child(
                                div().id("confirm-remove-no")
                                    .px(self.s(12.0)).py(self.s(7.0)).rounded(self.s(8.0))
                                    .bg(self.bg_alpha(t.surface_raised())).border_1().border_color(t.border())
                                    .cursor_pointer()
                                    .text_size(self.s(11.0)).text_color(t.text())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_remove_agent = None;
                                        cx.notify();
                                    }))
                                    .child("no")
                            )
                            .child(
                                div().id("confirm-remove-yes")
                                    .px(self.s(12.0)).py(self.s(7.0)).rounded(self.s(8.0))
                                    .bg(t.red()).cursor_pointer()
                                    .text_size(self.s(11.0)).text_color(t.bg())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(idx) = this.confirm_remove_agent.take() {
                                            this.remove_agent_and_dependents(idx);
                                        }
                                        cx.notify();
                                    }))
                                    .child("yes")
                            )
                    )
            )
    }

    fn render_copy_icon(&self, text: String, element_id: String, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        div().id(ElementId::Name(element_id.into()))
            .flex_shrink_0()
            .px(self.s(6.0)).py(self.s(3.0)).rounded(self.s(6.0))
            .cursor_pointer()
            .text_size(self.s(13.0)).text_color(self.theme.text_muted())
            .hover(|s| s.bg(self.theme.surface_raised()).text_color(self.theme.text()))
            .on_click(cx.listener(move |_this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }))
            .child("⧉")
    }

    fn transcript_font(&self) -> SharedString {
        if self.config.terminal_text {
            self.font_family.clone().into()
        } else {
            SharedString::from("Helvetica Neue")
        }
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let titlebar_safe_left = self.s(112.0);
        div().w_full()
            .pl(titlebar_safe_left).pr(self.s(12.0)).py(self.s(4.0))
            .bg(self.bg_alpha(t.surface()))
            .border_b_1().border_color(t.border())
            .flex().items_center().gap(self.s(8.0))
            .child(div().flex_grow())
            .child(
                div().flex().items_center().gap(self.s(4.0))
                    .child(
                        div().id("topbar-settings")
                            .px(self.s(7.0)).py(self.s(4.0)).rounded(self.s(6.0))
                            .cursor_pointer()
                            .text_size(self.s(14.0)).text_color(t.text_muted())
                            .hover(|s| s.bg(self.bg_alpha(t.surface_raised())).text_color(t.text()))
                            .child("⚙")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_palette(&OpenPalette, window, cx);
                            }))
                    )
                    .child(
                        div().id("topbar-stats")
                            .px(self.s(7.0)).py(self.s(4.0)).rounded(self.s(6.0))
                            .cursor_pointer()
                            .text_size(self.s(14.0)).text_color(if self.show_stats { t.blue() } else { t.text_muted() })
                            .hover(|s| s.bg(self.bg_alpha(t.surface_raised())).text_color(t.text()))
                            .child("⊞")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_stats(&ShowStats, window, cx);
                            }))
                    )
            )
    }
}

impl Render for OpenSquirrel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.clamp_focus();

        let top_bar = self.render_top_bar(cx);
        let sidebar = self.render_sidebar(cx);
        let vis = self.agents_in_current_group();

        let t = &self.theme;
        let content = match self.view_mode {
            ViewMode::Grid => {
                let n = vis.len();
                if n == 0 {
                    div().flex_grow().flex_shrink().min_w(px(0.)).h_full().p(px(4.0)).flex().justify_center().items_center()
                        .child(div().text_size(self.s(14.0)).text_color(t.text_faint()).child("No agents. Press [n] to create one."))
                } else {
                    // Compute grid layout: cols x rows
                    let (cols, rows) = match n {
                        1 => (1, 1),
                        2 => (2, 1),
                        3 => (2, 2), // 2 top, 1 bottom
                        4 => (2, 2),
                        5 | 6 => (3, 2),
                        7..=9 => (3, 3),
                        _ => {
                            let c = (n as f32).sqrt().ceil() as usize;
                            let r = (n + c - 1) / c;
                            (c, r)
                        }
                    };

                    let mut grid = div().flex_grow().flex_shrink().min_w(px(0.)).h_full()
                        .p(px(3.0)).flex().flex_col().gap(px(3.0)).overflow_hidden();

                    let mut tile_idx = 0;
                    for _row in 0..rows {
                        let tiles_in_row = if tile_idx + cols <= n { cols } else { n - tile_idx };
                        if tiles_in_row == 0 { break; }

                        let mut row_div = div().w_full().flex_grow().flex_shrink().flex_basis(px(0.))
                            .flex().gap(px(3.0)).overflow_hidden();

                        for _ in 0..tiles_in_row {
                            if tile_idx < n {
                                row_div = row_div.child(self.render_agent_tile(vis[tile_idx], cx));
                                tile_idx += 1;
                            }
                        }
                        grid = grid.child(row_div);
                    }
                    grid
                }
            }
            ViewMode::Focus => {
                let mut focus_div = div().flex_grow().flex_shrink().min_w(px(0.)).h_full().p(px(4.0)).flex().flex_col().overflow_hidden();
                if vis.contains(&self.focused_agent) {
                    focus_div = focus_div.child(self.render_agent_tile(self.focused_agent, cx));
                } else if let Some(&first) = vis.first() {
                    focus_div = focus_div.child(self.render_agent_tile(first, cx));
                } else {
                    focus_div = focus_div.child(div().flex_grow().h_full().flex().justify_center().items_center()
                        .child(div().text_size(self.s(14.0)).text_color(t.text_faint()).child("No agents. Press [n] to create one.")));
                }
                focus_div
            }
            ViewMode::Pipeline => {
                let mut pipe = div().flex_grow().flex_shrink().min_w(px(0.)).h_full().p(px(4.0)).flex().overflow_x_hidden().overflow_hidden();
                if vis.is_empty() {
                    pipe = pipe.child(div().flex_grow().h_full().flex().justify_center().items_center()
                        .child(div().text_size(self.s(14.0)).text_color(t.text_faint()).child("No agents. Press [n] to create one.")));
                } else {
                    for (pos, &idx) in vis.iter().enumerate() {
                        let a = &self.agents[idx];
                        let focused = idx == self.focused_agent;
                        let bc = if focused { t.border_focus() } else { t.border() };

                        let stage_shadow = if focused {
                            vec![BoxShadow {
                                color: t.glow_focus().into(),
                                offset: point(px(0.), px(0.)),
                                blur_radius: self.s(12.0),
                                spread_radius: self.s(1.0),
                            }]
                        } else {
                            vec![BoxShadow {
                                color: t.shadow().into(),
                                offset: point(px(0.), self.s(2.0)),
                                blur_radius: self.s(6.0),
                                spread_radius: px(0.),
                            }]
                        };

                        let stage = div()
                            .min_w(self.s(200.0)).max_w(self.s(350.0)).h_full().flex_shrink()
                            .bg(self.bg_alpha(t.bg())).border_1().border_color(bc).rounded(self.s(10.0)).m(self.s(4.0))
                            .flex().flex_col().overflow_hidden()
                            .shadow(stage_shadow)
                            .child(
                                div().w_full().px(self.s(10.0)).py(self.s(7.0))
                                    .bg(linear_gradient(
                                        180.0,
                                        linear_color_stop(self.bg_alpha(t.header_gradient_start()), 0.0),
                                        linear_color_stop(self.bg_alpha(t.header_gradient_end()), 1.0),
                                    ))
                                    .flex().items_center().gap(self.s(6.0))
                                    .child(div().text_size(self.s(10.0)).text_color(a.status.color(t)).child(a.status.dot()))
                                    .child(div().text_size(self.s(12.0)).text_color(t.text()).child(a.name.clone()))
                                    .child(div().flex_grow())
                                    .child(div().text_size(self.s(10.0)).text_color(a.status.color(t)).child(a.status.label()))
                            )
                            .child({
                                let mut out = div().flex_grow().px(self.s(8.0)).py(self.s(4.0)).overflow_hidden()
                                    .font_family(self.font_family.clone()).text_size(self.s(11.0)).line_height(self.s(16.0));
                                let start = a.output_lines.len().saturating_sub(10);
                                for line in &a.output_lines[start..] {
                                    let kind = classify_line(line);
                                    let c = match kind {
                                        LineKind::UserInput => t.user_input(),
                                        LineKind::Error => t.red(),
                                        LineKind::System => t.yellow(),
                                        LineKind::DiffAdd => t.green(),
                                        LineKind::DiffRemove => t.red(),
                                        _ => t.text_muted(),
                                    };
                                    out = out.child(div().text_color(c).child(line.clone()));
                                }
                                out
                            });

                        pipe = pipe.child(stage);

                        // Arrow between stages
                        if pos < vis.len() - 1 {
                            pipe = pipe.child(
                                div().h_full().flex().items_center().px(self.s(6.0))
                                    .child(div().text_size(self.s(18.0)).text_color(t.text_faint()).child("->"))
                            );
                        }
                    }
                }
                pipe
            }
        };

        let key_ctx = match self.mode {
            Mode::Normal => "NormalMode",
            Mode::Palette => "PaletteMode", Mode::Setup => "SetupMode",
            Mode::Search => "SearchMode",
        };

        // Wrap content in an overflow-hidden container so text can't push past window
        let content = div().flex_grow().flex_shrink().min_w(px(0.)).h_full().overflow_hidden().child(content);
        let body = div().flex_grow().min_h(px(0.)).w_full().flex().overflow_hidden()
            .child(sidebar)
            .child(content);

        let is_ops = self.config.theme == "ops";
        let mut root = div()
            .key_context(key_ctx).track_focus(&self.focus_handle).size_full().text_color(t.text())
            .when(!is_ops, |d| d.bg(self.bg_alpha(t.bg())))
            .flex().flex_col()
            .on_action(cx.listener(Self::enter_command_mode))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::close_palette))
            .on_action(cx.listener(Self::submit_input))
            .on_action(cx.listener(Self::delete_char))
            .on_action(cx.listener(Self::nav_up))
            .on_action(cx.listener(Self::nav_down))
            .on_action(cx.listener(Self::pane_left))
            .on_action(cx.listener(Self::pane_right))
            .on_action(cx.listener(Self::next_pane))
            .on_action(cx.listener(Self::prev_pane))
            .on_action(cx.listener(Self::next_group))
            .on_action(cx.listener(Self::prev_group))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_action(cx.listener(Self::spawn_agent))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .on_action(cx.listener(Self::quit_app))
            .on_action(cx.listener(Self::setup_next))
            .on_action(cx.listener(Self::setup_prev))
            .on_action(cx.listener(Self::setup_toggle))
            .on_action(cx.listener(Self::cycle_theme))
            .on_action(cx.listener(Self::kill_agent))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(cx.listener(Self::change_agent))
            .on_action(cx.listener(Self::restart_agent))
            .on_action(cx.listener(Self::toggle_auto_scroll))
            .on_action(cx.listener(Self::pipe_to_agent))
            .on_action(cx.listener(Self::open_terminal))
            .on_action(cx.listener(Self::show_stats))
            .on_action(cx.listener(Self::toggle_custom_endpoint))
            .on_action(cx.listener(Self::cursor_left))
            .on_action(cx.listener(Self::cursor_right))
            .on_action(cx.listener(Self::cursor_word_left))
            .on_action(cx.listener(Self::cursor_word_right))
            .on_action(cx.listener(Self::cursor_home))
            .on_action(cx.listener(Self::cursor_end))
            .on_action(cx.listener(Self::delete_word_back))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::insert_newline))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_action(cx.listener(Self::continue_turn))
            .on_action(cx.listener(Self::view_grid))
            .on_action(cx.listener(Self::view_pipeline))
            .on_action(cx.listener(Self::view_focus))
            .on_action(cx.listener(Self::search_open))
            .on_action(cx.listener(Self::search_close))
            .on_action(cx.listener(Self::toggle_voice))
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(top_bar)
            .child(body);

        if self.mode == Mode::Palette { root = root.child(self.render_palette(cx)); }
        if self.mode == Mode::Setup && self.setup.is_some() { root = root.child(self.render_setup(cx)); }
        if self.mode == Mode::Search { root = root.child(self.render_search()); }
        if self.show_stats { root = root.child(self.render_stats()); }
        if self.confirm_remove_agent.is_some() { root = root.child(self.render_remove_confirm(cx)); }

        // Wrap with starfield background for ops theme
        if self.config.theme == "ops" {
            let wrapper = div().size_full().relative()
                .child(
                    div().absolute().top(px(0.)).left(px(0.)).size_full()
                        .child(self.render_starfield(cx))
                )
                .child(root);
            return wrapper.into_any_element();
        }

        root.into_any_element()
    }
}

// ── Background thread ───────────────────────────────────────────

// Parse Claude stream-json output line
fn parse_claude_json(line: &str, msg_tx: &async_channel::Sender<AgentMsg>, line_buf: &mut String) -> bool {
    if let Ok(v) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "content_block_delta" => {
                if let Some(delta) = v.get("delta") {
                    if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                        // Buffer text fragments and only emit on newline boundaries
                        line_buf.push_str(text);
                        while let Some(nl) = line_buf.find('\n') {
                            let complete_line: String = line_buf.drain(..=nl).collect();
                            let trimmed = complete_line.trim_end_matches('\n');
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(trimmed.to_string()));
                        }
                    }
                    if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                        for l in thinking.split('\n') {
                            if !l.trim().is_empty() {
                                let _ = msg_tx.send_blocking(AgentMsg::OutputLine(format!("[think] {}", l)));
                            }
                        }
                    }
                }
            }
            "content_block_stop" => {
                // Flush any remaining buffered text when a content block ends
                if !line_buf.is_empty() {
                    let remaining = line_buf.drain(..).collect::<String>();
                    if !remaining.trim().is_empty() {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(remaining));
                    }
                }
            }
            "result" => {
                // Final result with full token stats
                let mut stats = TokenStats::default();
                if let Some(usage) = v.get("usage") {
                    stats.input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.cache_read_tokens = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.cache_write_tokens = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                }
                // Use the top-level "model" field if present (from system/init), otherwise
                // pick the primary model from modelUsage (highest output token count)
                if let Some(model_usage) = v.get("modelUsage").and_then(|v| v.as_object()) {
                    let mut primary_model = String::new();
                    let mut max_output = 0u64;
                    for (model_name, mu) in model_usage {
                        let out = mu.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        if out > max_output || primary_model.is_empty() {
                            max_output = out;
                            primary_model = model_name.clone();
                        }
                        stats.input_tokens += mu.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.output_tokens += mu.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.cache_read_tokens += mu.get("cacheReadInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.cache_write_tokens += mu.get("cacheCreationInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.context_window = mu.get("contextWindow").and_then(|v| v.as_u64()).unwrap_or(200_000);
                        stats.max_output_tokens = mu.get("maxOutputTokens").and_then(|v| v.as_u64()).unwrap_or(32_000);
                    }
                    stats.model = primary_model;
                }
                stats.cost_usd = v.get("total_cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                stats.session_id = v.get("session_id").and_then(|v| v.as_str()).map(String::from);
                if let Some(fm) = v.get("fast_mode_state").and_then(|v| v.as_str()) {
                    stats.thinking_enabled = fm != "off";
                }
                let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));

                // Also show the text result if present
                if let Some(result) = v.get("result").and_then(|v| v.as_str()) {
                    if !result.is_empty() {
                        for l in result.split('\n') {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(l.to_string()));
                        }
                    }
                }
                return true; // signals done
            }
            "error" => {
                let err = v.get("error").and_then(|e| e.get("message").and_then(|m| m.as_str()))
                    .or_else(|| v.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("unknown error");
                let _ = msg_tx.send_blocking(AgentMsg::Error(err.to_string()));
            }
            // Track tool calls from Claude's subagent/assistant messages
            "assistant" => {
                if let Some(content) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                                let _ = msg_tx.send_blocking(AgentMsg::ToolCall(name.to_string()));
                            }
                        }
                    }
                }
            }
            // Capture model from system/init event (authoritative source)
            "system" => {
                if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                    if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                        let mut stats = TokenStats::default();
                        stats.model = model.to_string();
                        if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                            stats.session_id = Some(sid.to_string());
                        }
                        let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                    }
                }
            }
            _ => {} // ignore other event types
        }
    }
    false
}

// Parse Codex JSONL output line
fn parse_codex_json(line: &str, msg_tx: &async_channel::Sender<AgentMsg>) -> bool {
    if let Ok(v) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "message.delta" | "content_block_delta" => {
                if let Some(delta) = v.get("delta") {
                    if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                        for l in text.split('\n') {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(l.to_string()));
                        }
                    }
                }
            }
            "item.completed" => {
                if let Some(item) = v.get("item") {
                    // Track tool calls
                    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if item_type == "function_call" || item_type == "tool_use" {
                        let name = item.get("name").and_then(|n| n.as_str())
                            .or_else(|| item.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
                            .unwrap_or("tool");
                        let _ = msg_tx.send_blocking(AgentMsg::ToolCall(name.to_string()));
                    }
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        for l in text.split('\n') {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(l.to_string()));
                        }
                    }
                    // Error items
                    if let Some(msg) = item.get("message").and_then(|m| m.as_str()) {
                        if item_type == "error" {
                            let _ = msg_tx.send_blocking(AgentMsg::Error(msg.to_string()));
                        }
                    }
                }
            }
            "turn.completed" => {
                // Extract usage if present
                if let Some(usage) = v.get("usage") {
                    let mut stats = TokenStats::default();
                    stats.input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.context_window = 200_000;
                    let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                }
                return true;
            }
            "turn.failed" => {
                let raw = v.get("error").and_then(|e| e.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("turn failed");
                // Error message is often nested JSON like {"detail":"..."} -- extract it
                let err = if let Ok(inner) = serde_json::from_str::<JsonValue>(raw) {
                    inner.get("detail").and_then(|d| d.as_str()).unwrap_or(raw).to_string()
                } else { raw.to_string() };
                let _ = msg_tx.send_blocking(AgentMsg::Error(err));
                return true;
            }
            "error" => {
                // Error events are also reported in turn.failed, skip to avoid duplicates
            }
            "thread.started" | "turn.started" => {} // ignore
            _ => {}
        }
    }
    false
}

// Parse OpenCode JSON output (non-streaming: full text per step).
// Tool calls collapsed to a single summary line.
fn parse_opencode_json(line: &str, msg_tx: &async_channel::Sender<AgentMsg>) -> bool {
    if let Ok(v) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "text" => {
                if let Some(text) = v.get("part").and_then(|p| p.get("text")).and_then(|t| t.as_str()) {
                    for l in text.split('\n') {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(l.to_string()));
                    }
                }
            }
            "tool_use" => {
                // Collapsed: one-line summary, no output dump
                if let Some(part) = v.get("part") {
                    let tool = part.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
                    let status = part.get("state")
                        .and_then(|s| s.get("status"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    let title = part.get("state")
                        .and_then(|s| s.get("title"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    // Track tool call
                    if status == "completed" {
                        let _ = msg_tx.send_blocking(AgentMsg::ToolCall(tool.to_string()));
                    }
                    // Only show completed tool calls (skip pending/running noise)
                    if status == "completed" {
                        let preview = part.get("state")
                            .and_then(|s| s.get("metadata"))
                            .and_then(|m| m.get("preview"))
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        let display = if !title.is_empty() {
                            format!("[{}] {}", tool, title)
                        } else if !preview.is_empty() {
                            // Truncate preview to first line
                            let first_line = preview.split('\n').next().unwrap_or("");
                            format!("[{}] {}", tool, first_line)
                        } else {
                            format!("[{}] done", tool)
                        };
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(display));
                    }
                }
            }
            "step_finish" => {
                if let Some(part) = v.get("part") {
                    if let Some(tokens) = part.get("tokens") {
                        let mut stats = TokenStats::default();
                        stats.input_tokens = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.output_tokens = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.cache_read_tokens = tokens.get("cache").and_then(|c| c.get("read")).and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.cache_write_tokens = tokens.get("cache").and_then(|c| c.get("write")).and_then(|v| v.as_u64()).unwrap_or(0);
                        stats.context_window = 200_000;
                        stats.cost_usd = part.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        stats.session_id = v.get("sessionID").and_then(|s| s.as_str()).map(String::from);
                        let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                    }
                    if part.get("reason").and_then(|r| r.as_str()) == Some("stop") {
                        return true;
                    }
                }
            }
            _ => {} // step_start, etc
        }
    }
    false
}

fn parse_cursor_json(line: &str, msg_tx: &async_channel::Sender<AgentMsg>, line_buf: &mut String) -> bool {
    if let Ok(v) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "system" => {
                // Init event: extract session_id and model
                let mut stats = TokenStats::default();
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    stats.session_id = Some(sid.to_string());
                }
                if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                    stats.model = model.to_string();
                }
                stats.context_window = 200_000;
                let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
            }
            "assistant" => {
                // With --stream-partial-output, streaming deltas have timestamp_ms.
                // The final summary (no timestamp_ms) duplicates the full text -- skip it.
                let is_streaming_delta = v.get("timestamp_ms").is_some();
                if is_streaming_delta {
                    if let Some(content) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                        for block in content {
                            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if block_type == "text" {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    line_buf.push_str(text);
                                    while let Some(nl) = line_buf.find('\n') {
                                        let complete_line: String = line_buf.drain(..=nl).collect();
                                        let trimmed = complete_line.trim_end_matches('\n');
                                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(trimmed.to_string()));
                                    }
                                }
                            } else if block_type == "tool_use" {
                                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                                    let _ = msg_tx.send_blocking(AgentMsg::ToolCall(name.to_string()));
                                }
                            }
                        }
                    }
                }
            }
            "tool_call" => {
                let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                if subtype == "completed" {
                    // Extract tool name from nested structure
                    if let Some(tc) = v.get("tool_call") {
                        let tool_name = tc.get("shellToolCall").map(|_| "bash")
                            .or_else(|| tc.get("readToolCall").map(|_| "read"))
                            .or_else(|| tc.get("editToolCall").map(|_| "edit"))
                            .or_else(|| tc.get("writeToolCall").map(|_| "write"))
                            .unwrap_or("tool");
                        let _ = msg_tx.send_blocking(AgentMsg::ToolCall(tool_name.to_string()));
                        // Show tool description
                        let desc = tc.as_object()
                            .and_then(|obj| obj.values().next())
                            .and_then(|v: &JsonValue| v.get("description"))
                            .and_then(|d: &JsonValue| d.as_str())
                            .unwrap_or("");
                        if !desc.is_empty() {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(format!("[{}] {}", tool_name, desc)));
                        }
                    }
                }
            }
            "result" => {
                // Flush remaining buffer
                if !line_buf.is_empty() {
                    let remaining = line_buf.drain(..).collect::<String>();
                    if !remaining.trim().is_empty() {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(remaining));
                    }
                }
                // Extract usage stats
                if let Some(usage) = v.get("usage") {
                    let mut stats = TokenStats::default();
                    stats.input_tokens = usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.output_tokens = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.cache_read_tokens = usage.get("cacheReadTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.cache_write_tokens = usage.get("cacheWriteTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    stats.context_window = 200_000;
                    stats.session_id = v.get("session_id").and_then(|s| s.as_str()).map(String::from);
                    let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                }
                let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                if is_error {
                    let err = v.get("result").and_then(|r| r.as_str()).unwrap_or("cursor error");
                    let _ = msg_tx.send_blocking(AgentMsg::Error(err.to_string()));
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

fn build_remote_shell_command(program: &str, args: &[String], prompt: &str, workdir: Option<&str>) -> String {
    let mut parts = vec![shell_escape(program)];
    for arg in args {
        parts.push(shell_escape(arg));
    }
    parts.push(shell_escape(prompt));
    let invoke = parts.join(" ");
    if let Some(dir) = workdir.filter(|dir| !dir.is_empty()) {
        format!("cd {} && {}", shell_escape(dir), invoke)
    } else {
        invoke
    }
}

fn build_remote_wrapped_command(
    runtime: &RuntimeDef,
    args: &[String],
    prompt: &str,
    workdir: Option<&str>,
) -> String {
    let mut command = String::new();
    if !runtime.env_remove.is_empty() {
        command.push_str("env");
        for env in &runtime.env_remove {
            command.push(' ');
            command.push_str("-u ");
            command.push_str(&shell_escape(env));
        }
        command.push(' ');
    }
    for (key, value) in &runtime.env_set {
        command.push_str(&format!("{}={} ", key, shell_escape(value)));
    }
    command.push_str(&build_remote_shell_command(&runtime.command, args, prompt, workdir));
    command
}

fn run_ssh_script(destination: &str, script: &str) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    let mut child = Command::new("ssh")
        .arg(destination)
        .arg("bash")
        .arg("-l")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(script.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    drop(child.stdin.take());
    child.wait_with_output()
}

fn launch_remote_tmux_session(
    destination: &str,
    session_name: &str,
    runtime: &RuntimeDef,
    args: &[String],
    prompt: &str,
    workdir: Option<&str>,
) -> anyhow::Result<()> {
    let remote_command = build_remote_wrapped_command(runtime, args, prompt, workdir);
    // Wrap in a login shell so PATH includes user-installed tools (claude, codex, etc.)
    let login_wrapped = format!("bash -lc {}", shell_escape(&remote_command));
    let escaped_session = shell_escape(session_name);
    let script = format!(
        "tmux kill-session -t {session} >/dev/null 2>&1 || true; \
         tmux new-session -d -s {session} -x 300 -y 50 {command}; \
         tmux set-option -t {session} history-limit 50000 >/dev/null 2>&1 || true; \
         tmux set-option -t {session} remain-on-exit on >/dev/null 2>&1",
        session = escaped_session,
        command = shell_escape(&login_wrapped),
    );
    let output = run_ssh_script(destination, &script)?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
}

fn remote_tmux_snapshot(destination: &str, session_name: &str) -> anyhow::Result<(Vec<String>, bool)> {
    let script = format!(
        "if ! tmux has-session -t {session} 2>/dev/null; then echo '__OSQ_NO_SESSION__'; exit 0; fi; \
         tmux capture-pane -p -J -S - -t {session}; \
         printf '\\n__OSQ_PANE_DEAD__=%s\\n' \"$(tmux display-message -p -t {session} '#{{pane_dead}}' 2>/dev/null || echo 1)\"",
        session = shell_escape(session_name),
    );
    let output = run_ssh_script(destination, &script)?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("__OSQ_NO_SESSION__") {
        return Ok((Vec::new(), true));
    }
    let mut lines = stdout.lines().map(|line| line.to_string()).collect::<Vec<_>>();
    let pane_dead = if let Some(last) = lines.last() {
        if let Some(value) = last.strip_prefix("__OSQ_PANE_DEAD__=") {
            value.trim() == "1"
        } else {
            false
        }
    } else {
        false
    };
    if lines.last().map(|line| line.starts_with("__OSQ_PANE_DEAD__=")).unwrap_or(false) {
        lines.pop();
    }
    Ok((lines, pane_dead))
}

fn stream_remote_tmux_session(
    msg_tx: &async_channel::Sender<AgentMsg>,
    destination: &str,
    session_name: &str,
    mut parse_json: impl FnMut(&str) -> bool,
    start_cursor: usize,
) {
    let mut cursor = start_cursor;
    loop {
        match remote_tmux_snapshot(destination, session_name) {
            Ok((lines, pane_dead)) => {
                let start = cursor.min(lines.len());
                for line in &lines[start..] {
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    if trimmed.starts_with("Pane is dead") { continue; }
                    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line.clone()));
                        continue;
                    }
                    parse_json(line);
                }
                cursor = lines.len();
                let _ = msg_tx.send_blocking(AgentMsg::RemoteCursor(cursor));
                if pane_dead {
                    break;
                }
            }
            Err(error) => {
                let _ = msg_tx.send_blocking(AgentMsg::Error(format!(
                    "remote tmux polling failed for '{}': {}",
                    session_name,
                    error
                )));
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn build_mcp_config_args(mcps: &[McpDef]) -> Vec<String> {
    if mcps.is_empty() { return Vec::new(); }
    let mut servers = serde_json::Map::new();
    for mcp in mcps {
        servers.insert(mcp.name.clone(), serde_json::json!({
            "command": mcp.command,
            "args": mcp.args,
        }));
    }
    let config = serde_json::json!({ "mcpServers": servers });
    vec!["--mcp-config".into(), config.to_string()]
}

fn agent_thread(
    msg_tx: async_channel::Sender<AgentMsg>,
    prompt_rx: mpsc::Receiver<String>,
    runtime: RuntimeDef,
    model_override: Option<String>,
    target: MachineTarget,
    role: AgentRole,
    remote_session_name: Option<String>,
    mcps: Vec<McpDef>,
) {
    let _ = msg_tx.send_blocking(AgentMsg::Ready);
    let is_claude = runtime.name == "claude";
    let is_codex = runtime.name == "codex";
    let is_opencode = runtime.name == "opencode";
    let is_cursor = runtime.name == "cursor";
    let is_json_mode = is_claude || is_codex || is_opencode || is_cursor;

    // Claude supports persistent stdin with stream-json input/output.
    // We spawn once and keep writing messages to stdin for multi-turn.
    if is_claude && role == AgentRole::Coordinator && target.ssh_destination.is_none() {
        agent_thread_claude_persistent(&msg_tx, &prompt_rx, &runtime, &model_override, &mcps);
        return;
    }

    // Other runtimes: one process per prompt (legacy mode)
    while let Ok(raw_msg) = prompt_rx.recv() {
        let reattach_session = raw_msg.strip_prefix("__OSQ_REATTACH__").and_then(|s| {
            let (cursor, session) = s.split_once("::")?;
            let cursor = cursor.parse::<usize>().ok()?;
            Some((cursor, session.to_string()))
        });
        if let (Some(destination), Some((line_cursor, session_name))) = (target.ssh_destination.as_ref(), reattach_session) {
            let msg_tx_clone = msg_tx.clone();
            let mut line_buf = String::new();
            stream_remote_tmux_session(
                &msg_tx_clone,
                destination,
                &session_name,
                |line| {
                    if is_claude {
                        parse_claude_json(line, &msg_tx_clone, &mut line_buf)
                    } else if is_cursor {
                        parse_cursor_json(line, &msg_tx_clone, &mut line_buf)
                    } else if is_opencode {
                        parse_opencode_json(line, &msg_tx_clone)
                    } else {
                        parse_codex_json(line, &msg_tx_clone)
                    }
                },
                line_cursor,
            );
            let _ = msg_tx.send_blocking(AgentMsg::Done { session_id: None });
            continue;
        }

        let (session_id, prompt) = if let Some(rest) = raw_msg.strip_prefix("SESSION:") {
            if let Some(nl) = rest.find('\n') {
                (Some(rest[..nl].to_string()), rest[nl + 1..].to_string())
            } else { (None, raw_msg) }
        } else { (None, raw_msg) };

        let mut turn_args = runtime.args.clone();

        if let Some(ref model) = model_override {
            if !model.is_empty() && !runtime.model_flag.is_empty() {
                if runtime.model_flag == "-c" {
                    turn_args.extend(["-c".into(), format!("model=\"{}\"", model)]);
                } else {
                    turn_args.extend([runtime.model_flag.clone(), model.clone()]);
                }
            }
        }

        if let Some(ref sid) = session_id {
            if is_opencode { turn_args.extend(["--session".into(), sid.clone(), "--continue".into()]); }
            if is_cursor { turn_args.extend(["--resume".into(), sid.clone()]); }
        }

        // Add MCP server configs (Claude-style --mcp-config)
        if is_claude && !mcps.is_empty() {
            turn_args.extend(build_mcp_config_args(&mcps));
        }

        let mut cmd = if let Some(destination) = target.ssh_destination.as_ref() {
            let session_name = remote_session_name.clone()
                .unwrap_or_else(|| make_tmux_session_name(&runtime.name));
            match launch_remote_tmux_session(
                destination,
                &session_name,
                &runtime,
                &turn_args,
                &prompt,
                target.workdir.as_deref(),
            ) {
                Ok(()) => {}
                Err(error) => {
                    let _ = msg_tx.send_blocking(AgentMsg::Error(format!(
                        "remote tmux launch failed on '{}': {}",
                        target.name,
                        error
                    )));
                    continue;
                }
            }
            let msg_tx_clone = msg_tx.clone();
            let mut line_buf = String::new();
            stream_remote_tmux_session(
                &msg_tx_clone,
                destination,
                &session_name,
                |line| {
                    if is_claude {
                        parse_claude_json(line, &msg_tx_clone, &mut line_buf)
                    } else if is_cursor {
                        parse_cursor_json(line, &msg_tx_clone, &mut line_buf)
                    } else if is_opencode {
                        parse_opencode_json(line, &msg_tx_clone)
                    } else {
                        parse_codex_json(line, &msg_tx_clone)
                    }
                },
                0,
            );
            let _ = msg_tx.send_blocking(AgentMsg::Done { session_id: session_id.clone() });
            continue;
        } else {
            let mut cmd = Command::new(&runtime.command);
            cmd.args(&turn_args);
            cmd.arg(&prompt);
            if let Some(dir) = target.workdir.as_ref().filter(|dir| !dir.is_empty()) {
                cmd.current_dir(dir);
            }
            cmd
        };

        for env in &runtime.env_remove { cmd.env_remove(env); }
        for (k, v) in &runtime.env_set { cmd.env(k, v); }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());

        match cmd.spawn() {
            Ok(mut child) => {
                let stderr_tx = msg_tx.clone();
                let stderr_handle = if let Some(stderr) = child.stderr.take() {
                    Some(std::thread::spawn(move || {
                        for text in BufReader::new(stderr).lines().map_while(Result::ok) {
                            if !text.trim().is_empty() {
                                let _ = stderr_tx.send_blocking(AgentMsg::StderrLine(text));
                            }
                        }
                    }))
                } else { None };

                if let Some(stdout) = child.stdout.take() {
                    let mut line_buf = String::new();
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        if is_json_mode {
                            let done = if is_claude {
                                parse_claude_json(&line, &msg_tx, &mut line_buf)
                            } else if is_cursor {
                                parse_cursor_json(&line, &msg_tx, &mut line_buf)
                            } else if is_opencode {
                                parse_opencode_json(&line, &msg_tx)
                            } else {
                                parse_codex_json(&line, &msg_tx)
                            };
                            if done { break; }
                        } else {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line));
                        }
                    }
                    if !line_buf.is_empty() {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line_buf));
                    }
                }

                if let Some(h) = stderr_handle { let _ = h.join(); }
                let _ = child.wait();
                let _ = msg_tx.send_blocking(AgentMsg::Done { session_id: session_id.clone() });
            }
            Err(e) => {
                let _ = msg_tx.send_blocking(AgentMsg::Error(format!(
                    "spawn '{}' on '{}' failed: {}",
                    runtime.command,
                    target.name,
                    e
                )));
            }
        }
    }
}

/// Persistent Claude process with stdin pipe for true multi-turn conversations.
/// Uses --input-format stream-json --output-format stream-json to keep a single
/// process alive across all user messages.
fn agent_thread_claude_persistent(
    msg_tx: &async_channel::Sender<AgentMsg>,
    prompt_rx: &mpsc::Receiver<String>,
    runtime: &RuntimeDef,
    model_override: &Option<String>,
    mcps: &[McpDef],
) {
    use std::io::Write;

    // Wait for first prompt to arrive before spawning
    let first_msg = match prompt_rx.recv() {
        Ok(m) => m,
        Err(_) => return,
    };

    let (session_id, first_prompt) = parse_session_prompt(&first_msg);

    let mut args = build_persistent_runtime_args(
        &runtime.args,
        &runtime.model_flag,
        model_override.as_deref(),
        session_id.as_deref(),
    );

    // Add MCP server configs
    if !mcps.is_empty() {
        args.extend(build_mcp_config_args(mcps));
    }

    let mut cmd = Command::new(&runtime.command);
    cmd.args(&args);

    for env in &runtime.env_remove { cmd.env_remove(env); }
    for (k, v) in &runtime.env_set { cmd.env(k, v); }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = msg_tx.send_blocking(AgentMsg::Error(format!("spawn '{}' failed: {}", runtime.command, e)));
            return;
        }
    };

    let mut stdin = child.stdin.take().expect("stdin piped");

    // Stderr reader thread
    let stderr_tx = msg_tx.clone();
    let stderr_handle = if let Some(stderr) = child.stderr.take() {
        Some(std::thread::spawn(move || {
            for text in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !text.trim().is_empty() {
                    let _ = stderr_tx.send_blocking(AgentMsg::StderrLine(text));
                }
            }
        }))
    } else { None };

    // Stdout reader thread -- runs continuously, sends AgentMsg::Done on each "result"
    let stdout_tx = msg_tx.clone();
    let stdout_handle = if let Some(stdout) = child.stdout.take() {
        Some(std::thread::spawn(move || {
            let mut line_buf = String::new();
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // parse_claude_json returns true on "result" (turn done)
                let turn_done = parse_claude_json(&line, &stdout_tx, &mut line_buf);
                if turn_done {
                    // Extract session_id from the result line
                    let sid = serde_json::from_str::<JsonValue>(&line).ok()
                        .and_then(|v| v.get("session_id").and_then(|s| s.as_str()).map(String::from));
                    let _ = stdout_tx.send_blocking(AgentMsg::Done { session_id: sid });
                    // Don't break -- keep reading for the next turn
                }
            }
        }))
    } else { None };

    // Send first prompt as stream-json user message
    let user_msg = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": first_prompt }
    });
    if writeln!(stdin, "{}", user_msg).is_err() {
        let _ = msg_tx.send_blocking(AgentMsg::Error("failed to write to claude stdin".into()));
        return;
    }
    let _ = stdin.flush();

    // Main loop: wait for next prompt, write to stdin
    while let Ok(raw_msg) = prompt_rx.recv() {
        let (_sid, prompt) = if let Some(rest) = raw_msg.strip_prefix("SESSION:") {
            if let Some(nl) = rest.find('\n') {
                (Some(rest[..nl].to_string()), rest[nl + 1..].to_string())
            } else { (None, raw_msg) }
        } else { (None, raw_msg) };

        let user_msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt }
        });
        if writeln!(stdin, "{}", user_msg).is_err() {
            let _ = msg_tx.send_blocking(AgentMsg::Error("claude process stdin closed".into()));
            break;
        }
        let _ = stdin.flush();
    }

    // prompt_rx closed (agent killed / app quitting) -- drop stdin to signal EOF
    drop(stdin);
    if let Some(h) = stdout_handle { let _ = h.join(); }
    if let Some(h) = stderr_handle { let _ = h.join(); }
    let _ = child.wait();
}

// ── Main ────────────────────────────────────────────────────────

fn main() {
    // Single-instance: kill any existing raw/bundled OpenSquirrel process (except ourselves)
    let my_pid = std::process::id();
    for process_name in ["opensquirrel", "OpenSquirrel"] {
        if let Ok(output) = Command::new("pgrep").arg("-x").arg(process_name).output() {
            let pids = String::from_utf8_lossy(&output.stdout);
            for line in pids.lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    if pid != my_pid {
                        let _ = Command::new("kill").arg(pid.to_string()).output();
                    }
                }
            }
        }
    }

    Application::new().with_assets(Assets).run(|app| {
        app.bind_keys([
            // Escape: dismiss overlays (palette, setup, search)
            KeyBinding::new("escape", EnterCommandMode, None),

            // Text input (always active in Normal mode)
            KeyBinding::new("enter", SubmitInput, None),
            KeyBinding::new("cmd-v", PasteClipboard, None),
            KeyBinding::new("backspace", DeleteChar, None),
            KeyBinding::new("left", CursorLeft, None),
            KeyBinding::new("right", CursorRight, None),
            KeyBinding::new("alt-left", CursorWordLeft, None),
            KeyBinding::new("alt-right", CursorWordRight, None),
            KeyBinding::new("cmd-left", CursorHome, None),
            KeyBinding::new("cmd-right", CursorEnd, None),
            KeyBinding::new("alt-backspace", DeleteWordBack, None),
            KeyBinding::new("cmd-backspace", DeleteToStart, None),

            // Pane/group navigation
            KeyBinding::new("cmd-]", NextPane, None),
            KeyBinding::new("cmd-[", PrevPane, None),
            KeyBinding::new("cmd-}", NextGroup, None),
            KeyBinding::new("cmd-{", PrevGroup, None),

            // App actions (Cmd- keybinds, always available)
            KeyBinding::new("cmd-k", OpenPalette, None),
            KeyBinding::new("cmd-shift-p", OpenPalette, None),
            KeyBinding::new("cmd-n", SpawnAgent, None),
            KeyBinding::new("cmd-f", SearchOpen, None),
            KeyBinding::new("cmd-=", ZoomIn, None),
            KeyBinding::new("cmd--", ZoomOut, None),
            KeyBinding::new("cmd-0", ZoomReset, None),

            // Palette overlay
            KeyBinding::new("escape", ClosePalette, Some("PaletteMode")),
            KeyBinding::new("up", NavUp, Some("PaletteMode")),
            KeyBinding::new("down", NavDown, Some("PaletteMode")),

            // Setup wizard overlay
            KeyBinding::new("tab", SetupNext, Some("SetupMode")),
            KeyBinding::new("shift-tab", SetupPrev, Some("SetupMode")),
            KeyBinding::new("space", SetupToggle, Some("SetupMode")),
            KeyBinding::new("up", NavUp, Some("SetupMode")),
            KeyBinding::new("down", NavDown, Some("SetupMode")),
            KeyBinding::new("ctrl-e", ToggleCustomEndpoint, Some("SetupMode")),

            // Search overlay
            KeyBinding::new("escape", SearchClose, Some("SearchMode")),
            KeyBinding::new("up", NavUp, Some("SearchMode")),
            KeyBinding::new("down", NavDown, Some("SearchMode")),
        ]);

        // Load config to apply bg_opacity/bg_blur at window creation.
        // Note: Window background cannot be changed at runtime; restart required for blur/transparency changes.
        let config = AppConfig::load();
        let window_background = if config.bg_blur > 0.0 {
            WindowBackgroundAppearance::Blurred
        } else if config.bg_opacity < 1.0 {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Opaque
        };

        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1400.0), px(900.0)), app))),
            titlebar: Some(TitlebarOptions {
                title: Some("OpenSquirrel".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(10.0), px(10.0))),
            }),
            window_background,
            ..Default::default()
        };

        app.open_window(opts, |window, app| {
            let view = app.new(|cx| OpenSquirrel::new(cx));
            view.update(app, |this, _cx| { this.focus_handle.focus(window); });
            view
        }).unwrap();
    });
}
