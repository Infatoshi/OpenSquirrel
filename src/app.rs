#![allow(unreachable_patterns)]
use crate::assets::Assets;
use crate::changes::ChangesState;
use crate::config::{
    AppConfig, McpDef, ModelOption, RecentProject, RuntimeInfo, SavedAgentState, SavedAppState,
    read_runtime_info,
};
use crate::hooks::HooksServer;
use crate::runtime::{
    agent_thread, fetch_cursor_model_list, fetch_opencode_model_list, make_tmux_session_name,
};
use crate::theme::{ThemeColors, ThemeDef, builtin_themes};
use crate::worktree::WorktreeInfo;
use gpui::prelude::*;
use gpui::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Write adapter that wraps an Arc<Mutex<Box<dyn Write>>> so we can share the writer
/// between TerminalView (which owns a writer) and our paste handler.
struct SharedWriter(Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .flush()
    }
}

/// Convert a GPUI keystroke to PTY bytes. Handles modifier combos that
/// gpui-terminal's keystroke_to_bytes misses (Alt+arrows, Cmd+arrows).
fn keystroke_to_pty_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let key = ks.key.as_str();
    let ctrl = ks.modifiers.control;
    let alt = ks.modifiers.alt;
    let shift = ks.modifiers.shift;
    let cmd = ks.modifiers.platform;

    // Cmd key mappings (iTerm2-style): translate macOS shortcuts to terminal equivalents
    // Keys NOT mapped here fall through to app-level actions (Cmd+N, Cmd+K, Cmd+], etc.)
    if cmd {
        return match key {
            "left" => Some(b"\x01".to_vec()), // Cmd+Left  -> Ctrl+A (beginning of line)
            "right" => Some(b"\x05".to_vec()), // Cmd+Right -> Ctrl+E (end of line)
            "backspace" => Some(b"\x15".to_vec()), // Cmd+Backspace -> Ctrl+U (kill line)
            "delete" => Some(b"\x0b".to_vec()), // Cmd+Delete -> Ctrl+K (kill to end)
            _ => None,
        };
    }

    // Alt+arrow: use readline/zsh word-nav sequences (ESC+b / ESC+f)
    // These work universally in macOS shells, unlike xterm CSI modifier sequences.
    if alt && !ctrl && !shift {
        match key {
            "left" => return Some(b"\x1bb".to_vec()),   // backward-word
            "right" => return Some(b"\x1bf".to_vec()),  // forward-word
            "up" => return Some(b"\x1b[1;3A".to_vec()), // keep CSI for up/down
            "down" => return Some(b"\x1b[1;3B".to_vec()),
            _ => {}
        }
    }

    // Arrow keys with modifiers (xterm-style CSI sequences)
    let arrow_suffix = match key {
        "up" => Some("A"),
        "down" => Some("B"),
        "right" => Some("C"),
        "left" => Some("D"),
        _ => None,
    };
    if let Some(suffix) = arrow_suffix {
        let modifier_code = match (shift, alt, ctrl) {
            (false, false, false) => return Some(format!("\x1b[{}", suffix).into_bytes()),
            (true, false, false) => 2,
            (false, true, false) => 3, // fallthrough for non-arrow alt combos
            (true, true, false) => 4,
            (false, false, true) => 5,
            (true, false, true) => 6,
            (false, true, true) => 7,
            (true, true, true) => 8,
        };
        return Some(format!("\x1b[1;{}{}", modifier_code, suffix).into_bytes());
    }

    // Special keys
    match key {
        "enter" => return Some(b"\r".to_vec()),
        "escape" => return Some(b"\x1b".to_vec()),
        "backspace" => {
            if alt {
                return Some(b"\x1b\x7f".to_vec());
            } // Alt+Backspace = delete word
            return Some(b"\x7f".to_vec());
        }
        "tab" => {
            if shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(b"\t".to_vec());
        }
        "space" => {
            if ctrl {
                return Some(b"\x00".to_vec());
            }
            return Some(b" ".to_vec());
        }
        "home" => return Some(b"\x1b[H".to_vec()),
        "end" => return Some(b"\x1b[F".to_vec()),
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        "delete" => return Some(b"\x1b[3~".to_vec()),
        "f1" => return Some(b"\x1bOP".to_vec()),
        "f2" => return Some(b"\x1bOQ".to_vec()),
        "f3" => return Some(b"\x1bOR".to_vec()),
        "f4" => return Some(b"\x1bOS".to_vec()),
        "f5" => return Some(b"\x1b[15~".to_vec()),
        "f6" => return Some(b"\x1b[17~".to_vec()),
        "f7" => return Some(b"\x1b[18~".to_vec()),
        "f8" => return Some(b"\x1b[19~".to_vec()),
        "f9" => return Some(b"\x1b[20~".to_vec()),
        "f10" => return Some(b"\x1b[21~".to_vec()),
        "f11" => return Some(b"\x1b[23~".to_vec()),
        "f12" => return Some(b"\x1b[24~".to_vec()),
        _ => {}
    }

    // Ctrl+letter -> control character
    if ctrl && key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            let ctrl_byte = (ch.to_ascii_uppercase() as u8) - b'@';
            if alt {
                return Some(vec![b'\x1b', ctrl_byte]);
            }
            return Some(vec![ctrl_byte]);
        }
    }

    // Alt+letter -> ESC + char
    if alt && key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii() {
            return Some(vec![b'\x1b', ch as u8]);
        }
    }

    // Regular printable characters
    if !ctrl && !alt {
        if let Some(kc) = &ks.key_char {
            return Some(kc.as_bytes().to_vec());
        }
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii() {
                return Some(vec![ch as u8]);
            }
        }
    }

    None
}

fn open_in_terminal(dir: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg("-a")
            .arg("Terminal")
            .arg(dir)
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        for terminal in &[
            "xdg-terminal-exec",
            "gnome-terminal",
            "konsole",
            "xterm",
            "alacritty",
            "kitty",
            "foot",
        ] {
            if Command::new(terminal).current_dir(dir).spawn().is_ok() {
                return;
            }
        }
        let _ = Command::new("xdg-open").arg(dir).spawn();
    }
}

fn list_subdirs(parent: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                dirs.push(entry.path().display().to_string());
            }
        }
    }
    // Sort: non-hidden first, then hidden, alphabetically within each group
    dirs.sort_by(|a, b| {
        let a_name = std::path::Path::new(a).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let b_name = std::path::Path::new(b).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let a_hidden = a_name.starts_with('.');
        let b_hidden = b_name.starts_with('.');
        match (a_hidden, b_hidden) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => a_name.to_lowercase().cmp(&b_name.to_lowercase()),
        }
    });
    dirs
}

fn list_home_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    list_subdirs(&home)
}

// ── Starfield ───────────────────────────────────────────────────

struct Star {
    x: f32, // 0.0..1.0 normalized
    y: f32,
    size: f32,       // radius in px
    brightness: f32, // base brightness 0.0..1.0
    phase: f32,      // twinkle phase offset
    speed: f32,      // twinkle speed
}

struct Particle {
    x: f32, // absolute px position
    y: f32,
    vx: f32, // velocity px per tick
    vy: f32,
    life: f32,  // 1.0 -> 0.0
    decay: f32, // life lost per tick
    size: f32,
    color: Rgba,
}

fn generate_stars(count: usize, seed: u64) -> Vec<Star> {
    let mut stars = Vec::with_capacity(count);
    // Simple LCG pseudo-random for deterministic star positions
    let mut rng = seed;
    let next = |r: &mut u64| -> f32 {
        *r = r
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*r >> 33) as f32) / (u32::MAX as f32 / 2.0)
    };
    for _ in 0..count {
        let x = next(&mut rng);
        let y = next(&mut rng);
        let size = 0.4 + next(&mut rng) * 1.2; // 0.4 to 1.6px
        let brightness = 0.15 + next(&mut rng) * 0.85;
        let phase = next(&mut rng) * std::f32::consts::TAU;
        let speed = 0.3 + next(&mut rng) * 1.5;
        stars.push(Star {
            x,
            y,
            size,
            brightness,
            phase,
            speed,
        });
    }
    stars
}

// ── Token tracking ─────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub(crate) struct TokenStats {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) context_window: u64,
    pub(crate) max_output_tokens: u64,
    pub(crate) cost_usd: f64,
    pub(crate) model: String,
    pub(crate) thinking_enabled: bool,
    pub(crate) session_id: Option<String>,
}

impl TokenStats {
    fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens
    }

    fn context_usage_pct(&self) -> f32 {
        if self.context_window == 0 {
            return 0.0;
        }
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
    AgentMenu,
    Settings,
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
        OpenSettings,
        ClosePalette,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        SetupNext,   // Tab: next step in setup
        SetupPrev,   // Shift-Tab: previous step
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
        NextPane,
        PrevPane,
        NextGroup,
        PrevGroup,
        CloseTile,
        ToggleChanges,
        ChangesUp,
        ChangesDown,
        ChangesStage,
        ChangesRefresh,
        Quit,
    ]
);

// ── Agent ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Grid,
    Pipeline,
    Focus,
}

impl ViewMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Pipeline => "pipeline",
            Self::Focus => "focus",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "pipeline" => Self::Pipeline,
            "focus" => Self::Focus,
            _ => Self::Grid,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentStatus {
    Working,
    Idle,
    Blocked,
    Starting,
    Interrupted,
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
pub(crate) enum AgentRole {
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
pub(crate) struct MachineTarget {
    pub(crate) name: String,
    pub(crate) ssh_destination: Option<String>,
    pub(crate) workdir: Option<String>,
}

pub(crate) enum AgentMsg {
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
    fn total(&self) -> u32 {
        self.edits + self.reads + self.bash + self.writes + self.other
    }
    fn summary(&self) -> String {
        let total = self.total();
        if total == 0 {
            return String::new();
        }
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
    token_history: Vec<u64>,
    // Animation triggers
    status_changed_at: Instant,
    last_tool_call_at: Option<Instant>,
    spawn_time: Instant,
    delegate_buf: Option<String>,
    // Terminal tile
    is_terminal: bool,
    terminal_entity: Option<Entity<gpui_terminal::TerminalView>>,
    terminal_pty_writer: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    // Worktree isolation
    worktree_info: Option<WorktreeInfo>,
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
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        Self {
            name: name.into(),
            group: group.into(),
            runtime_name: runtime.into(),
            target_machine: "local".into(),
            role: AgentRole::Coordinator,
            status: AgentStatus::Starting,
            output_lines: Vec::new(),
            input_buffer: String::new(),
            input_cursor: 0,
            message_count: 0,
            scroll_offset: 0,
            session_id: None,
            prompt_tx: None,
            _reader_task: None,
            tokens,
            runtime_info,
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
            token_history: Vec::new(),
            status_changed_at: Instant::now(),
            last_tool_call_at: None,
            spawn_time: Instant::now(),
            delegate_buf: None,
            is_terminal: false,
            terminal_entity: None,
            terminal_pty_writer: None,
            worktree_info: None,
        }
    }
}

// ── Setup wizard state ──────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Runtime,
    Machine,
    Directory,
}

struct SetupState {
    step: SetupStep,
    // Runtime step
    runtime_cursor: usize,
    selected_runtime: String,
    // Machine step
    machine_cursor: usize,
    selected_machine: String,
    // Directory step
    show_recent: bool,
    recent_cursor: usize,
    dir_filter: String,
    dir_entries: Vec<String>,
    dir_cursor: usize,
    selected_dir: String,
    // For editing existing agent
    editing_agent: Option<usize>,
    worktree_mode: bool,
    worktree_branch: String,
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

struct PaletteItem {
    label: String,
    action: PaletteAction,
}
enum PaletteAction {
    NewAgent,
    NewGroup,
    SetTheme(String),
    SetView(ViewMode),
    KillCurrent,
    CompactContext,
    ToggleSidebarTab,
    ToggleCautiousEnter,
    ToggleTerminalText,
    SetBgOpacity(f32),
    SetBgBlur(f32),
    Quit,
}

// ── Agent Menu (per-agent action dropdown) ──────────────────────

struct AgentMenuItem {
    label: String,
    desc: String,
    command: AgentMenuCommand,
}

#[derive(Clone)]
enum AgentMenuCommand {
    /// Send a slash command string to the agent
    SlashCommand(String),
    /// Toggle auto-scroll
    ToggleAutoScroll,
    /// Toggle favorite
    ToggleFavorite,
    /// Copy last response to clipboard
    CopyLastResponse,
}

fn build_agent_menu(runtime: &str) -> Vec<AgentMenuItem> {
    let mut items = Vec::new();

    // Context management
    let compact_cmd = match runtime {
        "cursor" => "/compress",
        _ => "/compact",
    };
    items.push(AgentMenuItem {
        label: "Compact context".into(),
        desc: "Summarize conversation to free tokens".into(),
        command: AgentMenuCommand::SlashCommand(compact_cmd.into()),
    });

    let clear_cmd = match runtime {
        "cursor" => "/new-chat",
        _ => "/clear",
    };
    items.push(AgentMenuItem {
        label: "Clear context".into(),
        desc: "Reset conversation history".into(),
        command: AgentMenuCommand::SlashCommand(clear_cmd.into()),
    });

    // Cost/usage
    let cost_cmd = match runtime {
        "claude" => Some("/cost"),
        "cursor" => Some("/usage"),
        _ => None,
    };
    if let Some(cmd) = cost_cmd {
        items.push(AgentMenuItem {
            label: "Show cost / usage".into(),
            desc: "Display token usage and spending".into(),
            command: AgentMenuCommand::SlashCommand(cmd.into()),
        });
    }

    // Effort / thinking
    match runtime {
        "claude" => {
            items.push(AgentMenuItem {
                label: "Effort: high".into(),
                desc: "Set thinking effort to high".into(),
                command: AgentMenuCommand::SlashCommand("/effort high".into()),
            });
            items.push(AgentMenuItem {
                label: "Effort: low".into(),
                desc: "Set thinking effort to low".into(),
                command: AgentMenuCommand::SlashCommand("/effort low".into()),
            });
        }
        "cursor" => {
            items.push(AgentMenuItem {
                label: "Toggle max mode".into(),
                desc: "Enable/disable max mode for extended thinking".into(),
                command: AgentMenuCommand::SlashCommand("/max-mode".into()),
            });
        }
        _ => {}
    }

    // Plan mode
    match runtime {
        "claude" | "cursor" => {
            items.push(AgentMenuItem {
                label: "Plan mode".into(),
                desc: "Switch to planning mode (read-only analysis)".into(),
                command: AgentMenuCommand::SlashCommand("/plan".into()),
            });
        }
        _ => {}
    }

    // Export
    if runtime == "claude" {
        items.push(AgentMenuItem {
            label: "Export conversation".into(),
            desc: "Save conversation to file".into(),
            command: AgentMenuCommand::SlashCommand("/export".into()),
        });
        items.push(AgentMenuItem {
            label: "Context visualization".into(),
            desc: "Show context window usage breakdown".into(),
            command: AgentMenuCommand::SlashCommand("/context".into()),
        });
        items.push(AgentMenuItem {
            label: "Diagnostics".into(),
            desc: "Run /doctor to check installation".into(),
            command: AgentMenuCommand::SlashCommand("/doctor".into()),
        });
    }

    if runtime == "cursor" {
        items.push(AgentMenuItem {
            label: "Ask mode (read-only)".into(),
            desc: "Switch to ask mode -- no code execution".into(),
            command: AgentMenuCommand::SlashCommand("/ask".into()),
        });
    }

    // Universal actions
    items.push(AgentMenuItem {
        label: "Toggle auto-scroll".into(),
        desc: "Pin/unpin transcript scroll position".into(),
        command: AgentMenuCommand::ToggleAutoScroll,
    });
    items.push(AgentMenuItem {
        label: "Toggle favorite".into(),
        desc: "Star/unstar this agent".into(),
        command: AgentMenuCommand::ToggleFavorite,
    });
    items.push(AgentMenuItem {
        label: "Copy last response".into(),
        desc: "Copy agent's last response to clipboard".into(),
        command: AgentMenuCommand::CopyLastResponse,
    });

    items
}

// ── Search ─────────────────────────────────────────────────────

struct SearchResult {
    agent_idx: usize,
    agent_name: String,
    line_idx: usize,
    line: String,
}

// ── Diff classification (re-exported from lib.rs for tests) ─────
use opensquirrel::{
    DiffSummary, LineKind, Span, classify_line, extract_latest_turn_output, parse_bullet,
    parse_code_fence, parse_heading, parse_spans, summarize_diff,
};

// ── Groups ──────────────────────────────────────────────────────

struct Group {
    name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Agents,
    Workers,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    Appearance,
    Behavior,
    Runtimes,
    Machines,
    Mcps,
    Keybinds,
}

impl SettingsSection {
    const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Behavior,
        Self::Runtimes,
        Self::Machines,
        Self::Mcps,
        Self::Keybinds,
    ];

    fn label(&self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Behavior => "Behavior",
            Self::Runtimes => "Runtimes",
            Self::Machines => "Machines",
            Self::Mcps => "MCPs",
            Self::Keybinds => "Keybinds",
        }
    }
}

#[derive(Clone)]
struct SettingsState {
    section: SettingsSection,
    item_cursor: usize,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            section: SettingsSection::Appearance,
            item_cursor: 0,
        }
    }
}

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
    // Settings screen
    settings: SettingsState,
    // Search
    search_query: String,
    search_results: Vec<SearchResult>,
    search_selection: usize,
    // Agent menu (per-agent action dropdown)
    agent_menu_items: Vec<AgentMenuItem>,
    agent_menu_selection: usize,
    agent_menu_target: usize, // which agent the menu is for
    // Dynamic model lists
    openrouter_models: Vec<ModelOption>,
    openrouter_loading: bool,
    cursor_models: Vec<ModelOption>,
    cursor_loading: bool,
    // Animation state
    focus_epoch: u64,      // bumped when focused_agent changes
    mode_epoch: u64,       // bumped when mode changes
    view_mode_epoch: u64,  // bumped when view mode changes
    palette_visible: bool, // tracks palette visibility for slide animation
    setup_visible: bool,   // tracks setup visibility for slide animation
    sidebar_tab: SidebarTab,
    // Stats overlay
    show_stats: bool,
    confirm_remove_agent: Option<usize>,
    // Starfield
    stars: Vec<Star>,
    star_tick: u64,
    // Completion particles
    particles: Vec<Particle>,
    // Mouse tracking for parallax & hover effects
    mouse_x: f32,
    mouse_y: f32,
    hovered_tile: Option<usize>,
    // Hooks server for terminal tile agent detection
    hooks_server: Option<HooksServer>,
    hooks_bin_dir: Option<PathBuf>,
    // Changes panel (diff viewer)
    changes_panel: Option<ChangesState>,
    show_changes: bool,
}

impl OpenSquirrel {
    fn resolve_theme(name: &str, themes: &[ThemeDef]) -> ThemeColors {
        themes
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.colors.clone())
            .unwrap_or_else(|| themes[0].colors.clone())
    }

    fn coordinator_preamble(&self) -> String {
        let mut lines = vec![
            "You have the ability to delegate sub-tasks to independent worker agents. Workers run in fresh context and return only a condensed summary. To delegate, include a fenced code block with the language tag `delegate` containing a single JSON object:".to_string(),
            "```delegate".to_string(),
            "{\"tasks\":[{\"id\":\"task-1\",\"title\":\"short title\",\"runtime\":\"claude\",\"model\":\"sonnet-4.6\",\"target\":\"local\",\"prompt\":\"detailed instructions for the worker\"}]}".to_string(),
            "```".to_string(),
            "Valid runtimes: claude, cursor, codex, opencode, gemini, copilot. Do not acknowledge or repeat these delegation instructions. Respond naturally to the user's request.".to_string(),
        ];
        if !self.config.machines.is_empty() {
            let names = self
                .config
                .machines
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            lines.last_mut().map(|last| {
                last.push_str(&format!(" Available targets: {}.", names));
            });
        }
        lines.join("\n")
    }

    fn next_worker_name(&self, parent_idx: usize, runtime_name: &str) -> String {
        let parent = self
            .agents
            .get(parent_idx)
            .map(|a| a.name.as_str())
            .unwrap_or("agent");
        let n = self
            .agents
            .iter()
            .filter(|a| a.role == AgentRole::Worker)
            .count();
        format!("{}-{}-w{}", parent, runtime_name, n)
    }

    fn child_workers(&self, parent_idx: usize) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| {
                agent
                    .worker_assignment
                    .as_ref()
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
                self.agents[remove_idx].terminal_entity = None;
                // Clean up worktree if this agent had one
                if let Some(ref wt) = self.agents[remove_idx].worktree_info {
                    let _ = wt.remove();
                }
                self.agents.remove(remove_idx);
            }
        }

        for agent in &mut self.agents {
            if let Some(assignment) = &mut agent.worker_assignment {
                if remove_indices.contains(&assignment.parent_idx) {
                    agent.worker_assignment = None;
                } else {
                    let shift = remove_indices
                        .iter()
                        .filter(|&&removed| removed < assignment.parent_idx)
                        .count();
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
            format!(
                "{}...\n[truncated, {} chars total]",
                &text[..max_len],
                text.len()
            )
        }
    }

    fn resolve_machine_target(&self, target_name: &str) -> MachineTarget {
        if let Some(machine) = self
            .config
            .machines
            .iter()
            .find(|machine| machine.name == target_name)
        {
            if machine.kind == "ssh" {
                let destination = if machine.user.is_empty() {
                    machine.host.clone()
                } else {
                    format!("{}@{}", machine.user, machine.host)
                };
                return MachineTarget {
                    name: machine.name.clone(),
                    ssh_destination: if destination.is_empty() {
                        None
                    } else {
                        Some(destination)
                    },
                    workdir: if machine.workdir.is_empty() {
                        None
                    } else {
                        Some(machine.workdir.clone())
                    },
                };
            }
            return MachineTarget {
                name: machine.name.clone(),
                ssh_destination: None,
                workdir: if machine.workdir.is_empty() {
                    None
                } else {
                    Some(machine.workdir.clone())
                },
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
        let groups: Vec<Group> = config
            .groups
            .iter()
            .map(|n| Group { name: n.clone() })
            .collect();
        let themes = builtin_themes();
        let theme = Self::resolve_theme(&config.theme, &themes);

        let mut app = Self {
            mode: Mode::Normal,
            agents: Vec::new(),
            groups: if groups.is_empty() {
                vec![Group {
                    name: "default".into(),
                }]
            } else {
                groups
            },
            focused_group: 0,
            focused_agent: 0,
            focus_handle: cx.focus_handle(),
            ui_scale: 1.0,
            view_mode: ViewMode::from_str(&config.default_view_mode),
            config: config.clone(),
            theme,
            themes,
            font_family: config.font_family.clone(),
            font_size: config.font_size,
            palette_input: String::new(),
            palette_items: Vec::new(),
            palette_selection: 0,
            setup: None,
            settings: SettingsState::default(),
            search_query: String::new(),
            search_results: Vec::new(),
            search_selection: 0,
            agent_menu_items: Vec::new(),
            agent_menu_selection: 0,
            agent_menu_target: 0,
            openrouter_models: Vec::new(),
            openrouter_loading: false,
            cursor_models: Vec::new(),
            cursor_loading: false,
            focus_epoch: 0,
            mode_epoch: 0,
            view_mode_epoch: 0,
            palette_visible: false,
            setup_visible: false,
            sidebar_tab: SidebarTab::Agents,
            show_stats: false,
            confirm_remove_agent: None,
            stars: generate_stars(200, 0xDEADBEEF42),
            star_tick: 0,
            particles: Vec::new(),
            mouse_x: 0.5,
            mouse_y: 0.5,
            hovered_tile: None,
            hooks_server: None,
            hooks_bin_dir: None,
            changes_panel: None,
            show_changes: false,
        };

        // Restore from saved state, or create fresh agent
        if let Some(saved) = SavedAppState::load() {
            // Restore groups
            if !saved.groups.is_empty() {
                app.groups = saved
                    .groups
                    .iter()
                    .map(|n| Group { name: n.clone() })
                    .collect();
            }
            app.view_mode = ViewMode::from_str(&saved.view_mode);
            app.sidebar_tab = match saved.sidebar_tab.as_str() {
                "swarms" | "workers" => SidebarTab::Workers,
                _ => SidebarTab::Agents,
            };
            app.ui_scale = saved.ui_scale;

            // Restore each agent
            if !saved.agents.is_empty() {
                for sa in &saved.agents {
                    let model = if sa.model.is_empty() {
                        None
                    } else {
                        Some(sa.model.as_str())
                    };
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
                        if sa.working_dir.is_empty() {
                            None
                        } else {
                            Some(sa.working_dir.as_str())
                        },
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
                        edits: sa.tool_edits,
                        reads: sa.tool_reads,
                        bash: sa.tool_bash,
                        writes: sa.tool_writes,
                        other: sa.tool_other,
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
                        a.restore_notice =
                            Some("[restored session -- send a message to reconnect]".into());
                    }
                }
                let name_to_idx: HashMap<String, usize> = app
                    .agents
                    .iter()
                    .enumerate()
                    .map(|(idx, agent)| (agent.name.clone(), idx))
                    .collect();
                for (idx, sa) in saved.agents.iter().enumerate() {
                    if let Some(parent_name) = &sa.parent_name {
                        if let Some(&parent_idx) = name_to_idx.get(parent_name) {
                            app.agents[idx].worker_assignment = Some(WorkerAssignment {
                                parent_idx,
                                task_id: sa
                                    .task_id
                                    .clone()
                                    .unwrap_or_else(|| format!("restored-{}", idx)),
                                task_title: sa
                                    .task_title
                                    .clone()
                                    .unwrap_or_else(|| "restored worker".into()),
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
                                    saved.agents[idx].remote_line_cursor, session_name
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
                let model = app
                    .config
                    .runtimes
                    .iter()
                    .find(|r| r.name == rt)
                    .map(|r| r.last_model.clone());
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
                    None,
                    cx,
                );
            }
        } else {
            // No saved state, create fresh
            let rt = app.config.last_runtime.clone();
            let model = app
                .config
                .runtimes
                .iter()
                .find(|r| r.name == rt)
                .map(|r| r.last_model.clone());
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
                None,
                cx,
            );
        }
        // Start hooks server for agent lifecycle detection in terminal tiles
        let hooks_server = HooksServer::start();
        let runtime_names: Vec<&str> = app
            .config
            .runtimes
            .iter()
            .filter(|r| !r.command.is_empty() && r.name != "terminal")
            .map(|r| r.command.as_str())
            .collect();
        app.hooks_bin_dir = crate::hooks::generate_wrapper_scripts(&runtime_names).ok();
        app.hooks_server = Some(hooks_server);

        // Starfield twinkle timer -- ~30fps update
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let Ok(()) = this.update(cx, |view, cx| {
                    view.star_tick = view.star_tick.wrapping_add(1);
                    let has_particles = !view.particles.is_empty();
                    if has_particles {
                        view.update_particles();
                    }
                    // Poll hooks server for agent lifecycle events
                    if let Some(ref server) = view.hooks_server {
                        for event in server.drain_events() {
                            match event {
                                crate::hooks::HookEvent::Start { pane_id, .. } => {
                                    if let Some(a) =
                                        view.agents.iter_mut().find(|a| a.name == pane_id)
                                    {
                                        a.status = AgentStatus::Working;
                                        a.status_changed_at = Instant::now();
                                    }
                                }
                                crate::hooks::HookEvent::Stop { pane_id } => {
                                    if let Some(a) =
                                        view.agents.iter_mut().find(|a| a.name == pane_id)
                                    {
                                        a.status = AgentStatus::Idle;
                                        a.status_changed_at = Instant::now();
                                    }
                                }
                            }
                        }
                    }
                    // Notify if starfield visible or particles active or any agent working (for pulse)
                    let any_working = view.agents.iter().any(|a| a.status == AgentStatus::Working);
                    if view.config.theme == "ops" || has_particles || any_working {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();

        app
    }

    fn save_config(&mut self) {
        self.config.groups = self.groups.iter().map(|g| g.name.clone()).collect();
        self.config.save();
    }

    fn save_recent_project(&mut self, path: &str, machine: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.config.recent_projects.retain(|p| p.path != path);
        self.config.recent_projects.insert(
            0,
            RecentProject {
                path: path.into(),
                last_used: now,
                machine: machine.into(),
            },
        );
        self.config
            .recent_projects
            .sort_by(|a, b| b.last_used.cmp(&a.last_used));
        if self.config.recent_projects.len() > 20 {
            self.config.recent_projects.truncate(20);
        }
        self.config.save();
    }

    fn save_state(&self) {
        let agents: Vec<SavedAgentState> = self
            .agents
            .iter()
            .map(|a| SavedAgentState {
                name: a.name.clone(),
                group: a.group.clone(),
                runtime_name: a.runtime_name.clone(),
                target_machine: a.target_machine.clone(),
                role: a.role.as_str().into(),
                parent_name: a
                    .worker_assignment
                    .as_ref()
                    .and_then(|assignment| self.agents.get(assignment.parent_idx))
                    .map(|parent| parent.name.clone()),
                task_id: a
                    .worker_assignment
                    .as_ref()
                    .map(|assignment| assignment.task_id.clone()),
                task_title: a
                    .worker_assignment
                    .as_ref()
                    .map(|assignment| assignment.task_title.clone()),
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
            })
            .collect();
        let state = SavedAppState {
            agents,
            groups: self.groups.iter().map(|g| g.name.clone()).collect(),
            focused_group: self.focused_group,
            focused_agent: self.focused_agent,
            view_mode: self.view_mode.as_str().into(),
            ui_scale: self.ui_scale,
            sidebar_tab: match self.sidebar_tab {
                SidebarTab::Agents => "agents",
                SidebarTab::Workers => "workers",
            }
            .into(),
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
        self.config
            .runtimes
            .iter()
            .find(|r| r.name == runtime_name)
            .map(|r| r.models.clone())
            .unwrap_or_default()
    }

    fn fetch_opencode_models(&mut self, cx: &mut Context<Self>) {
        if self.openrouter_loading || !self.openrouter_models.is_empty() {
            return;
        }
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
                    })
                    .ok();
                })
                .ok();
            }
        })
        .detach();
    }

    fn fetch_cursor_models(&mut self, cx: &mut Context<Self>) {
        if self.cursor_loading || !self.cursor_models.is_empty() {
            return;
        }
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
                    })
                    .ok();
                })
                .ok();
            }
        })
        .detach();
    }

    // ── Agent lifecycle ─────────────────────────────────────────

    fn filter_dirs(entries: &[String], filter: &str) -> Vec<String> {
        if filter.is_empty() {
            return entries.to_vec();
        }
        let q: Vec<char> = filter.to_lowercase().chars().collect();
        let mut scored: Vec<(usize, String)> = entries
            .iter()
            .filter_map(|d| {
                // Fuzzy match against basename
                let basename = std::path::Path::new(d)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| d.clone());
                let lower = basename.to_lowercase();
                let mut qi = 0;
                let mut score = 0usize;
                for (ci, c) in lower.chars().enumerate() {
                    if qi < q.len() && c == q[qi] {
                        // Bonus for consecutive matches and start-of-word
                        if qi > 0 && ci > 0 {
                            score += 1;
                        }
                        if ci == 0 {
                            score += 10; // start of name bonus
                        }
                        qi += 1;
                    }
                }
                if qi == q.len() {
                    Some((score, d.clone()))
                } else {
                    None
                }
            })
            .collect();
        // Sort by score descending (best matches first)
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, d)| d).collect()
    }

    fn resolve_setup_directory(selected_dir: &str) -> (String, Vec<String>) {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let mut resolved = if selected_dir.is_empty() {
            home.clone()
        } else {
            selected_dir.to_string()
        };
        if !std::path::Path::new(&resolved).is_dir() {
            resolved = home;
        }
        let entries = list_subdirs(&resolved);
        (resolved, entries)
    }

    fn time_ago(last_used: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let delta = now.saturating_sub(last_used);
        if delta < 60 {
            "just now".into()
        } else if delta < 3_600 {
            format!("{}m ago", delta / 60)
        } else if delta < 86_400 {
            format!("{}h ago", delta / 3_600)
        } else {
            format!("{}d ago", delta / 86_400)
        }
    }

    fn selected_mcp_defs(&self) -> Vec<McpDef> {
        self.config
            .mcps
            .iter()
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
        initial_workdir: Option<&str>,
        cx: &mut Context<Self>,
    ) -> usize {
        let agent_idx = self.agents.len();
        self.agents.push(AgentState::new(name, group, runtime_name));
        let mut machine_target = self.resolve_machine_target(target_machine);
        if machine_target.ssh_destination.is_none() {
            if let Some(wd) = initial_workdir.filter(|s| !s.is_empty()) {
                machine_target.workdir = Some(wd.to_string());
            }
        }
        if let Some(wd) = initial_workdir.filter(|s| !s.is_empty()) {
            self.agents[agent_idx].working_dir = wd.to_string();
        }
        let remote_session_name = if machine_target.ssh_destination.is_some() {
            remote_session_name_override.or_else(|| Some(make_tmux_session_name(name)))
        } else {
            None
        };
        self.agents[agent_idx].target_machine = machine_target.name.clone();
        self.agents[agent_idx].role = role;
        self.agents[agent_idx].prompt_preamble = prompt_preamble.clone();
        self.agents[agent_idx].worker_assignment = worker_assignment;
        self.agents[agent_idx].remote_session_name = remote_session_name.clone();

        // Terminal tile: spawn a PTY instead of an agent subprocess
        if runtime_name == "terminal" {
            self.agents[agent_idx].is_terminal = true;
            self.agents[agent_idx].status = AgentStatus::Idle;
            let working_dir = self.agents[agent_idx].working_dir.clone();

            let pty_system = native_pty_system();
            let pty_pair = pty_system.openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            });
            match pty_pair {
                Ok(pair) => {
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
                    let mut cmd = CommandBuilder::new(&shell);
                    cmd.env("TERM", "xterm-256color");
                    cmd.env("COLORTERM", "truecolor");
                    // Set hooks env vars so agent wrappers can report lifecycle
                    if let Some(ref server) = self.hooks_server {
                        if let Some(ref bin_dir) = self.hooks_bin_dir {
                            for (k, v) in crate::hooks::hook_env_vars(
                                server.port(),
                                name,
                                &working_dir,
                                bin_dir,
                            ) {
                                cmd.env(&k, &v);
                            }
                        }
                    }
                    if !working_dir.is_empty() {
                        cmd.cwd(&working_dir);
                    }
                    match pair.slave.spawn_command(cmd) {
                        Ok(_child) => {
                            let writer = pair.master.take_writer().expect("PTY writer");
                            let reader = pair.master.try_clone_reader().expect("PTY reader");
                            let pty_master =
                                std::sync::Arc::new(std::sync::Mutex::new(pair.master));
                            let pty_master_resize = pty_master.clone();

                            // Shared writer for paste support
                            let shared_writer: Arc<
                                std::sync::Mutex<Box<dyn std::io::Write + Send>>,
                            > = Arc::new(std::sync::Mutex::new(writer));
                            let writer_for_tv = SharedWriter(shared_writer.clone());

                            let font_family = self.font_family.clone();
                            let font_size = self.font_size;
                            let t = &self.theme;
                            let bg = t.bg();
                            let fg = t.text();

                            let config = gpui_terminal::TerminalConfig {
                                cols: 80,
                                rows: 24,
                                font_family: font_family.clone(),
                                font_size: px(font_size),
                                line_height_multiplier: 1.2,
                                scrollback: 10000,
                                padding: Edges {
                                    top: px(4.0),
                                    bottom: px(4.0),
                                    left: px(8.0),
                                    right: px(8.0),
                                },
                                colors: gpui_terminal::ColorPaletteBuilder::new()
                                    .background(
                                        (bg.r * 255.0) as u8,
                                        (bg.g * 255.0) as u8,
                                        (bg.b * 255.0) as u8,
                                    )
                                    .foreground(
                                        (fg.r * 255.0) as u8,
                                        (fg.g * 255.0) as u8,
                                        (fg.b * 255.0) as u8,
                                    )
                                    .build(),
                            };

                            let (exit_tx, exit_rx) = async_channel::bounded::<()>(1);
                            let terminal_entity = cx.new(|cx| {
                                gpui_terminal::TerminalView::new(writer_for_tv, reader, config, cx)
                                    .with_key_handler(|_event| true) // We handle keys in handle_key_down
                                    .with_resize_callback(move |cols, rows| {
                                        if let Ok(master) = pty_master_resize.lock() {
                                            let _ = master.resize(PtySize {
                                                cols: cols as u16,
                                                rows: rows as u16,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                            });
                                        }
                                    })
                                    .with_exit_callback(move |_window, _cx| {
                                        let _ = exit_tx.try_send(());
                                    })
                            });
                            self.agents[agent_idx].terminal_entity = Some(terminal_entity);
                            self.agents[agent_idx].terminal_pty_writer = Some(shared_writer);

                            // Listen for shell exit and remove the tile
                            let exit_idx = agent_idx;
                            let task = cx.spawn(
                                async move |this: WeakEntity<OpenSquirrel>, cx: &mut AsyncApp| {
                                    let _ = exit_rx.recv().await;
                                    let _ = this.update(cx, |view, cx| {
                                        if exit_idx < view.agents.len()
                                            && view.agents[exit_idx].is_terminal
                                        {
                                            view.remove_agent_and_dependents(exit_idx);
                                            view.clamp_focus();
                                        }
                                        cx.notify();
                                    });
                                },
                            );
                            self.agents[agent_idx]._reader_task = Some(task);
                        }
                        Err(e) => {
                            self.agents[agent_idx]
                                .output_lines
                                .push(format!("[terminal] failed to spawn shell: {}", e));
                            self.agents[agent_idx].status = AgentStatus::Idle;
                        }
                    }
                }
                Err(e) => {
                    self.agents[agent_idx]
                        .output_lines
                        .push(format!("[terminal] failed to open PTY: {}", e));
                    self.agents[agent_idx].status = AgentStatus::Idle;
                }
            }
            self.focused_agent = agent_idx;
            cx.notify();
            return agent_idx;
        }

        let (msg_tx, msg_rx) = async_channel::unbounded::<AgentMsg>();
        let (prompt_tx, prompt_rx) = mpsc::channel::<String>();
        self.agents[agent_idx].prompt_tx = Some(prompt_tx);

        let runtime = if let Some(runtime) = self
            .config
            .runtimes
            .iter()
            .find(|r| r.name == runtime_name)
            .cloned()
        {
            runtime
        } else if let Some(runtime) = self.config.runtimes.first().cloned() {
            runtime
        } else {
            self.agents[agent_idx]
                .output_lines
                .push("[setup] no runtimes configured".into());
            self.agents[agent_idx].status = AgentStatus::Idle;
            cx.notify();
            return agent_idx;
        };

        let model_override = model.map(String::from);
        let mcps = self.selected_mcp_defs();

        if let Some(ref m) = model_override {
            self.agents[agent_idx].tokens.model = m.clone();
        }
        self.agents[agent_idx].last_model = model_override.clone();

        // For Claude coordinators, pass the delegation preamble as a system prompt
        // instead of prepending to user messages. Other runtimes still use the
        // prompt_preamble prepend approach since they lack --append-system-prompt.
        let system_prompt = if role == AgentRole::Coordinator && runtime.name == "claude" {
            prompt_preamble.clone()
        } else {
            None
        };

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
                system_prompt,
            );
        });

        let task = cx.spawn(
            async move |this: WeakEntity<OpenSquirrel>, cx: &mut AsyncApp| {
                while let Ok(msg) = msg_rx.recv().await {
                    let idx = agent_idx;
                    let ok = this.update(cx, |view, cx| {
                        if idx >= view.agents.len() {
                            return false;
                        }

                        let mut pending_delegate: Option<DelegateRequest> = None;
                        let mut worker_done = false;
                        let mut turn_done = false;

                        let prev_status = view.agents[idx].status;
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
                                if a.role == AgentRole::Coordinator
                                    && trimmed == "```delegate"
                                    && a.delegate_buf.is_none()
                                {
                                    a.delegate_buf = Some(String::new());
                                } else if trimmed == "```" && a.delegate_buf.is_some() {
                                    let json_str = a.delegate_buf.take().unwrap_or_default();
                                    match serde_json::from_str::<DelegateRequest>(&json_str) {
                                        Ok(request) => pending_delegate = Some(request),
                                        Err(_) => a
                                            .output_lines
                                            .push("[delegate] failed to parse JSON".into()),
                                    }
                                } else if let Some(ref mut buf) = a.delegate_buf {
                                    buf.push_str(&l);
                                    buf.push('\n');
                                }
                                a.output_lines.push(l);
                                if a.auto_scroll {
                                    let len = a.output_lines.len();
                                    if len > 40 {
                                        a.scroll_offset = len - 40;
                                    }
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
                                if let Some(id) = session_id {
                                    a.session_id = Some(id);
                                }
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
                                if lname.contains("edit") {
                                    a.tool_calls.edits += 1;
                                } else if lname.contains("bash")
                                    || lname.contains("shell")
                                    || lname.contains("command")
                                {
                                    a.tool_calls.bash += 1;
                                } else if lname.contains("read")
                                    || lname.contains("glob")
                                    || lname.contains("grep")
                                {
                                    a.tool_calls.reads += 1;
                                } else if lname.contains("write") {
                                    a.tool_calls.writes += 1;
                                } else {
                                    a.tool_calls.other += 1;
                                }
                            }
                            AgentMsg::TokenUpdate(stats) => {
                                if stats.input_tokens > 0
                                    || stats.output_tokens > 0
                                    || stats.cost_usd > 0.0
                                {
                                    a.tokens.input_tokens = stats.input_tokens;
                                    a.tokens.output_tokens = stats.output_tokens;
                                    a.tokens.cache_read_tokens = stats.cache_read_tokens;
                                    a.tokens.cache_write_tokens = stats.cache_write_tokens;
                                    a.tokens.cost_usd = stats.cost_usd;
                                }
                                if stats.context_window > 0 {
                                    a.tokens.context_window = stats.context_window;
                                }
                                if stats.max_output_tokens > 0 {
                                    a.tokens.max_output_tokens = stats.max_output_tokens;
                                }
                                if !stats.model.is_empty() {
                                    a.tokens.model = stats.model;
                                }
                                a.tokens.thinking_enabled = stats.thinking_enabled;
                                // Record token history snapshot
                                let total = a.tokens.total_tokens();
                                if total > 0 {
                                    if a.token_history.len() >= 30 {
                                        a.token_history.remove(0);
                                    }
                                    a.token_history.push(total);
                                }
                                if stats.session_id.is_some() {
                                    a.session_id = stats.session_id.clone();
                                    a.tokens.session_id = stats.session_id;
                                }
                            }
                            AgentMsg::RemoteCursor(cursor) => {
                                a.remote_line_cursor = cursor;
                            }
                        }

                        // macOS notification on status transitions
                        let new_status = view.agents[idx].status;
                        if prev_status == AgentStatus::Working && new_status == AgentStatus::Idle {
                            let name = view.agents[idx].name.clone();
                            std::thread::spawn(move || {
                                let _ = Command::new("osascript")
                                    .args(["-e", &format!(
                                        "display notification \"{}\" with title \"OpenSquirrel\" subtitle \"Agent finished\"",
                                        name
                                    )])
                                    .output();
                            });
                        } else if new_status == AgentStatus::Blocked && prev_status != AgentStatus::Blocked {
                            let name = view.agents[idx].name.clone();
                            std::thread::spawn(move || {
                                let _ = Command::new("osascript")
                                    .args(["-e", &format!(
                                        "display notification \"{}\" with title \"OpenSquirrel\" subtitle \"Agent needs attention\"",
                                        name
                                    )])
                                    .output();
                            });
                        }

                        // Spawn completion particles when transitioning from Working
                        if prev_status == AgentStatus::Working
                            && view.agents[idx].status != AgentStatus::Working
                        {
                            let rt_color = view.theme.runtime_color(&view.agents[idx].runtime_name);
                            // Approximate tile center -- use window midpoint as fallback
                            // The actual position will be refined in render, but for now
                            // we use a rough estimate based on agent index
                            let vis = view.agents_in_current_group();
                            let pos_in_vis = vis.iter().position(|&i| i == idx).unwrap_or(0);
                            let tile_count = vis.len().max(1) as f32;
                            let tile_frac = (pos_in_vis as f32 + 0.5) / tile_count;
                            // Use approximate window dimensions (will be painted absolute)
                            let approx_x = tile_frac * 1400.0;
                            let approx_y = 200.0;
                            view.spawn_completion_particles(approx_x, approx_y, rt_color);
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
                    if !matches!(ok, Ok(true)) {
                        break;
                    }
                }
            },
        );
        self.agents[agent_idx]._reader_task = Some(task);
        self.save_state();
        agent_idx
    }

    fn handle_delegate_request(
        &mut self,
        coordinator_idx: usize,
        request: DelegateRequest,
        cx: &mut Context<Self>,
    ) {
        if coordinator_idx >= self.agents.len() || request.tasks.is_empty() {
            return;
        }

        let group = self.agents[coordinator_idx].group.clone();
        self.agents[coordinator_idx].output_lines.push(format!(
            "[delegate] spawning {} worker(s)",
            request.tasks.len()
        ));

        for task in request.tasks {
            let target_machine = task.target.clone().unwrap_or_else(|| "local".into());
            let worker_name = self.next_worker_name(coordinator_idx, &task.runtime);
            let assignment = WorkerAssignment {
                parent_idx: coordinator_idx,
                task_id: task.id.clone(),
                task_title: task.title.clone(),
            };
            let coord_wd = self.agents[coordinator_idx].working_dir.clone();
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
                if coord_wd.is_empty() {
                    None
                } else {
                    Some(coord_wd.as_str())
                },
                cx,
            );
            self.agents[coordinator_idx].output_lines.push(format!(
                "[delegate] -> {} [{} @ {}] {}",
                worker_name, task.runtime, target_machine, task.title
            ));
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
            if worker.status == AgentStatus::Blocked {
                "failed"
            } else {
                "success"
            },
            worker.tokens.input_tokens,
            worker.tokens.output_tokens,
            worker.tokens.cost_usd,
            tool_summary,
            diff_summary,
            output,
        )
    }

    fn handle_delegated_worker_done(&mut self, worker_idx: usize, cx: &mut Context<Self>) {
        let assignment = match self
            .agents
            .get(worker_idx)
            .and_then(|agent| agent.worker_assignment.clone())
        {
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

        self.agents[assignment.parent_idx]
            .output_lines
            .push(format!(
                "[delegate] <- {} completed {}",
                worker_name, assignment.task_id
            ));
        self.send_prompt(assignment.parent_idx, handoff, cx);
    }

    fn send_prompt(&mut self, idx: usize, prompt: String, cx: &mut Context<Self>) {
        if idx >= self.agents.len() {
            return;
        }
        let a = &mut self.agents[idx];
        a.status = AgentStatus::Working;
        a.turn_state = TurnState::Running;
        a.turn_started = Some(Instant::now());
        a.pending_prompt = Some(prompt.clone());
        a.message_count += 1;
        a.output_lines.push(format!("> {}", prompt));
        a.output_lines.push(String::new());
        cx.notify();
        // For non-Claude runtimes, prepend the delegation preamble to the first
        // user message (they don't support --append-system-prompt).
        // Claude coordinators get it via --append-system-prompt in the agent thread.
        let is_claude = a.runtime_name == "claude";
        let prompt = if let Some(ref preamble) = a.prompt_preamble {
            if a.message_count == 1 && !is_claude {
                format!("{}\n\n{}", preamble, prompt)
            } else {
                prompt
            }
        } else {
            prompt
        };
        let msg = if let Some(ref sid) = a.session_id {
            format!("SESSION:{}\n{}", sid, prompt)
        } else {
            prompt
        };
        if let Some(tx) = &a.prompt_tx {
            let _ = tx.send(msg);
        }
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
        self.groups
            .get(self.focused_group)
            .map(|g| g.name.as_str())
            .unwrap_or("default")
    }

    fn agents_in_current_group(&self) -> Vec<usize> {
        let gn = self.current_group_name().to_string();
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, a)| a.group == gn)
            .map(|(i, _)| i)
            .collect()
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
        if vis.is_empty() {
            self.focused_agent = 0;
        } else if !vis.contains(&self.focused_agent) {
            self.set_focus(vis[0]);
        }
    }

    fn set_theme(&mut self, name: &str) {
        self.theme = Self::resolve_theme(name, &self.themes);
        self.config.theme = name.to_string();
        self.save_config();
    }

    fn cycle_theme(&mut self, _: &CycleTheme, _w: &mut Window, cx: &mut Context<Self>) {
        let current = &self.config.theme;
        let idx = self
            .themes
            .iter()
            .position(|t| t.name == *current)
            .unwrap_or(0);
        let next = (idx + 1) % self.themes.len();
        let name = self.themes[next].name.clone();
        self.set_theme(&name);
        cx.notify();
    }

    fn font_family_options(&self) -> Vec<String> {
        let mut options = if cfg!(target_os = "macos") {
            vec![
                "Menlo".to_string(),
                "Monaco".to_string(),
                "SF Mono".to_string(),
                "JetBrains Mono".to_string(),
            ]
        } else {
            vec![
                "Monospace".to_string(),
                "DejaVu Sans Mono".to_string(),
                "JetBrains Mono".to_string(),
            ]
        };
        if !options.iter().any(|opt| opt == &self.font_family) {
            options.push(self.font_family.clone());
        }
        options
    }

    fn settings_item_count(&self, section: SettingsSection) -> usize {
        match section {
            SettingsSection::Appearance => 5,
            SettingsSection::Behavior => 3,
            SettingsSection::Runtimes
            | SettingsSection::Machines
            | SettingsSection::Mcps
            | SettingsSection::Keybinds => 1,
        }
    }

    fn clamp_settings_cursor(&mut self) {
        let max = self
            .settings_item_count(self.settings.section)
            .saturating_sub(1);
        self.settings.item_cursor = self.settings.item_cursor.min(max);
    }

    fn settings_section_index(section: SettingsSection) -> usize {
        SettingsSection::ALL
            .iter()
            .position(|candidate| *candidate == section)
            .unwrap_or(0)
    }

    fn settings_change_section(&mut self, delta: i32) {
        let idx = Self::settings_section_index(self.settings.section) as i32;
        let last = SettingsSection::ALL.len() as i32 - 1;
        let next = (idx + delta).clamp(0, last) as usize;
        self.settings.section = SettingsSection::ALL[next];
        self.clamp_settings_cursor();
    }

    fn set_default_view_mode(&mut self, mode: ViewMode) {
        self.view_mode = mode;
        self.view_mode_epoch += 1;
        self.config.default_view_mode = mode.as_str().to_string();
        self.save_config();
        self.save_state();
    }

    fn settings_step_current_item(&mut self, delta: i32) -> bool {
        match self.settings.section {
            SettingsSection::Appearance => match self.settings.item_cursor {
                0 => {
                    let current = &self.config.theme;
                    let idx = self
                        .themes
                        .iter()
                        .position(|t| t.name == *current)
                        .unwrap_or(0) as i32;
                    let len = self.themes.len().max(1) as i32;
                    let next = (idx + delta).rem_euclid(len) as usize;
                    if let Some(theme) = self.themes.get(next) {
                        self.set_theme(&theme.name.clone());
                        return true;
                    }
                }
                1 => {
                    let options = self.font_family_options();
                    let idx = options
                        .iter()
                        .position(|name| name == &self.font_family)
                        .unwrap_or(0) as i32;
                    let len = options.len().max(1) as i32;
                    let next = (idx + delta).rem_euclid(len) as usize;
                    self.font_family = options[next].clone();
                    self.config.font_family = self.font_family.clone();
                    self.save_config();
                    return true;
                }
                2 => {
                    let step = if delta >= 0 { 1.0 } else { -1.0 };
                    let next = (self.font_size + step).clamp(10.0, 28.0);
                    if (next - self.font_size).abs() > f32::EPSILON {
                        self.font_size = next;
                        self.config.font_size = next;
                        self.save_config();
                        return true;
                    }
                }
                3 => {
                    let step = if delta >= 0 { 0.05 } else { -0.05 };
                    let next = (self.config.bg_opacity + step).clamp(0.0, 1.0);
                    if (next - self.config.bg_opacity).abs() > 0.001 {
                        self.config.bg_opacity = next;
                        self.save_config();
                        return true;
                    }
                }
                4 => {
                    let step = if delta >= 0 { 1.0 } else { -1.0 };
                    let next = (self.config.bg_blur + step).clamp(0.0, 20.0);
                    if (next - self.config.bg_blur).abs() > 0.001 {
                        self.config.bg_blur = next;
                        self.save_config();
                        return true;
                    }
                }
                _ => {}
            },
            SettingsSection::Behavior => match self.settings.item_cursor {
                0 => {
                    self.config.cautious_enter = !self.config.cautious_enter;
                    self.save_config();
                    return true;
                }
                1 => {
                    self.config.terminal_text = !self.config.terminal_text;
                    self.save_config();
                    return true;
                }
                2 => {
                    let modes = [ViewMode::Grid, ViewMode::Pipeline, ViewMode::Focus];
                    let idx = modes
                        .iter()
                        .position(|m| *m == ViewMode::from_str(&self.config.default_view_mode))
                        .unwrap_or(0) as i32;
                    let len = modes.len() as i32;
                    let next = (idx + delta).rem_euclid(len) as usize;
                    self.set_default_view_mode(modes[next]);
                    return true;
                }
                _ => {}
            },
            SettingsSection::Runtimes
            | SettingsSection::Machines
            | SettingsSection::Mcps
            | SettingsSection::Keybinds => {}
        }
        false
    }

    fn settings_activate_current_item(&mut self) -> bool {
        self.settings_step_current_item(1)
    }

    fn rebuild_palette(&mut self) {
        let q = self.palette_input.to_lowercase();
        let mut all = vec![
            PaletteItem {
                label: "New Agent".into(),
                action: PaletteAction::NewAgent,
            },
            PaletteItem {
                label: "New Group".into(),
                action: PaletteAction::NewGroup,
            },
        ];
        for t in &self.themes {
            let active = if t.name == self.config.theme {
                " (active)"
            } else {
                ""
            };
            all.push(PaletteItem {
                label: format!("Theme: {}{}", t.name, active),
                action: PaletteAction::SetTheme(t.name.clone()),
            });
        }
        all.push(PaletteItem {
            label: "View: Grid".into(),
            action: PaletteAction::SetView(ViewMode::Grid),
        });
        all.push(PaletteItem {
            label: "View: Pipeline".into(),
            action: PaletteAction::SetView(ViewMode::Pipeline),
        });
        all.push(PaletteItem {
            label: "View: Focus".into(),
            action: PaletteAction::SetView(ViewMode::Focus),
        });
        all.push(PaletteItem {
            label: "Toggle Sidebar: Agents/Workers".into(),
            action: PaletteAction::ToggleSidebarTab,
        });
        all.push(PaletteItem {
            label: format!(
                "Setting: Cautious Enter ({})",
                if self.config.cautious_enter {
                    "on"
                } else {
                    "off"
                }
            ),
            action: PaletteAction::ToggleCautiousEnter,
        });
        all.push(PaletteItem {
            label: format!(
                "Setting: Terminal Text ({})",
                if self.config.terminal_text {
                    "on"
                } else {
                    "off"
                }
            ),
            action: PaletteAction::ToggleTerminalText,
        });
        for pct in [100, 90, 80, 70, 60, 50, 40, 30, 20, 10] {
            let val = pct as f32 / 100.0;
            let active = if (self.config.bg_opacity - val).abs() < 0.01 {
                " (active)"
            } else {
                ""
            };
            all.push(PaletteItem {
                label: format!("Background Opacity: {}%{}", pct, active),
                action: PaletteAction::SetBgOpacity(val),
            });
        }
        for pct in [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            let val = pct as f32 / 100.0;
            let active = if (self.config.bg_blur - val).abs() < 0.01 {
                " (active)"
            } else {
                ""
            };
            all.push(PaletteItem {
                label: format!("Background Blur: {}%{}", pct, active),
                action: PaletteAction::SetBgBlur(val),
            });
        }
        all.push(PaletteItem {
            label: "Compact Context".into(),
            action: PaletteAction::CompactContext,
        });
        all.push(PaletteItem {
            label: "Kill Agent".into(),
            action: PaletteAction::KillCurrent,
        });
        all.push(PaletteItem {
            label: "Quit".into(),
            action: PaletteAction::Quit,
        });
        self.palette_items = all
            .into_iter()
            .filter(|i| q.is_empty() || i.label.to_lowercase().contains(&q))
            .collect();
        self.palette_selection = 0;
    }

    fn start_setup(&mut self) {
        let last_rt = &self.config.last_runtime;
        let rt_cursor = self
            .config
            .runtimes
            .iter()
            .position(|r| r.name == *last_rt)
            .unwrap_or(0);
        let last_machine = &self.config.last_machine;
        let machine_cursor = self
            .config
            .machines
            .iter()
            .position(|m| m.name == *last_machine)
            .unwrap_or(0);
        let rt = self.config.runtimes.get(rt_cursor);
        let (selected_dir, dir_entries) = Self::resolve_setup_directory("");
        self.setup = Some(SetupState {
            step: SetupStep::Runtime,
            runtime_cursor: rt_cursor,
            selected_runtime: rt.map(|r| r.name.clone()).unwrap_or_default(),
            machine_cursor,
            selected_machine: self
                .config
                .machines
                .get(machine_cursor)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "local".into()),
            show_recent: !self.config.recent_projects.is_empty(),
            recent_cursor: 0,
            dir_filter: String::new(),
            dir_entries,
            dir_cursor: 0,
            selected_dir,
            editing_agent: None,
            worktree_mode: false,
            worktree_branch: String::new(),
        });
        self.set_mode(Mode::Setup);
    }

    fn start_change_agent(&mut self, _cx: &mut Context<Self>) {
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx >= self.agents.len() {
            return;
        }
        if self.agents[idx].status == AgentStatus::Working {
            return;
        }

        // Extract values before borrowing self again
        let rt_name = self.agents[idx].runtime_name.clone();
        let machine_name = self.agents[idx].target_machine.clone();
        let rt_cursor = self
            .config
            .runtimes
            .iter()
            .position(|r| r.name == rt_name)
            .unwrap_or(0);
        let machine_cursor = self
            .config
            .machines
            .iter()
            .position(|m| m.name == machine_name)
            .unwrap_or(0);
        let (selected_dir, dir_entries) =
            Self::resolve_setup_directory(&self.agents[idx].working_dir);

        self.setup = Some(SetupState {
            step: SetupStep::Runtime,
            runtime_cursor: rt_cursor,
            selected_runtime: rt_name,
            machine_cursor,
            selected_machine: self
                .config
                .machines
                .get(machine_cursor)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "local".into()),
            show_recent: !self.config.recent_projects.is_empty(),
            recent_cursor: 0,
            dir_filter: String::new(),
            dir_entries,
            dir_cursor: 0,
            selected_dir,
            editing_agent: Some(idx),
            worktree_mode: false,
            worktree_branch: String::new(),
        });
        self.set_mode(Mode::Setup);
    }

    fn finish_setup(&mut self, cx: &mut Context<Self>) {
        if let Some(setup) = self.setup.take() {
            let runtime_prefs = self
                .config
                .runtime_prefs
                .get(&setup.selected_runtime)
                .cloned()
                .unwrap_or_default();
            let fallback_model = self
                .config
                .runtimes
                .iter()
                .find(|r| r.name == setup.selected_runtime)
                .map(|r| r.last_model.clone())
                .unwrap_or_default();
            let selected_model = if runtime_prefs.preferred_model.is_empty() {
                fallback_model
            } else {
                runtime_prefs.preferred_model.clone()
            };
            let model = if selected_model.is_empty() {
                None
            } else {
                Some(selected_model.clone())
            };

            self.config.last_runtime = setup.selected_runtime.clone();
            self.config.last_machine = setup.selected_machine.clone();
            self.config.last_mcps = if runtime_prefs.default_mcps.is_empty() {
                Vec::new()
            } else {
                self.config
                    .mcps
                    .iter()
                    .filter(|m| !m.global && runtime_prefs.default_mcps.contains(&m.name))
                    .map(|m| m.name.clone())
                    .collect()
            };
            if let Some(rt) = self
                .config
                .runtimes
                .iter_mut()
                .find(|r| r.name == setup.selected_runtime)
            {
                rt.last_model = selected_model.clone();
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
                        if setup.selected_dir.is_empty() {
                            None
                        } else {
                            Some(setup.selected_dir.as_str())
                        },
                        cx,
                    );
                    if n > edit_idx {
                        let last = self.agents.pop().unwrap();
                        self.agents.insert(edit_idx, last);
                    }
                    self.set_focus(edit_idx);
                }
            } else {
                let n = self.agents.len();
                let group = self.current_group_name().to_string();
                let name_prefix = if setup.selected_runtime == "terminal" {
                    "terminal"
                } else {
                    "agent"
                };
                self.create_agent_with_role(
                    &format!("{}-{}", name_prefix, n),
                    &group,
                    &setup.selected_runtime,
                    model.as_deref(),
                    &setup.selected_machine,
                    AgentRole::Coordinator,
                    Some(self.coordinator_preamble()),
                    None,
                    None,
                    if setup.selected_dir.is_empty() {
                        None
                    } else {
                        Some(setup.selected_dir.as_str())
                    },
                    cx,
                );
                // Create worktree if requested
                if setup.worktree_mode && !setup.worktree_branch.is_empty() {
                    let working_dir = std::path::Path::new(&self.agents[n].working_dir);
                    if crate::worktree::is_git_repo(working_dir) {
                        match WorktreeInfo::create(working_dir, &setup.worktree_branch) {
                            Ok(wt) => {
                                let wt_path = wt.worktree_path.display().to_string();
                                // Run setup script if present
                                let setup_lines = crate::worktree::run_setup_script(
                                    &wt.repo_root,
                                    &wt.worktree_path,
                                    &self.agents[n].name,
                                );
                                for line in &setup_lines {
                                    self.agents[n]
                                        .output_lines
                                        .push(format!("[setup] {}", line));
                                }
                                self.agents[n].working_dir = wt_path;
                                self.agents[n].worktree_info = Some(wt);
                            }
                            Err(e) => {
                                self.agents[n]
                                    .output_lines
                                    .push(format!("[worktree] {}", e));
                            }
                        }
                    }
                }
                self.set_focus(n);
            }

            if !setup.selected_dir.is_empty() {
                self.save_recent_project(&setup.selected_dir, &setup.selected_machine);
            }
            self.save_config();
            self.save_state();
        }
        self.set_mode(Mode::Normal);
        cx.notify();
    }

    // ── Actions ─────────────────────────────────────────────────

    fn enter_command_mode(
        &mut self,
        _: &EnterCommandMode,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == Mode::Setup {
            self.setup = None;
        }
        if self.mode == Mode::Search {
            self.search_query.clear();
        }
        if self.mode == Mode::AgentMenu {
            self.agent_menu_items.clear();
        }
        if self.mode == Mode::Settings {
            self.settings = SettingsState::default();
        }
        self.set_mode(Mode::Normal);
        self.palette_input.clear();
        cx.notify();
    }

    fn open_settings(&mut self, _: &OpenSettings, _w: &mut Window, cx: &mut Context<Self>) {
        self.settings = SettingsState::default();
        self.set_mode(Mode::Settings);
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

    fn open_agent_menu(&mut self, agent_idx: usize, cx: &mut Context<Self>) {
        if agent_idx >= self.agents.len() {
            return;
        }
        let runtime = self.agents[agent_idx].runtime_name.clone();
        self.agent_menu_items = build_agent_menu(&runtime);
        self.agent_menu_selection = 0;
        self.agent_menu_target = agent_idx;
        self.set_mode(Mode::AgentMenu);
        cx.notify();
    }

    fn execute_agent_menu_item(&mut self, cx: &mut Context<Self>) {
        let idx = self.agent_menu_target;
        let sel = self.agent_menu_selection;
        if sel >= self.agent_menu_items.len() || idx >= self.agents.len() {
            self.set_mode(Mode::Normal);
            cx.notify();
            return;
        }
        let command = self.agent_menu_items[sel].command.clone();
        self.agent_menu_items.clear();
        self.set_mode(Mode::Normal);
        match command {
            AgentMenuCommand::SlashCommand(cmd) => {
                self.send_prompt(idx, cmd, cx);
            }
            AgentMenuCommand::ToggleAutoScroll => {
                if let Some(a) = self.agents.get_mut(idx) {
                    a.auto_scroll = !a.auto_scroll;
                    if a.auto_scroll {
                        let len = a.output_lines.len();
                        if len > 40 {
                            a.scroll_offset = len - 40;
                        }
                    }
                }
            }
            AgentMenuCommand::ToggleFavorite => {
                if let Some(a) = self.agents.get_mut(idx) {
                    a.favorite = !a.favorite;
                }
            }
            AgentMenuCommand::CopyLastResponse => {
                if let Some(a) = self.agents.get(idx) {
                    let response = extract_latest_turn_output(&a.output_lines);
                    if !response.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(response));
                    }
                }
            }
        }
        cx.notify();
    }

    fn submit_input(&mut self, _: &SubmitInput, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                self.clamp_focus();
                let idx = self.focused_agent;
                if idx >= self.agents.len() {
                    return;
                }
                if self.agents[idx].is_terminal {
                    return;
                }
                if self.agents[idx].status == AgentStatus::Working {
                    return;
                }
                let prompt = self.agents[idx].input_buffer.trim().to_string();
                if prompt.is_empty() {
                    return;
                }
                self.agents[idx].input_buffer.clear();
                self.agents[idx].input_cursor = 0;
                self.set_mode(Mode::Normal);
                self.send_prompt(idx, prompt, cx);
            }
            Mode::Palette => {
                if let Some(item) = self.palette_items.get(self.palette_selection) {
                    match &item.action {
                        PaletteAction::NewAgent => {
                            self.start_setup();
                        }
                        PaletteAction::NewGroup => {
                            let n = self.groups.len();
                            self.groups.push(Group {
                                name: format!("group-{}", n),
                            });
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
                            self.view_mode_epoch += 1;
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
                        PaletteAction::CompactContext => {
                            self.clamp_focus();
                            let idx = self.focused_agent;
                            if idx < self.agents.len() {
                                let compact_cmd = match self.agents[idx].runtime_name.as_str() {
                                    "cursor" => "/compress",
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
                        PaletteAction::Quit => {
                            cx.quit();
                            return;
                        }
                    }
                } else {
                    self.set_mode(Mode::Normal);
                }
                self.palette_input.clear();
                cx.notify();
            }
            Mode::Settings => {
                if self.settings_activate_current_item() {
                    cx.notify();
                }
            }
            Mode::Setup => {
                let is_dir_step = self
                    .setup
                    .as_ref()
                    .map(|s| s.step == SetupStep::Directory)
                    .unwrap_or(false);
                if is_dir_step {
                    let recent_projects = self.config.recent_projects.clone();
                    // Enter selects a recent path or drills into a browsed directory.
                    if let Some(ref mut s) = self.setup {
                        if s.show_recent && !recent_projects.is_empty() {
                            if s.recent_cursor < recent_projects.len() {
                                s.selected_dir = recent_projects[s.recent_cursor].path.clone();
                            } else {
                                s.show_recent = false;
                                s.dir_filter.clear();
                                s.dir_cursor = 0;
                                if s.selected_dir.is_empty() {
                                    s.dir_entries = list_home_dirs();
                                } else {
                                    s.dir_entries = list_subdirs(&s.selected_dir);
                                }
                            }
                        } else {
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
                    }
                    cx.notify();
                    return;
                }
                self.setup_next(&SetupNext, _w, cx);
                return;
            }
            Mode::Search => {
                // Jump to selected search result
                let search_target = self
                    .search_results
                    .get(self.search_selection)
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
            Mode::AgentMenu => {
                self.execute_agent_menu_item(cx);
            }
            _ => {}
        }
    }

    fn delete_char(&mut self, _: &DeleteChar, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                if let Some(a) = self.agents.get_mut(self.focused_agent) {
                    if a.is_terminal {
                        return;
                    }
                    if a.input_cursor > 0 {
                        // Find previous char boundary
                        let prev = a.input_buffer[..a.input_cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        a.input_buffer.drain(prev..a.input_cursor);
                        a.input_cursor = prev;
                    }
                }
            }
            Mode::Palette => {
                self.palette_input.pop();
                self.rebuild_palette();
            }
            Mode::Search => {
                self.search_query.pop();
                self.rebuild_search();
            }
            Mode::Setup => {
                let mut did_delete = false;
                if let Some(ref mut s) = self.setup {
                    if s.step == SetupStep::Directory {
                        if s.show_recent {
                            s.show_recent = false;
                            s.recent_cursor = 0;
                            did_delete = true;
                        } else if !s.dir_filter.is_empty() {
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
        // Terminal tiles: forward keystrokes directly to PTY
        if self.mode == Mode::Normal {
            if let Some(a) = self.agents.get(self.focused_agent) {
                if a.is_terminal {
                    if let Some(ref writer) = a.terminal_pty_writer {
                        if let Some(bytes) = keystroke_to_pty_bytes(&event.keystroke) {
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(&bytes);
                                let _ = w.flush();
                            }
                        }
                    }
                    return;
                }
            }
        }
        if self.mode == Mode::Settings {
            if event.keystroke.modifiers.platform
                || event.keystroke.modifiers.control
                || event.keystroke.modifiers.alt
            {
                return;
            }
            let mut changed = false;
            match event.keystroke.key.as_str() {
                "-" => changed = self.settings_step_current_item(-1),
                "=" if event.keystroke.modifiers.shift => {
                    changed = self.settings_step_current_item(1)
                }
                _ => {
                    if let Some(ch) = &event.keystroke.key_char {
                        if ch == "+" {
                            changed = self.settings_step_current_item(1);
                        } else if ch == "-" {
                            changed = self.settings_step_current_item(-1);
                        }
                    }
                }
            }
            if changed {
                cx.notify();
            }
            return;
        }
        let is_setup_typing = self.mode == Mode::Setup
            && self
                .setup
                .as_ref()
                .map(|s| s.step == SetupStep::Directory && !s.show_recent)
                .unwrap_or(false);
        if self.mode == Mode::Normal
            || self.mode == Mode::Palette
            || self.mode == Mode::Search
            || is_setup_typing
        {
            if event.keystroke.modifiers.platform
                || event.keystroke.modifiers.control
                || event.keystroke.modifiers.alt
            {
                return;
            }
            match event.keystroke.key.as_str() {
                "escape" | "enter" | "backspace" | "tab" | "left" | "right" | "up" | "down" => {
                    return;
                }
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
                    Mode::Palette => {
                        self.palette_input.push_str(ch);
                        self.rebuild_palette();
                        cx.notify();
                    }
                    Mode::Search => {
                        self.search_query.push_str(ch);
                        self.rebuild_search();
                        cx.notify();
                    }
                    Mode::Setup => {
                        if let Some(ref mut s) = self.setup {
                            if s.step == SetupStep::Directory && !s.show_recent {
                                s.dir_filter.push_str(ch);
                                s.dir_cursor = 0;
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
        let Some(ref s) = self.setup else {
            return;
        };
        let step = s.step;

        match step {
            SetupStep::Runtime => {
                let rt_name = self
                    .config
                    .runtimes
                    .get(s.runtime_cursor)
                    .map(|r| r.name.clone())
                    .unwrap_or_default();
                let show_machine_step = self.config.machines.len() > 1 && rt_name != "terminal";
                if let Some(ref mut s) = self.setup {
                    s.selected_runtime = rt_name;
                    if show_machine_step {
                        s.step = SetupStep::Machine;
                    } else {
                        let (selected_dir, dir_entries) =
                            Self::resolve_setup_directory(&s.selected_dir);
                        s.step = SetupStep::Directory;
                        s.show_recent = !self.config.recent_projects.is_empty();
                        s.recent_cursor = 0;
                        s.dir_filter.clear();
                        s.dir_cursor = 0;
                        s.selected_dir = selected_dir;
                        s.dir_entries = dir_entries;
                    }
                }
            }
            SetupStep::Machine => {
                let machine_name = self
                    .config
                    .machines
                    .get(
                        self.setup
                            .as_ref()
                            .map(|state| state.machine_cursor)
                            .unwrap_or(0),
                    )
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| "local".into());
                if let Some(ref mut s) = self.setup {
                    let (selected_dir, dir_entries) =
                        Self::resolve_setup_directory(&s.selected_dir);
                    s.selected_machine = machine_name.clone();
                    s.step = SetupStep::Directory;
                    s.show_recent = !self.config.recent_projects.is_empty();
                    s.recent_cursor = 0;
                    s.dir_filter.clear();
                    s.dir_cursor = 0;
                    s.selected_dir = selected_dir;
                    s.dir_entries = dir_entries;
                }
            }
            SetupStep::Directory => {
                self.finish_setup(cx);
                return;
            }
        }
        cx.notify();
    }

    fn setup_prev(&mut self, _: &SetupPrev, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(current_step) = self.setup.as_ref().map(|s| s.step) else {
            return;
        };
        match current_step {
            SetupStep::Directory => {
                let show_machine_step = self
                    .setup
                    .as_ref()
                    .map(|s| self.config.machines.len() > 1 && s.selected_runtime != "terminal")
                    .unwrap_or(false);
                if let Some(ref mut s) = self.setup {
                    s.step = if show_machine_step {
                        SetupStep::Machine
                    } else {
                        SetupStep::Runtime
                    };
                }
            }
            SetupStep::Machine => {
                if let Some(ref mut s) = self.setup {
                    s.step = SetupStep::Runtime;
                }
            }
            SetupStep::Runtime => {
                self.setup = None;
                self.set_mode(Mode::Normal);
                cx.notify();
                return;
            }
        }
        cx.notify();
    }

    fn setup_toggle(&mut self, _: &SetupToggle, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut s) = self.setup {
            if s.step == SetupStep::Directory {
                s.worktree_mode = !s.worktree_mode;
                if !s.worktree_mode {
                    s.worktree_branch.clear();
                }
            }
        }
        cx.notify();
    }

    // ── Text editing (Insert mode) ────────────────────────────────

    fn cursor_left(&mut self, _: &CursorLeft, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor > 0 {
                a.input_cursor = a.input_buffer[..a.input_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
        cx.notify();
    }

    fn cursor_right(&mut self, _: &CursorRight, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor < a.input_buffer.len() {
                a.input_cursor = a.input_buffer[a.input_cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| a.input_cursor + i)
                    .unwrap_or(a.input_buffer.len());
            }
        }
        cx.notify();
    }

    fn cursor_word_left(&mut self, _: &CursorWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let s = &a.input_buffer[..a.input_cursor];
            // Skip trailing whitespace, then skip word chars
            let trimmed = s.trim_end();
            if trimmed.is_empty() {
                a.input_cursor = 0;
            } else {
                let last_space = trimmed
                    .char_indices()
                    .filter(|(_, c)| c.is_whitespace())
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                a.input_cursor = last_space;
            }
        }
        cx.notify();
    }

    fn cursor_word_right(&mut self, _: &CursorWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let s = &a.input_buffer[a.input_cursor..];
            // Skip current word chars, then skip whitespace
            let word_end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
            let after_ws = s[word_end..]
                .find(|c: char| !c.is_whitespace())
                .map(|i| word_end + i)
                .unwrap_or(s.len());
            a.input_cursor += after_ws;
        }
        cx.notify();
    }

    fn cursor_home(&mut self, _: &CursorHome, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            // Go to start of current line
            let before = &a.input_buffer[..a.input_cursor];
            a.input_cursor = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        cx.notify();
    }

    fn cursor_end(&mut self, _: &CursorEnd, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            // Go to end of current line
            let after = &a.input_buffer[a.input_cursor..];
            a.input_cursor += after.find('\n').unwrap_or(after.len());
        }
        cx.notify();
    }

    fn delete_word_back(&mut self, _: &DeleteWordBack, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            if a.input_cursor > 0 {
                let s = &a.input_buffer[..a.input_cursor];
                let trimmed = s.trim_end();
                let target = if trimmed.is_empty() {
                    0
                } else {
                    trimmed
                        .char_indices()
                        .filter(|(_, c)| c.is_whitespace())
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0)
                };
                a.input_buffer.drain(target..a.input_cursor);
                a.input_cursor = target;
            }
        }
        cx.notify();
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
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
        if self.mode != Mode::Normal {
            return;
        }
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
        if self.mode != Mode::Normal {
            return;
        }
        if let Some(clip) = cx.read_from_clipboard() {
            let text = clip.text().unwrap_or_default();
            if !text.is_empty() {
                if let Some(a) = self.agents.get_mut(self.focused_agent) {
                    if a.is_terminal {
                        if let Some(ref writer) = a.terminal_pty_writer {
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(text.as_bytes());
                                let _ = w.flush();
                            }
                        }
                    } else {
                        a.input_buffer.insert_str(a.input_cursor, &text);
                        a.input_cursor += text.len();
                    }
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
                        SetupStep::Runtime => {
                            s.runtime_cursor = s.runtime_cursor.saturating_sub(1);
                        }
                        SetupStep::Machine => {
                            s.machine_cursor = s.machine_cursor.saturating_sub(1);
                        }
                        SetupStep::Directory => {
                            if s.show_recent {
                                s.recent_cursor = s.recent_cursor.saturating_sub(1);
                            } else {
                                s.dir_cursor = s.dir_cursor.saturating_sub(1);
                            }
                        }
                    }
                }
            }
            Mode::Settings => {
                self.settings.item_cursor = self.settings.item_cursor.saturating_sub(1);
            }
            Mode::Palette => {
                self.palette_selection = self.palette_selection.saturating_sub(1);
            }
            Mode::Search => {
                self.search_selection = self.search_selection.saturating_sub(1);
            }
            Mode::AgentMenu => {
                self.agent_menu_selection = self.agent_menu_selection.saturating_sub(1);
            }
        }
        cx.notify();
    }

    fn nav_down(&mut self, _: &NavDown, _w: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Normal => {
                if self.focused_group < self.groups.len().saturating_sub(1) {
                    self.focused_group += 1;
                }
                self.clamp_focus();
            }
            Mode::Setup => {
                // Extract what we need before mutating
                let info = self.setup.as_ref().map(|s| (s.step, s.show_recent));
                if let Some((step, show_recent)) = info {
                    let max = match step {
                        SetupStep::Runtime => self.config.runtimes.len().saturating_sub(1),
                        SetupStep::Machine => self.config.machines.len().saturating_sub(1),
                        SetupStep::Directory => {
                            if show_recent && !self.config.recent_projects.is_empty() {
                                self.config.recent_projects.len()
                            } else {
                                let filter = self
                                    .setup
                                    .as_ref()
                                    .map(|s| s.dir_filter.clone())
                                    .unwrap_or_default();
                                let entries = self
                                    .setup
                                    .as_ref()
                                    .map(|s| s.dir_entries.clone())
                                    .unwrap_or_default();
                                Self::filter_dirs(&entries, &filter).len().saturating_sub(1)
                            }
                        }
                    };
                    if let Some(ref mut s) = self.setup {
                        match step {
                            SetupStep::Runtime => {
                                s.runtime_cursor = (s.runtime_cursor + 1).min(max)
                            }
                            SetupStep::Machine => {
                                s.machine_cursor = (s.machine_cursor + 1).min(max)
                            }
                            SetupStep::Directory => {
                                if s.show_recent && !self.config.recent_projects.is_empty() {
                                    s.recent_cursor = (s.recent_cursor + 1).min(max);
                                } else {
                                    s.dir_cursor = (s.dir_cursor + 1).min(max);
                                }
                            }
                        }
                    }
                }
            }
            Mode::Settings => {
                let max = self
                    .settings_item_count(self.settings.section)
                    .saturating_sub(1);
                self.settings.item_cursor = (self.settings.item_cursor + 1).min(max);
            }
            Mode::Palette => {
                let max = self.palette_items.len().saturating_sub(1);
                self.palette_selection = (self.palette_selection + 1).min(max);
            }
            Mode::Search => {
                let max = self.search_results.len().saturating_sub(1);
                self.search_selection = (self.search_selection + 1).min(max);
            }
            Mode::AgentMenu => {
                let max = self.agent_menu_items.len().saturating_sub(1);
                self.agent_menu_selection = (self.agent_menu_selection + 1).min(max);
            }
        }
        cx.notify();
    }

    fn pane_left(&mut self, _: &PaneLeft, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Settings {
            self.settings_change_section(-1);
            cx.notify();
            return;
        }
        if self.mode != Mode::Normal {
            return;
        }
        let vis = self.agents_in_current_group();
        if vis.is_empty() {
            return;
        }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            if pos > 0 {
                self.set_focus(vis[pos - 1]);
            }
        } else {
            self.set_focus(vis[0]);
        }

        cx.notify();
    }

    fn pane_right(&mut self, _: &PaneRight, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Settings {
            self.settings_change_section(1);
            cx.notify();
            return;
        }
        if self.mode != Mode::Normal {
            return;
        }
        let vis = self.agents_in_current_group();
        if vis.is_empty() {
            return;
        }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            if pos < vis.len() - 1 {
                self.set_focus(vis[pos + 1]);
            }
        } else {
            self.set_focus(vis[0]);
        }

        cx.notify();
    }

    fn next_pane(&mut self, _: &NextPane, _w: &mut Window, cx: &mut Context<Self>) {
        let vis = self.agents_in_current_group();
        if vis.is_empty() {
            return;
        }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            let next = (pos + 1) % vis.len();
            self.set_focus(vis[next]);
        } else {
            self.set_focus(vis[0]);
        }

        cx.notify();
    }

    fn prev_pane(&mut self, _: &PrevPane, _w: &mut Window, cx: &mut Context<Self>) {
        let vis = self.agents_in_current_group();
        if vis.is_empty() {
            return;
        }
        if let Some(pos) = vis.iter().position(|&i| i == self.focused_agent) {
            let prev = if pos == 0 { vis.len() - 1 } else { pos - 1 };
            self.set_focus(vis[prev]);
        } else {
            self.set_focus(vis[0]);
        }

        cx.notify();
    }

    fn next_group(&mut self, _: &NextGroup, _w: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() <= 1 {
            return;
        }
        self.focused_group = (self.focused_group + 1) % self.groups.len();
        self.clamp_focus();
        cx.notify();
    }

    fn prev_group(&mut self, _: &PrevGroup, _w: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() <= 1 {
            return;
        }
        self.focused_group = if self.focused_group == 0 {
            self.groups.len() - 1
        } else {
            self.focused_group - 1
        };
        self.clamp_focus();
        cx.notify();
    }

    fn scroll_up(&mut self, _: &ScrollUp, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.scroll_offset = a.scroll_offset.saturating_sub(3);
        }
        cx.notify();
    }

    fn scroll_down(&mut self, _: &ScrollDown, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let max = a.output_lines.len().saturating_sub(1);
            a.scroll_offset = (a.scroll_offset + 3).min(max);
        }
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.scroll_offset = a.scroll_offset.saturating_sub(20);
        }
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let max = a.output_lines.len().saturating_sub(1);
            a.scroll_offset = (a.scroll_offset + 20).min(max);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.scroll_offset = 0;
        }
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            let len = a.output_lines.len();
            a.scroll_offset = if len > 40 { len - 40 } else { 0 };
        }
        cx.notify();
    }

    fn spawn_agent(&mut self, _: &SpawnAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.start_setup();
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _w: &mut Window, cx: &mut Context<Self>) {
        self.ui_scale = (self.ui_scale + 0.1).min(2.0);
        cx.notify();
    }
    fn zoom_out(&mut self, _: &ZoomOut, _w: &mut Window, cx: &mut Context<Self>) {
        self.ui_scale = (self.ui_scale - 0.1).max(0.5);
        cx.notify();
    }
    fn zoom_reset(&mut self, _: &ZoomReset, _w: &mut Window, cx: &mut Context<Self>) {
        self.ui_scale = 1.0;
        cx.notify();
    }
    fn close_tile(&mut self, _: &CloseTile, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx < self.agents.len() {
            if self.agents[idx].is_terminal {
                // Terminal tiles close immediately, no confirm
                self.remove_agent_and_dependents(idx);
            } else {
                self.confirm_remove_agent = Some(idx);
            }
        }
        cx.notify();
    }

    fn toggle_changes(&mut self, _: &ToggleChanges, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.show_changes = !self.show_changes;
        if self.show_changes {
            if let Some(a) = self.agents.get(self.focused_agent) {
                let workdir = if let Some(ref wt) = a.worktree_info {
                    wt.worktree_path.clone()
                } else {
                    std::path::PathBuf::from(&a.working_dir)
                };
                self.changes_panel = Some(ChangesState::new(&workdir));
            }
        } else {
            self.changes_panel = None;
        }
        cx.notify();
    }

    fn changes_up(&mut self, _: &ChangesUp, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut panel) = self.changes_panel {
            panel.select_prev();
        }
        cx.notify();
    }

    fn changes_down(&mut self, _: &ChangesDown, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut panel) = self.changes_panel {
            panel.select_next();
        }
        cx.notify();
    }

    fn changes_stage(&mut self, _: &ChangesStage, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut panel) = self.changes_panel {
            let idx = panel.selected_index;
            if let Some((path, is_staged)) = panel.file_at(idx).map(|(p, s)| (p.to_string(), s)) {
                if is_staged {
                    panel.unstage_file(&path);
                } else {
                    panel.stage_file(&path);
                }
            }
        }
        cx.notify();
    }

    fn changes_refresh(&mut self, _: &ChangesRefresh, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut panel) = self.changes_panel {
            panel.refresh();
            panel.load_diff();
        }
        cx.notify();
    }

    fn kill_agent(&mut self, _: &KillAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx < self.agents.len() {
            // Drop the prompt sender to signal the thread to stop
            self.agents[idx].prompt_tx = None;
            self.agents[idx]._reader_task = None;
            self.agents[idx].terminal_entity = None;
            self.agents[idx].status = AgentStatus::Idle;
            self.agents[idx].output_lines.push("[killed]".into());
        }
        cx.notify();
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.favorite = !a.favorite;
        }
        cx.notify();
    }

    fn change_agent(&mut self, _: &ChangeAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.start_change_agent(cx);
        cx.notify();
    }

    fn restart_agent(&mut self, _: &RestartAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx >= self.agents.len() {
            return;
        }
        if self.agents[idx].status == AgentStatus::Working {
            return;
        }
        let name = self.agents[idx].name.clone();
        let group = self.agents[idx].group.clone();
        let rt = self.agents[idx].runtime_name.clone();
        let model = self.agents[idx].last_model.clone();
        let target_machine = self.agents[idx].target_machine.clone();
        let role = self.agents[idx].role;
        let prompt_preamble = self.agents[idx].prompt_preamble.clone();
        let worker_assignment = self.agents[idx].worker_assignment.clone();
        let working_dir = self.agents[idx].working_dir.clone();
        // Kill old
        self.agents[idx].prompt_tx = None;
        self.agents[idx]._reader_task = None;
        self.agents[idx].terminal_entity = None;
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
            if working_dir.is_empty() {
                None
            } else {
                Some(working_dir.as_str())
            },
            cx,
        );
        if n > idx {
            let last = self.agents.pop().unwrap();
            self.agents.insert(idx, last);
        }
        self.set_focus(idx);
        cx.notify();
    }

    fn toggle_auto_scroll(
        &mut self,
        _: &ToggleAutoScroll,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        if let Some(a) = self.agents.get_mut(self.focused_agent) {
            a.auto_scroll = !a.auto_scroll;
            if a.auto_scroll {
                let len = a.output_lines.len();
                if len > 40 {
                    a.scroll_offset = len - 40;
                }
            }
        }
        cx.notify();
    }

    fn show_stats(&mut self, _: &ShowStats, _w: &mut Window, cx: &mut Context<Self>) {
        self.show_stats = !self.show_stats;
        cx.notify();
    }

    fn spawn_completion_particles(&mut self, x: f32, y: f32, color: Rgba) {
        let mut rng: u64 = (x as u64)
            .wrapping_mul(6364136223846793005)
            .wrapping_add((y as u64).wrapping_mul(1442695040888963407))
            .wrapping_add(self.star_tick);
        let next_f = |r: &mut u64| -> f32 {
            *r = r
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((*r >> 33) as f32) / (u32::MAX as f32 / 2.0) - 1.0 // -1.0..1.0
        };
        for _ in 0..18 {
            let angle = next_f(&mut rng) * std::f32::consts::PI;
            let speed = 1.5 + (next_f(&mut rng).abs()) * 3.5;
            let vx = angle.cos() * speed;
            let vy = angle.sin() * speed;
            self.particles.push(Particle {
                x,
                y,
                vx,
                vy,
                life: 1.0,
                decay: 0.018 + next_f(&mut rng).abs() * 0.012,
                size: 2.0 + next_f(&mut rng).abs() * 3.0,
                color,
            });
        }
    }

    fn update_particles(&mut self) {
        for p in &mut self.particles {
            p.x += p.vx;
            p.y += p.vy;
            p.vy += 0.08; // gravity
            p.vx *= 0.97; // drag
            p.vy *= 0.97;
            p.life -= p.decay;
        }
        self.particles.retain(|p| p.life > 0.0);
    }

    fn render_particles(&self) -> impl IntoElement {
        let particles_data: Vec<(f32, f32, f32, f32, Rgba)> = self
            .particles
            .iter()
            .map(|p| (p.x, p.y, p.size, p.life, p.color))
            .collect();
        canvas(
            move |_bounds, _window, _cx| {},
            move |_bounds, _, window, _cx| {
                for (px_x, px_y, size, life, color) in &particles_data {
                    let alpha = life * color.a;
                    let particle_bounds = Bounds {
                        origin: point(px(px_x - size * 0.5), px(px_y - size * 0.5)),
                        size: Size {
                            width: px(*size),
                            height: px(*size),
                        },
                    };
                    let c = Rgba {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                        a: alpha,
                    };
                    window.paint_quad(fill(particle_bounds, c));
                }
            },
        )
        .size_full()
        .absolute()
        .top_0()
        .left_0()
    }

    fn render_starfield(&self, _cx: &Context<Self>) -> impl IntoElement {
        let stars_data: Vec<(f32, f32, f32, f32, f32, f32)> = self
            .stars
            .iter()
            .map(|s| (s.x, s.y, s.size, s.brightness, s.phase, s.speed))
            .collect();
        let tick = self.star_tick;
        let bg_hex = self.theme.bg;
        let bg_c = self.bg_alpha(rgba(bg_hex));
        let mx = self.mouse_x;
        let my = self.mouse_y;
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

                    // Parallax: bigger stars shift more for depth effect
                    let parallax_strength = 0.02 + 0.03 * (size / 3.0).min(1.0);
                    let shift_x = (mx - 0.5) * parallax_strength * w;
                    let shift_y = (my - 0.5) * parallax_strength * h;

                    let px_x = off_x + sx * w + shift_x;
                    let px_y = off_y + sy * h + shift_y;

                    // Twinkle: sinusoidal brightness modulation
                    let twinkle = 0.5 + 0.5 * (time * speed + phase).sin();
                    let alpha = brightness * (0.3 + 0.7 * twinkle);

                    let star_bounds = Bounds {
                        origin: point(px(px_x - size * 0.5), px(px_y - size * 0.5)),
                        size: Size {
                            width: px(*size),
                            height: px(*size),
                        },
                    };

                    // Warm white with slight blue tint for some stars
                    let tint = if *brightness > 0.7 { 0.95 } else { 0.85 };
                    let star_color = Rgba {
                        r: tint,
                        g: tint,
                        b: 1.0,
                        a: alpha,
                    };
                    window.paint_quad(fill(star_bounds, star_color));
                }
            },
        )
        .size_full()
    }

    fn open_terminal(&mut self, _: &OpenTerminal, _w: &mut Window, _cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx < self.agents.len() {
            let dir = &self.agents[idx].working_dir;
            if !dir.is_empty() {
                open_in_terminal(dir);
            }
        }
    }

    fn pipe_to_agent(&mut self, _: &PipeToAgent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        let idx = self.focused_agent;
        if idx >= self.agents.len() {
            return;
        }
        // Get last response from source agent
        let lines = &self.agents[idx].output_lines;
        if lines.is_empty() {
            return;
        }
        let last_prompt = lines.iter().rposition(|l| l.starts_with("> ")).unwrap_or(0);
        let start = (last_prompt + 1).min(lines.len());
        let response: String = lines[start..].join("\n").trim().to_string();
        if response.is_empty() {
            return;
        }
        // Find next agent in the same group
        let vis = self.agents_in_current_group();
        if vis.len() < 2 {
            return;
        }
        let cur_pos = vis.iter().position(|&i| i == idx).unwrap_or(0);
        let next_idx = vis[(cur_pos + 1) % vis.len()];
        if next_idx == idx || next_idx >= self.agents.len() {
            return;
        }
        if self.agents[next_idx].status == AgentStatus::Working {
            return;
        }
        // Pipe as a prompt
        let src_name = self.agents[idx].name.clone();
        let piped = format!("[piped from {}] {}", src_name, response);
        self.set_focus(next_idx);
        self.send_prompt(next_idx, piped, cx);
        cx.notify();
    }

    fn continue_turn(&mut self, _: &ContinueTurn, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.clamp_focus();
        let idx = self.focused_agent;
        self.continue_pending_turn(idx, cx);
        cx.notify();
    }

    fn view_grid(&mut self, _: &ViewGrid, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.view_mode = ViewMode::Grid;
        self.view_mode_epoch += 1;
        cx.notify();
    }
    fn view_pipeline(&mut self, _: &ViewPipeline, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.view_mode = ViewMode::Pipeline;
        self.view_mode_epoch += 1;
        cx.notify();
    }
    fn view_focus(&mut self, _: &ViewFocus, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
        self.view_mode = ViewMode::Focus;
        self.view_mode_epoch += 1;
        cx.notify();
    }

    fn search_open(&mut self, _: &SearchOpen, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Normal {
            return;
        }
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
        if q.is_empty() {
            return;
        }
        for (ai, agent) in self.agents.iter().enumerate() {
            for (li, line) in agent.output_lines.iter().enumerate() {
                if line.to_lowercase().contains(&q) {
                    self.search_results.push(SearchResult {
                        agent_idx: ai,
                        agent_name: agent.name.clone(),
                        line_idx: li,
                        line: line.clone(),
                    });
                    if self.search_results.len() >= 50 {
                        break;
                    }
                }
            }
            if self.search_results.len() >= 50 {
                break;
            }
        }
        self.search_selection = 0;
    }

    fn quit_app(&mut self, _: &Quit, _w: &mut Window, cx: &mut Context<Self>) {
        self.save_config();
        self.save_state();
        cx.quit();
    }

    // ── Render ──────────────────────────────────────────────────

    fn s(&self, base: f32) -> Pixels {
        px(base * self.ui_scale)
    }

    /// Apply background opacity to a color. Used for bg, surface, surface_raised so the desktop shows through.
    fn bg_alpha(&self, c: Rgba) -> Rgba {
        let o = self.config.bg_opacity;
        Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a * o,
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let surface = self.bg_alpha(t.surface());
        let surface_raised = self.bg_alpha(t.surface_raised());
        let mut sb = div()
            .w(self.s(220.0))
            .h_full()
            .bg(surface)
            .border_r_1()
            .border_color(t.border())
            .pt(self.s(16.0))
            .pb(self.s(16.0))
            .px(self.s(14.0))
            .flex()
            .flex_col()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(self.s(2.0), px(0.)),
                blur_radius: self.s(8.0),
                spread_radius: px(0.),
            }]);

        // Tab switcher
        let agents_active = self.sidebar_tab == SidebarTab::Agents;
        sb = sb.child(
            div()
                .flex()
                .gap(self.s(2.0))
                .mb(self.s(8.0))
                .child(
                    div()
                        .id("tab-agents")
                        .px(self.s(8.0))
                        .py(self.s(3.0))
                        .rounded(self.s(4.0))
                        .bg(if agents_active {
                            surface_raised
                        } else {
                            rgba(0x00000000)
                        })
                        .text_size(self.s(11.0))
                        .text_color(if agents_active {
                            t.text()
                        } else {
                            t.text_faint()
                        })
                        .cursor_pointer()
                        .child("agents")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_tab = SidebarTab::Agents;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("tab-workers")
                        .px(self.s(8.0))
                        .py(self.s(3.0))
                        .rounded(self.s(4.0))
                        .bg(if !agents_active {
                            surface_raised
                        } else {
                            rgba(0x00000000)
                        })
                        .text_size(self.s(11.0))
                        .text_color(if !agents_active {
                            t.text()
                        } else {
                            t.text_faint()
                        })
                        .cursor_pointer()
                        .child("workers")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_tab = SidebarTab::Workers;
                            cx.notify();
                        })),
                ),
        );

        match self.sidebar_tab {
            SidebarTab::Agents => {
                for (gi, group) in self.groups.iter().enumerate() {
                    let focused = gi == self.focused_group;
                    let bg = if focused {
                        surface_raised
                    } else {
                        rgba(0x00000000)
                    };
                    let tc = if focused { t.text() } else { t.text_muted() };

                    sb = sb.child(
                        div()
                            .id(ElementId::Name(format!("grp-{}", gi).into()))
                            .w_full()
                            .px(self.s(8.0))
                            .py(self.s(5.0))
                            .rounded(self.s(6.0))
                            .bg(bg)
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(self.s(8.0))
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(if focused { t.blue() } else { t.text_faint() })
                                    .child(if focused { ">" } else { " " }),
                            )
                            .child(
                                div()
                                    .text_size(self.s(13.0))
                                    .text_color(tc)
                                    .child(group.name.clone()),
                            )
                            .child(div().flex_grow())
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text_faint())
                                    .child(format!(
                                        "{}",
                                        self.agents
                                            .iter()
                                            .filter(|a| a.group == group.name)
                                            .count()
                                    )),
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.focused_group = gi;
                                this.clamp_focus();
                                cx.notify();
                            })),
                    );

                    if focused {
                        for (i, agent) in self.agents.iter().enumerate() {
                            if agent.group != group.name {
                                continue;
                            }
                            let af = i == self.focused_agent;
                            let tc = if af { t.text() } else { t.text_muted() };
                            let fav_icon = if agent.favorite { "* " } else { "" };
                            let role_icon = if agent.role == AgentRole::Worker {
                                "↳ "
                            } else {
                                ""
                            };
                            let rt_c = t.runtime_color(&agent.runtime_name);
                            sb =
                                sb.child(
                                    div()
                                        .id(ElementId::Name(format!("sa-{}", i).into()))
                                        .w_full()
                                        .pl(self.s(20.0))
                                        .py(self.s(2.0))
                                        .flex()
                                        .items_center()
                                        .gap(self.s(6.0))
                                        .cursor_pointer()
                                        .child(
                                            div()
                                                .w(self.s(3.0))
                                                .h(self.s(14.0))
                                                .rounded(self.s(2.0))
                                                .bg(rt_c),
                                        )
                                        .child(
                                            div()
                                                .text_size(self.s(10.0))
                                                .text_color(agent.status.color(t))
                                                .child(agent.status.dot()),
                                        )
                                        .child(div().text_size(self.s(12.0)).text_color(tc).child(
                                            format!("{}{}{}", role_icon, fav_icon, agent.name),
                                        ))
                                        .child(div().flex_grow())
                                        .child(
                                            div()
                                                .text_size(self.s(10.0))
                                                .text_color(t.text_faint())
                                                .child(format!(
                                                    "{}@{}",
                                                    agent.runtime_name, agent.target_machine
                                                )),
                                        )
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
                    div()
                        .id("btn-new-agent")
                        .w_full()
                        .px(self.s(8.0))
                        .py(self.s(5.0))
                        .rounded(self.s(6.0))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap(self.s(8.0))
                        .hover(move |s| s.bg(surface_raised))
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text_faint())
                                .child("+"),
                        )
                        .child(
                            div()
                                .text_size(self.s(11.0))
                                .text_color(t.text_faint())
                                .child("new agent"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.start_setup();
                            cx.notify();
                        })),
                );
            }
            SidebarTab::Workers => {
                sb = sb.child(
                    div()
                        .text_size(self.s(10.0))
                        .text_color(t.text_faint())
                        .mb(self.s(6.0))
                        .child("FOCUSED AGENT WORKERS"),
                );
                let workers = self.child_workers(self.focused_agent);
                if workers.is_empty() {
                    sb = sb.child(
                        div()
                            .text_size(self.s(11.0))
                            .text_color(t.text_muted())
                            .child("No delegated workers yet."),
                    );
                } else {
                    for worker_idx in workers {
                        let worker = &self.agents[worker_idx];
                        let task_title = worker
                            .worker_assignment
                            .as_ref()
                            .map(|assignment| assignment.task_title.clone())
                            .unwrap_or_else(|| "worker".into());
                        sb = sb.child(
                            div()
                                .id(ElementId::Name(format!("worker-{}", worker_idx).into()))
                                .w_full()
                                .px(self.s(8.0))
                                .py(self.s(5.0))
                                .rounded(self.s(6.0))
                                .bg(if worker_idx == self.focused_agent {
                                    surface_raised
                                } else {
                                    rgba(0x00000000)
                                })
                                .cursor_pointer()
                                .flex()
                                .flex_col()
                                .gap(self.s(2.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(self.s(6.0))
                                        .child(
                                            div()
                                                .text_size(self.s(10.0))
                                                .text_color(worker.status.color(t))
                                                .child(worker.status.dot()),
                                        )
                                        .child(
                                            div()
                                                .text_size(self.s(12.0))
                                                .text_color(t.text())
                                                .child(worker.name.clone()),
                                        )
                                        .child(div().flex_grow())
                                        .child(
                                            div()
                                                .text_size(self.s(10.0))
                                                .text_color(t.text_faint())
                                                .child(format!(
                                                    "{}@{}",
                                                    worker.runtime_name, worker.target_machine
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(self.s(10.0))
                                        .text_color(t.text_muted())
                                        .child(task_title),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_focus(worker_idx);
                                    cx.notify();
                                })),
                        );
                    }
                }
            }
        }

        sb = sb.child(div().flex_grow());

        // Aggregate stats
        let total_cost: f64 = self.agents.iter().map(|a| a.tokens.cost_usd).sum();
        let total_tokens: u64 = self.agents.iter().map(|a| a.tokens.total_tokens()).sum();
        let working_count = self
            .agents
            .iter()
            .filter(|a| a.status == AgentStatus::Working)
            .count();
        let idle_count = self
            .agents
            .iter()
            .filter(|a| a.status == AgentStatus::Idle)
            .count();
        let total_count = self.agents.len();

        sb = sb.child(
            div()
                .border_t_1()
                .border_color(t.border())
                .pt(self.s(8.0))
                .pb(self.s(4.0))
                .flex()
                .flex_col()
                .gap(self.s(3.0))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(t.text_faint())
                                .child("total cost"),
                        )
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(t.green())
                                .child(format!("${:.2}", total_cost)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(t.text_faint())
                                .child("tokens"),
                        )
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(t.text_muted())
                                .child(format!("{}k", total_tokens / 1000)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(t.text_faint())
                                .child("agents"),
                        )
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(if working_count > 0 {
                                    t.yellow()
                                } else {
                                    t.text_muted()
                                })
                                .child(format!(
                                    "{} total  {} working  {} idle",
                                    total_count, working_count, idle_count
                                )),
                        ),
                ),
        );

        let (ml, mc) = match self.mode {
            Mode::Normal => ("", t.text_faint()),
            Mode::Palette => ("PALETTE", t.yellow()),
            Mode::Setup => ("SETUP", t.yellow()),
            Mode::Search => ("SEARCH", t.yellow()),
            Mode::AgentMenu => ("MENU", t.green()),
            Mode::Settings => ("SETTINGS", t.blue()),
        };
        let vl = match self.view_mode {
            ViewMode::Grid => "grid",
            ViewMode::Pipeline => "pipe",
            ViewMode::Focus => "focus",
        };
        let mode_epoch = self.mode_epoch;
        let mode_badge = div()
            .px(self.s(6.0))
            .py(self.s(2.0))
            .rounded(self.s(4.0))
            .bg(mc)
            .text_size(self.s(10.0))
            .text_color(t.bg())
            .child(ml)
            .with_animation(
                ElementId::Name(format!("mode-{}", mode_epoch).into()),
                Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            );
        sb = sb.child(
            div()
                .mt(self.s(8.0))
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .flex()
                        .gap(self.s(6.0))
                        .items_center()
                        .child(mode_badge)
                        .child(
                            div()
                                .px(self.s(5.0))
                                .py(self.s(2.0))
                                .rounded(self.s(4.0))
                                .bg(self.bg_alpha(t.surface_raised()))
                                .text_size(self.s(10.0))
                                .text_color(t.text_muted())
                                .child(vl),
                        ),
                )
                .child(
                    div()
                        .text_size(self.s(10.0))
                        .text_color(t.text_faint())
                        .child(format!("{}%", (self.ui_scale * 100.0) as u32)),
                ),
        );
        sb
    }

    fn render_sparkline(
        &self,
        data: &[u64],
        color: Rgba,
        width: f32,
        height: f32,
    ) -> impl IntoElement {
        let bars: Vec<(f32, f32)> = if data.len() < 2 {
            Vec::new()
        } else {
            let min_val = *data.iter().min().unwrap_or(&0);
            let max_val = *data.iter().max().unwrap_or(&1);
            let range = if max_val == min_val {
                1.0
            } else {
                (max_val - min_val) as f32
            };
            let n = data.len() as f32;
            data.iter()
                .enumerate()
                .map(|(i, &v)| {
                    let x_frac = i as f32 / (n - 1.0);
                    let norm = (v - min_val) as f32 / range;
                    (x_frac, norm)
                })
                .collect()
        };
        let c = color;
        canvas(
            move |_bounds, _window, _cx| {},
            move |bounds, _, window, _cx| {
                let w: f32 = bounds.size.width.into();
                let h: f32 = bounds.size.height.into();
                let ox: f32 = bounds.origin.x.into();
                let oy: f32 = bounds.origin.y.into();
                let bar_w: f32 = 1.5;
                for &(x_frac, norm) in &bars {
                    let bar_h = (norm * h).max(1.0);
                    let bx = ox + x_frac * (w - bar_w);
                    let by = oy + h - bar_h;
                    let bar_bounds = Bounds {
                        origin: point(px(bx), px(by)),
                        size: Size {
                            width: px(bar_w),
                            height: px(bar_h),
                        },
                    };
                    window.paint_quad(fill(bar_bounds, c));
                }
            },
        )
        .w(px(width))
        .h(px(height))
    }

    /// Render a line with inline markdown using StyledText + TextRun for proper reflow.
    fn render_styled_line(
        &self,
        line: &str,
        base_color: Rgba,
        t: &ThemeColors,
    ) -> impl IntoElement + use<'_> {
        let spans = parse_spans(line);
        let transcript_font = self.transcript_font();
        let base_font = Font {
            family: transcript_font,
            features: FontFeatures::default(),
            fallbacks: None,
            weight: FontWeight::default(),
            style: FontStyle::Normal,
        };

        let mut full_text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();

        for span in &spans {
            let (text, font, color, bg) = match span {
                Span::Text(s) => (s.as_str(), base_font.clone(), base_color, None),
                Span::Code(s) => (
                    s.as_str(),
                    Font {
                        family: SharedString::from(if cfg!(target_os = "macos") {
                            "Menlo"
                        } else {
                            "Monospace"
                        }),
                        weight: FontWeight::default(),
                        style: FontStyle::Normal,
                        ..base_font.clone()
                    },
                    t.user_input(),
                    Some(self.bg_alpha(t.surface_raised())),
                ),
                Span::Bold(s) => (
                    s.as_str(),
                    Font {
                        weight: FontWeight::BOLD,
                        ..base_font.clone()
                    },
                    base_color,
                    None,
                ),
                Span::Italic(s) => (
                    s.as_str(),
                    Font {
                        style: FontStyle::Italic,
                        ..base_font.clone()
                    },
                    t.text_muted(),
                    None,
                ),
                Span::BoldItalic(s) => (
                    s.as_str(),
                    Font {
                        weight: FontWeight::BOLD,
                        style: FontStyle::Italic,
                        ..base_font.clone()
                    },
                    base_color,
                    None,
                ),
            };
            let byte_len = text.len();
            if byte_len > 0 {
                full_text.push_str(text);
                runs.push(TextRun {
                    len: byte_len,
                    font,
                    color: color.into(),
                    background_color: bg.map(|c| c.into()),
                    underline: None,
                    strikethrough: None,
                });
            }
        }

        // If no runs (empty line), push a single space to satisfy StyledText
        if runs.is_empty() {
            full_text.push(' ');
            runs.push(TextRun {
                len: 1,
                font: base_font,
                color: base_color.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }

        StyledText::new(SharedString::from(full_text)).with_runs(runs)
    }

    fn render_agent_tile(&self, idx: usize, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let a = &self.agents[idx];
        let t = &self.theme;
        let focused = idx == self.focused_agent;
        let rt_color = t.runtime_color(&a.runtime_name);
        let bc = if focused { rt_color } else { t.border() };

        // Shadow: runtime-colored glow for focused, hover glow for hovered, subtle for unfocused
        let is_hovered = self.hovered_tile == Some(idx) && !focused;
        let tile_shadow = if focused {
            vec![
                BoxShadow {
                    color: Rgba {
                        r: rt_color.r,
                        g: rt_color.g,
                        b: rt_color.b,
                        a: 0.35,
                    }
                    .into(),
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
        } else if is_hovered {
            vec![
                BoxShadow {
                    color: Rgba {
                        r: rt_color.r,
                        g: rt_color.g,
                        b: rt_color.b,
                        a: 0.18,
                    }
                    .into(),
                    offset: point(px(0.), px(0.)),
                    blur_radius: self.s(12.0),
                    spread_radius: self.s(1.0),
                },
                BoxShadow {
                    color: t.shadow().into(),
                    offset: point(px(0.), self.s(3.0)),
                    blur_radius: self.s(10.0),
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
            .flex_grow()
            .flex_shrink()
            .flex_basis(px(0.))
            .min_w(px(0.))
            .h_full()
            .bg(self.bg_alpha(t.bg()))
            .flex()
            .flex_col()
            .overflow_hidden()
            .cursor_pointer()
            .shadow(tile_shadow)
            .on_mouse_move(
                cx.listener(move |this, _event: &MouseMoveEvent, _window, cx| {
                    if this.hovered_tile != Some(idx) {
                        this.hovered_tile = Some(idx);
                        cx.notify();
                    }
                }),
            )
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
            if secs < 60 {
                format!("{}s", secs)
            } else {
                format!("{}m{}s", secs / 60, secs % 60)
            }
        } else {
            String::new()
        };

        let pct = a.tokens.context_usage_pct();
        let token_info = format!(
            "{}k/{}k ${:.3}",
            a.tokens.total_tokens() / 1000,
            a.tokens.context_window / 1000,
            a.tokens.cost_usd,
        );

        let mut header = div()
            .w_full()
            .min_w(px(0.))
            .px(self.s(10.0))
            .pt(px(36.0))
            .pb(self.s(4.0))
            .bg(self.bg_alpha(t.surface()))
            .border_b_1()
            .border_color(t.border())
            .flex()
            .items_center()
            .gap(self.s(6.0))
            .overflow_hidden();

        // Squirrel icon: spinning when working, static otherwise
        if is_working {
            header = header.child(
                svg()
                    .path("assets/squirrel_spin.svg")
                    .size(self.s(13.0))
                    .text_color(status_color)
                    .with_animation(
                        ElementId::Name(format!("spin-{}", idx).into()),
                        Animation::new(Duration::from_millis(1200))
                            .repeat()
                            .with_easing(ease_in_out),
                        |sv, delta| {
                            sv.with_transformation(Transformation::rotate(percentage(delta)))
                        },
                    ),
            );
        } else {
            header = header.child(
                svg()
                    .path("assets/squirrel.svg")
                    .size(self.s(12.0))
                    .text_color(status_color),
            );
        }

        // Name
        header = header.child(
            div()
                .text_size(self.s(12.0))
                .text_color(t.text())
                .child(format!("{}{}", a.name, fav_label)),
        );

        // Status dot + label
        header = header.child(
            div()
                .text_size(self.s(10.0))
                .text_color(status_color)
                .child(a.status.label()),
        );

        // Elapsed time
        if !elapsed_str.is_empty() {
            header = header.child(
                div()
                    .text_size(self.s(9.0))
                    .text_color(t.yellow())
                    .child(elapsed_str),
            );
        }

        // Spacer
        header = header.child(div().flex_grow());

        // Model | tokens | cost — compact right side
        header = header
            .child(
                div()
                    .text_size(self.s(9.0))
                    .text_color(t.text_muted())
                    .child(a.tokens.model.clone()),
            )
            .child(
                div()
                    .text_size(self.s(9.0))
                    .text_color(t.text_faint())
                    .child(token_info),
            );

        // Token usage sparkline
        if a.token_history.len() >= 2 {
            let spark_color = Rgba {
                r: rt_color.r,
                g: rt_color.g,
                b: rt_color.b,
                a: 0.7,
            };
            header = header.child(self.render_sparkline(&a.token_history, spark_color, 60.0, 16.0));
        }

        // Context usage percentage
        header = header.child(
            div()
                .text_size(self.s(9.0))
                .text_color(if pct > 80.0 {
                    t.red()
                } else if pct > 50.0 {
                    t.yellow()
                } else {
                    t.text_faint()
                })
                .child(format!("{:.0}%", pct)),
        );

        // Inline action icons
        let btn_size = self.s(11.0);
        let btn_color = t.text_muted();
        let btn_hover = t.text();
        let red = t.red();
        let sr = t.surface_raised();

        if a.status == AgentStatus::Interrupted && a.pending_prompt.is_some() {
            header = header.child(
                div()
                    .id(ElementId::Name(format!("btn-continue-{}", idx).into()))
                    .px(self.s(4.0))
                    .py(self.s(2.0))
                    .rounded(self.s(4.0))
                    .cursor_pointer()
                    .text_size(btn_size)
                    .text_color(t.yellow())
                    .hover(|s| s.bg(sr).text_color(t.text()))
                    .child("↺")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.continue_pending_turn(idx, cx);
                        cx.notify();
                    })),
            );
        }

        let wd = a.working_dir.clone();
        if !wd.is_empty() {
            header = header.child(
                div()
                    .id(ElementId::Name(format!("btn-term-{}", idx).into()))
                    .px(self.s(4.0))
                    .py(self.s(2.0))
                    .rounded(self.s(4.0))
                    .cursor_pointer()
                    .text_size(btn_size)
                    .text_color(btn_color)
                    .hover(|s| s.bg(sr).text_color(btn_hover))
                    .child("↗")
                    .on_click(cx.listener(move |this, _, _, _cx| {
                        let dir = if idx < this.agents.len() {
                            this.agents[idx].working_dir.clone()
                        } else {
                            String::new()
                        };
                        if !dir.is_empty() {
                            let _ = Command::new("open")
                                .arg("-a")
                                .arg("Terminal")
                                .arg(&dir)
                                .spawn();
                        }
                    })),
            );
        }

        header = header.child(
            div()
                .id(ElementId::Name(format!("btn-restart-{}", idx).into()))
                .px(self.s(4.0))
                .py(self.s(2.0))
                .rounded(self.s(4.0))
                .cursor_pointer()
                .text_size(btn_size)
                .text_color(btn_color)
                .hover(|s| s.bg(sr).text_color(btn_hover))
                .child("↻")
                .on_click(cx.listener(move |this, _, _w, cx| {
                    this.set_focus(idx);
                    this.restart_agent(&RestartAgent, _w, cx);
                })),
        );

        header = header.child(
            div()
                .id(ElementId::Name(format!("btn-kill-{}", idx).into()))
                .px(self.s(4.0))
                .py(self.s(2.0))
                .rounded(self.s(4.0))
                .cursor_pointer()
                .text_size(btn_size)
                .text_color(btn_color)
                .hover(|s| s.bg(sr).text_color(red))
                .child("✕")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.confirm_remove_agent = Some(idx);
                    cx.notify();
                })),
        );

        tile = tile.child(header);

        // Terminal tile: render the terminal emulator directly, skip transcript/input
        if a.is_terminal {
            if let Some(ref terminal_entity) = a.terminal_entity {
                tile = tile.child(
                    div()
                        .flex_grow()
                        .flex_shrink()
                        .w_full()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .child(terminal_entity.clone()),
                );
            } else {
                tile = tile.child(
                    div()
                        .flex_grow()
                        .w_full()
                        .p(self.s(20.0))
                        .text_size(self.s(12.0))
                        .text_color(t.text_muted())
                        .child("Terminal failed to start"),
                );
            }
            return tile.into_any_element();
        }

        // Compact badges row with agent menu trigger
        let tc_summary = a.tool_calls.summary();
        let sr = self.bg_alpha(t.surface_raised());
        {
            let mut badges = div()
                .w_full()
                .px(self.s(10.0))
                .py(self.s(3.0))
                .bg(self.bg_alpha(t.surface()))
                .border_b_1()
                .border_color(t.border())
                .flex()
                .items_center()
                .gap(self.s(4.0))
                .overflow_hidden();

            if a.tokens.thinking_enabled {
                badges = badges.child(
                    div()
                        .px(self.s(4.0))
                        .py(self.s(1.0))
                        .rounded(self.s(3.0))
                        .bg(sr)
                        .text_size(self.s(8.0))
                        .text_color(t.yellow())
                        .child("think"),
                );
            }
            if !a.runtime_info.plugins.is_empty() {
                let n = a.runtime_info.plugins.len();
                badges = badges.child(
                    div()
                        .px(self.s(4.0))
                        .py(self.s(1.0))
                        .rounded(self.s(3.0))
                        .bg(sr)
                        .text_size(self.s(8.0))
                        .text_color(t.blue_muted())
                        .child(if n == 1 {
                            "1 plugin".into()
                        } else {
                            format!("{} plugins", n)
                        }),
                );
            }
            if !a.runtime_info.mcps.is_empty() {
                let n = a.runtime_info.mcps.len();
                badges = badges.child(
                    div()
                        .px(self.s(4.0))
                        .py(self.s(1.0))
                        .rounded(self.s(3.0))
                        .bg(sr)
                        .text_size(self.s(8.0))
                        .text_color(t.green())
                        .child(if n == 1 {
                            "1 mcp".into()
                        } else {
                            format!("{} mcps", n)
                        }),
                );
            }
            if !tc_summary.is_empty() {
                let tc_recent = a
                    .last_tool_call_at
                    .map(|t| t.elapsed() < Duration::from_millis(600))
                    .unwrap_or(false);
                let tc_badge = div()
                    .px(self.s(4.0))
                    .py(self.s(1.0))
                    .rounded(self.s(3.0))
                    .bg(sr)
                    .text_size(self.s(8.0))
                    .text_color(if tc_recent { t.green() } else { t.text_muted() })
                    .child(tc_summary);
                if tc_recent {
                    let tc_count = a.tool_calls.total();
                    badges = badges.child(tc_badge.with_animation(
                        ElementId::Name(format!("tc-bump-{}-{}", idx, tc_count).into()),
                        Animation::new(Duration::from_millis(400)).with_easing(ease_in_out),
                        |el, delta| el.opacity(0.5 + 0.5 * delta),
                    ));
                } else {
                    badges = badges.child(tc_badge);
                }
            }
            if !a.auto_scroll {
                badges = badges.child(
                    div()
                        .px(self.s(4.0))
                        .py(self.s(1.0))
                        .rounded(self.s(3.0))
                        .bg(sr)
                        .text_size(self.s(8.0))
                        .text_color(t.yellow())
                        .child("pinned")
                        .with_animation(
                            ElementId::Name(format!("pin-{}", idx).into()),
                            Animation::new(Duration::from_secs(2))
                                .repeat()
                                .with_easing(pulsating_between(0.6, 1.0)),
                            |el, delta| el.opacity(delta),
                        ),
                );
            }

            // Spacer + agent menu trigger button
            badges = badges.child(div().flex_grow());
            badges = badges.child(
                div()
                    .id(ElementId::Name(format!("amenu-btn-{}", idx).into()))
                    .px(self.s(6.0))
                    .py(self.s(1.0))
                    .rounded(self.s(3.0))
                    .cursor_pointer()
                    .text_size(self.s(9.0))
                    .text_color(t.text_faint())
                    .hover(|s| s.bg(sr).text_color(t.text()))
                    .child("...")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_agent_menu(idx, cx);
                    })),
            );

            tile = tile.child(badges);
        }

        if a.role == AgentRole::Coordinator {
            let workers = self.child_workers(idx);
            if !workers.is_empty() {
                let mut worker_strip = div()
                    .w_full()
                    .px(self.s(14.0))
                    .py(self.s(6.0))
                    .bg(self.bg_alpha(t.surface()))
                    .border_b_1()
                    .border_color(t.border())
                    .flex()
                    .flex_col()
                    .gap(self.s(4.0))
                    .child(
                        div()
                            .text_size(self.s(10.0))
                            .text_color(t.text_faint())
                            .child("workers"),
                    );
                for worker_idx in workers {
                    let worker = &self.agents[worker_idx];
                    let task_title = worker
                        .worker_assignment
                        .as_ref()
                        .map(|assignment| assignment.task_title.clone())
                        .unwrap_or_else(|| "worker".into());
                    worker_strip = worker_strip.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(self.s(6.0))
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(worker.status.color(t))
                                    .child(worker.status.dot()),
                            )
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text())
                                    .child(worker.name.clone()),
                            )
                            .child(div().flex_grow())
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_muted())
                                    .child(task_title),
                            ),
                    );
                }
                tile = tile.child(worker_strip);
            }
        }

        if let Some(notice) = &a.restore_notice {
            tile = tile.child(
                div()
                    .w_full()
                    .px(self.s(14.0))
                    .py(self.s(10.0))
                    .bg(self.bg_alpha(t.surface()))
                    .border_b_1()
                    .border_color(t.border())
                    .child(
                        div()
                            .w_full()
                            .px(self.s(10.0))
                            .py(self.s(8.0))
                            .rounded(self.s(8.0))
                            .bg(self.bg_alpha(t.surface_raised()))
                            .text_size(self.s(12.0))
                            .text_color(t.text_muted())
                            .child(notice.clone()),
                    ),
            );
        }

        // Output area with markdown rendering
        let fs = self.font_size;
        let transcript_font = self.transcript_font();
        let mut out = div()
            .id(ElementId::Name(format!("output-{}", idx).into()))
            .flex_grow()
            .flex_shrink()
            .w_full()
            .min_w(px(0.))
            .max_w_full()
            .px(self.s(14.0))
            .py(self.s(8.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .font_family(transcript_font.clone())
            .text_size(self.s(fs))
            .line_height(self.s(fs + 8.0))
            .gap(self.s(8.0))
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                if idx >= this.agents.len() {
                    return;
                }
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
                        a.scroll_offset =
                            a.scroll_offset.saturating_sub((-lines_to_scroll) as usize);
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
                div()
                    .id(ElementId::Name(format!("empty-click-{}", idx).into()))
                    .flex_grow()
                    .w_full()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_focus(idx);
                        this.set_mode(Mode::Normal);
                        cx.notify();
                    })),
            );
        }
        let start = a.scroll_offset.min(a.output_lines.len());
        let lines = &a.output_lines[start..];

        // Pre-pass: split lines into turns (user input vs agent response blocks)
        struct Turn {
            is_user: bool,
            start: usize,
            end: usize,
        }
        let mut turns: Vec<Turn> = Vec::new();
        {
            let mut i = 0;
            while i < lines.len() {
                if classify_line(&lines[i]) == LineKind::UserInput {
                    turns.push(Turn {
                        is_user: true,
                        start: i,
                        end: i + 1,
                    });
                    i += 1;
                    // Skip blank lines after user input
                    while i < lines.len() && lines[i].trim().is_empty() {
                        i += 1;
                    }
                } else {
                    // Agent response: collect until next user input
                    let block_start = i;
                    while i < lines.len() && classify_line(&lines[i]) != LineKind::UserInput {
                        i += 1;
                    }
                    if block_start < i {
                        turns.push(Turn {
                            is_user: false,
                            start: block_start,
                            end: i,
                        });
                    }
                }
            }
        }

        for turn in &turns {
            if turn.is_user {
                let line = &lines[turn.start];
                let display_text = line.strip_prefix("> ").unwrap_or(line);
                out = out.child(
                    div()
                        .w_full()
                        .px(self.s(12.0))
                        .py(self.s(10.0))
                        .rounded(self.s(10.0))
                        .bg(self.bg_alpha(t.surface()))
                        .border_1()
                        .border_color(t.blue())
                        .child(
                            div()
                                .text_color(t.blue())
                                .text_size(self.s(fs))
                                .child(display_text.to_string()),
                        ),
                );
                continue;
            }

            // Agent response: render as ONE card
            let block_lines = &lines[turn.start..turn.end];
            let full_text: String = block_lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");

            if full_text.is_empty() {
                continue;
            }

            let mut card = div()
                .flex_grow()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(self.s(3.0));
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
                                div()
                                    .w_full()
                                    .px(self.s(8.0))
                                    .pt(self.s(6.0))
                                    .pb(self.s(2.0))
                                    .bg(self.bg_alpha(t.surface_raised()))
                                    .rounded_t(self.s(4.0))
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_faint())
                                    .child(lang),
                            );
                        }
                        j += 1;
                        continue;
                    }
                }

                // Inside code block
                if in_code_block {
                    card = card.child(
                        div()
                            .w_full()
                            .px(self.s(8.0))
                            .py(self.s(1.0))
                            .bg(self.bg_alpha(t.surface_raised()))
                            .text_size(self.s(fs - 1.0))
                            .text_color(t.text())
                            .font_family(SharedString::from(if cfg!(target_os = "macos") {
                                "Menlo"
                            } else {
                                "Monospace"
                            }))
                            .child(bline.clone()),
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
                        div()
                            .flex_grow()
                            .pt(self.s(6.0))
                            .pb(self.s(2.0))
                            .text_size(self.s(size))
                            .text_color(t.text())
                            .font_weight(FontWeight::BOLD)
                            .child(content.to_string()),
                    );
                    j += 1;
                    continue;
                }

                // Bullet point
                if let Some((indent, content)) = parse_bullet(bline) {
                    let indent_px = self.s(indent as f32 * 16.0 + 4.0);
                    let styled = self.render_styled_line(content, t.text(), t);
                    let row = div()
                        .w_full()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .pl(indent_px)
                        .flex()
                        .items_start()
                        .gap(self.s(6.0))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(t.text_muted())
                                .child("  -"),
                        )
                        .child(div().flex_shrink().min_w(px(0.)).child(styled));
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
                    let mut line_div = div()
                        .text_color(text_color)
                        .w_full()
                        .min_w(px(0.))
                        .px(self.s(4.0))
                        .rounded(self.s(2.0));
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
                let styled = self.render_styled_line(bline, t.text(), t);
                card = card.child(div().w_full().min_w(px(0.)).child(styled));
                j += 1;
            }

            out = out.child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .px(self.s(8.0))
                    .py(self.s(6.0))
                    .flex()
                    .items_start()
                    .gap(self.s(8.0))
                    .child(card)
                    .child(self.render_copy_icon(
                        full_text,
                        format!("copy-{}-{}", idx, turn.start),
                        cx,
                    )),
            );
        }
        tile = tile.child(out);

        // Input bar — always active
        if focused {
            let tc = t.text();
            let pc = t.blue();

            let mut input_area = div()
                .id(ElementId::Name(format!("input-click-{}", idx).into()))
                .w_full()
                .px(self.s(14.0))
                .py(self.s(10.0))
                .bg(self.bg_alpha(t.surface()))
                .border_t_1()
                .border_color(t.border_focus())
                .flex()
                .flex_col()
                .gap(self.s(4.0))
                .font_family(self.font_family.clone())
                .text_size(self.s(fs))
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
                        div()
                            .flex()
                            .items_start()
                            .gap(self.s(6.0))
                            .child(div().text_color(pc).text_size(self.s(14.0)).child(">"))
                            .child(div().text_color(t.text_faint()).child(prompt_hint)),
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
                            div()
                                .flex()
                                .items_start()
                                .w_full()
                                .child(
                                    div()
                                        .text_color(pc)
                                        .text_size(self.s(14.0))
                                        .pr(self.s(6.0))
                                        .child(prompt_char),
                                )
                                .child(div().text_color(tc).child(before_cursor.to_string()))
                                .child(div().text_color(t.blue()).child("│"))
                                .child(div().text_color(tc).child(after_cursor.to_string()))
                        } else {
                            div()
                                .flex()
                                .items_start()
                                .w_full()
                                .child(
                                    div()
                                        .text_color(pc)
                                        .text_size(self.s(14.0))
                                        .pr(self.s(6.0))
                                        .child(prompt_char),
                                )
                                .child(div().text_color(tc).child(line.to_string()))
                        };
                        input_area = input_area.child(row);
                        byte_offset = line_end + 1; // +1 for the \n
                    }
                }

                // Line count indicator for multiline
                if line_count > 1 {
                    input_area = input_area.child(
                        div()
                            .text_size(self.s(10.0))
                            .text_color(t.text_faint())
                            .child(format!("{} lines", line_count)),
                    );
                }
            }
            tile = tile.child(input_area);
        }
        // Spawn fade-in animation (within first 500ms of agent creation)
        let spawn_age = a.spawn_time.elapsed();
        if spawn_age < Duration::from_millis(500) {
            return tile
                .with_animation(
                    ElementId::Name(
                        format!("spawn-{}-{}", idx, a.spawn_time.elapsed().as_millis()).into(),
                    ),
                    Animation::new(Duration::from_millis(400)).with_easing(ease_out_quint()),
                    |el, delta| el.opacity(delta),
                )
                .into_any_element();
        }
        // Focus glow transition
        if focused {
            let fe = self.focus_epoch;
            return tile
                .with_animation(
                    ElementId::Name(format!("focus-{}-{}", idx, fe).into()),
                    Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
                    |el, delta| el.opacity(0.85 + 0.15 * delta),
                )
                .into_any_element();
        }
        // Breathing pulse for working agents
        if is_working {
            return tile
                .with_animation(
                    ElementId::Name(format!("pulse-{}", idx).into()),
                    Animation::new(Duration::from_millis(2000))
                        .repeat()
                        .with_easing(ease_in_out),
                    |el, delta| {
                        let pulse = 0.75 + 0.25 * (delta * std::f32::consts::PI * 2.0).sin();
                        el.opacity(pulse)
                    },
                )
                .into_any_element();
        }
        tile.into_any_element()
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let mut p = div()
            .absolute()
            .top(self.s(100.0))
            .left(self.s(400.0))
            .w(self.s(500.0))
            .bg(t.palette_bg())
            .border_1()
            .border_color(t.palette_border())
            .rounded(self.s(12.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(px(0.), self.s(8.0)),
                blur_radius: self.s(24.0),
                spread_radius: self.s(4.0),
            }]);

        p = p.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(10.0))
                .border_b_1()
                .border_color(t.palette_border())
                .flex()
                .items_center()
                .gap(self.s(8.0))
                .font_family(self.font_family.clone())
                .text_size(self.s(14.0))
                .child(div().text_color(t.blue()).child(">"))
                .child(
                    div()
                        .text_color(t.text())
                        .child(if self.palette_input.is_empty() {
                            "type a command...".into()
                        } else {
                            format!("{}|", self.palette_input)
                        }),
                ),
        );

        for (i, item) in self.palette_items.iter().enumerate() {
            let sel = i == self.palette_selection;
            p = p.child(
                div()
                    .id(ElementId::Name(format!("pal-{}", i).into()))
                    .w_full()
                    .px(self.s(14.0))
                    .py(self.s(8.0))
                    .cursor_pointer()
                    .bg(if sel {
                        t.selected_row()
                    } else {
                        rgba(0x00000000)
                    })
                    .text_size(self.s(13.0))
                    .text_color(if sel { t.text() } else { t.text_muted() })
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
            Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
            |el, delta| el.opacity(delta),
        )
    }

    fn render_agent_menu(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let idx = self.agent_menu_target;
        let agent_name = if idx < self.agents.len() {
            self.agents[idx].name.clone()
        } else {
            "Agent".into()
        };
        let runtime_name = if idx < self.agents.len() {
            self.agents[idx].runtime_name.clone()
        } else {
            String::new()
        };
        let rt_color = t.runtime_color(&runtime_name);

        let mut menu = div()
            .absolute()
            .top(self.s(80.0))
            .left(self.s(420.0))
            .w(self.s(380.0))
            .bg(t.palette_bg())
            .border_1()
            .border_color(t.palette_border())
            .rounded(self.s(12.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(px(0.), self.s(8.0)),
                blur_radius: self.s(24.0),
                spread_radius: self.s(4.0),
            }]);

        // Header
        menu = menu.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(10.0))
                .border_b_1()
                .border_color(t.palette_border())
                .flex()
                .items_center()
                .gap(self.s(8.0))
                .child(
                    div()
                        .w(self.s(3.0))
                        .h(self.s(14.0))
                        .rounded(self.s(2.0))
                        .bg(rt_color),
                )
                .child(
                    div()
                        .text_size(self.s(13.0))
                        .text_color(t.text())
                        .child(agent_name),
                )
                .child(div().flex_grow())
                .child(
                    div()
                        .text_size(self.s(10.0))
                        .text_color(t.text_faint())
                        .child(runtime_name),
                ),
        );

        // Menu items
        for (i, item) in self.agent_menu_items.iter().enumerate() {
            let sel = i == self.agent_menu_selection;
            menu = menu.child(
                div()
                    .id(ElementId::Name(format!("amenu-{}", i).into()))
                    .w_full()
                    .px(self.s(14.0))
                    .py(self.s(7.0))
                    .cursor_pointer()
                    .bg(if sel {
                        t.selected_row()
                    } else {
                        rgba(0x00000000)
                    })
                    .flex()
                    .flex_col()
                    .gap(self.s(2.0))
                    .child(
                        div()
                            .text_size(self.s(12.0))
                            .text_color(if sel { t.text() } else { t.text_muted() })
                            .child(item.label.clone()),
                    )
                    .child(
                        div()
                            .text_size(self.s(9.0))
                            .text_color(t.text_faint())
                            .child(item.desc.clone()),
                    )
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.agent_menu_selection = i;
                        this.submit_input(&SubmitInput, window, cx);
                    })),
            );
        }

        // Footer hint
        menu = menu.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(6.0))
                .border_t_1()
                .border_color(t.palette_border())
                .text_size(self.s(9.0))
                .text_color(t.text_faint())
                .child("arrows to navigate, enter to execute, esc to close"),
        );

        let epoch = self.mode_epoch;
        menu.with_animation(
            ElementId::Name(format!("agentmenu-{}", epoch).into()),
            Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
            |el, delta| el.opacity(delta),
        )
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let setup = self.setup.as_ref().unwrap();
        let t = &self.theme;
        let show_machine_step =
            self.config.machines.len() > 1 && setup.selected_runtime != "terminal";

        let mut w = div()
            .absolute()
            .top(self.s(80.0))
            .left(self.s(350.0))
            .w(self.s(600.0))
            .bg(t.palette_bg())
            .border_1()
            .border_color(t.palette_border())
            .rounded(self.s(12.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(px(0.), self.s(8.0)),
                blur_radius: self.s(24.0),
                spread_radius: self.s(4.0),
            }]);

        let steps: Vec<(&str, SetupStep)> = if show_machine_step {
            vec![
                ("Runtime", SetupStep::Runtime),
                ("Machine", SetupStep::Machine),
                ("Directory", SetupStep::Directory),
            ]
        } else {
            vec![
                ("Runtime", SetupStep::Runtime),
                ("Directory", SetupStep::Directory),
            ]
        };
        let mut step_row = div()
            .w_full()
            .px(self.s(14.0))
            .py(self.s(10.0))
            .border_b_1()
            .border_color(t.palette_border())
            .flex()
            .items_center()
            .gap(self.s(16.0));
        for (label, step) in steps {
            let active = setup.step == step;
            let c = if active { t.blue() } else { t.text_faint() };
            step_row = step_row.child(div().text_size(self.s(12.0)).text_color(c).child(
                if active {
                    format!("> {}", label)
                } else {
                    label.to_string()
                },
            ));
        }
        w = w.child(step_row);

        match setup.step {
            SetupStep::Runtime => {
                w = w.child(
                    div()
                        .px(self.s(14.0))
                        .py(self.s(8.0))
                        .text_size(self.s(11.0))
                        .text_color(t.text_muted())
                        .child("Select runtime (arrows to move, Tab to continue)"),
                );
                let mut cards = div()
                    .px(self.s(14.0))
                    .py(self.s(10.0))
                    .flex()
                    .flex_wrap()
                    .gap(self.s(10.0));
                for (i, rt) in self.config.runtimes.iter().enumerate() {
                    let sel = i == setup.runtime_cursor;
                    let icon = rt
                        .name
                        .chars()
                        .next()
                        .map(|ch| ch.to_ascii_uppercase().to_string())
                        .unwrap_or_else(|| "?".into());
                    let rt_name = rt.name.clone();
                    cards = cards.child(
                        div()
                            .id(ElementId::Name(format!("srt-card-{}", i).into()))
                            .w(self.s(130.0))
                            .h(self.s(90.0))
                            .rounded(self.s(10.0))
                            .border_1()
                            .border_color(if sel { t.blue() } else { t.palette_border() })
                            .bg(if sel {
                                self.bg_alpha(t.selected_row())
                            } else {
                                self.bg_alpha(t.surface())
                            })
                            .p(self.s(8.0))
                            .cursor_pointer()
                            .flex()
                            .flex_col()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(self.s(24.0))
                                    .text_color(if sel { t.blue() } else { t.text_faint() })
                                    .child(icon),
                            )
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(if sel { t.text() } else { t.text_muted() })
                                    .child(rt_name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(self.s(9.0))
                                    .text_color(t.text_faint())
                                    .child(rt.description.clone()),
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                if let Some(ref mut s) = this.setup {
                                    s.runtime_cursor = i;
                                    s.selected_runtime = rt_name.clone();
                                }
                                // Advance to next step (Machine or Directory)
                                this.setup_next(&SetupNext, _window, cx);
                            })),
                    );
                }
                w = w.child(cards);
            }
            SetupStep::Machine => {
                w = w.child(
                    div()
                        .px(self.s(14.0))
                        .py(self.s(8.0))
                        .text_size(self.s(11.0))
                        .text_color(t.text_muted())
                        .child("Select machine target (arrows to move, Tab to continue)"),
                );
                for (i, machine) in self.config.machines.iter().enumerate() {
                    let sel = i == setup.machine_cursor;
                    let detail = if machine.kind == "ssh" {
                        if machine.host.is_empty() {
                            "ssh".to_string()
                        } else {
                            format!("ssh {}", machine.host)
                        }
                    } else {
                        "local".to_string()
                    };
                    w = w.child(
                        div()
                            .id(ElementId::Name(format!("smachine-{}", i).into()))
                            .w_full()
                            .px(self.s(14.0))
                            .py(self.s(6.0))
                            .cursor_pointer()
                            .bg(if sel {
                                t.selected_row()
                            } else {
                                rgba(0x00000000)
                            })
                            .flex()
                            .items_center()
                            .gap(self.s(10.0))
                            .child(
                                div()
                                    .text_size(self.s(13.0))
                                    .text_color(if sel { t.blue() } else { t.text_faint() })
                                    .child(if sel { ">" } else { " " }),
                            )
                            .child(
                                div()
                                    .text_size(self.s(13.0))
                                    .text_color(if sel { t.text() } else { t.text_muted() })
                                    .child(machine.name.clone()),
                            )
                            .child(div().flex_grow())
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text_faint())
                                    .child(detail),
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                if let Some(ref mut s) = this.setup {
                                    s.machine_cursor = i;
                                }
                                cx.notify();
                            })),
                    );
                }
            }
            SetupStep::Directory => {
                if setup.show_recent && !self.config.recent_projects.is_empty() {
                    w = w.child(
                        div()
                            .px(self.s(14.0))
                            .py(self.s(8.0))
                            .text_size(self.s(11.0))
                            .text_color(t.text_muted())
                            .child("Recent projects"),
                    );
                    for (i, project) in self.config.recent_projects.iter().enumerate() {
                        let sel = i == setup.recent_cursor;
                        let path = project.path.clone();
                        let machine = project.machine.clone();
                        let ago = Self::time_ago(project.last_used);
                        w = w.child(
                            div()
                                .id(ElementId::Name(format!("srecent-{}", i).into()))
                                .w_full()
                                .px(self.s(14.0))
                                .py(self.s(6.0))
                                .cursor_pointer()
                                .bg(if sel {
                                    t.selected_row()
                                } else {
                                    rgba(0x00000000)
                                })
                                .flex()
                                .items_center()
                                .gap(self.s(10.0))
                                .child(
                                    div()
                                        .text_size(self.s(12.0))
                                        .text_color(if sel { t.blue() } else { t.text_faint() })
                                        .child(if sel { ">" } else { " " }),
                                )
                                .child(
                                    div()
                                        .flex_grow()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_size(self.s(12.0))
                                                .text_color(if sel {
                                                    t.text()
                                                } else {
                                                    t.text_muted()
                                                })
                                                .child(path.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(self.s(9.0))
                                                .text_color(t.text_faint())
                                                .child(format!("{}  •  {}", machine, ago)),
                                        ),
                                )
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    if let Some(ref mut s) = this.setup {
                                        s.recent_cursor = i;
                                        s.selected_dir = path.clone();
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    let browse_idx = self.config.recent_projects.len();
                    let browse_sel = setup.recent_cursor == browse_idx;
                    w = w.child(
                        div()
                            .id(ElementId::Name("srecent-browse".into()))
                            .w_full()
                            .px(self.s(14.0))
                            .py(self.s(8.0))
                            .cursor_pointer()
                            .bg(if browse_sel {
                                t.selected_row()
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(self.s(12.0))
                            .text_color(if browse_sel { t.blue() } else { t.text_muted() })
                            .child("+ Browse new directory...")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                if let Some(ref mut s) = this.setup {
                                    s.show_recent = false;
                                    s.recent_cursor = 0;
                                    s.dir_filter.clear();
                                    s.dir_cursor = 0;
                                }
                                cx.notify();
                            })),
                    );
                } else {
                    let filtered = Self::filter_dirs(&setup.dir_entries, &setup.dir_filter);
                    let current_display = setup.selected_dir.clone();

                    // ncdu-style header: current path prominently displayed
                    w = w.child(
                        div()
                            .w_full()
                            .px(self.s(14.0))
                            .py(self.s(8.0))
                            .bg(self.bg_alpha(t.surface_raised()))
                            .border_b_1()
                            .border_color(t.palette_border())
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .text_size(self.s(13.0))
                                    .text_color(t.text())
                                    .child(format!("{}/", current_display)),
                            )
                            .child(div().flex_grow())
                            .child(
                                div()
                                    .text_size(self.s(9.0))
                                    .text_color(t.text_faint())
                                    .child(format!("{} items", filtered.len())),
                            ),
                    );

                    // Filter bar (only if typing)
                    if !setup.dir_filter.is_empty() {
                        w = w.child(
                            div()
                                .w_full()
                                .px(self.s(14.0))
                                .py(self.s(4.0))
                                .flex()
                                .items_center()
                                .gap(self.s(6.0))
                                .child(
                                    div()
                                        .text_size(self.s(11.0))
                                        .text_color(t.blue())
                                        .child("/"),
                                )
                                .child(
                                    div()
                                        .text_size(self.s(12.0))
                                        .text_color(t.text())
                                        .child(format!("{}│", setup.dir_filter)),
                                ),
                        );
                    }

                    // Directory listing (ncdu-style: basenames only, ../ at top)
                    let visible_count = 18;
                    let start = if setup.dir_cursor >= visible_count {
                        setup.dir_cursor - visible_count + 1
                    } else {
                        0
                    };
                    let end = (start + visible_count).min(filtered.len());
                    for (vi, dir) in filtered[start..end].iter().enumerate() {
                        let list_idx = start + vi;
                        let sel = list_idx == setup.dir_cursor;
                        let full_path = dir.clone();
                        // Show only the basename (ncdu-style)
                        let basename = std::path::Path::new(&full_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| full_path.clone());
                        w = w.child(
                            div()
                                .id(ElementId::Name(format!("sdir-{}", list_idx).into()))
                                .w_full()
                                .px(self.s(14.0))
                                .py(self.s(3.0))
                                .cursor_pointer()
                                .bg(if sel {
                                    t.selected_row()
                                } else {
                                    rgba(0x00000000)
                                })
                                .flex()
                                .items_center()
                                .gap(self.s(8.0))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .text_size(self.s(12.0))
                                        .text_color(if sel { t.blue() } else { t.text_faint() })
                                        .child(if sel { ">" } else { " " }),
                                )
                                .child(
                                    div()
                                        .text_size(self.s(11.0))
                                        .text_color(t.blue())
                                        .child("/"),
                                )
                                .child(
                                    div()
                                        .flex_shrink()
                                        .min_w(px(0.))
                                        .text_size(self.s(12.0))
                                        .text_color(if sel { t.text() } else { t.text_muted() })
                                        .child(format!("{}/", basename)),
                                )
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    if let Some(ref mut s) = this.setup {
                                        s.dir_cursor = list_idx;
                                        s.selected_dir = full_path.clone();
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    if filtered.is_empty() {
                        w = w.child(
                            div()
                                .px(self.s(14.0))
                                .py(self.s(12.0))
                                .text_size(self.s(11.0))
                                .text_color(t.text_faint())
                                .child("Empty directory"),
                        );
                    }
                    if filtered.len() > visible_count {
                        w = w.child(
                            div()
                                .px(self.s(14.0))
                                .py(self.s(4.0))
                                .text_size(self.s(10.0))
                                .text_color(t.text_faint())
                                .child(format!(
                                    "showing {}-{} of {}",
                                    start + 1,
                                    end,
                                    filtered.len()
                                )),
                        );
                    }
                }

                w = w.child(
                    div()
                        .px(self.s(14.0))
                        .py(self.s(8.0))
                        .text_size(self.s(10.0))
                        .text_color(if setup.worktree_mode {
                            t.green()
                        } else {
                            t.text_faint()
                        })
                        .child(if setup.worktree_mode {
                            "worktree: on"
                        } else {
                            "worktree: off"
                        }),
                );
            }
        }

        let tab_hint = if setup.step == SetupStep::Directory {
            if setup.show_recent {
                "Enter: select  Tab: create"
            } else if setup.editing_agent.is_some() {
                "Enter: open  ⌫: parent  Tab: select"
            } else {
                "Enter: open  ⌫: parent  Tab: select"
            }
        } else {
            "Tab: next"
        };

        w = w.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(8.0))
                .border_t_1()
                .border_color(t.palette_border())
                .flex()
                .gap(self.s(16.0))
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child(tab_hint),
                )
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("Shift-Tab: back"),
                )
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("Space: worktree"),
                )
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("Esc: cancel"),
                ),
        );

        let epoch = self.mode_epoch;
        w.with_animation(
            ElementId::Name(format!("setup-slide-{}", epoch).into()),
            Animation::new(Duration::from_millis(250)).with_easing(ease_out_quint()),
            |el, delta| el.opacity(delta),
        )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> Div {
        let t = &self.theme;
        let section = self.settings.section;
        let current_item = self.settings.item_cursor;
        let mut sidebar = div()
            .w(self.s(230.0))
            .h_full()
            .bg(self.bg_alpha(t.surface()))
            .border_r_1()
            .border_color(t.border())
            .p(self.s(12.0))
            .flex()
            .flex_col()
            .gap(self.s(6.0));

        sidebar = sidebar.child(
            div()
                .px(self.s(8.0))
                .py(self.s(6.0))
                .text_size(self.s(11.0))
                .text_color(t.text_faint())
                .child("Settings"),
        );

        for section_item in SettingsSection::ALL {
            let is_active = section_item == section;
            sidebar = sidebar.child(
                div()
                    .id(ElementId::Name(
                        format!("settings-section-{}", section_item.label()).into(),
                    ))
                    .w_full()
                    .px(self.s(10.0))
                    .py(self.s(8.0))
                    .rounded(self.s(6.0))
                    .cursor_pointer()
                    .bg(if is_active {
                        self.bg_alpha(t.selected_row())
                    } else {
                        rgba(0x00000000)
                    })
                    .text_size(self.s(12.0))
                    .text_color(if is_active { t.text() } else { t.text_muted() })
                    .child(if is_active {
                        format!("> {}", section_item.label())
                    } else {
                        section_item.label().to_string()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.section = section_item;
                        this.settings.item_cursor = 0;
                        this.clamp_settings_cursor();
                        cx.notify();
                    })),
            );
        }

        let mut content = div()
            .flex_grow()
            .h_full()
            .p(self.s(16.0))
            .flex()
            .flex_col()
            .gap(self.s(10.0))
            .overflow_hidden();

        content = content.child(
            div()
                .w_full()
                .pb(self.s(8.0))
                .border_b_1()
                .border_color(t.border())
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_size(self.s(18.0))
                        .text_color(t.text())
                        .child(section.label().to_string()),
                )
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("Arrows: navigate  Enter: edit  +/-: adjust  Esc: close"),
                ),
        );

        match section {
            SettingsSection::Appearance => {
                let row_bg = |selected: bool| {
                    if selected {
                        self.bg_alpha(t.selected_row())
                    } else {
                        self.bg_alpha(t.surface())
                    }
                };
                let row_border = |selected: bool| if selected { t.blue() } else { t.border() };

                let theme_selected = current_item == 0;
                let mut swatches = div().flex().flex_wrap().gap(self.s(6.0));
                for theme in &self.themes {
                    let is_active = theme.name == self.config.theme;
                    let theme_name = theme.name.clone();
                    swatches = swatches.child(
                        div()
                            .id(ElementId::Name(format!("settings-theme-{}", theme_name).into()))
                            .w(self.s(22.0))
                            .h(self.s(14.0))
                            .rounded(self.s(3.0))
                            .border_1()
                            .border_color(if is_active { t.blue() } else { t.border() })
                            .bg(rgba(theme.colors.bg))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_theme(&theme_name);
                                cx.notify();
                            })),
                    );
                }
                content = content.child(
                    div()
                        .id("settings-appearance-theme")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(theme_selected))
                        .bg(row_bg(theme_selected))
                        .cursor_pointer()
                        .flex()
                        .flex_col()
                        .gap(self.s(6.0))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(self.s(13.0))
                                        .text_color(t.text())
                                        .child("Theme"),
                                )
                                .child(
                                    div()
                                        .text_size(self.s(12.0))
                                        .text_color(t.blue())
                                        .child(self.config.theme.clone()),
                                ),
                        )
                        .child(swatches)
                        .child(
                            div()
                                .text_size(self.s(10.0))
                                .text_color(t.text_faint())
                                .child("Press Enter to cycle theme"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 0;
                            cx.notify();
                        })),
                );

                let family_selected = current_item == 1;
                content = content.child(
                    div()
                        .id("settings-appearance-font-family")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(family_selected))
                        .bg(row_bg(family_selected))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Font Family"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(t.text_muted())
                                .child(self.font_family.clone()),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 1;
                            cx.notify();
                        })),
                );

                let size_selected = current_item == 2;
                content = content.child(
                    div()
                        .id("settings-appearance-font-size")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(size_selected))
                        .bg(row_bg(size_selected))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Font Size"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(t.text_muted())
                                .child(format!("{:.0}px", self.font_size)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 2;
                            cx.notify();
                        })),
                );

                let opacity_selected = current_item == 3;
                content = content.child(
                    div()
                        .id("settings-appearance-bg-opacity")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(opacity_selected))
                        .bg(row_bg(opacity_selected))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Background Opacity"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(t.text_muted())
                                .child(format!("{:.0}%", self.config.bg_opacity * 100.0)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 3;
                            cx.notify();
                        })),
                );

                let blur_selected = current_item == 4;
                content = content.child(
                    div()
                        .id("settings-appearance-bg-blur")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(blur_selected))
                        .bg(row_bg(blur_selected))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Background Blur"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(t.text_muted())
                                .child(format!("{:.0}", self.config.bg_blur)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 4;
                            cx.notify();
                        })),
                );
            }
            SettingsSection::Behavior => {
                let row_bg = |selected: bool| {
                    if selected {
                        self.bg_alpha(t.selected_row())
                    } else {
                        self.bg_alpha(t.surface())
                    }
                };
                let row_border = |selected: bool| if selected { t.blue() } else { t.border() };
                let default_view = ViewMode::from_str(&self.config.default_view_mode);
                let default_view_label = match default_view {
                    ViewMode::Grid => "Grid",
                    ViewMode::Pipeline => "Pipeline",
                    ViewMode::Focus => "Focus",
                };

                content = content.child(
                    div()
                        .id("settings-behavior-cautious-enter")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(current_item == 0))
                        .bg(row_bg(current_item == 0))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Cautious Enter"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(if self.config.cautious_enter {
                                    t.green()
                                } else {
                                    t.text_muted()
                                })
                                .child(if self.config.cautious_enter { "On" } else { "Off" }),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 0;
                            cx.notify();
                        })),
                );

                content = content.child(
                    div()
                        .id("settings-behavior-terminal-text")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(current_item == 1))
                        .bg(row_bg(current_item == 1))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Terminal Text"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(if self.config.terminal_text {
                                    t.green()
                                } else {
                                    t.text_muted()
                                })
                                .child(if self.config.terminal_text { "On" } else { "Off" }),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 1;
                            cx.notify();
                        })),
                );

                content = content.child(
                    div()
                        .id("settings-behavior-default-view")
                        .w_full()
                        .p(self.s(10.0))
                        .rounded(self.s(8.0))
                        .border_1()
                        .border_color(row_border(current_item == 2))
                        .bg(row_bg(current_item == 2))
                        .cursor_pointer()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(13.0))
                                .text_color(t.text())
                                .child("Default View Mode"),
                        )
                        .child(
                            div()
                                .text_size(self.s(12.0))
                                .text_color(t.text_muted())
                                .child(default_view_label),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings.item_cursor = 2;
                            cx.notify();
                        })),
                );
            }
            SettingsSection::Runtimes => {
                content = content.child(
                    div()
                        .text_size(self.s(13.0))
                        .text_color(t.text_muted())
                        .child("Runtime management is coming soon in this screen."),
                );
            }
            SettingsSection::Machines => {
                content = content.child(
                    div()
                        .text_size(self.s(13.0))
                        .text_color(t.text_muted())
                        .child("Machine management is coming soon in this screen."),
                );
            }
            SettingsSection::Mcps => {
                content = content.child(
                    div()
                        .text_size(self.s(13.0))
                        .text_color(t.text_muted())
                        .child("MCP management is coming soon in this screen."),
                );
            }
            SettingsSection::Keybinds => {
                content = content
                    .child(
                        div()
                            .text_size(self.s(13.0))
                            .text_color(t.text_muted())
                            .child("Read-only for now. Editable keybinds are planned."),
                    )
                    .child(
                        div()
                            .mt(self.s(6.0))
                            .flex()
                            .flex_col()
                            .gap(self.s(4.0))
                            .child(self.shortcut_row("Cmd+,", "open settings"))
                            .child(self.shortcut_row("Cmd+K", "command palette"))
                            .child(self.shortcut_row("Cmd+N", "new agent setup"))
                            .child(self.shortcut_row("Cmd+F", "search transcripts"))
                            .child(self.shortcut_row("Esc", "close overlay")),
                    );
            }
        }

        div()
            .flex_grow()
            .min_h(px(0.))
            .w_full()
            .flex()
            .overflow_hidden()
            .child(sidebar)
            .child(content)
    }

    fn render_stats(&self) -> Div {
        let t = &self.theme;
        let mut panel = div()
            .absolute()
            .top(self.s(0.0))
            .right(self.s(0.0))
            .w(self.s(500.0))
            .h_full()
            .bg(t.palette_bg())
            .border_l_1()
            .border_color(t.palette_border())
            .py(self.s(16.0))
            .px(self.s(20.0))
            .flex()
            .flex_col()
            .gap(self.s(8.0))
            .overflow_hidden()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(self.s(-4.0), px(0.)),
                blur_radius: self.s(16.0),
                spread_radius: px(0.),
            }]);

        panel = panel.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(self.s(16.0))
                        .text_color(t.text())
                        .font_weight(FontWeight::BOLD)
                        .child("Usage Stats"),
                )
                .child(
                    div()
                        .text_size(self.s(12.0))
                        .text_color(t.text_faint())
                        .child("[?] to close"),
                ),
        );

        // Aggregate totals
        let total_cost: f64 = self.agents.iter().map(|a| a.tokens.cost_usd).sum();
        let total_input: u64 = self.agents.iter().map(|a| a.tokens.input_tokens).sum();
        let total_output: u64 = self.agents.iter().map(|a| a.tokens.output_tokens).sum();
        let total_cache_read: u64 = self.agents.iter().map(|a| a.tokens.cache_read_tokens).sum();
        let total_cache_write: u64 = self
            .agents
            .iter()
            .map(|a| a.tokens.cache_write_tokens)
            .sum();
        let total_msgs: u32 = self.agents.iter().map(|a| a.message_count).sum();
        let total_tools: u32 = self.agents.iter().map(|a| a.tool_calls.total()).sum();

        panel = panel.child(
            div()
                .w_full()
                .p(self.s(12.0))
                .bg(self.bg_alpha(t.surface_raised()))
                .rounded(self.s(8.0))
                .flex()
                .flex_col()
                .gap(self.s(6.0))
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_muted())
                        .font_weight(FontWeight::BOLD)
                        .child("TOTALS"),
                )
                .child(self.stat_row("Total Cost", &format!("${:.4}", total_cost), t.green()))
                .child(self.stat_row("Input Tokens", &format!("{}", total_input), t.text()))
                .child(self.stat_row("Output Tokens", &format!("{}", total_output), t.text()))
                .child(self.stat_row("Cache Read", &format!("{}", total_cache_read), t.blue()))
                .child(self.stat_row(
                    "Cache Write",
                    &format!("{}", total_cache_write),
                    t.blue_muted(),
                ))
                .child(self.stat_row("Messages", &format!("{}", total_msgs), t.text()))
                .child(self.stat_row("Tool Calls", &format!("{}", total_tools), t.yellow())),
        );

        // Per-agent breakdown
        panel = panel.child(
            div()
                .text_size(self.s(11.0))
                .text_color(t.text_muted())
                .font_weight(FontWeight::BOLD)
                .mt(self.s(8.0))
                .child("PER AGENT"),
        );

        for a in &self.agents {
            let rt_color = t.runtime_color(&a.runtime_name);
            let pct = a.tokens.context_usage_pct();
            panel = panel.child(
                div()
                    .w_full()
                    .p(self.s(10.0))
                    .bg(self.bg_alpha(t.surface()))
                    .rounded(self.s(6.0))
                    .border_l_2()
                    .border_color(rt_color)
                    .flex()
                    .flex_col()
                    .gap(self.s(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(self.s(8.0))
                            .child(
                                div()
                                    .text_size(self.s(12.0))
                                    .text_color(t.text())
                                    .font_weight(FontWeight::BOLD)
                                    .child(a.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(rt_color)
                                    .child(a.runtime_name.clone()),
                            )
                            .child(div().flex_grow())
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(a.status.color(t))
                                    .child(a.status.label()),
                            )
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.green())
                                    .child(format!("${:.4}", a.tokens.cost_usd)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(self.s(12.0))
                            .flex_wrap()
                            .child(self.mini_stat("in", a.tokens.input_tokens, t.text_muted()))
                            .child(self.mini_stat("out", a.tokens.output_tokens, t.text_muted()))
                            .child(self.mini_stat("cache-r", a.tokens.cache_read_tokens, t.blue()))
                            .child(self.mini_stat(
                                "cache-w",
                                a.tokens.cache_write_tokens,
                                t.blue_muted(),
                            ))
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_faint())
                                    .child(format!("ctx {:.0}%", pct)),
                            )
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_faint())
                                    .child(format!("{}", a.tokens.model)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(self.s(8.0))
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_faint())
                                    .child(format!(
                                        "{}ed {}sh {}rd {}wr",
                                        a.tool_calls.edits,
                                        a.tool_calls.bash,
                                        a.tool_calls.reads,
                                        a.tool_calls.writes
                                    )),
                            )
                            .child(div().flex_grow())
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_faint())
                                    .child(format!("{} msgs", a.message_count)),
                            )
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(if a.tokens.thinking_enabled {
                                        t.yellow()
                                    } else {
                                        t.text_faint()
                                    })
                                    .child(if a.tokens.thinking_enabled {
                                        "thinking ON"
                                    } else {
                                        ""
                                    }),
                            ),
                    ),
            );
        }

        // Keyboard shortcuts
        panel = panel.child(
            div()
                .text_size(self.s(11.0))
                .text_color(t.text_muted())
                .font_weight(FontWeight::BOLD)
                .mt(self.s(12.0))
                .child("SHORTCUTS"),
        );
        panel = panel.child(
            div()
                .w_full()
                .p(self.s(10.0))
                .bg(self.bg_alpha(t.surface()))
                .rounded(self.s(6.0))
                .flex()
                .flex_col()
                .gap(self.s(3.0))
                .child(self.shortcut_row("i / click", "enter insert mode"))
                .child(self.shortcut_row("esc", "command mode"))
                .child(self.shortcut_row(
                    if self.config.cautious_enter {
                        "Cmd+Enter"
                    } else {
                        "Enter"
                    },
                    if self.config.cautious_enter {
                        "send prompt"
                    } else {
                        "send prompt (default)"
                    },
                ))
                .child(self.shortcut_row(
                    if self.config.cautious_enter {
                        "Enter"
                    } else {
                        "Cmd+Enter"
                    },
                    if self.config.cautious_enter {
                        "insert newline"
                    } else {
                        "alternate send"
                    },
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
                .child(self.shortcut_row("?", "this panel")),
        );

        panel
    }

    fn stat_row(&self, label: &str, value: &str, color: Rgba) -> Div {
        div()
            .flex()
            .justify_between()
            .child(
                div()
                    .text_size(self.s(11.0))
                    .text_color(self.theme.text_faint())
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(self.s(11.0))
                    .text_color(color)
                    .child(value.to_string()),
            )
    }

    fn mini_stat(&self, label: &str, value: u64, color: Rgba) -> Div {
        div()
            .flex()
            .gap(self.s(4.0))
            .child(
                div()
                    .text_size(self.s(10.0))
                    .text_color(self.theme.text_faint())
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(self.s(10.0))
                    .text_color(color)
                    .child(format!("{}", value)),
            )
    }

    fn shortcut_row(&self, key: &str, desc: &str) -> Div {
        div()
            .flex()
            .gap(self.s(8.0))
            .child(
                div()
                    .text_size(self.s(11.0))
                    .text_color(self.theme.blue_muted())
                    .w(self.s(80.0))
                    .child(key.to_string()),
            )
            .child(
                div()
                    .text_size(self.s(11.0))
                    .text_color(self.theme.text_faint())
                    .child(desc.to_string()),
            )
    }

    fn render_search(&self) -> Div {
        let t = &self.theme;
        let mut panel = div()
            .absolute()
            .top(self.s(0.0))
            .right(self.s(0.0))
            .w(self.s(500.0))
            .h_full()
            .bg(t.palette_bg())
            .border_l_1()
            .border_color(t.palette_border())
            .flex()
            .flex_col()
            .shadow(vec![BoxShadow {
                color: t.shadow().into(),
                offset: point(self.s(-4.0), px(0.)),
                blur_radius: self.s(20.0),
                spread_radius: px(0.),
            }]);

        // Search input
        let query_display = if self.search_query.is_empty() {
            "Search across agents...".to_string()
        } else {
            self.search_query.clone()
        };
        let query_color = if self.search_query.is_empty() {
            t.text_faint()
        } else {
            t.text()
        };

        panel = panel.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(10.0))
                .border_b_1()
                .border_color(t.palette_border())
                .flex()
                .items_center()
                .gap(self.s(8.0))
                .child(
                    div()
                        .text_size(self.s(14.0))
                        .text_color(t.text_muted())
                        .child("/"),
                )
                .child(
                    div()
                        .text_size(self.s(13.0))
                        .text_color(query_color)
                        .child(query_display),
                ),
        );

        // Result count
        panel = panel.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(4.0))
                .text_size(self.s(11.0))
                .text_color(t.text_faint())
                .child(format!("{} results", self.search_results.len())),
        );

        // Results list
        let mut results = div()
            .flex_grow()
            .w_full()
            .overflow_hidden()
            .flex()
            .flex_col();
        for (i, sr) in self.search_results.iter().enumerate() {
            let selected = i == self.search_selection;
            let bg = if selected {
                t.selected_row()
            } else {
                rgba(0x00000000)
            };
            let line_preview: String = sr.line.chars().take(80).collect();
            results = results.child(
                div()
                    .w_full()
                    .px(self.s(14.0))
                    .py(self.s(4.0))
                    .bg(bg)
                    .flex()
                    .flex_col()
                    .gap(self.s(2.0))
                    .child(
                        div()
                            .flex()
                            .gap(self.s(8.0))
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.blue())
                                    .child(sr.agent_name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(self.s(10.0))
                                    .text_color(t.text_faint())
                                    .child(format!("line {}", sr.line_idx + 1)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(self.s(12.0))
                            .text_color(t.text_muted())
                            .child(line_preview),
                    ),
            );
        }
        panel = panel.child(results);

        // Bottom hint
        panel = panel.child(
            div()
                .w_full()
                .px(self.s(14.0))
                .py(self.s(6.0))
                .border_t_1()
                .border_color(t.palette_border())
                .flex()
                .gap(self.s(16.0))
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("W/S: navigate"),
                )
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("Enter: jump"),
                )
                .child(
                    div()
                        .text_size(self.s(11.0))
                        .text_color(t.text_faint())
                        .child("Esc: close"),
                ),
        );

        panel
    }

    fn render_changes_panel(&self) -> Div {
        let t = &self.theme;
        let surface = self.bg_alpha(t.surface());

        let mut panel = div()
            .w(self.s(320.0))
            .h_full()
            .bg(surface)
            .border_l_1()
            .border_color(t.border())
            .flex()
            .flex_col()
            .overflow_hidden();

        // Header
        panel = panel.child(
            div()
                .w_full()
                .px(self.s(12.0))
                .py(self.s(8.0))
                .flex()
                .items_center()
                .child(
                    div()
                        .text_size(self.s(12.0))
                        .text_color(t.text())
                        .child("Changes"),
                )
                .child(div().flex_grow())
                .child(
                    div()
                        .text_size(self.s(10.0))
                        .text_color(t.text_faint())
                        .child("Cmd+L to close"),
                ),
        );

        if let Some(ref panel_state) = self.changes_panel {
            // Staged section
            if !panel_state.staged.is_empty() {
                panel = panel.child(
                    div()
                        .px(self.s(12.0))
                        .py(self.s(4.0))
                        .text_size(self.s(10.0))
                        .text_color(t.green())
                        .child(format!("STAGED ({})", panel_state.staged.len())),
                );
                for (i, fc) in panel_state.staged.iter().enumerate() {
                    let selected = i == panel_state.selected_index;
                    let bg = if selected {
                        self.bg_alpha(t.selected_row())
                    } else {
                        rgba(0x00000000)
                    };
                    let status_char = match fc.status {
                        crate::changes::FileStatus::Modified => "M",
                        crate::changes::FileStatus::Added => "A",
                        crate::changes::FileStatus::Deleted => "D",
                        crate::changes::FileStatus::Renamed => "R",
                        crate::changes::FileStatus::Copied => "C",
                        crate::changes::FileStatus::Untracked => "?",
                    };
                    panel = panel.child(
                        div()
                            .px(self.s(16.0))
                            .py(self.s(1.0))
                            .bg(bg)
                            .flex()
                            .gap(self.s(6.0))
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.green())
                                    .child(status_char),
                            )
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text())
                                    .child(fc.path.clone()),
                            ),
                    );
                }
            }

            // Unstaged section
            if !panel_state.unstaged.is_empty() {
                panel = panel.child(
                    div()
                        .px(self.s(12.0))
                        .py(self.s(4.0))
                        .text_size(self.s(10.0))
                        .text_color(t.yellow())
                        .child(format!("UNSTAGED ({})", panel_state.unstaged.len())),
                );
                let offset = panel_state.staged.len();
                for (i, fc) in panel_state.unstaged.iter().enumerate() {
                    let flat_idx = offset + i;
                    let selected = flat_idx == panel_state.selected_index;
                    let bg = if selected {
                        self.bg_alpha(t.selected_row())
                    } else {
                        rgba(0x00000000)
                    };
                    let status_char = match fc.status {
                        crate::changes::FileStatus::Modified => "M",
                        crate::changes::FileStatus::Added => "A",
                        crate::changes::FileStatus::Deleted => "D",
                        crate::changes::FileStatus::Renamed => "R",
                        crate::changes::FileStatus::Copied => "C",
                        crate::changes::FileStatus::Untracked => "?",
                    };
                    panel = panel.child(
                        div()
                            .px(self.s(16.0))
                            .py(self.s(1.0))
                            .bg(bg)
                            .flex()
                            .gap(self.s(6.0))
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.yellow())
                                    .child(status_char),
                            )
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text())
                                    .child(fc.path.clone()),
                            ),
                    );
                }
            }

            // Untracked section
            if !panel_state.untracked.is_empty() {
                panel = panel.child(
                    div()
                        .px(self.s(12.0))
                        .py(self.s(4.0))
                        .text_size(self.s(10.0))
                        .text_color(t.red())
                        .child(format!("UNTRACKED ({})", panel_state.untracked.len())),
                );
                let offset = panel_state.staged.len() + panel_state.unstaged.len();
                for (i, path) in panel_state.untracked.iter().enumerate() {
                    let flat_idx = offset + i;
                    let selected = flat_idx == panel_state.selected_index;
                    let bg = if selected {
                        self.bg_alpha(t.selected_row())
                    } else {
                        rgba(0x00000000)
                    };
                    panel = panel.child(
                        div()
                            .px(self.s(16.0))
                            .py(self.s(1.0))
                            .bg(bg)
                            .flex()
                            .gap(self.s(6.0))
                            .child(div().text_size(self.s(11.0)).text_color(t.red()).child("?"))
                            .child(
                                div()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text())
                                    .child(path.clone()),
                            ),
                    );
                }
            }

            // Diff content
            if !panel_state.diff_content.is_empty() {
                panel = panel.child(
                    div()
                        .w_full()
                        .mt(self.s(4.0))
                        .border_t_1()
                        .border_color(t.border())
                        .px(self.s(8.0))
                        .py(self.s(4.0))
                        .text_size(self.s(10.0))
                        .text_color(t.text_faint())
                        .child("DIFF"),
                );
                let mut diff_div = div()
                    .w_full()
                    .flex_grow()
                    .flex_shrink()
                    .overflow_hidden()
                    .px(self.s(8.0));

                let visible_start = panel_state.diff_scroll;
                let visible_end = (visible_start + 100).min(panel_state.diff_content.len());
                for line in &panel_state.diff_content[visible_start..visible_end] {
                    let kind = opensquirrel::classify_line(line);
                    let color = match kind {
                        LineKind::DiffAdd => t.green(),
                        LineKind::DiffRemove => t.red(),
                        LineKind::DiffHunk => t.blue(),
                        LineKind::DiffMeta => t.text_muted(),
                        _ => t.text(),
                    };
                    diff_div = diff_div.child(
                        div()
                            .w_full()
                            .text_size(self.s(11.0))
                            .text_color(color)
                            .child(line.clone()),
                    );
                }
                panel = panel.child(diff_div);
            }

            // Empty state
            if panel_state.total_items() == 0 {
                panel = panel.child(
                    div()
                        .px(self.s(12.0))
                        .py(self.s(20.0))
                        .text_size(self.s(12.0))
                        .text_color(t.text_muted())
                        .child("No changes detected."),
                );
            }
        }

        panel
    }

    fn render_remove_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let idx = self.confirm_remove_agent.unwrap_or(0);
        let label = self
            .agents
            .get(idx)
            .map(|agent| {
                if agent.role == AgentRole::Coordinator {
                    format!(
                        "Remove `{}` and its delegated workers from view?",
                        agent.name
                    )
                } else {
                    format!("Remove `{}` from view?", agent.name)
                }
            })
            .unwrap_or_else(|| "Remove this agent from view?".into());

        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full()
            .bg(rgba(0x00000066))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(self.s(420.0))
                    .bg(t.palette_bg())
                    .border_1()
                    .border_color(t.palette_border())
                    .rounded(self.s(12.0))
                    .shadow(vec![BoxShadow {
                        color: t.shadow().into(),
                        offset: point(px(0.), self.s(10.0)),
                        blur_radius: self.s(24.0),
                        spread_radius: px(0.),
                    }])
                    .p(self.s(16.0))
                    .flex()
                    .flex_col()
                    .gap(self.s(12.0))
                    .child(
                        div()
                            .text_size(self.s(15.0))
                            .text_color(t.text())
                            .font_weight(FontWeight::BOLD)
                            .child("Are you sure?"),
                    )
                    .child(
                        div()
                            .text_size(self.s(12.0))
                            .text_color(t.text_muted())
                            .child(label),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(self.s(8.0))
                            .child(
                                div()
                                    .id("confirm-remove-no")
                                    .px(self.s(12.0))
                                    .py(self.s(7.0))
                                    .rounded(self.s(8.0))
                                    .bg(self.bg_alpha(t.surface_raised()))
                                    .border_1()
                                    .border_color(t.border())
                                    .cursor_pointer()
                                    .text_size(self.s(11.0))
                                    .text_color(t.text())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_remove_agent = None;
                                        cx.notify();
                                    }))
                                    .child("no"),
                            )
                            .child(
                                div()
                                    .id("confirm-remove-yes")
                                    .px(self.s(12.0))
                                    .py(self.s(7.0))
                                    .rounded(self.s(8.0))
                                    .bg(t.red())
                                    .cursor_pointer()
                                    .text_size(self.s(11.0))
                                    .text_color(t.bg())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(idx) = this.confirm_remove_agent.take() {
                                            this.remove_agent_and_dependents(idx);
                                        }
                                        cx.notify();
                                    }))
                                    .child("yes"),
                            ),
                    ),
            )
    }

    fn render_copy_icon(
        &self,
        text: String,
        element_id: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<'_> {
        div()
            .id(ElementId::Name(element_id.into()))
            .flex_shrink_0()
            .px(self.s(6.0))
            .py(self.s(3.0))
            .rounded(self.s(6.0))
            .cursor_pointer()
            .text_size(self.s(13.0))
            .text_color(self.theme.text_muted())
            .hover(|s| {
                s.bg(self.theme.surface_raised())
                    .text_color(self.theme.text())
            })
            .on_click(cx.listener(move |_this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }))
            .child("⧉")
    }

    fn transcript_font(&self) -> SharedString {
        if self.config.terminal_text {
            self.font_family.clone().into()
        } else {
            SharedString::from(if cfg!(target_os = "macos") {
                "Helvetica Neue"
            } else {
                "Sans"
            })
        }
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<'_> {
        let t = &self.theme;
        let titlebar_safe_left = self.s(112.0);
        div()
            .w_full()
            .pl(titlebar_safe_left)
            .pr(self.s(12.0))
            .py(self.s(4.0))
            .bg(self.bg_alpha(t.surface()))
            .border_b_1()
            .border_color(t.border())
            .flex()
            .items_center()
            .gap(self.s(8.0))
            .child(div().flex_grow())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(self.s(4.0))
                    .child(
                        div()
                            .id("topbar-settings")
                            .px(self.s(7.0))
                            .py(self.s(4.0))
                            .rounded(self.s(6.0))
                            .cursor_pointer()
                            .text_size(self.s(14.0))
                            .text_color(t.text_muted())
                            .hover(|s| s.bg(self.bg_alpha(t.surface_raised())).text_color(t.text()))
                            .child("⚙")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(&OpenSettings, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("topbar-stats")
                            .px(self.s(7.0))
                            .py(self.s(4.0))
                            .rounded(self.s(6.0))
                            .cursor_pointer()
                            .text_size(self.s(14.0))
                            .text_color(if self.show_stats {
                                t.blue()
                            } else {
                                t.text_muted()
                            })
                            .hover(|s| s.bg(self.bg_alpha(t.surface_raised())).text_color(t.text()))
                            .child("⊞")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_stats(&ShowStats, window, cx);
                            })),
                    ),
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
                    div()
                        .flex_grow()
                        .flex_shrink()
                        .min_w(px(0.))
                        .h_full()
                        .p(px(4.0))
                        .flex()
                        .justify_center()
                        .items_center()
                        .child(
                            div()
                                .text_size(self.s(14.0))
                                .text_color(t.text_faint())
                                .child("No agents. Press [n] to create one."),
                        )
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

                    let mut grid = div()
                        .flex_grow()
                        .flex_shrink()
                        .min_w(px(0.))
                        .h_full()
                        .p(px(3.0))
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .overflow_hidden();

                    let mut tile_idx = 0;
                    for _row in 0..rows {
                        let tiles_in_row = if tile_idx + cols <= n {
                            cols
                        } else {
                            n - tile_idx
                        };
                        if tiles_in_row == 0 {
                            break;
                        }

                        let mut row_div = div()
                            .w_full()
                            .flex_grow()
                            .flex_shrink()
                            .flex_basis(px(0.))
                            .flex()
                            .gap(px(3.0))
                            .overflow_hidden();

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
                let mut focus_div = div()
                    .flex_grow()
                    .flex_shrink()
                    .min_w(px(0.))
                    .h_full()
                    .p(px(4.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden();
                if vis.contains(&self.focused_agent) {
                    focus_div = focus_div.child(self.render_agent_tile(self.focused_agent, cx));
                } else if let Some(&first) = vis.first() {
                    focus_div = focus_div.child(self.render_agent_tile(first, cx));
                } else {
                    focus_div = focus_div.child(
                        div()
                            .flex_grow()
                            .h_full()
                            .flex()
                            .justify_center()
                            .items_center()
                            .child(
                                div()
                                    .text_size(self.s(14.0))
                                    .text_color(t.text_faint())
                                    .child("No agents. Press [n] to create one."),
                            ),
                    );
                }
                focus_div
            }
            ViewMode::Pipeline => {
                let mut pipe = div()
                    .flex_grow()
                    .flex_shrink()
                    .min_w(px(0.))
                    .h_full()
                    .p(px(4.0))
                    .flex()
                    .overflow_x_hidden()
                    .overflow_hidden();
                if vis.is_empty() {
                    pipe = pipe.child(
                        div()
                            .flex_grow()
                            .h_full()
                            .flex()
                            .justify_center()
                            .items_center()
                            .child(
                                div()
                                    .text_size(self.s(14.0))
                                    .text_color(t.text_faint())
                                    .child("No agents. Press [n] to create one."),
                            ),
                    );
                } else {
                    for (pos, &idx) in vis.iter().enumerate() {
                        let a = &self.agents[idx];
                        let focused = idx == self.focused_agent;
                        let bc = if focused {
                            t.border_focus()
                        } else {
                            t.border()
                        };

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
                            .min_w(self.s(200.0))
                            .max_w(self.s(350.0))
                            .h_full()
                            .flex_shrink()
                            .bg(self.bg_alpha(t.bg()))
                            .border_1()
                            .border_color(bc)
                            .rounded(self.s(10.0))
                            .m(self.s(4.0))
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .shadow(stage_shadow)
                            .child(
                                div()
                                    .w_full()
                                    .px(self.s(10.0))
                                    .py(self.s(7.0))
                                    .bg(linear_gradient(
                                        180.0,
                                        linear_color_stop(
                                            self.bg_alpha(t.header_gradient_start()),
                                            0.0,
                                        ),
                                        linear_color_stop(
                                            self.bg_alpha(t.header_gradient_end()),
                                            1.0,
                                        ),
                                    ))
                                    .flex()
                                    .items_center()
                                    .gap(self.s(6.0))
                                    .child(
                                        div()
                                            .text_size(self.s(10.0))
                                            .text_color(a.status.color(t))
                                            .child(a.status.dot()),
                                    )
                                    .child(
                                        div()
                                            .text_size(self.s(12.0))
                                            .text_color(t.text())
                                            .child(a.name.clone()),
                                    )
                                    .child(div().flex_grow())
                                    .child(
                                        div()
                                            .text_size(self.s(10.0))
                                            .text_color(a.status.color(t))
                                            .child(a.status.label()),
                                    ),
                            )
                            .child({
                                let mut out = div()
                                    .flex_grow()
                                    .px(self.s(8.0))
                                    .py(self.s(4.0))
                                    .overflow_hidden()
                                    .font_family(self.font_family.clone())
                                    .text_size(self.s(11.0))
                                    .line_height(self.s(16.0));
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
                                div().h_full().flex().items_center().px(self.s(6.0)).child(
                                    div()
                                        .text_size(self.s(18.0))
                                        .text_color(t.text_faint())
                                        .child("->"),
                                ),
                            );
                        }
                    }
                }
                pipe
            }
        };

        let focused_is_terminal = self.mode == Mode::Normal
            && self
                .agents
                .get(self.focused_agent)
                .map(|a| a.is_terminal)
                .unwrap_or(false);
        let key_ctx = match self.mode {
            Mode::Normal if focused_is_terminal => "TerminalMode",
            Mode::Normal => "NormalMode",
            Mode::Palette => "PaletteMode",
            Mode::Setup => "SetupMode",
            Mode::Search => "SearchMode",
            Mode::AgentMenu => "AgentMenuMode",
            Mode::Settings => "SettingsMode",
        };

        // Wrap content in an overflow-hidden container so text can't push past window
        let vme = self.view_mode_epoch;
        let content = div()
            .flex_grow()
            .flex_shrink()
            .min_w(px(0.))
            .h_full()
            .overflow_hidden()
            .child(content)
            .with_animation(
                ElementId::Name(format!("viewmode-{}", vme).into()),
                Animation::new(Duration::from_millis(250)).with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            );
        let mut body = div()
            .flex_grow()
            .min_h(px(0.))
            .w_full()
            .flex()
            .overflow_hidden()
            .child(sidebar)
            .child(content);

        if self.mode == Mode::Settings {
            body = self.render_settings(cx);
        } else if self.show_changes {
            body = body.child(self.render_changes_panel());
        }

        let is_ops = self.config.theme == "ops";
        let mut root = div()
            .key_context(key_ctx)
            .track_focus(&self.focus_handle)
            .size_full()
            .text_color(t.text())
            .when(!is_ops, |d| d.bg(self.bg_alpha(t.bg())))
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::enter_command_mode))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_settings))
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
            .on_action(cx.listener(Self::close_tile))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(cx.listener(Self::change_agent))
            .on_action(cx.listener(Self::restart_agent))
            .on_action(cx.listener(Self::toggle_auto_scroll))
            .on_action(cx.listener(Self::pipe_to_agent))
            .on_action(cx.listener(Self::open_terminal))
            .on_action(cx.listener(Self::show_stats))
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
            .on_action(cx.listener(Self::toggle_changes))
            .on_action(cx.listener(Self::changes_up))
            .on_action(cx.listener(Self::changes_down))
            .on_action(cx.listener(Self::changes_stage))
            .on_action(cx.listener(Self::changes_refresh))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let bounds = window.bounds();
                let win_w: f32 = bounds.size.width.into();
                let win_h: f32 = bounds.size.height.into();
                let win_w = win_w.max(1.0);
                let win_h = win_h.max(1.0);
                let ex: f32 = event.position.x.into();
                let ey: f32 = event.position.y.into();
                let nx = (ex / win_w).clamp(0.0, 1.0);
                let ny = (ey / win_h).clamp(0.0, 1.0);
                if (this.mouse_x - nx).abs() > 0.001 || (this.mouse_y - ny).abs() > 0.001 {
                    this.mouse_x = nx;
                    this.mouse_y = ny;
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(top_bar)
            .child(body);

        if self.mode == Mode::Palette {
            root = root.child(self.render_palette(cx));
        }
        if self.mode == Mode::Setup && self.setup.is_some() {
            root = root.child(self.render_setup(cx));
        }
        if self.mode == Mode::Search {
            root = root.child(self.render_search());
        }
        if self.mode == Mode::AgentMenu {
            root = root.child(self.render_agent_menu(cx));
        }
        if self.show_stats {
            root = root.child(self.render_stats());
        }
        if self.confirm_remove_agent.is_some() {
            root = root.child(self.render_remove_confirm(cx));
        }

        // Particle overlay (rendered on top of everything)
        let has_particles = !self.particles.is_empty();

        // Wrap with starfield background for ops theme
        if self.config.theme == "ops" {
            let mut wrapper = div()
                .size_full()
                .relative()
                .child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .left(px(0.))
                        .size_full()
                        .child(self.render_starfield(cx)),
                )
                .child(root);
            if has_particles {
                wrapper = wrapper.child(self.render_particles());
            }
            return wrapper.into_any_element();
        }

        if has_particles {
            let wrapper = div()
                .size_full()
                .relative()
                .child(root)
                .child(self.render_particles());
            return wrapper.into_any_element();
        }

        root.into_any_element()
    }
}

// ── Main ────────────────────────────────────────────────────────

pub(crate) fn run() {
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
            // Escape: dismiss overlays (palette, setup, search) -- not in TerminalMode
            KeyBinding::new("escape", EnterCommandMode, Some("NormalMode")),
            KeyBinding::new("escape", EnterCommandMode, Some("TerminalMode")),
            // Text input (active in Normal/Setup/Palette/Search modes, NOT TerminalMode)
            KeyBinding::new("enter", SubmitInput, Some("NormalMode")),
            KeyBinding::new("enter", SubmitInput, Some("SetupMode")),
            KeyBinding::new("enter", SubmitInput, Some("PaletteMode")),
            KeyBinding::new("enter", SubmitInput, Some("SearchMode")),
            KeyBinding::new("enter", SubmitInput, Some("SettingsMode")),
            KeyBinding::new("cmd-v", PasteClipboard, Some("NormalMode")),
            KeyBinding::new("cmd-v", PasteClipboard, Some("TerminalMode")),
            KeyBinding::new("backspace", DeleteChar, Some("NormalMode")),
            KeyBinding::new("backspace", DeleteChar, Some("SetupMode")),
            KeyBinding::new("backspace", DeleteChar, Some("PaletteMode")),
            KeyBinding::new("backspace", DeleteChar, Some("SearchMode")),
            KeyBinding::new("left", CursorLeft, Some("NormalMode")),
            KeyBinding::new("right", CursorRight, Some("NormalMode")),
            KeyBinding::new("alt-left", CursorWordLeft, Some("NormalMode")),
            KeyBinding::new("alt-right", CursorWordRight, Some("NormalMode")),
            KeyBinding::new("cmd-left", CursorHome, Some("NormalMode")),
            KeyBinding::new("cmd-right", CursorEnd, Some("NormalMode")),
            KeyBinding::new("alt-backspace", DeleteWordBack, Some("NormalMode")),
            KeyBinding::new("cmd-backspace", DeleteToStart, Some("NormalMode")),
            // Pane/group navigation
            KeyBinding::new("cmd-]", NextPane, None),
            KeyBinding::new("cmd-[", PrevPane, None),
            KeyBinding::new("cmd-}", NextGroup, None),
            KeyBinding::new("cmd-{", PrevGroup, None),
            // App actions (Cmd- keybinds, always available)
            KeyBinding::new("cmd-k", OpenPalette, None),
            KeyBinding::new("cmd-shift-p", OpenPalette, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-n", SpawnAgent, None),
            KeyBinding::new("cmd-f", SearchOpen, None),
            KeyBinding::new("cmd-w", CloseTile, None),
            KeyBinding::new("cmd-l", ToggleChanges, Some("NormalMode")),
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
            // Search overlay
            KeyBinding::new("escape", SearchClose, Some("SearchMode")),
            KeyBinding::new("up", NavUp, Some("SearchMode")),
            KeyBinding::new("down", NavDown, Some("SearchMode")),
            // Settings screen
            KeyBinding::new("escape", EnterCommandMode, Some("SettingsMode")),
            KeyBinding::new("up", NavUp, Some("SettingsMode")),
            KeyBinding::new("down", NavDown, Some("SettingsMode")),
            KeyBinding::new("left", PaneLeft, Some("SettingsMode")),
            KeyBinding::new("right", PaneRight, Some("SettingsMode")),
            // Linux keybinds (Ctrl instead of Cmd)
            KeyBinding::new("ctrl-v", PasteClipboard, Some("NormalMode")),
            KeyBinding::new("ctrl-v", PasteClipboard, Some("TerminalMode")),
            KeyBinding::new("ctrl-]", NextPane, None),
            KeyBinding::new("ctrl-[", PrevPane, None),
            KeyBinding::new("ctrl-k", OpenPalette, None),
            KeyBinding::new("ctrl-shift-p", OpenPalette, None),
            KeyBinding::new("ctrl-,", OpenSettings, None),
            KeyBinding::new("ctrl-n", SpawnAgent, None),
            KeyBinding::new("ctrl-f", SearchOpen, None),
            KeyBinding::new("ctrl-w", CloseTile, None),
            KeyBinding::new("ctrl-l", ToggleChanges, Some("NormalMode")),
            KeyBinding::new("ctrl-=", ZoomIn, None),
            KeyBinding::new("ctrl--", ZoomOut, None),
            KeyBinding::new("ctrl-0", ZoomReset, None),
            KeyBinding::new("home", CursorHome, Some("NormalMode")),
            KeyBinding::new("end", CursorEnd, Some("NormalMode")),
            KeyBinding::new("ctrl-backspace", DeleteWordBack, Some("NormalMode")),
            KeyBinding::new("ctrl-shift-backspace", DeleteToStart, Some("NormalMode")),
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
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1400.0), px(900.0)),
                app,
            ))),
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
            view.update(app, |this, _cx| {
                this.focus_handle.focus(window);
            });
            view
        })
        .unwrap();
    });
}
