# OpenSquirrel

GPU-rendered, keyboard-driven agent session hub. A native control plane for AI coding agents with coordinator/worker delegation, remote machine targeting, and persistent sessions.

Written in Rust. Built on GPUI. No Electron. No web tech. Keyboard-first.

## Current State (what exists and works)

### Core Stack
- **UI**: GPUI (standalone crate, not a Zed fork)
- **Language**: Rust, 100%
- **Platform**: macOS (Metal) primary, Linux (Vulkan via Blade) possible
- **App bundle**: `scripts/launch-opensquirrel-app.sh` builds release binary and opens `dist/OpenSquirrel.app` with proper icon

### Agent Model
- Agents are subprocesses spawned from CLI runtimes (Claude Code, Cursor, Codex, OpenCode)
- Local Claude coordinator uses persistent stream-json stdin for multi-turn conversations
- Other runtimes use one-process-per-prompt
- Coordinators get a delegation preamble instructing them to spawn workers via ```delegate fenced blocks
- Workers are fresh-context agents that return condensed results (final text + metadata + diff summary) to the coordinator
- Workers can target local or remote (SSH + tmux) machines

### Machine Targets
- Configured in `~/.osq/config.toml` under `[[machines]]`
- Default: `local` and `theodolos` (SSH)
- Remote workers launch inside named tmux sessions on the target machine
- Remote session names and line cursors are persisted for reattach on app restart
- Machine selection is available in the setup wizard and in delegated task JSON

### Persistence
- Config: `~/.osq/config.toml` (runtimes, machines, MCPs, theme, font, settings)
- State: `~/.osq/state.json` (agents, transcripts, scroll positions, worker assignments, pending prompts, turn state, remote session info)
- Turn-boundary journaling: pending prompts and turn state are saved so interrupted turns can be resumed
- Restored agents show a banner (not injected into transcript history)

### UI Layout
- Top bar: minimal — ⚙ settings (opens command palette), ⊞ stats toggle. No search bar (use `/` key).
- Left sidebar: agents tab / workers tab, group navigation, agent list with role/runtime/machine indicators
- Main area: focused agent tile (default view), grid view, pipeline view
- Agent tile: single compact header row (squirrel icon | name | status | elapsed | model | tokens/cost | context% | action icons), optional badges row, worker strip, transcript area, input bar
- Confirmation modal for destructive remove action (red yes / normal no)
- Command palette (Cmd-K): themes, settings toggles, new agent, mic selection

### Keybinds (current)
- `Esc` → command mode
- `i` → insert mode
- `Enter` → send prompt (default) or insert newline (cautious mode)
- `Cmd+Enter` → always send
- `j/k` → scroll transcript
- `w/s` → switch groups
- `a/d` → switch panes (left/right)
- `n` → new agent (opens setup wizard)
- `c` → change agent runtime/model/machine
- `r` → relaunch agent
- `x` → stop agent
- `f` → toggle favorite
- `p` → toggle auto-scroll
- `|` → pipe output to next agent
- `g t` → open working directory in Terminal
- `/` → open search panel
- `t` → cycle theme
- `?` → stats/shortcuts panel
- `` ` `` → toggle voice recording (whisper.cpp)
- `1/2/3` → grid/pipeline/focus view
- `Cmd-K` → command palette

### Settings (toggleable via command palette)
- Cautious Enter (off by default): makes Enter insert newline, Cmd+Enter send
- Terminal Text (off by default): uses monospace font for transcript instead of prose font
- Whisper Model: configurable model name (default: large-v3-turbo)
- Audio Device: selectable microphone from available input devices

### Voice Input
- whisper.cpp via `whisper-rs` crate with Metal GPU acceleration
- `cpal` for audio capture at device native sample rate
- Resamples to 16kHz before inference
- Model files expected at `~/.osq/models/ggml-{name}.bin`
- Toggle with backtick key, shows red REC indicator while recording

### Themes
midnight, charcoal, gruvbox, solarized-dark, light, solarized-light, ops, monokai-pro

### Transcript Rendering
- Prose font (Helvetica Neue) by default, monospace optional via setting
- Message blocks with rounded card styling and spacing
- User prompts in distinct bordered cards
- Code blocks with monospace font and syntax-aware background
- Headings, bullets, inline markdown (bold, italic, code spans)
- Diff lines color-coded (green add, red remove, blue hunk)
- System/error/thinking lines styled distinctly
- Per-message copy icon (not per-line)
- Mouse/trackpad scroll wheel support on transcript area

## Next Steps (from raw thoughts + session direction)

### Immediate (ship-quality polish)
- [x] Hide search bar from top bar entirely; search is `/`-only, no visible trigger needed
- [x] Compress agent info into a single thin bar: model | status | tokens | cost — one line, not multi-row
- [x] Replace remaining text-based icons with proper icon symbols (⚙ settings, ⊞ stats)
- [x] Remove voice feature for v1 ship (gated behind `VOICE_ENABLED` const, keybind + palette hidden)

### Short-term (product direction)
- [ ] Let the model control delegation entirely — don't build swarm UX with keybinds, give Opus the ability to spawn/manage sub-agents and trust it to improve over time
- [ ] Group chat mode: API-only chat room where multiple agents can be added, with @mentions, configurable reply order, and manual turn-taking option
- [ ] Test coordinator → worker delegation with Opus actually driving it end-to-end on a real task
- [ ] Customizable keybinds settings UI

### Medium-term (differentiation)
- [ ] Tab completion model for IDE actions (requires fast local inference — not feasible with current token generation speeds, revisit when local models are faster)
- [ ] Streaming/chunked voice transcription instead of record-then-transcribe
- [ ] Remote Parakeet on CUDA targets for voice-to-text on GPU boxes
- [ ] In-app model download for whisper variants

### Non-Goals (for now)
- Code editor (agents edit code, not the user)
- File tree browser
- LSP integration
- Plugin/extension system
- Collaboration/team features
- Approval queue (removed — not part of the workflow)

---

## Remote Daemon (osqd)

### Problem

When you select a remote machine (e.g. Theodolos) in the setup wizard, it shows local directories and has no way to browse remote filesystems, spawn agents remotely with persistence, or survive laptop sleep. The current tmux-based approach breaks after extended disconnects.

### Architecture

The daemon (`osqd`) is the same OpenSquirrel binary run with `--daemon` flag. It runs headless on remote Linux machines and provides:

1. **Agent lifecycle management** -- spawn, monitor, kill agent subprocesses (claude, codex, gemini, etc.) that persist independently of the SSH connection
2. **Filesystem access** -- directory listings, file reads, git status for the remote machine so the setup wizard shows remote paths
3. **GPU/system info** -- report GPU utilization, VRAM, running processes for machine selection

### Transport

**SSH tunnel + TCP** (JSON-lines protocol over localhost):
- Daemon listens on `127.0.0.1:{port}` only (never exposed externally)
- Local app opens SSH port forward: `ssh -L {local_port}:127.0.0.1:{remote_port} {host}`
- Communication is JSON-lines over the forwarded TCP socket
- Works through any network, uses existing SSH keys, zero extra attack surface
- Connection drops cleanly on laptop sleep; local app reconnects automatically

### Persistence Model

- Agents continue running on the remote machine when the local app disconnects (laptop sleep, network drop, app quit)
- On reconnect, local app receives buffered output that accumulated while disconnected
- Agent state is persisted to `~/.osq/state.json` on the remote machine
- The daemon process itself is a long-running background process (not tied to any terminal)

### UI Integration

- Remote agents appear in the same mixed grid as local agents
- Machine name shown in tile header (already exists)
- No separate views per machine
- Setup wizard shows remote directories when a remote machine is selected (daemon serves `list_dirs` request)

### Protocol (JSON-lines over TCP)

```
// Client -> Daemon
{"cmd": "list_dirs", "path": "/home/user"}
{"cmd": "spawn_agent", "runtime": "claude", "model": "opus-4.6", "workdir": "/home/user/project", "prompt": "..."}
{"cmd": "kill_agent", "agent_id": "agent-0"}
{"cmd": "send_prompt", "agent_id": "agent-0", "prompt": "..."}
{"cmd": "list_agents"}
{"cmd": "gpu_info"}
{"cmd": "ping"}

// Daemon -> Client
{"event": "agent_output", "agent_id": "agent-0", "line": "..."}
{"event": "agent_status", "agent_id": "agent-0", "status": "working"}
{"event": "agent_done", "agent_id": "agent-0", "session_id": "..."}
{"event": "dirs", "entries": ["/home/user/projects", "/home/user/repos"]}
{"event": "gpu", "gpus": [{"name": "RTX 3090", "util": 45, "vram_used": 8192, "vram_total": 24576}]}
{"event": "pong", "version": "0.1.0", "uptime_secs": 3600}
```

### Deployment

- First use: clone repo to `~/.osq/repo/` on remote, `cargo build --release`, symlink to `~/.osq/bin/osqd`
- Updates: `git pull && cargo build --release` triggered by local app when version mismatch detected
- Daemon starts as background process: `nohup ~/.osq/bin/osqd &`
- PID file at `~/.osq/osqd.pid` for lifecycle management
- Auto-start: local app runs `ssh {host} '~/.osq/bin/osqd --ensure'` which starts the daemon if not running

### Remote Directory Structure (~/.osq/ on remote)

```
~/.osq/
  config.toml          # daemon config (port, log level)
  state.json           # persisted agent state
  osqd.pid             # daemon PID
  osqd.log             # daemon log (rotated)
  bin/
    osqd -> ../repo/target/release/opensquirrel
  repo/                # git clone of OpenSquirrel (for building)
    ...
  worktrees/           # git worktrees for isolated agents
    feature-branch/
```

### Daemon Lifecycle

1. **First connection to a new machine**: local app SSHs in, checks for `~/.osq/bin/osqd`. If missing, clones repo and builds.
2. **Subsequent connections**: local app SSHs in, runs `osqd --ensure` (no-op if already running, starts if not).
3. **Version mismatch**: daemon responds to `ping` with its version. If local app version differs, it triggers `git pull && cargo build --release && osqd --restart`.
4. **Daemon crash recovery**: on next connection, `--ensure` detects stale PID file and restarts.

### Implementation Plan

1. `src/daemon.rs` -- daemon mode entry point, TCP listener, JSON protocol handler
2. `src/daemon_client.rs` -- client-side connection manager, SSH tunnel setup, reconnection logic
3. Modify `create_agent_with_role` -- for remote machines, send `spawn_agent` to daemon instead of local subprocess
4. Modify setup wizard -- for remote machines, send `list_dirs` to daemon instead of local `list_subdirs()`
5. Add `--daemon` and `--ensure` CLI flags to main.rs
6. Add GPU info collection (parse `nvidia-smi` output)

### TODO (plan more thoroughly)
- [ ] Exact version negotiation protocol
- [ ] GitHub-based update notifications (check latest release tag)
- [ ] Graceful daemon shutdown and agent cleanup
- [ ] Log rotation and disk space management
- [ ] Multi-user support (multiple local apps connecting to same daemon)
- [ ] Authentication/authorization for daemon connections
