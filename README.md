# limits-tui

Claude, Codex and Devin usage limits in one terminal UI.

```text
┌ limits ──────────────────────────────────────────────────────────────────────────────────────────────┐
│                                                                                                      │
│ ✻ Claude  5h ███░░░░░░░░░  25% 3h 10m  ·  7d ████░░░░░░░░  30% 4d 2h                                 │
│ ✱ Codex   5h ██░░░░░░░░░░  20% 3h 40m  ·  wk ██████░░░░░░  50% 1d 0h  ·  plus                        │
│ ◆ Devin   day ██░░░░░░░░░░  10% 21h 5m  ·  wk ████░░░░░░░░  35% 3d 21h  ·  $5.00 over  ·  Pro        │
│                                                                                                      │
│ r refresh   q quit                                                                                   │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## Features

- The 5-hour and weekly windows of Claude and Codex and the daily and weekly ones of Devin: how much of each is used
  and when it resets. Other windows a service adds are not shown.
- Read-only: it reads what each tool already stores and never changes your accounts.
- Runs fine over SSH; `--once` prints plain lines for scripts.

## Requirements

- Linux (other Unix systems are untested), and a Rust toolchain recent enough for edition 2024 (current stable).
- For each service you want to see:
  - **Claude**: a Claude Pro or Max plan, Claude Code, and `jq` (see [Claude setup](#claude-setup)).
  - **Codex**: the `codex` CLI, signed in with a ChatGPT plan.
  - **Devin**: the Devin CLI, signed in.

A service you don't use shows a short status (for example "not installed") instead of numbers.

## Install

```sh
cargo install --path .
```

This installs `limits-tui` to `~/.cargo/bin`.

## Usage

```sh
limits-tui          # the TUI
limits-tui --once   # print one line per service and exit (also -1)
limits-tui --help
```

| Key | Action |
| --- | --- |
| `r` | refresh all services now |
| `q`, `Esc`, `Ctrl-C` | quit |

Claude is re-read every 15 seconds; Codex and Devin every 5 minutes, and right after a window resets.

## Setup

### Claude setup

limits-tui doesn't call Anthropic for Claude usage. It reads
`~/.local/state/herdr-claude-limits/rate_limits.json`, which is written by
`scripts/claude-limits-tap`. Until that file exists, the Claude row shows
"start a Claude Code session".

Claude Code passes the session's `rate_limits` (5-hour and 7-day windows) to its
statusLine command on stdin. The tap saves them to that file, then passes the same
input on to your real status line, so your status bar looks the same as before.
`rate_limits` is only sent on a Claude Pro or Max plan.

1. Install the script:

   ```sh
   install -m 755 scripts/claude-limits-tap ~/.local/bin/claude-limits-tap
   ```

2. Add your real status line at the end of the script, fed the same input:
   `printf '%s' "$input" | exec <your current statusLine.command>`. The script
   runs no status line by default (so it pulls in no extra package), and your
   status bar stays empty until you add one; the comment there shows ccstatusline
   as an example.

3. Point `statusLine` in `~/.claude/settings.json` at the script (use the full path):

   ```json
   "statusLine": {
     "type": "command",
     "command": "/home/<you>/.local/bin/claude-limits-tap"
   }
   ```

4. Send one message in any Claude Code session (`rate_limits` arrives after the
   first response), then check:

   ```sh
   cat ~/.local/state/herdr-claude-limits/rate_limits.json
   limits-tui --once
   ```

   A running limits-tui picks it up within 15 seconds; press `r` to refresh now.

### Codex setup

Sign in to the `codex` CLI. limits-tui runs `codex app-server` and asks it for the account's rate limits; this makes
no model request. `codex` is looked up in `CODEX_BIN`, then `PATH`, then your login shell's `PATH` (for
launches from a desktop that lacks it).

### Devin setup

Sign in to the Devin CLI. limits-tui reads the API key from `~/.local/share/devin/credentials.toml` and asks the
API server configured there (by default `https://server.codeium.com`) for your quota. The key is only ever sent
over https. The server's address is looked up in a short-lived copy of limits-tui, so a DNS lookup that hangs is
stopped at `DEVIN_LIMITS_TIMEOUT` instead of running on in the background.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `CODEX_BIN` | found on `PATH` | path to the `codex` binary |
| `CODEX_LIMITS_TIMEOUT` | `15` | seconds a Codex check may take |
| `DEVIN_CREDENTIALS` | `~/.local/share/devin/credentials.toml` | Devin credentials file |
| `DEVIN_LIMITS_TIMEOUT` | `15` | seconds a Devin check may take |

## What the screen shows

| On screen | Meaning |
| --- | --- |
| `not signed in` | the tool has no signed-in account |
| `not installed` | `codex` wasn't found |
| `no quota` | the account has no usage windows (for example, Devin without quota billing) |
| `timeout` | the check took longer than its timeout |
| `unavailable` | the check failed in another way |
| `update failed` next to numbers | the last refresh failed; the numbers are from the one before |
| `updating…` | a refresh you asked for with `r` is running |
| dimmed row | Claude data older than 15 minutes, or numbers from before a failed refresh |

## Development

`master` only accepts pull requests. Before opening one, run the checks:

```sh
.claude/skills/verify/verify.sh quick   # cargo fmt --check, clippy -D warnings, cargo test
.claude/skills/verify/verify.sh         # also runs the real binary and drives the TUI in tmux (needs tmux)
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
