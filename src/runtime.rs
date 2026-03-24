use crate::app::{AgentMsg, AgentRole, MachineTarget, TokenStats};
use crate::config::{McpDef, ModelOption, RuntimeDef};
use opensquirrel::{build_persistent_runtime_args, parse_session_prompt, shell_escape};
use serde_json::Value as JsonValue;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub(crate) fn fetch_opencode_model_list() -> Vec<ModelOption> {
    let output = match Command::new("opencode").args(["models"]).output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut models: Vec<ModelOption> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let id = line.trim().to_string();
            ModelOption {
                free: id.ends_with(":free"),
                label: id.clone(),
                id,
            }
        })
        .collect();
    models.sort_by(|left, right| left.id.to_lowercase().cmp(&right.id.to_lowercase()));
    models
}

pub(crate) fn fetch_cursor_model_list() -> Vec<ModelOption> {
    let output = match Command::new("cursor").args(["agent", "models"]).output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut models: Vec<ModelOption> = stdout
        .lines()
        .filter(|line| {
            !line.trim().is_empty()
                && !line.contains("Available models")
                && !line.contains("Tip:")
                && !line.contains("Loading")
        })
        .filter_map(|line| {
            let stripped: String = line.trim().chars().filter(|c| !c.is_control()).collect();
            let stripped = stripped.trim();
            if stripped.is_empty() {
                return None;
            }
            let parts: Vec<&str> = stripped.splitn(2, " - ").collect();
            let id = parts[0].trim().to_string();
            if id.is_empty() {
                return None;
            }
            let label = if parts.len() > 1 {
                format!("{} ({id})", parts[1].trim())
            } else {
                id.clone()
            };
            Some(ModelOption {
                id,
                label,
                free: false,
            })
        })
        .collect();
    models.sort_by(|left, right| left.id.to_lowercase().cmp(&right.id.to_lowercase()));
    models
}

pub(crate) fn make_tmux_session_name(agent_name: &str) -> String {
    let safe = agent_name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!(
        "osq-{}-{suffix}",
        if safe.is_empty() {
            "agent"
        } else {
            safe.as_str()
        }
    )
}

fn parse_claude_json(
    line: &str,
    msg_tx: &async_channel::Sender<AgentMsg>,
    line_buf: &mut String,
) -> bool {
    if let Ok(value) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match msg_type {
            "content_block_delta" => {
                if let Some(delta) = value.get("delta") {
                    if let Some(text) = delta.get("text").and_then(|value| value.as_str()) {
                        line_buf.push_str(text);
                        while let Some(newline) = line_buf.find('\n') {
                            let complete_line: String = line_buf.drain(..=newline).collect();
                            let trimmed = complete_line.trim_end_matches('\n');
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(trimmed.to_string()));
                        }
                    }
                    if let Some(thinking) = delta.get("thinking").and_then(|value| value.as_str()) {
                        for line in thinking.split('\n') {
                            if !line.trim().is_empty() {
                                let _ = msg_tx
                                    .send_blocking(AgentMsg::OutputLine(format!("[think] {line}")));
                            }
                        }
                    }
                }
            }
            "content_block_stop" => {
                if !line_buf.is_empty() {
                    let remaining = line_buf.drain(..).collect::<String>();
                    if !remaining.trim().is_empty() {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(remaining));
                    }
                }
            }
            "result" => {
                let mut stats = TokenStats::default();
                if let Some(usage) = value.get("usage") {
                    stats.input_tokens = usage
                        .get("input_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.output_tokens = usage
                        .get("output_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.cache_write_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                }
                if let Some(model_usage) =
                    value.get("modelUsage").and_then(|value| value.as_object())
                {
                    let mut primary_model = String::new();
                    let mut max_output = 0_u64;
                    for (model_name, usage) in model_usage {
                        let output = usage
                            .get("outputTokens")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        if output > max_output || primary_model.is_empty() {
                            max_output = output;
                            primary_model = model_name.clone();
                        }
                        stats.input_tokens += usage
                            .get("inputTokens")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.output_tokens += usage
                            .get("outputTokens")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.cache_read_tokens += usage
                            .get("cacheReadInputTokens")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.cache_write_tokens += usage
                            .get("cacheCreationInputTokens")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.context_window = usage
                            .get("contextWindow")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(200_000);
                        stats.max_output_tokens = usage
                            .get("maxOutputTokens")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(32_000);
                    }
                    stats.model = primary_model;
                }
                stats.cost_usd = value
                    .get("total_cost_usd")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0);
                stats.session_id = value
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .map(String::from);
                if let Some(state) = value
                    .get("fast_mode_state")
                    .and_then(|value| value.as_str())
                {
                    stats.thinking_enabled = state != "off";
                }
                let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));

                if let Some(result) = value.get("result").and_then(|value| value.as_str()) {
                    if !result.is_empty() {
                        for line in result.split('\n') {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line.to_string()));
                        }
                    }
                }
                return true;
            }
            "error" => {
                let message = value
                    .get("error")
                    .and_then(|error| error.get("message").and_then(|value| value.as_str()))
                    .or_else(|| value.get("message").and_then(|value| value.as_str()))
                    .unwrap_or("unknown error");
                let _ = msg_tx.send_blocking(AgentMsg::Error(message.to_string()));
            }
            "assistant" => {
                // Only extract tool_use from assistant messages.
                // Text content arrives via content_block_delta streaming events,
                // so we don't re-emit text here to avoid duplication.
                if let Some(content) = value
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(|content| content.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|value| value.as_str()) == Some("tool_use") {
                            if let Some(name) = block.get("name").and_then(|value| value.as_str()) {
                                let _ = msg_tx.send_blocking(AgentMsg::ToolCall(name.to_string()));
                            }
                        }
                    }
                }
            }
            "system" => {
                if value.get("subtype").and_then(|v| v.as_str()) == Some("init") {
                    if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
                        let mut stats = TokenStats::default();
                        stats.model = model.to_string();
                        if let Some(session_id) = value.get("session_id").and_then(|v| v.as_str()) {
                            stats.session_id = Some(session_id.to_string());
                        }
                        let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn parse_codex_json(line: &str, msg_tx: &async_channel::Sender<AgentMsg>) -> bool {
    if let Ok(value) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match msg_type {
            "message.delta" | "content_block_delta" => {
                if let Some(delta) = value.get("delta") {
                    if let Some(text) = delta.get("text").and_then(|value| value.as_str()) {
                        for line in text.split('\n') {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line.to_string()));
                        }
                    }
                }
            }
            "item.completed" => {
                if let Some(item) = value.get("item") {
                    let item_type = item
                        .get("type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if item_type == "function_call" || item_type == "tool_use" {
                        let name = item
                            .get("name")
                            .and_then(|value| value.as_str())
                            .or_else(|| {
                                item.get("function")
                                    .and_then(|function| function.get("name"))
                                    .and_then(|value| value.as_str())
                            })
                            .unwrap_or("tool");
                        let _ = msg_tx.send_blocking(AgentMsg::ToolCall(name.to_string()));
                    }
                    if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                        for line in text.split('\n') {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line.to_string()));
                        }
                    }
                    if let Some(message) = item.get("message").and_then(|value| value.as_str()) {
                        if item_type == "error" {
                            let _ = msg_tx.send_blocking(AgentMsg::Error(message.to_string()));
                        }
                    }
                }
            }
            "turn.completed" => {
                if let Some(usage) = value.get("usage") {
                    let mut stats = TokenStats::default();
                    stats.input_tokens = usage
                        .get("input_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.output_tokens = usage
                        .get("output_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.context_window = 200_000;
                    let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                }
                return true;
            }
            "turn.failed" => {
                let raw = value
                    .get("error")
                    .and_then(|error| error.get("message").and_then(|value| value.as_str()))
                    .unwrap_or("turn failed");
                let error = if let Ok(inner) = serde_json::from_str::<JsonValue>(raw) {
                    inner
                        .get("detail")
                        .and_then(|value| value.as_str())
                        .unwrap_or(raw)
                        .to_string()
                } else {
                    raw.to_string()
                };
                let _ = msg_tx.send_blocking(AgentMsg::Error(error));
                return true;
            }
            "error" | "thread.started" | "turn.started" => {}
            _ => {}
        }
    }
    false
}

fn parse_opencode_json(line: &str, msg_tx: &async_channel::Sender<AgentMsg>) -> bool {
    if let Ok(value) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match msg_type {
            "text" => {
                if let Some(text) = value
                    .get("part")
                    .and_then(|part| part.get("text"))
                    .and_then(|value| value.as_str())
                {
                    for line in text.split('\n') {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line.to_string()));
                    }
                }
            }
            "tool_use" => {
                if let Some(part) = value.get("part") {
                    let tool = part
                        .get("tool")
                        .and_then(|value| value.as_str())
                        .unwrap_or("tool");
                    let status = part
                        .get("state")
                        .and_then(|state| state.get("status"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let title = part
                        .get("state")
                        .and_then(|state| state.get("title"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if status == "completed" {
                        let _ = msg_tx.send_blocking(AgentMsg::ToolCall(tool.to_string()));
                        let preview = part
                            .get("state")
                            .and_then(|state| state.get("metadata"))
                            .and_then(|metadata| metadata.get("preview"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let display = if !title.is_empty() {
                            format!("[{tool}] {title}")
                        } else if !preview.is_empty() {
                            format!("[{tool}] {}", preview.split('\n').next().unwrap_or(""))
                        } else {
                            format!("[{tool}] done")
                        };
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(display));
                    }
                }
            }
            "step_finish" => {
                if let Some(part) = value.get("part") {
                    if let Some(tokens) = part.get("tokens") {
                        let mut stats = TokenStats::default();
                        stats.input_tokens = tokens
                            .get("input")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.output_tokens = tokens
                            .get("output")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.cache_read_tokens = tokens
                            .get("cache")
                            .and_then(|cache| cache.get("read"))
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.cache_write_tokens = tokens
                            .get("cache")
                            .and_then(|cache| cache.get("write"))
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        stats.context_window = 200_000;
                        stats.cost_usd = part
                            .get("cost")
                            .and_then(|value| value.as_f64())
                            .unwrap_or(0.0);
                        stats.session_id = value
                            .get("sessionID")
                            .and_then(|value| value.as_str())
                            .map(String::from);
                        let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                    }
                    if part.get("reason").and_then(|value| value.as_str()) == Some("stop") {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn parse_cursor_json(
    line: &str,
    msg_tx: &async_channel::Sender<AgentMsg>,
    line_buf: &mut String,
) -> bool {
    if let Ok(value) = serde_json::from_str::<JsonValue>(line) {
        let msg_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match msg_type {
            "system" => {
                let mut stats = TokenStats::default();
                if let Some(session_id) = value.get("session_id").and_then(|value| value.as_str()) {
                    stats.session_id = Some(session_id.to_string());
                }
                if let Some(model) = value.get("model").and_then(|value| value.as_str()) {
                    stats.model = model.to_string();
                }
                stats.context_window = 200_000;
                let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
            }
            "assistant" => {
                if value.get("timestamp_ms").is_some() {
                    if let Some(content) = value
                        .get("message")
                        .and_then(|message| message.get("content"))
                        .and_then(|value| value.as_array())
                    {
                        for block in content {
                            let block_type = block
                                .get("type")
                                .and_then(|value| value.as_str())
                                .unwrap_or("");
                            if block_type == "text" {
                                if let Some(text) =
                                    block.get("text").and_then(|value| value.as_str())
                                {
                                    line_buf.push_str(text);
                                    while let Some(newline) = line_buf.find('\n') {
                                        let complete_line: String =
                                            line_buf.drain(..=newline).collect();
                                        let trimmed = complete_line.trim_end_matches('\n');
                                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(
                                            trimmed.to_string(),
                                        ));
                                    }
                                }
                            } else if block_type == "tool_use" {
                                if let Some(name) =
                                    block.get("name").and_then(|value| value.as_str())
                                {
                                    let _ =
                                        msg_tx.send_blocking(AgentMsg::ToolCall(name.to_string()));
                                }
                            }
                        }
                    }
                }
            }
            "tool_call" => {
                if value.get("subtype").and_then(|value| value.as_str()) == Some("completed") {
                    if let Some(tool_call) = value.get("tool_call") {
                        let tool_name = tool_call
                            .get("shellToolCall")
                            .map(|_| "bash")
                            .or_else(|| tool_call.get("readToolCall").map(|_| "read"))
                            .or_else(|| tool_call.get("editToolCall").map(|_| "edit"))
                            .or_else(|| tool_call.get("writeToolCall").map(|_| "write"))
                            .unwrap_or("tool");
                        let _ = msg_tx.send_blocking(AgentMsg::ToolCall(tool_name.to_string()));
                        let description = tool_call
                            .as_object()
                            .and_then(|object| object.values().next())
                            .and_then(|value| value.get("description"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if !description.is_empty() {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(format!(
                                "[{tool_name}] {description}"
                            )));
                        }
                    }
                }
            }
            "result" => {
                if !line_buf.is_empty() {
                    let remaining = line_buf.drain(..).collect::<String>();
                    if !remaining.trim().is_empty() {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(remaining));
                    }
                }
                if let Some(usage) = value.get("usage") {
                    let mut stats = TokenStats::default();
                    stats.input_tokens = usage
                        .get("inputTokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.output_tokens = usage
                        .get("outputTokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.cache_read_tokens = usage
                        .get("cacheReadTokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.cache_write_tokens = usage
                        .get("cacheWriteTokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    stats.context_window = 200_000;
                    stats.session_id = value
                        .get("session_id")
                        .and_then(|value| value.as_str())
                        .map(String::from);
                    let _ = msg_tx.send_blocking(AgentMsg::TokenUpdate(stats));
                }
                if value
                    .get("is_error")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    let error = value
                        .get("result")
                        .and_then(|value| value.as_str())
                        .unwrap_or("cursor error");
                    let _ = msg_tx.send_blocking(AgentMsg::Error(error.to_string()));
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

fn build_remote_shell_command(
    program: &str,
    args: &[String],
    prompt: &str,
    workdir: Option<&str>,
) -> String {
    let mut parts = vec![shell_escape(program)];
    for arg in args {
        parts.push(shell_escape(arg));
    }
    parts.push(shell_escape(prompt));
    let invoke = parts.join(" ");
    if let Some(dir) = workdir.filter(|dir| !dir.is_empty()) {
        format!("cd {} && {invoke}", shell_escape(dir))
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
        command.push_str(&format!("{key}={} ", shell_escape(value)));
    }
    command.push_str(&build_remote_shell_command(
        &runtime.command,
        args,
        prompt,
        workdir,
    ));
    command
}

fn run_ssh_script(destination: &str, script: &str) -> std::io::Result<std::process::Output> {
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
    let login_wrapped = format!("bash -lc {}", shell_escape(&remote_command));
    let escaped_session = shell_escape(session_name);
    let script = format!(
        "tmux kill-session -t {escaped_session} >/dev/null 2>&1 || true; \
         tmux new-session -d -s {escaped_session} -x 300 -y 50 {command}; \
         tmux set-option -t {escaped_session} history-limit 50000 >/dev/null 2>&1 || true; \
         tmux set-option -t {escaped_session} remain-on-exit on >/dev/null 2>&1",
        command = shell_escape(&login_wrapped),
    );
    let output = run_ssh_script(destination, &script)?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
}

fn remote_tmux_snapshot(
    destination: &str,
    session_name: &str,
) -> anyhow::Result<(Vec<String>, bool)> {
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
    let mut lines = stdout
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let pane_dead = lines
        .last()
        .and_then(|line| line.strip_prefix("__OSQ_PANE_DEAD__="))
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    if lines
        .last()
        .map(|line| line.starts_with("__OSQ_PANE_DEAD__="))
        .unwrap_or(false)
    {
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
                    if trimmed.is_empty() || trimmed.starts_with("Pane is dead") {
                        continue;
                    }
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
                    "remote tmux polling failed for '{session_name}': {error}"
                )));
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn build_mcp_config_args(mcps: &[McpDef]) -> Vec<String> {
    if mcps.is_empty() {
        return Vec::new();
    }
    let mut servers = serde_json::Map::new();
    for mcp in mcps {
        servers.insert(
            mcp.name.clone(),
            serde_json::json!({
                "command": mcp.command,
                "args": mcp.args,
            }),
        );
    }
    let config = serde_json::json!({ "mcpServers": servers });
    vec!["--mcp-config".into(), config.to_string()]
}

pub(crate) fn agent_thread(
    msg_tx: async_channel::Sender<AgentMsg>,
    prompt_rx: mpsc::Receiver<String>,
    runtime: RuntimeDef,
    model_override: Option<String>,
    target: MachineTarget,
    role: AgentRole,
    remote_session_name: Option<String>,
    mcps: Vec<McpDef>,
    system_prompt: Option<String>,
) {
    let _ = msg_tx.send_blocking(AgentMsg::Ready);
    let is_claude = runtime.name == "claude";
    let is_opencode = runtime.name == "opencode";
    let is_cursor = runtime.name == "cursor";
    let is_json_mode = is_claude || runtime.name == "codex" || is_opencode || is_cursor;

    if is_claude && role == AgentRole::Coordinator && target.ssh_destination.is_none() {
        agent_thread_claude_persistent(
            &msg_tx,
            &prompt_rx,
            &runtime,
            &model_override,
            &mcps,
            system_prompt.as_deref(),
        );
        return;
    }

    while let Ok(raw_msg) = prompt_rx.recv() {
        let reattach_session = raw_msg.strip_prefix("__OSQ_REATTACH__").and_then(|value| {
            let (cursor, session) = value.split_once("::")?;
            let cursor = cursor.parse::<usize>().ok()?;
            Some((cursor, session.to_string()))
        });
        if let (Some(destination), Some((line_cursor, session_name))) =
            (target.ssh_destination.as_ref(), reattach_session)
        {
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
            if let Some(newline) = rest.find('\n') {
                (
                    Some(rest[..newline].to_string()),
                    rest[newline + 1..].to_string(),
                )
            } else {
                (None, raw_msg)
            }
        } else {
            (None, raw_msg)
        };

        let mut turn_args = runtime.args.clone();
        if let Some(ref model) = model_override {
            if !model.is_empty() && !runtime.model_flag.is_empty() {
                turn_args.extend([runtime.model_flag.clone(), model.clone()]);
            }
        }
        if let Some(ref session_id) = session_id {
            if is_opencode {
                turn_args.extend(["--session".into(), session_id.clone(), "--continue".into()]);
            }
            if is_cursor {
                turn_args.extend(["--resume".into(), session_id.clone()]);
            }
        }
        if is_claude && !mcps.is_empty() {
            turn_args.extend(build_mcp_config_args(&mcps));
        }
        if is_claude {
            if let Some(ref prompt) = system_prompt {
                turn_args.push("--append-system-prompt".into());
                turn_args.push(prompt.clone());
            }
        }

        let mut cmd = if let Some(destination) = target.ssh_destination.as_ref() {
            let session_name = remote_session_name
                .clone()
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
                        "remote tmux launch failed on '{}': {error}",
                        target.name
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
            let _ = msg_tx.send_blocking(AgentMsg::Done {
                session_id: session_id.clone(),
            });
            continue;
        } else {
            let mut command = Command::new(&runtime.command);
            command.args(&turn_args);
            command.arg(&prompt);
            if let Some(dir) = target.workdir.as_ref().filter(|dir| !dir.is_empty()) {
                command.current_dir(dir);
            }
            command
        };

        for env in &runtime.env_remove {
            cmd.env_remove(env);
        }
        for (key, value) in &runtime.env_set {
            cmd.env(key, value);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        match cmd.spawn() {
            Ok(mut child) => {
                let stderr_tx = msg_tx.clone();
                let stderr_handle = child.stderr.take().map(|stderr| {
                    std::thread::spawn(move || {
                        for text in BufReader::new(stderr).lines().map_while(Result::ok) {
                            if !text.trim().is_empty() {
                                let _ = stderr_tx.send_blocking(AgentMsg::StderrLine(text));
                            }
                        }
                    })
                });

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
                            if done {
                                break;
                            }
                        } else {
                            let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line));
                        }
                    }
                    if !line_buf.is_empty() {
                        let _ = msg_tx.send_blocking(AgentMsg::OutputLine(line_buf));
                    }
                }

                if let Some(handle) = stderr_handle {
                    let _ = handle.join();
                }
                let _ = child.wait();
                let _ = msg_tx.send_blocking(AgentMsg::Done {
                    session_id: session_id.clone(),
                });
            }
            Err(error) => {
                let _ = msg_tx.send_blocking(AgentMsg::Error(format!(
                    "spawn '{}' on '{}' failed: {error}",
                    runtime.command, target.name
                )));
            }
        }
    }
}

fn agent_thread_claude_persistent(
    msg_tx: &async_channel::Sender<AgentMsg>,
    prompt_rx: &mpsc::Receiver<String>,
    runtime: &RuntimeDef,
    model_override: &Option<String>,
    mcps: &[McpDef],
    system_prompt: Option<&str>,
) {
    let first_msg = match prompt_rx.recv() {
        Ok(message) => message,
        Err(_) => return,
    };

    let (session_id, first_prompt) = parse_session_prompt(&first_msg);
    let mut args = build_persistent_runtime_args(
        &runtime.args,
        &runtime.model_flag,
        model_override.as_deref(),
        session_id.as_deref(),
    );
    if !mcps.is_empty() {
        args.extend(build_mcp_config_args(mcps));
    }
    if let Some(system_prompt) = system_prompt {
        args.push("--append-system-prompt".into());
        args.push(system_prompt.into());
    }

    let mut cmd = Command::new(&runtime.command);
    cmd.args(&args);
    for env in &runtime.env_remove {
        cmd.env_remove(env);
    }
    for (key, value) in &runtime.env_set {
        cmd.env(key, value);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = msg_tx.send_blocking(AgentMsg::Error(format!(
                "spawn '{}' failed: {error}",
                runtime.command
            )));
            return;
        }
    };

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stderr_tx = msg_tx.clone();
    let stderr_handle = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            for text in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !text.trim().is_empty() {
                    let _ = stderr_tx.send_blocking(AgentMsg::StderrLine(text));
                }
            }
        })
    });

    let stdout_tx = msg_tx.clone();
    let stdout_handle = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut line_buf = String::new();
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let turn_done = parse_claude_json(&line, &stdout_tx, &mut line_buf);
                if turn_done {
                    let session_id =
                        serde_json::from_str::<JsonValue>(&line)
                            .ok()
                            .and_then(|value| {
                                value
                                    .get("session_id")
                                    .and_then(|value| value.as_str())
                                    .map(String::from)
                            });
                    let _ = stdout_tx.send_blocking(AgentMsg::Done { session_id });
                }
            }
        })
    });

    let first_message = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": first_prompt }
    });
    if writeln!(stdin, "{first_message}").is_err() {
        let _ = msg_tx.send_blocking(AgentMsg::Error("failed to write to claude stdin".into()));
        return;
    }
    let _ = stdin.flush();

    while let Ok(raw_msg) = prompt_rx.recv() {
        let prompt = if let Some(rest) = raw_msg.strip_prefix("SESSION:") {
            if let Some(newline) = rest.find('\n') {
                rest[newline + 1..].to_string()
            } else {
                raw_msg
            }
        } else {
            raw_msg
        };
        let user_msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt }
        });
        if writeln!(stdin, "{user_msg}").is_err() {
            let _ = msg_tx.send_blocking(AgentMsg::Error("claude process stdin closed".into()));
            break;
        }
        let _ = stdin.flush();
    }

    drop(stdin);
    if let Some(handle) = stdout_handle {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_handle {
        let _ = handle.join();
    }
    let _ = child.wait();
}
