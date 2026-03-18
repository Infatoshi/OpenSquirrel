# Superset Rewrite: Terminal Multiplexer + Agent Monitoring

## Origin

This spec comes from attempting to install and run [Superset](https://github.com/superset-sh/superset) (v1.1.7), a terminal app for running multiple coding agents in parallel with git worktree isolation. The install required Neon PostgreSQL, Stripe, Resend, Upstash Redis, and GitHub OAuth just to get past the login screen. After patching out Stripe API calls and filling in dummy keys for every SaaS dependency, the app still didn't work properly. The core terminal/agent features are buried under ~800KB of SaaS billing, email, and analytics infrastructure.

The actual useful functionality is ~15-20K lines of TypeScript across five areas. This spec extracts that functionality for native implementation in OpenSquirrel, which already has the agent subprocess model, PTY handling, and GPUI rendering.

## What Superset Does That OpenSquirrel Doesn't (Yet)

### 1. Git Worktree Isolation

**Problem**: Running multiple agents on the same repo causes conflicts -- agents step on each other's files.

**Superset's approach**: Each "workspace" gets its own git worktree (separate working directory, shared .git).

**What to implement in OpenSquirrel**:

- When creating an agent that targets a git repo, offer "isolated worktree" mode
- `git worktree add ~/.osq/worktrees/{branch-name} -b {branch-name}` from the repo root
- Agent's working directory becomes the worktree path instead of the main repo
- On agent/workspace deletion: `git worktree remove --force {path}`
- Store worktree metadata in state.json: `{ path, branch, base_branch, repo_root }`
- Setup hook: if repo has `.osq/setup.sh`, run it in the worktree after creation (for dep install, env copy, etc.)
- Teardown hook: `.osq/teardown.sh` before worktree removal

**Implementation location**: New module `src/worktree.rs`

**Data model addition to AgentState**:
```rust
struct WorktreeInfo {
    repo_root: PathBuf,       // original repo
    worktree_path: PathBuf,   // ~/.osq/worktrees/{name}
    branch: String,           // working branch
    base_branch: String,      // branch it was created from
}
```

**Crate**: `git2` for worktree operations, or shell out to `git` directly (simpler, fewer edge cases with auth/ssh).

### 2. Agent Shell Wrappers (Lifecycle Hooks)

**Problem**: OpenSquirrel currently monitors agents via stdout parsing. It can't detect when an agent inside a terminal tile starts working, stops, or needs permission -- because terminal tiles are raw shells.

**Superset's approach**: On app startup, create wrapper scripts in `~/.superset/bin/` for each agent CLI (claude, codex, gemini, opencode, cursor, copilot). Prepend that directory to PATH. The wrappers intercept the real binary and POST HTTP lifecycle events (Start, Stop, PermissionRequest) to a per-workspace hooks server on localhost.

**What to implement in OpenSquirrel**:

- On app startup, generate wrapper scripts in `~/.osq/bin/` for each configured runtime
- Each wrapper:
  1. Posts `{ "event": "Start", "agent": "{name}", "pane_id": "$OPENSQUIRREL_PANE_ID" }` to `http://localhost:$OPENSQUIRREL_PORT/hooks`
  2. Exec's the real binary with all original args
  3. Posts `{ "event": "Stop", ... }` on exit (via trap)
- Run a tiny HTTP server (one per app instance) on a random port
- Set environment variables in every spawned PTY/subprocess:
  ```
  OPENSQUIRREL_PORT={hooks_server_port}
  OPENSQUIRREL_PANE_ID={agent_id}
  OPENSQUIRREL_WORKSPACE_PATH={worktree_or_cwd}
  PATH=~/.osq/bin:$PATH
  ```
- When hook events arrive, update AgentStatus accordingly

**Why this matters**: This lets terminal tiles (raw shells) report agent status. User opens a terminal, runs `claude`, the wrapper fires Start, OpenSquirrel shows "Working" status. Agent finishes, wrapper fires Stop, status goes to "Idle" or "Review".

**Implementation**: `src/hooks.rs` (HTTP server + wrapper generation). Use `axum` or `tiny_http` for the server. Wrapper scripts are just bash -- generate them as string literals.

**Wrapper template** (per agent):
```bash
#!/bin/bash
_osq_cleanup() { curl -s -X POST "http://localhost:${OPENSQUIRREL_PORT}/hooks" \
  -d "{\"event\":\"Stop\",\"pane_id\":\"${OPENSQUIRREL_PANE_ID}\"}" 2>/dev/null; }
trap _osq_cleanup EXIT
curl -s -X POST "http://localhost:${OPENSQUIRREL_PORT}/hooks" \
  -d "{\"event\":\"Start\",\"pane_id\":\"${OPENSQUIRREL_PANE_ID}\"}" 2>/dev/null
exec /usr/local/bin/{real_binary} "$@"
```

### 3. Diff Viewer Panel

**Problem**: OpenSquirrel classifies diff lines in transcripts (green/red/blue) but has no way to view the actual file changes an agent made.

**Superset's approach**: Right-side "Changes" panel showing git status (staged/unstaged files), file tree, and inline diff viewer with syntax highlighting.

**What to implement in OpenSquirrel**:

- New panel (toggled with `Cmd+L` or keybind `l` in command mode): "Changes" panel on the right side
- Shows `git status --porcelain` output for the focused agent's working directory
- File list grouped by: staged, unstaged, untracked
- Clicking/selecting a file shows unified diff (`git diff {file}` or `git diff --cached {file}`)
- Diff rendering: reuse existing LineKind::DiffAdd/DiffRemove/DiffHunk coloring from lib.rs
- Git operations from the panel:
  - Stage/unstage files (checkbox or keybind)
  - Commit with message (inline input)
  - Push (after commit)

**Implementation**: `src/changes.rs` for git operations, new GPUI view for the panel. Use `git2` crate or shell out to `git`.

**Data model**:
```rust
struct ChangesState {
    workdir: PathBuf,
    staged: Vec<FileChange>,
    unstaged: Vec<FileChange>,
    untracked: Vec<String>,
    selected_file: Option<usize>,
    diff_content: Option<String>,
}

struct FileChange {
    path: String,
    status: FileStatus,  // Modified, Added, Deleted, Renamed
}
```

### 4. Parallel Agent Monitoring Dashboard

**Problem**: OpenSquirrel has Grid/Pipeline/Focus views but no at-a-glance status dashboard for many concurrent agents.

**Superset's approach**: Sidebar shows all workspaces with color-coded status indicators. Notifications when agents need attention (permission request, error, completion).

**What to implement in OpenSquirrel**:

- Enhance existing sidebar agent list with prominent status indicators:
  - Green dot = idle/done
  - Yellow pulse = working
  - Red dot = error/blocked
  - Blue dot = needs permission/attention
- System notification (macOS native) when:
  - Agent finishes a task (transition Working -> Idle)
  - Agent requests permission (PermissionRequest hook)
  - Agent errors out
- Sound alert option (configurable, off by default)
- Badge count on dock icon showing agents needing attention

**Implementation**: Extend existing `AgentStatus` rendering. Use `notify-rust` or macOS `NSUserNotification` via objc crate for system notifications.

### 5. Gemini CLI Integration

**Problem**: Superset supports Gemini CLI. OpenSquirrel's runtime list doesn't include it.

**What to implement**:

Add to default runtimes in config.rs:
```rust
RuntimeDef {
    name: "gemini".into(),
    cli: "gemini".into(),
    model_flag: Some("-m".into()),
    default_model: Some("gemini-2.5-pro".into()),
    // ...
}
```

Also add GitHub Copilot:
```rust
RuntimeDef {
    name: "copilot".into(),
    cli: "github-copilot-cli".into(),
    // ...
}
```

### 6. Setup/Teardown Scripts (Workspace Presets)

**Problem**: When creating a worktree, you often need to install deps, copy .env files, run migrations, etc.

**Superset's approach**: `.superset/config.json` with `setup` and `teardown` arrays of shell commands. Environment variables `SUPERSET_WORKSPACE_NAME` and `SUPERSET_ROOT_PATH` available in scripts.

**What to implement**:

- Check for `.osq/setup.sh` in the repo root when creating a worktree agent
- Run it with env vars:
  ```
  OPENSQUIRREL_WORKSPACE_NAME={agent_name}
  OPENSQUIRREL_ROOT_PATH={repo_root}
  OPENSQUIRREL_WORKTREE_PATH={worktree_path}
  ```
- Check for `.osq/teardown.sh` before worktree removal
- Show setup output in agent transcript as system messages

## Architecture Comparison

| Concern | Superset (Electron/TS) | OpenSquirrel (Rust/GPUI) |
|---------|----------------------|------------------------|
| Terminal | node-pty + xterm.js + daemon subprocess | portable-pty + gpui-terminal |
| Git | simple-git (JS) + shell | git2 crate or shell |
| Agent spawn | Shell wrapper scripts + HTTP hooks | Direct subprocess + stdout parsing |
| State | SQLite (drizzle) + Neon PostgreSQL | JSON file (~/.osq/state.json) |
| IPC | tRPC over Electron IPC | In-process (single binary) |
| UI | React + Tailwind + xterm.js | GPUI (Metal/Vulkan) |
| Auth | Better-auth + GitHub OAuth + Stripe | None needed (local app) |
| Email | Resend | None needed |
| Analytics | PostHog + Sentry | None needed |
| Rate limiting | Upstash Redis | None needed |

## What NOT to Port

- **Auth/billing/teams** -- OpenSquirrel is a local tool, not a SaaS
- **Electric SQL sync** -- no cloud sync needed
- **Organization model** -- single user
- **Email notifications** -- system notifications instead
- **tRPC / Electron IPC** -- OpenSquirrel is single-process, no IPC needed
- **Marketing site, admin dashboard, docs site** -- irrelevant
- **MCP server for desktop** -- OpenSquirrel already has MCP client support via config

## Implementation Priority

1. **Git worktree isolation** (highest value -- enables safe parallel agents)
2. **Agent shell wrappers + hooks server** (enables terminal tile status tracking)
3. **Diff viewer panel** (review agent changes without leaving the app)
4. **System notifications** (know when agents need attention)
5. **Gemini + Copilot runtime definitions** (trivial config additions)
6. **Setup/teardown scripts** (quality of life for worktree workflows)

## Estimated Scope

| Feature | New lines (est.) | New files |
|---------|-----------------|-----------|
| Worktree manager | ~400 | src/worktree.rs |
| Shell wrappers + hooks server | ~500 | src/hooks.rs |
| Diff viewer panel | ~800 | src/changes.rs + UI in app.rs |
| System notifications | ~100 | in app.rs |
| New runtime defs | ~30 | in config.rs |
| Setup/teardown scripts | ~100 | in worktree.rs |
| **Total** | **~1,930** | **3 new files** |

Compare to Superset's ~15-20K lines of equivalent TypeScript (plus ~50K of SaaS infrastructure). The Rust implementation benefits from being single-process (no IPC), having no auth/billing, and reusing OpenSquirrel's existing agent model.
