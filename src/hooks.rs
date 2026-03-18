use std::collections::HashMap;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub(crate) enum HookEvent {
    Start { pane_id: String, agent: String },
    Stop { pane_id: String },
}

pub(crate) struct HooksServer {
    port: u16,
    _thread: std::thread::JoinHandle<()>,
    events: Arc<Mutex<Vec<HookEvent>>>,
}

impl HooksServer {
    /// Start the hooks server on a random available port.
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind hooks server");
        let port = listener.local_addr().unwrap().port();
        let events: Arc<Mutex<Vec<HookEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);

        let thread = std::thread::Builder::new()
            .name("hooks-server".into())
            .spawn(move || {
                Self::serve(listener, events_clone);
            })
            .expect("failed to spawn hooks server thread");

        Self {
            port,
            _thread: thread,
            events,
        }
    }

    /// Get the port the server is listening on.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Drain all pending events.
    pub(crate) fn drain_events(&self) -> Vec<HookEvent> {
        let mut lock = self.events.lock().unwrap();
        std::mem::take(&mut *lock)
    }

    fn serve(listener: TcpListener, events: Arc<Mutex<Vec<HookEvent>>>) {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Read the full HTTP request
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let request = String::from_utf8_lossy(&buf[..n]);

            // Only handle POST /hooks
            if !request.starts_with("POST /hooks") {
                let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                continue;
            }

            // Extract JSON body: everything after the \r\n\r\n header delimiter
            let body = match request.find("\r\n\r\n") {
                Some(pos) => &request[pos + 4..],
                None => {
                    let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                    continue;
                }
            };

            // Parse JSON with serde_json
            let parsed: Result<HashMap<String, String>, _> = serde_json::from_str(body.trim());
            match parsed {
                Ok(map) => {
                    let event_type = map.get("event").map(|s| s.as_str()).unwrap_or("");
                    let pane_id = map.get("pane_id").cloned().unwrap_or_default();

                    let hook_event = match event_type {
                        "Start" => {
                            let agent = map.get("agent").cloned().unwrap_or_default();
                            Some(HookEvent::Start { pane_id, agent })
                        }
                        "Stop" => Some(HookEvent::Stop { pane_id }),
                        _ => None,
                    };

                    if let Some(ev) = hook_event {
                        events.lock().unwrap().push(ev);
                    }

                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                }
                Err(_) => {
                    let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                }
            }
        }
    }
}

/// Generate wrapper scripts in ~/.osq/bin/ for each runtime.
///
/// Each wrapper:
/// 1. Traps EXIT to POST a Stop event
/// 2. POSTs a Start event
/// 3. exec's the real binary with all args
///
/// Returns the path to the bin directory on success.
pub(crate) fn generate_wrapper_scripts(runtime_names: &[&str]) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let bin_dir = PathBuf::from(&home).join(".osq").join("bin");

    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("failed to create {}: {}", bin_dir.display(), e))?;

    for name in runtime_names {
        // Find the real binary, skipping our own wrapper directory
        let real_path = find_real_binary(name, &bin_dir)?;

        let script = generate_script(name, &real_path);

        let script_path = bin_dir.join(name);
        std::fs::write(&script_path, &script)
            .map_err(|e| format!("failed to write {}: {}", script_path.display(), e))?;

        // chmod +x
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&script_path, perms)
                .map_err(|e| format!("failed to chmod {}: {}", script_path.display(), e))?;
        }
    }

    Ok(bin_dir)
}

/// Locate the real binary for `name`, skipping any path that lives inside `wrapper_dir`.
fn find_real_binary(name: &str, wrapper_dir: &Path) -> Result<String, String> {
    // Use `which -a` to get all candidates, then pick the first one that is not our wrapper.
    let output = std::process::Command::new("which")
        .arg("-a")
        .arg(name)
        .output()
        .map_err(|e| format!("failed to run `which -a {}`: {}", name, e))?;

    if !output.status.success() {
        return Err(format!(
            "`{}` not found in PATH -- cannot create wrapper",
            name
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let wrapper_dir_canon = wrapper_dir
        .canonicalize()
        .unwrap_or_else(|_| wrapper_dir.to_path_buf());

    for line in stdout.lines() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        let candidate_path = PathBuf::from(candidate);
        let candidate_canon = candidate_path
            .canonicalize()
            .unwrap_or_else(|_| candidate_path.clone());
        if !candidate_canon.starts_with(&wrapper_dir_canon) {
            return Ok(candidate.to_string());
        }
    }

    Err(format!(
        "could not find a real binary for `{}` outside of {}",
        name,
        wrapper_dir.display()
    ))
}

fn generate_script(name: &str, real_path: &str) -> String {
    format!(
        r##"#!/usr/bin/env bash
# OpenSquirrel agent wrapper for {name}
# Auto-generated -- do not edit manually.

REAL_BIN="{real_path}"
HOOKS_PORT="${{OPENSQUIRREL_HOOKS_PORT:-}}"
PANE_ID="${{OPENSQUIRREL_PANE_ID:-}}"

_osq_post() {{
    if [ -n "$HOOKS_PORT" ] && [ -n "$PANE_ID" ]; then
        curl -s -o /dev/null -X POST "http://127.0.0.1:$HOOKS_PORT/hooks" \
            -H "Content-Type: application/json" \
            -d "$1" 2>/dev/null || true
    fi
}}

# POST Stop on exit (covers normal exit, signals, exec failure)
trap '_osq_post "{{\"event\":\"Stop\",\"pane_id\":\"'$PANE_ID'\"}}"' EXIT

# POST Start
_osq_post '{{"event":"Start","agent":"{name}","pane_id":"'"$PANE_ID"'"}}'

# Hand off to the real binary
exec "$REAL_BIN" "$@"
"##,
        name = name,
        real_path = real_path,
    )
}

/// Get the environment variables to set in spawned PTY/subprocesses so that
/// wrapper scripts can communicate back to the hooks server.
pub(crate) fn hook_env_vars(
    port: u16,
    pane_id: &str,
    workspace_path: &str,
    bin_dir: &Path,
) -> Vec<(String, String)> {
    vec![
        ("OPENSQUIRREL_HOOKS_PORT".into(), port.to_string()),
        ("OPENSQUIRREL_PANE_ID".into(), pane_id.into()),
        ("OPENSQUIRREL_WORKSPACE".into(), workspace_path.into()),
        (
            "PATH".into(),
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        ),
    ]
}
