# Tethys

Desktop app for managing multiple Claude Code CLI sessions in parallel across git worktrees. Each "workspace" bundles N worktrees (one per repo) plus the Claude sessions running inside them, with "your turn" notifications driven by Claude Code hooks.

**This is a personal tool built for Ryan.** No multi-user, no cross-platform, no distribution plans. macOS-only for the foreseeable future — feel free to take macOS-specific paths, shell invocations, or Tauri features without guarding them.

## Stack

Tauri 2.x shell · Rust core (`src-tauri/`) · React + TypeScript frontend (`src/`) · xterm.js (canvas addon) for terminal rendering · `portable-pty` for PTY spawning · JSON file persistence (no SQLite).

## Authoritative docs

- **`PLAN.md`** — architectural decisions, data model, IPC contract, subsystem designs. Update it when you change architecture.
- **`TASKS.md`** — milestone-scoped checkbox list. Tick boxes as work ships; add to the "Deferred" section if you punt something.

Both files already reflect past conversations — don't re-open decisions they've settled without flagging it first.

## Running

```
pnpm tauri dev
```

State lives at `~/Library/Application Support/app.tethys.dev/` (`state.json`, `logs/`, `repos.toml`, auto-generated `repos.schema.json`).
