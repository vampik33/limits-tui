---
name: verify
description: Verify a limits-tui change before committing it - rustfmt, clippy and the tests, then the real binary (--once, and the TUI driven in a private tmux server with its screens captured). Use before every commit that touches src/, Cargo.toml or Cargo.lock.
---

# Verify limits-tui

From the repo root:

```bash
.claude/skills/verify/verify.sh          # all: quick, then run
.claude/skills/verify/verify.sh quick    # cargo fmt --check, clippy -D warnings, cargo test
.claude/skills/verify/verify.sh run      # build, --once, the TUI in tmux, quit-kills
```

## Which tier

- `src/main.rs`, `src/ui.rs`, anything the TUI draws or how it starts and quits: **all**. Only `run` catches a TUI that panics or never draws; `cargo test` doesn't drive the event loop.
- `src/probe/**`: **all**. `--once` runs every probe for real.
- Tests only: **quick**. Docs only: nothing.

## Reading the result

- One `PASS`/`FAIL` line per step. The first `FAIL` stops the run and prints the last 40 lines of that step's log. Exit 0 only when every step passed: read the whole output and the exit code, never through a pipe.
- `WARN once: <source>: error ...`: that source answered with an error (logged out, offline, rate-limited). That is account state, not the change: say so in the report, don't count it as that source working.
- `quit-kills`: the TUI runs again with a fake codex (`CODEX_BIN`) that never answers and keeps a background job in its process group; after `q` both must be gone (dead or zombie) within 3 s. On failure it names the surviving pids and kills the fake's group.
- Evidence: `target/verify/screen-start.txt`, `screen-refresh.txt`, `screen-exit.txt` (the TUI as drawn), `once.log`, `quit-kills.log` (the fake's pids); each step's log is `target/verify/<step>.log`.

## Not covered

The numbers on screen (they come from your accounts), terminal sizes other than 120x40, and the Esc / Ctrl-C quit paths.

## Timings and reliability (2026-11-23)

- `all`: 7-10 s warm; about 22 s on the first run after a change (clippy 12 s); about 45 s with an empty target dir.
- 6 clean runs on HEAD passed, with no tmux socket or process left behind.
- Deliberately broken changes, each caught at its step: a misformatted line -> `FAIL fmt`; control characters no longer escaped in `printable` -> `FAIL test`; a panic at TUI start -> `FAIL tui` with "exited (status 101)" and the panic text; tmux missing from PATH -> `FAIL deps`; `probe::kill_children()` removed from `main.rs` (2026-11-25) -> `FAIL quit-kills` naming both surviving pids.

The tmux server uses its own socket in a private directory (`mktemp -d`, mode 0700) under `$TMPDIR`, removed with it on exit, so it never touches your tmux sessions. The app only reads usage data.
