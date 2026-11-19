# limits-tui

Claude / Codex / Devin usage limits in one TUI.

## Claude limits: install the statusLine tap

limits-tui doesn't call Anthropic for Claude usage. It reads
`~/.local/state/herdr-claude-limits/rate_limits.json`, which is written by
`scripts/claude-limits-tap`. Until that file exists, the Claude row shows
"start a Claude Code session".

Claude Code passes the session's `rate_limits` (5-hour and 7-day windows) to its
statusLine command on stdin. The tap saves them to that file, then passes the same
input on to your real status line, so your status bar looks the same as before.

Requires `jq`, and a Claude Pro/Max subscription (`rate_limits` isn't sent otherwise).

1. Install the script:

   ```sh
   install -m 755 scripts/claude-limits-tap ~/.local/bin/claude-limits-tap
   ```

2. The script's last line runs your real status line. It ships with
   `bunx -y ccstatusline@2.2.34`; edit that line to whatever your current
   `statusLine.command` is.

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
   cargo run -q -- --once
   ```

   A running limits-tui picks it up within 15 seconds; press `r` to refresh now.
   Data older than 15 minutes is shown dimmed.
