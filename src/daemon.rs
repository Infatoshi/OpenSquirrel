use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// State for a single managed agent subprocess.
struct ManagedAgent {
    child: Child,
    runtime: String,
    model: String,
    workdir: String,
    status: String, // "running", "done", "error"
    output_buffer: Vec<String>,
}

/// Shared state across all client handler threads.
pub(crate) struct DaemonState {
    agents: HashMap<String, ManagedAgent>,
    start_time: Instant,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            agents: HashMap::new(),
            start_time: Instant::now(),
        }
    }
}

/// Run the daemon: bind TCP, write pid/port files, accept connections.
pub(crate) fn run() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind TCP listener");
    let port = listener.local_addr().unwrap().port();

    let run_dir = dirs_path("run");
    std::fs::create_dir_all(&run_dir).ok();

    let port_path = format!("{}/daemon.port", run_dir);
    let pid_path = format!("{}/daemon.pid", run_dir);

    std::fs::write(&port_path, port.to_string()).expect("failed to write port file");
    std::fs::write(&pid_path, std::process::id().to_string()).expect("failed to write pid file");

    eprintln!(
        "osqd: listening on 127.0.0.1:{port} (pid {})",
        std::process::id()
    );

    let state = Arc::new(Mutex::new(DaemonState::new()));

    // Install SIGTERM handler for graceful shutdown
    let port_path_clone = port_path.clone();
    let pid_path_clone = pid_path.clone();
    ctrlc::set_handler(move || {
        eprintln!("osqd: shutting down");
        std::fs::remove_file(&port_path_clone).ok();
        std::fs::remove_file(&pid_path_clone).ok();
        std::process::exit(0);
    })
    .ok();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    handle_client(stream, state);
                });
            }
            Err(e) => {
                eprintln!("osqd: accept error: {e}");
            }
        }
    }

    // Cleanup on normal exit
    std::fs::remove_file(&port_path).ok();
    std::fs::remove_file(&pid_path).ok();
}

/// Ensure the daemon is running. If a valid PID file exists and the process is alive, exit.
/// Otherwise start the daemon as a background process.
pub(crate) fn ensure() {
    let run_dir = dirs_path("run");
    let pid_path = format!("{}/daemon.pid", run_dir);

    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            // Check if process is alive (signal 0)
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if alive {
                eprintln!("osqd: already running (pid {pid})");
                return;
            }
        }
    }

    // Not running -- start as background process
    let exe = std::env::current_exe().expect("cannot determine executable path");
    let child = Command::new(&exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn daemon");

    eprintln!("osqd: started daemon (pid {})", child.id());
}

fn dirs_path(sub: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{home}/.osq/{sub}")
}

fn handle_client(stream: std::net::TcpStream, state: Arc<Mutex<DaemonState>>) {
    let peer = stream.peer_addr().ok();
    eprintln!("osqd: client connected from {peer:?}");

    let reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let msg: JsonValue = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = send_json(
                    &mut writer,
                    &json!({"event": "error", "message": format!("bad json: {e}")}),
                );
                continue;
            }
        };

        let cmd = msg["cmd"].as_str().unwrap_or("");
        let response = match cmd {
            "ping" => handle_ping(&state),
            "list_dirs" => handle_list_dirs(&msg),
            "gpu_info" => handle_gpu_info(),
            "spawn_agent" => handle_spawn_agent(&msg, &state),
            "kill_agent" => handle_kill_agent(&msg, &state),
            "send_prompt" => handle_send_prompt(&msg, &state),
            "list_agents" => handle_list_agents(&state),
            _ => json!({"event": "error", "message": format!("unknown command: {cmd}")}),
        };

        if send_json(&mut writer, &response).is_err() {
            break;
        }

        // After spawn, stream buffered output
        if cmd == "spawn_agent" {
            if let Some(agent_id) = msg["agent_id"].as_str() {
                stream_agent_output(&mut writer, agent_id, &state);
            }
        }
    }

    eprintln!("osqd: client disconnected ({peer:?})");
}

fn send_json(writer: &mut impl Write, value: &JsonValue) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn handle_ping(state: &Arc<Mutex<DaemonState>>) -> JsonValue {
    let uptime = state.lock().unwrap().start_time.elapsed().as_secs();
    json!({
        "event": "pong",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime,
    })
}

fn handle_list_dirs(msg: &JsonValue) -> JsonValue {
    let path = msg["path"].as_str().unwrap_or(".");
    let expanded = if path.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        path.replacen('~', &home, 1)
    } else {
        path.to_string()
    };

    match std::fs::read_dir(&expanded) {
        Ok(entries) => {
            let mut dirs: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter(|e| {
                    !e.file_name()
                        .to_str()
                        .map(|n| n.starts_with('.'))
                        .unwrap_or(true)
                })
                .map(|e| e.path().to_string_lossy().to_string())
                .collect();
            dirs.sort();
            json!({"event": "dirs", "entries": dirs})
        }
        Err(e) => json!({"event": "error", "message": format!("list_dirs failed: {e}")}),
    }
}

fn handle_gpu_info() -> JsonValue {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let gpus: Vec<JsonValue> = stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    let parts: Vec<&str> = line.split(", ").collect();
                    json!({
                        "name": parts.first().unwrap_or(&"unknown").trim(),
                        "util": parts.get(1).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0),
                        "vram_used": parts.get(2).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0),
                        "vram_total": parts.get(3).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0),
                    })
                })
                .collect();
            json!({"event": "gpu", "gpus": gpus})
        }
        _ => json!({"event": "gpu", "gpus": []}),
    }
}

fn handle_spawn_agent(msg: &JsonValue, state: &Arc<Mutex<DaemonState>>) -> JsonValue {
    let agent_id = msg["agent_id"].as_str().unwrap_or("agent-0").to_string();
    let runtime = msg["runtime"].as_str().unwrap_or("claude").to_string();
    let model = msg["model"].as_str().unwrap_or("").to_string();
    let workdir = msg["workdir"].as_str().unwrap_or(".").to_string();
    let prompt = msg["prompt"].as_str().unwrap_or("").to_string();

    // Build command based on runtime
    let (cmd_name, mut args) = match runtime.as_str() {
        "claude" => (
            "claude",
            vec![
                "-p".to_string(),
                prompt.clone(),
                "--output-format".to_string(),
                "stream-json".to_string(),
            ],
        ),
        "codex" => ("codex", vec!["exec".to_string(), prompt.clone()]),
        _ => ("claude", vec!["-p".to_string(), prompt.clone()]),
    };

    if !model.is_empty() {
        match runtime.as_str() {
            "claude" => {
                args.push("--model".to_string());
                args.push(model.clone());
            }
            "codex" => {
                args.push("--model".to_string());
                args.push(model.clone());
            }
            _ => {}
        }
    }

    let child = Command::new(cmd_name)
        .args(&args)
        .current_dir(&workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    match child {
        Ok(child) => {
            let managed = ManagedAgent {
                child,
                runtime,
                model,
                workdir,
                status: "running".to_string(),
                output_buffer: Vec::new(),
            };
            state
                .lock()
                .unwrap()
                .agents
                .insert(agent_id.clone(), managed);
            json!({"event": "agent_spawned", "agent_id": agent_id})
        }
        Err(e) => {
            json!({"event": "error", "message": format!("spawn failed: {e}")})
        }
    }
}

fn handle_kill_agent(msg: &JsonValue, state: &Arc<Mutex<DaemonState>>) -> JsonValue {
    let agent_id = msg["agent_id"].as_str().unwrap_or("");
    let mut st = state.lock().unwrap();
    if let Some(agent) = st.agents.get_mut(agent_id) {
        let _ = agent.child.kill();
        agent.status = "killed".to_string();
        json!({"event": "agent_killed", "agent_id": agent_id})
    } else {
        json!({"event": "error", "message": format!("agent not found: {agent_id}")})
    }
}

fn handle_send_prompt(msg: &JsonValue, state: &Arc<Mutex<DaemonState>>) -> JsonValue {
    let agent_id = msg["agent_id"].as_str().unwrap_or("");
    let prompt = msg["prompt"].as_str().unwrap_or("");
    let mut st = state.lock().unwrap();
    if let Some(agent) = st.agents.get_mut(agent_id) {
        if let Some(ref mut stdin) = agent.child.stdin {
            let data = format!("{}\n", prompt);
            if stdin.write_all(data.as_bytes()).is_ok() {
                return json!({"event": "prompt_sent", "agent_id": agent_id});
            }
        }
        json!({"event": "error", "message": "failed to write to agent stdin"})
    } else {
        json!({"event": "error", "message": format!("agent not found: {agent_id}")})
    }
}

fn handle_list_agents(state: &Arc<Mutex<DaemonState>>) -> JsonValue {
    let mut st = state.lock().unwrap();
    let agents: Vec<JsonValue> = st
        .agents
        .iter_mut()
        .map(|(id, agent)| {
            // Check if still running
            if agent.status == "running" {
                if let Ok(Some(_status)) = agent.child.try_wait() {
                    agent.status = "done".to_string();
                }
            }
            json!({
                "agent_id": id,
                "runtime": agent.runtime,
                "model": agent.model,
                "workdir": agent.workdir,
                "status": agent.status,
                "output_lines": agent.output_buffer.len(),
            })
        })
        .collect();
    json!({"event": "agents", "agents": agents})
}

/// Stream stdout from a spawned agent back to the client. Collects output into
/// the agent's buffer so reconnecting clients can retrieve missed lines.
fn stream_agent_output(writer: &mut impl Write, agent_id: &str, state: &Arc<Mutex<DaemonState>>) {
    // Take stdout out of the agent (we can only read it from one thread)
    let stdout = {
        let mut st = state.lock().unwrap();
        st.agents
            .get_mut(agent_id)
            .and_then(|a| a.child.stdout.take())
    };

    let Some(stdout) = stdout else { return };
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
        let Ok(line) = line else { break };
        // Buffer it
        {
            let mut st = state.lock().unwrap();
            if let Some(agent) = st.agents.get_mut(agent_id) {
                agent.output_buffer.push(line.clone());
            }
        }
        // Send to client
        let msg = json!({"event": "agent_output", "agent_id": agent_id, "line": line});
        if send_json(writer, &msg).is_err() {
            break;
        }
    }

    // Mark agent done
    {
        let mut st = state.lock().unwrap();
        if let Some(agent) = st.agents.get_mut(agent_id) {
            agent.status = "done".to_string();
        }
    }
    let _ = send_json(
        writer,
        &json!({"event": "agent_done", "agent_id": agent_id}),
    );
}
