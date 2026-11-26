# limits-tui

Rust TUI that shows Claude, Codex and Devin usage limits. User-facing setup is in README.md.

## Workflow

- `master` is protected, admins included: branch, open a PR; no direct pushes.
- Before every commit that touches `src/`, `Cargo.toml` or `Cargo.lock`, run the `verify` skill
  (`.claude/skills/verify/verify.sh`). Plain `cargo test` doesn't drive the TUI.
  `verify.sh quick` (fmt, clippy, tests) is enough for test-only changes; anything in `src/` needs the full run.
- Run it: `cargo run -- --once` prints one snapshot; `cargo run` starts the TUI.
- Don't add dependencies or run `cargo update` unless asked.

## Layout

- `src/main.rs`: CLI flags, `--help`, `--once`, the event loop. `src/ui.rs`: drawing.
- `src/probe/`: `claude.rs`, `codex.rs`, `devin.rs` (one per source); `child.rs` runs child processes with deadlines
  and kills their process groups (`CHILDREN` registry); `mod.rs` holds the result types (`FetchOk`, `ProbeError`),
  JSON and timeout helpers, safe `PATH` handling and `kill_children()`.
- `tests/resolve_child.rs`: checks that the binary's hidden `__resolve` mode (the DNS lookup child the Devin probe
  spawns) is dispatched from `main`.

## Rules

- Coding rules are path-scoped in `.claude/rules/` and load with the files they cover.
- Reviews follow `REVIEW.md`, read from the base branch together with `.claude/rules/`.

## Pitfalls

- How each source is read: Claude from the capture file `~/.local/state/herdr-claude-limits/rate_limits.json`
  written by `scripts/claude-limits-tap` (no network); Codex through `codex app-server` JSON-RPC over stdio; Devin
  by an https POST with the key from its credentials TOML. The probes are ports of the herdr `*-limits` Python mods.
- Keep the capture path: the tap script and existing installs write there.
- `--once` output and the account numbers are real account data: never paste them into docs, commits or PRs.
- When adding or changing a CLI flag, key or environment variable, update `--help` in `src/main.rs` and the README
  in the same change.
