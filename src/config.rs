use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".osq").join("config.toml")
}

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".osq").join("state.json")
}

fn default_saved_target_machine() -> String {
    "local".into()
}
fn default_saved_agent_role() -> String {
    "coordinator".into()
}
fn default_saved_turn_state() -> String {
    "ready".into()
}
fn default_bg_opacity() -> f32 {
    1.0
}
fn default_bg_blur() -> f32 {
    0.0
}
fn default_view_mode() -> String {
    "grid".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SavedAgentState {
    pub(crate) name: String,
    pub(crate) group: String,
    pub(crate) runtime_name: String,
    #[serde(default = "default_saved_target_machine")]
    pub(crate) target_machine: String,
    #[serde(default = "default_saved_agent_role")]
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) parent_name: Option<String>,
    #[serde(default)]
    pub(crate) task_id: Option<String>,
    #[serde(default)]
    pub(crate) task_title: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) model: String,
    pub(crate) output_lines: Vec<String>,
    pub(crate) message_count: u32,
    pub(crate) scroll_offset: usize,
    pub(crate) favorite: bool,
    pub(crate) auto_scroll: bool,
    pub(crate) working_dir: String,
    #[serde(default)]
    pub(crate) remote_session_name: Option<String>,
    #[serde(default)]
    pub(crate) remote_line_cursor: usize,
    pub(crate) cost_usd: f64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    #[serde(default)]
    pub(crate) pending_prompt: Option<String>,
    #[serde(default = "default_saved_turn_state")]
    pub(crate) turn_state: String,
    pub(crate) tool_edits: u32,
    pub(crate) tool_reads: u32,
    pub(crate) tool_bash: u32,
    pub(crate) tool_writes: u32,
    pub(crate) tool_other: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SavedAppState {
    pub(crate) agents: Vec<SavedAgentState>,
    pub(crate) groups: Vec<String>,
    pub(crate) focused_group: usize,
    pub(crate) focused_agent: usize,
    pub(crate) view_mode: String,
    pub(crate) ui_scale: f32,
    pub(crate) sidebar_tab: String,
}

impl SavedAppState {
    pub(crate) fn save(&self) {
        let path = state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    pub(crate) fn load() -> Option<Self> {
        let path = state_path();
        if !path.exists() {
            return None;
        }
        let contents = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&contents).ok()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RuntimeDef {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env_remove: Vec<String>,
    #[serde(default)]
    pub(crate) env_set: Vec<(String, String)>,
    pub(crate) description: String,
    pub(crate) models: Vec<ModelOption>,
    pub(crate) model_flag: String,
    pub(crate) last_model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ModelOption {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) free: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct McpDef {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) description: String,
    pub(crate) global: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MachineDef {
    pub(crate) name: String,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) host: String,
    #[serde(default)]
    pub(crate) user: String,
    #[serde(default)]
    pub(crate) workdir: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RuntimePrefs {
    pub(crate) preferred_model: String,
    pub(crate) authenticated: bool,
    #[serde(default)]
    pub(crate) default_mcps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RecentProject {
    pub(crate) path: String,
    pub(crate) last_used: u64,
    pub(crate) machine: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AppConfig {
    pub(crate) runtimes: Vec<RuntimeDef>,
    pub(crate) mcps: Vec<McpDef>,
    #[serde(default)]
    pub(crate) machines: Vec<MachineDef>,
    pub(crate) last_runtime: String,
    #[serde(default = "default_saved_target_machine")]
    pub(crate) last_machine: String,
    #[serde(default)]
    pub(crate) runtime_prefs: std::collections::HashMap<String, RuntimePrefs>,
    #[serde(default)]
    pub(crate) recent_projects: Vec<RecentProject>,
    #[serde(default)]
    pub(crate) cautious_enter: bool,
    #[serde(default)]
    pub(crate) terminal_text: bool,
    pub(crate) last_mcps: Vec<String>,
    pub(crate) groups: Vec<String>,
    pub(crate) theme: String,
    pub(crate) font_family: String,
    pub(crate) font_size: f32,
    #[serde(default = "default_bg_opacity")]
    pub(crate) bg_opacity: f32,
    #[serde(default = "default_bg_blur")]
    pub(crate) bg_blur: f32,
    #[serde(default = "default_view_mode")]
    pub(crate) default_view_mode: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runtimes: vec![
                RuntimeDef {
                    name: "claude".into(),
                    command: "claude".into(),
                    args: vec![
                        "-p".into(),
                        "--output-format".into(),
                        "stream-json".into(),
                        "--verbose".into(),
                        "--dangerously-skip-permissions".into(),
                    ],
                    env_remove: vec!["CLAUDECODE".into()],
                    env_set: vec![],
                    description: "Claude Code (Anthropic)".into(),
                    models: vec![
                        ModelOption {
                            id: "claude-opus-4-6".into(),
                            label: "Opus 4.6".into(),
                            free: false,
                        },
                        ModelOption {
                            id: "claude-sonnet-4-6".into(),
                            label: "Sonnet 4.6".into(),
                            free: false,
                        },
                        ModelOption {
                            id: "claude-haiku-4-5-20251001".into(),
                            label: "Haiku 4.5".into(),
                            free: false,
                        },
                    ],
                    model_flag: "--model".into(),
                    last_model: "claude-opus-4-6".into(),
                },
                RuntimeDef {
                    name: "codex".into(),
                    command: "codex".into(),
                    args: vec![
                        "exec".into(),
                        "--json".into(),
                        "--dangerously-bypass-approvals-and-sandbox".into(),
                    ],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "Codex (OpenAI)".into(),
                    models: vec![
                        ModelOption {
                            id: "o3".into(),
                            label: "o3".into(),
                            free: false,
                        },
                        ModelOption {
                            id: "o4-mini".into(),
                            label: "o4-mini".into(),
                            free: true,
                        },
                        ModelOption {
                            id: "gpt-4.1".into(),
                            label: "GPT-4.1".into(),
                            free: true,
                        },
                        ModelOption {
                            id: "gpt-4.1-mini".into(),
                            label: "GPT-4.1 Mini".into(),
                            free: true,
                        },
                        ModelOption {
                            id: "gpt-5.3-codex".into(),
                            label: "GPT-5.3 Codex".into(),
                            free: false,
                        },
                        ModelOption {
                            id: "codex-mini-latest".into(),
                            label: "Codex Mini".into(),
                            free: true,
                        },
                    ],
                    model_flag: "-c".into(),
                    last_model: "o4-mini".into(),
                },
                RuntimeDef {
                    name: "opencode".into(),
                    command: "opencode".into(),
                    args: vec!["run".into(), "--format".into(), "json".into()],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "OpenCode (BYOK -- any provider)".into(),
                    models: vec![],
                    model_flag: "-m".into(),
                    last_model: "anthropic/claude-sonnet-4-6".into(),
                },
                RuntimeDef {
                    name: "cursor".into(),
                    command: "cursor".into(),
                    args: vec![
                        "agent".into(),
                        "--print".into(),
                        "--output-format".into(),
                        "stream-json".into(),
                        "--stream-partial-output".into(),
                        "--yolo".into(),
                    ],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "Cursor Agent (Cursor Pro subscription)".into(),
                    models: vec![],
                    model_flag: "--model".into(),
                    last_model: "sonnet-4.6".into(),
                },
                RuntimeDef {
                    name: "gemini".into(),
                    command: "gemini".into(),
                    args: vec![],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "Gemini CLI (Google)".into(),
                    models: vec![
                        ModelOption {
                            id: "gemini-2.5-pro".into(),
                            label: "Gemini 2.5 Pro".into(),
                            free: false,
                        },
                        ModelOption {
                            id: "gemini-2.5-flash".into(),
                            label: "Gemini 2.5 Flash".into(),
                            free: true,
                        },
                    ],
                    model_flag: "-m".into(),
                    last_model: "gemini-2.5-pro".into(),
                },
                RuntimeDef {
                    name: "copilot".into(),
                    command: "github-copilot-cli".into(),
                    args: vec![],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "GitHub Copilot CLI".into(),
                    models: vec![],
                    model_flag: String::new(),
                    last_model: String::new(),
                },
                RuntimeDef {
                    name: "terminal".into(),
                    command: String::new(),
                    args: vec![],
                    env_remove: vec![],
                    env_set: vec![],
                    description: "Terminal (local shell)".into(),
                    models: vec![],
                    model_flag: String::new(),
                    last_model: String::new(),
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
            runtime_prefs: std::collections::HashMap::new(),
            recent_projects: vec![],
            cautious_enter: false,
            terminal_text: false,
            last_mcps: vec![],
            groups: vec!["default".into()],
            theme: "midnight".into(),
            font_family: if cfg!(target_os = "macos") {
                "Menlo"
            } else {
                "Monospace"
            }
            .into(),
            font_size: 15.0,
            bg_opacity: default_bg_opacity(),
            bg_blur: default_bg_blur(),
            default_view_mode: default_view_mode(),
        }
    }
}

impl AppConfig {
    pub(crate) fn load() -> Self {
        let path = config_path();
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(mut config) = toml::from_str::<Self>(&contents) {
                    let defaults = Self::default();
                    for runtime in &defaults.runtimes {
                        if !config
                            .runtimes
                            .iter()
                            .any(|existing| existing.name == runtime.name)
                        {
                            config.runtimes.push(runtime.clone());
                        }
                    }
                    for machine in &defaults.machines {
                        if !config
                            .machines
                            .iter()
                            .any(|existing| existing.name == machine.name)
                        {
                            config.machines.push(machine.clone());
                        }
                    }
                    return config;
                }
            }
        }
        Self::default()
    }

    pub(crate) fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, contents);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeInfo {
    pub(crate) model: String,
    pub(crate) context_window: u64,
    pub(crate) max_output_tokens: u64,
    pub(crate) thinking_enabled: bool,
    pub(crate) plugins: Vec<String>,
    pub(crate) mcps: Vec<String>,
    pub(crate) skills: Vec<String>,
    pub(crate) hooks: Vec<String>,
    pub(crate) extra: Vec<(String, String)>,
}

fn read_claude_config() -> RuntimeInfo {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut info = RuntimeInfo {
        model: "claude-opus-4-6".into(),
        context_window: 200_000,
        max_output_tokens: 32_000,
        ..Default::default()
    };

    let settings_path = format!("{home}/.claude/settings.json");
    if let Ok(contents) = std::fs::read_to_string(&settings_path) {
        if let Ok(value) = serde_json::from_str::<JsonValue>(&contents) {
            if let Some(true) = value
                .get("alwaysThinkingEnabled")
                .and_then(|value| value.as_bool())
            {
                info.thinking_enabled = true;
            }
            if let Some(env) = value.get("env").and_then(|value| value.as_object()) {
                if let Some(tokens) = env
                    .get("MAX_THINKING_TOKENS")
                    .and_then(|value| value.as_str())
                {
                    info.extra.push(("think_tokens".into(), tokens.into()));
                }
                if let Some(tokens) = env
                    .get("CLAUDE_CODE_MAX_OUTPUT_TOKENS")
                    .and_then(|value| value.as_str())
                {
                    if let Ok(parsed) = tokens.parse::<u64>() {
                        info.max_output_tokens = parsed;
                    }
                }
            }
            if let Some(plugins) = value
                .get("enabledPlugins")
                .and_then(|value| value.as_object())
            {
                for (name, enabled) in plugins {
                    if enabled.as_bool().unwrap_or(false) {
                        info.plugins
                            .push(name.split('@').next().unwrap_or(name).into());
                    }
                }
            }
            if let Some(hooks) = value.get("hooks").and_then(|value| value.as_object()) {
                for key in hooks.keys() {
                    info.hooks.push(key.clone());
                }
            }
        }
    }

    let mcp_path = format!("{home}/.claude/mcp.json");
    if let Ok(contents) = std::fs::read_to_string(&mcp_path) {
        if let Ok(value) = serde_json::from_str::<JsonValue>(&contents) {
            if let Some(object) = value.as_object() {
                for key in object.keys() {
                    info.mcps.push(key.clone());
                }
            }
        }
    }

    info.extra.push(("memory".into(), "yes".into()));

    let claude_md = format!("{home}/.claude/CLAUDE.md");
    if Path::new(&claude_md).exists() {
        info.extra.push(("instructions".into(), "CLAUDE.md".into()));
    }

    info
}

fn read_codex_config() -> RuntimeInfo {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut info = RuntimeInfo {
        model: "gpt-5.3-codex".into(),
        context_window: 200_000,
        max_output_tokens: 32_000,
        ..Default::default()
    };

    let config_path = format!("{home}/.codex/config.toml");
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        if let Ok(value) = contents.parse::<toml::Value>() {
            if let Some(model) = value.get("model").and_then(|value| value.as_str()) {
                info.model = model.into();
            }
            if let Some(effort) = value
                .get("model_reasoning_effort")
                .and_then(|value| value.as_str())
            {
                info.thinking_enabled = true;
                info.extra.push(("reasoning".into(), effort.into()));
            }
            if let Some(personality) = value.get("personality").and_then(|value| value.as_str()) {
                info.extra.push(("personality".into(), personality.into()));
            }
            if let Some(mcps) = value.get("mcp_servers").and_then(|value| value.as_table()) {
                for key in mcps.keys() {
                    info.mcps.push(key.clone());
                }
            }
        }
    }

    let skills_dir = format!("{home}/.codex/skills");
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    info.skills.push(name.into());
                }
            }
        }
    }

    let agents_md = format!("{home}/.codex/AGENTS.md");
    if Path::new(&agents_md).exists() {
        info.extra.push(("instructions".into(), "AGENTS.md".into()));
    }

    info
}

fn read_opencode_config() -> RuntimeInfo {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut info = RuntimeInfo::default();

    let config_path = format!("{home}/.config/opencode/opencode.json");
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        if let Ok(value) = serde_json::from_str::<JsonValue>(&contents) {
            if let Some(model) = value.get("model").and_then(|value| value.as_str()) {
                info.model = model.into();
            }
            if let Some(providers) = value.get("provider").and_then(|value| value.as_object()) {
                for (name, provider) in providers {
                    info.extra.push(("provider".into(), name.clone()));
                    if let Some(options) =
                        provider.get("options").and_then(|value| value.as_object())
                    {
                        if let Some(url) = options.get("baseURL").and_then(|value| value.as_str()) {
                            info.extra.push(("endpoint".into(), url.into()));
                        }
                    }
                }
            }
        }
    }

    info
}

pub(crate) fn read_runtime_info(runtime_name: &str) -> RuntimeInfo {
    match runtime_name {
        "claude" => read_claude_config(),
        "codex" => read_codex_config(),
        "opencode" => read_opencode_config(),
        _ => RuntimeInfo::default(),
    }
}
