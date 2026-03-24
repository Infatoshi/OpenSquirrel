<p align="center">
  <img src="assets/logo.png" width="128" alt="OpenSquirrel">
</p>

<h1 align="center">OpenSquirrel</h1>

<p align="center">
  <strong>This project is no longer actively maintained.</strong><br>
  Fork it, customize it, make it yours.
</p>

<p align="center">
  <img src="assets/screenshot-main.png" width="800" alt="OpenSquirrel screenshot">
</p>

## What it was

A native, GPU-rendered tiling manager for AI coding agents. Rust + GPUI. Run Claude Code, Codex, Cursor, Gemini, and OpenCode side by side with automatic sub-agent delegation, remote machine targeting via SSH, and persistent sessions.

## Why it's archived

After a week of building this, I arrived at a simpler conclusion: **you don't need a custom GUI to orchestrate AI agents.**

Here's what I learned:

**The terminal already won.** Every AI coding CLI (Claude Code, Codex, Cursor Agent, Gemini CLI, OpenCode) ships with a polished terminal TUI. Building a Rust GUI that parses their JSON output and re-renders it will always be worse than just... using the native TUI. Users are comfortable in their terminal. They don't want a new window.

**Delegation is a prompt, not a product.** The entire coordinator/worker delegation system -- spawning sub-agents across runtimes, collecting results, feeding them back -- can be done with 4 lines in a `CLAUDE.md` file telling the model to use `cursor agent --print` or `codex exec` via Bash. No orchestration daemon needed. No hooks. No middleware. Claude Code's Agent tool already handles internal delegation. For external CLIs, just run them headless.

**Token tracking already exists.** [CodexBar](https://github.com/steipete/CodexBar) sits in your macOS menu bar and tracks usage across Claude, Codex, Cursor, Gemini, and more by reading their local data files. No need to build this into a GUI.

**The architectural mismatch.** Using Claude Code (a Node/Bun process) to build and iterate on a Rust GPU application through JSON stream parsing is a bizarre feedback loop. The model is trained on terminal interactions, not on debugging GPUI render pipelines. Every feature took 10x longer than it should have because the tooling fought the workflow.

**Models aren't good enough yet for opinionated UX.** Nobody knows the right workflow for multi-agent coding. Building a rigid UI around one workflow locks you in. The terminal is infinitely flexible. Wait for patterns to emerge before building products around them.

## What works instead

The setup I actually use now:

- **Terminal**: Ghostty (or whatever you prefer)
- **Agents**: Run them directly -- `claude`, `codex`, `cursor agent`, `gemini`
- **Delegation**: Instructions in `~/.claude/CLAUDE.md` telling Claude to run external CLIs via Bash when asked
- **Token tracking**: CodexBar (menu bar app, reads local files)
- **Multi-agent**: Just open multiple terminal tabs/panes

That's it. No custom software. The orchestration layer is a config file.

## If you want to use or fork this

The code works. The `legacy` branch at commit `78f1bf2` has the full feature set:

- Multi-agent grid with auto-layout
- Coordinator/worker delegation across runtimes
- Remote SSH targeting with tmux session persistence
- Reusable UI components (FuzzyList, modal builders, selectable rows)
- Model picker with fuzzy search (Cmd+M)
- Token/cost tracking per agent
- 7 themes, persistent state, MCP integration
- 93 passing tests

### Build & run

```bash
cargo build --release
# Run as .app bundle:
cp target/release/opensquirrel dist/OpenSquirrel.app/Contents/MacOS/OpenSquirrel-bin
open dist/OpenSquirrel.app
```

Requires Rust 1.85+ and macOS (Metal GPU). Linux (Vulkan) compiles and tests pass.

### Config

`~/.osq/config.toml` -- runtimes, machines, MCPs, themes, settings.

### Supported runtimes

| Runtime | CLI | Mode |
|---------|-----|------|
| Claude Code | `claude` | Persistent multi-turn |
| Codex | `codex` | One-shot |
| Cursor Agent | `cursor agent` | One-shot |
| Gemini CLI | `gemini` | One-shot |
| OpenCode | `opencode` | One-shot |

## License

MIT
