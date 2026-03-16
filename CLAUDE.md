# OpenSquirrel

GPU-rendered, keyboard-driven agent tiling manager built on GPUI (Rust).

## Build / Run / Test

```bash
cargo build                        # dev build
cargo build --release              # release build
cargo test                         # run all tests
scripts/launch-opensquirrel-app.sh # build + launch as macOS .app bundle
OPEN_SQUIRREL_PROFILE=debug scripts/launch-opensquirrel-app.sh  # debug launch
```

## Key Constraints

- Rust 2024 edition (≥ 1.85), gpui 0.2 standalone crate (not a Zed fork)
- macOS (Metal) and Linux (Vulkan, x86_64) supported
- `main.rs` is large (273K+) -- use offset/limit when reading, or grep for specific sections
- `lib.rs` contains line classification, markdown span parsing, diff summary utils
- whisper-rs: Metal feature on macOS, CPU-only on Linux; cpal for audio capture
- Agents are subprocesses (claude CLI, cursor CLI, opencode CLI) -- not an agent runtime
- Launch for manual testing via `scripts/launch-opensquirrel-app.sh`, not `cargo run`
- SPEC.md is the north star for features and phasing
