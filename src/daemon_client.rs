use serde_json::{Value as JsonValue, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

/// Client for connecting to a remote osqd daemon via SSH tunnel.
pub(crate) struct DaemonClient {
    stream: Option<TcpStream>,
    ssh_tunnel: Option<Child>,
    host: String,
    user: String,
    local_port: u16,
    remote_port: u16,
}

impl DaemonClient {
    pub(crate) fn new(host: &str, user: &str) -> Self {
        Self {
            stream: None,
            ssh_tunnel: None,
            host: host.to_string(),
            user: user.to_string(),
            local_port: 0,
            remote_port: 0,
        }
    }

    /// Connect to the remote daemon. Sets up SSH tunnel and TCP connection.
    /// If the daemon is not running, starts it via `osqd --ensure`.
    pub(crate) fn connect(&mut self) -> anyhow::Result<()> {
        // Ensure daemon is running on remote
        self.ensure_remote_daemon()?;

        // Read the remote daemon port
        self.remote_port = self.read_remote_port()?;

        // Pick a random local port
        let local_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        self.local_port = local_listener.local_addr()?.port();
        drop(local_listener);

        // Set up SSH port forward
        let destination = format!("{}@{}", self.user, self.host);
        let forward = format!("{}:127.0.0.1:{}", self.local_port, self.remote_port);
        let tunnel = Command::new("ssh")
            .args([
                "-N",
                "-L",
                &forward,
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                &destination,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        self.ssh_tunnel = Some(tunnel);

        // Give the tunnel a moment to establish
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Connect to the forwarded port
        let stream = TcpStream::connect(format!("127.0.0.1:{}", self.local_port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        self.stream = Some(stream);

        Ok(())
    }

    /// Disconnect and clean up tunnel.
    pub(crate) fn disconnect(&mut self) {
        self.stream = None;
        if let Some(ref mut tunnel) = self.ssh_tunnel {
            let _ = tunnel.kill();
        }
        self.ssh_tunnel = None;
    }

    /// Check if connected.
    pub(crate) fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Reconnect after a disconnect.
    pub(crate) fn reconnect(&mut self) -> anyhow::Result<()> {
        self.disconnect();
        self.connect()
    }

    // -- Protocol commands --

    pub(crate) fn ping(&mut self) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({"cmd": "ping"}))
    }

    pub(crate) fn list_dirs(&mut self, path: &str) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({"cmd": "list_dirs", "path": path}))
    }

    pub(crate) fn gpu_info(&mut self) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({"cmd": "gpu_info"}))
    }

    pub(crate) fn spawn_agent(
        &mut self,
        runtime: &str,
        model: &str,
        workdir: &str,
        prompt: &str,
        agent_id: &str,
    ) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({
            "cmd": "spawn_agent",
            "runtime": runtime,
            "model": model,
            "workdir": workdir,
            "prompt": prompt,
            "agent_id": agent_id,
        }))
    }

    pub(crate) fn kill_agent(&mut self, agent_id: &str) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({"cmd": "kill_agent", "agent_id": agent_id}))
    }

    pub(crate) fn send_prompt(
        &mut self,
        agent_id: &str,
        prompt: &str,
    ) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({
            "cmd": "send_prompt",
            "agent_id": agent_id,
            "prompt": prompt,
        }))
    }

    pub(crate) fn list_agents(&mut self) -> anyhow::Result<JsonValue> {
        self.send_command(&json!({"cmd": "list_agents"}))
    }

    /// Stream agent output, calling the callback for each line.
    /// Blocks until the agent finishes or the connection drops.
    pub(crate) fn stream_agent_output(
        &mut self,
        agent_id: &str,
        mut on_line: impl FnMut(&str),
    ) -> anyhow::Result<()> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let reader = BufReader::new(stream.try_clone()?);
        for line in reader.lines() {
            let line = line?;
            let msg: JsonValue = serde_json::from_str(&line)?;
            let event = msg["event"].as_str().unwrap_or("");
            let msg_agent = msg["agent_id"].as_str().unwrap_or("");

            if msg_agent != agent_id {
                continue;
            }

            match event {
                "agent_output" => {
                    if let Some(text) = msg["line"].as_str() {
                        on_line(text);
                    }
                }
                "agent_done" | "agent_killed" | "error" => break,
                _ => {}
            }
        }
        Ok(())
    }

    // -- Internal helpers --

    fn send_command(&mut self, cmd: &JsonValue) -> anyhow::Result<JsonValue> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.flush()?;

        // Read response
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut resp = String::new();
        reader.read_line(&mut resp)?;
        let value: JsonValue = serde_json::from_str(&resp)?;
        Ok(value)
    }

    fn ensure_remote_daemon(&self) -> anyhow::Result<()> {
        let destination = format!("{}@{}", self.user, self.host);
        let output = Command::new("ssh")
            .args([&destination, "~/.osq/bin/osqd --ensure"])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to ensure remote daemon: {stderr}");
        }
        Ok(())
    }

    fn read_remote_port(&self) -> anyhow::Result<u16> {
        let destination = format!("{}@{}", self.user, self.host);
        let output = Command::new("ssh")
            .args([&destination, "cat ~/.osq/run/daemon.port"])
            .output()?;
        if !output.status.success() {
            anyhow::bail!("failed to read remote daemon port");
        }
        let port_str = String::from_utf8_lossy(&output.stdout);
        let port: u16 = port_str.trim().parse()?;
        Ok(port)
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}
