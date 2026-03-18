<p align="center">
  <img src="assets/logo.png" width="128" alt="OpenSquirrel">
</p>

<h1 align="center">OpenSquirrel</h1>

<p align="center">Native, GPU-rendered control plane for AI coding agents. Rust + GPUI. No Electron.</p>

Run Claude Code, Codex, Cursor, and OpenCode side by side with automatic sub-agent delegation, remote machine targeting via SSH, and persistent multi-turn sessions.

## What it does

- **Multi-agent grid** — Run multiple agents simultaneously in a responsive tiled layout. Agents auto-arrange based on count (1=full, 2=split, 4=2×2, etc).
- **Coordinator/worker delegation** — A primary agent (Opus) can automatically spawn sub-agents for focused tasks. Workers return condensed results, not full transcripts.
- **Remote machine targeting** — Agents can target local or remote machines via SSH + tmux. Configure machines in `~/.osq/config.toml`.
- **MCP integration** — MCP servers (Playwright, browser-use, etc.) are wired directly to agent CLI args. Select per-agent during setup.
- **Persistent sessions** — Agent state, transcripts, scroll positions, and pending prompts survive app restarts. Interrupted turns can be resumed.
- **Structured output parsing** — Parses `stream-json` output from all runtimes. Custom markdown rendering with code blocks, diffs, headings, bullets.

## Supported runtimes

| Runtime | Mode | Permission bypass |
|---------|------|-------------------|
| Claude Code | Persistent stdin (multi-turn) | `--dangerously-skip-permissions` |
| Codex | One-shot per prompt | `--dangerously-bypass-approvals-and-sandbox` |
| Cursor Agent | One-shot per prompt | `--yolo` |
| OpenCode | One-shot per prompt | Auto-approved in `run` mode |

## Build & run

```bash
cargo build --release
./target/release/opensquirrel
```

Requires Rust toolchain and macOS (Metal GPU).

To run as a proper macOS `.app` bundle with the squirrel icon:

```bash
# Build
cargo build --release

# Create .app bundle
mkdir -p dist/OpenSquirrel.app/Contents/{MacOS,Resources}
cp target/release/opensquirrel dist/OpenSquirrel.app/Contents/MacOS/OpenSquirrel
cp assets/OpenSquirrel.icns dist/OpenSquirrel.app/Contents/Resources/

# Launch
open dist/OpenSquirrel.app
```

Note: macOS `.app` bundles don't inherit your shell PATH. If agents like `claude` or `npx` aren't found, run the binary directly instead of via the `.app` bundle.

## Keybinds

No vim modes — text input is always active. All actions use standard Cmd- keybinds.

| Key | Action |
|-----|--------|
| `Enter` | Send prompt |
| `Escape` | Dismiss overlay (palette, setup wizard, search) |
| `Cmd-N` | New agent (opens setup wizard) |
| `Cmd-K` | Command palette (themes, settings, compact context, kill, views) |
| `Cmd-F` | Search transcripts |
| `Cmd-]` / `Cmd-[` | Next / prev pane within group |
| `Cmd-}` / `Cmd-{` | Next / prev group |
| `Cmd-V` | Paste from clipboard |
| `Cmd-=` / `Cmd--` | Zoom in / out |

**Setup wizard:** Arrow keys to navigate, `Enter` to drill into directories, `Backspace` to go up, `Tab` to advance step, `Shift-Tab` to go back.

## Configuration

Config lives at `~/.osq/config.toml`. Defines runtimes, models, MCP servers, machines, themes, and settings.

State is persisted at `~/.osq/state.json` (agents, transcripts, scroll positions).

## Architecture

~7,200 lines of Rust across 3 files:
- `src/main.rs` — UI, agent lifecycle, rendering, keybinds, persistence
- `src/lib.rs` — Line classification, markdown parsing, diff summarization, helpers
- `tests/state_tests.rs` — 30 integration tests covering navigation, scrolling, themes, search, agent lifecycle

Built on [GPUI](https://crates.io/crates/gpui) (the UI framework from Zed, used as a standalone crate). GPU-rendered via Metal on macOS.

## Themes

midnight, charcoal, gruvbox, solarized-dark, light, solarized-light, ops, monokai-pro

## License

MIT
