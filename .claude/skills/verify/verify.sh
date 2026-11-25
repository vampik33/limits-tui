#!/usr/bin/env bash
# limits-tui verify: the checks to run before committing a change.
#
#   .claude/skills/verify/verify.sh [quick|run|all]   (default: all)
#
#   quick  cargo fmt --check, clippy (-D warnings), cargo test
#   run    build, then run the real binary: --once must print the three source lines, and the
#          TUI in a private tmux server must draw its frame, survive "r", and exit 0 on "q".
#          Then quit-kills: with a fake codex (CODEX_BIN) that never answers, "q" must also kill
#          the fake and the background job in its process group.
#          The captured screens are the evidence: target/verify/screen-*.txt
#
# One PASS/FAIL line per step; the first FAIL stops the run with the tail of that step's log.
# Exit 0 only when every step passed. Lines a source reports as an error (logged out, offline)
# are WARN, not FAIL: they depend on accounts, not on the code.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
out="$repo/target/verify"
bin="" # set by build(): the executable cargo reports, wherever the target dir is
sockdir="" # set by tui(): a private directory (mktemp -d, mode 0700) for the tmux socket
sock=""    # set by tui(): the tmux socket inside sockdir
tiers="${1:-all}"

case "$tiers" in quick | run | all) ;; *)
  echo "usage: verify.sh [quick|run|all]" >&2
  exit 2
  ;;
esac

cd "$repo" || exit 2
mkdir -p "$out"

cleanup() {
  [ -n "$sockdir" ] || return 0
  tmux -S "$sock" kill-server 2>/dev/null || true
  rm -rf "$sockdir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() { # $1 = step, $2 = log, $3 = why
  echo "FAIL $1: $3"
  if [ -s "$2" ]; then
    echo "---- last 40 lines of $2"
    tail -n 40 "$2"
  fi
  exit 1
}

step() { # $1 = name, rest = command; output goes to $out/<name>.log
  local name="$1" log="$out/$1.log" t0=$SECONDS
  shift
  if "$@" >"$log" 2>&1; then
    echo "PASS $name ($((SECONDS - t0))s)"
  else
    fail "$name" "$log" "exit $? from: $*"
  fi
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "deps" /dev/null "$1 not found on PATH"
}

quick() {
  need cargo
  step fmt cargo fmt --all -- --check
  step clippy cargo clippy --all-targets --all-features -- -D warnings
  step test cargo test --no-fail-fast
}

pane() { tmux -S "$sock" display -p -t v "$1" 2>/dev/null; }

# Prints how the dead pane's app exited: its exit status, "signal N", or "unknown". Ubuntu's tmux
# 3.4 is built with libutempter, which sets SIGCHLD to SIG_DFL while its helper runs as a pane
# closes; an app that exits in that window is never reaped, so tmux has no status for it until
# another child exits (tmux/tmux#4559, fixed in tmux 3.6). Sending the server SIGCHLD makes it reap
# and record it.
dead_status() {
  local i status signal
  for ((i = 0; i < 20; i++)); do
    status=$(pane '#{pane_dead_status}')
    signal=$(pane '#{pane_dead_signal}')
    if [ -n "$status" ]; then
      echo "$status"
      return 0
    elif [ -n "$signal" ]; then
      echo "signal $signal"
      return 0
    fi
    kill -CHLD "$(pane '#{pid}')" 2>/dev/null || true
    sleep 0.1
  done
  echo unknown
}

# Waits up to 5 s for the pane's app to exit.
wait_pane_dead() {
  local i
  for ((i = 0; i < 20; i++)); do
    [ "$(pane '#{pane_dead}')" = 1 ] && return 0
    sleep 0.25
  done
  return 1
}

# Waits until the pane shows every pattern given (fixed strings), up to 20 s.
wait_for_screen() { # $1 = capture file, rest = patterns
  local file="$1" i p missing
  shift
  for ((i = 0; i < 40; i++)); do
    tmux -S "$sock" capture-pane -p -t v >"$file" 2>/dev/null || true
    missing=""
    for p in "$@"; do grep -qF -- "$p" "$file" || missing="$missing [$p]"; done
    [ -z "$missing" ] && return 0
    if [ "$(pane '#{pane_dead}')" = 1 ]; then
      echo "the app exited (status $(dead_status)) before drawing:$missing"
      cat "$file"
      return 1
    fi
    sleep 0.5
  done
  echo "not on screen after 20 s:$missing"
  cat "$file"
  return 1
}

once() {
  local log="$out/once.log" t0=$SECONDS src line
  timeout 90 "$bin" --once >"$log" 2>&1 || fail once "$log" "--once exited non-zero or timed out"
  [ "$(wc -l <"$log")" -eq 3 ] || fail once "$log" "expected exactly 3 lines"
  for src in claude codex devin; do
    line=$(grep -m1 "^$src: " "$log") || fail once "$log" "no '$src: ' line"
    case "$line" in "$src: error"*) echo "WARN once: $line" ;; esac
  done
  echo "PASS once ($((SECONDS - t0))s)"
}

tui() {
  local log="$out/tui.log" t0=$SECONDS status q
  : >"$log"
  sockdir=$(mktemp -d "${TMPDIR:-/tmp}/limits-tui.XXXXXXXX" 2>>"$log") || fail tui "$log" "mktemp -d failed"
  sock="$sockdir/sock"
  # Placeholder first, so remain-on-exit is set before the app starts: a crash at startup
  # then leaves its exit status and panic message in the pane.
  tmux -S "$sock" -f /dev/null new-session -d -x 120 -y 40 -s v "sleep 600" >>"$log" 2>&1 ||
    fail tui "$log" "tmux could not start"
  tmux -S "$sock" set-option -g remain-on-exit on >>"$log" 2>&1
  # tmux runs the command through a shell: quote the path for that second parse.
  printf -v q %q "$bin"
  tmux -S "$sock" respawn-pane -k -t v "exec $q" >>"$log" 2>&1 || fail tui "$log" "tmux could not start the app"
  wait_for_screen "$out/screen-start.txt" "limits" "Claude" "Codex" "Devin" "r refresh" "q quit" >>"$log" ||
    fail tui "$log" "first frame incomplete"
  tmux -S "$sock" send-keys -t v r
  sleep 1
  wait_for_screen "$out/screen-refresh.txt" "limits" "Claude" "Codex" "Devin" "q quit" >>"$log" ||
    fail tui "$log" "frame broken after r"
  tmux -S "$sock" send-keys -t v q
  wait_pane_dead || fail tui "$log" "still running 5 s after q"
  tmux -S "$sock" capture-pane -p -t v >"$out/screen-exit.txt" 2>/dev/null || true
  status=$(dead_status)
  if [ "$status" != 0 ]; then
    cat "$out/screen-exit.txt" >>"$log"
    fail tui "$log" "exit status $status after q"
  fi
  if grep -q "panicked" "$out/screen-exit.txt"; then fail tui "$out/screen-exit.txt" "panic message on screen"; fi
  echo "PASS tui ($((SECONDS - t0))s) screens: $out/screen-{start,refresh,exit}.txt"
}

# Whether pid $1 is alive in process group $2: its /proc stat shows that group and a state other
# than Z. A pid gone, reaped, a zombie, or reused outside the group is not.
alive_in_group() {
  local stat fields
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  read -r -a fields <<<"${stat##*) }" # "pid (comm) state ppid pgrp ...": comm may hold anything
  [ "${fields[0]}" != Z ] && [ "${fields[2]}" = "$2" ]
}

# Whether $1 is a pid that may be signalled: numeric, and neither 0 nor 1.
valid_pid() { [[ $1 =~ ^[1-9][0-9]{0,8}$ ]] && [ "$1" -gt 1 ]; }

# Quitting must kill the children of probes still running: their threads die with the app. A fake
# codex that never answers starts a background job in its group and records both pids; once q has
# exited the app, neither may be alive.
quit_kills() {
  local log="$out/quit-kills.log" t0=$SECONDS fake="$sockdir/codex" pids="$sockdir/codex.pids"
  local i q f leader="" job="" status pid left=""
  : >"$log"
  # Kills what is left of the fake's group (validated pids only), then fails.
  qk_fail() {
    if [ -n "$leader" ]; then kill -KILL -- "-$leader" 2>>"$log" || true; fi
    fail quit-kills "$log" "$1"
  }
  cat >"$fake" <<'EOF' || fail quit-kills "$log" "cannot write the fake codex"
#!/bin/sh
sleep 600 &
echo "$$ $!" >"${0%/*}/codex.pids.new" && mv "${0%/*}/codex.pids.new" "${0%/*}/codex.pids"
wait
EOF
  chmod 700 "$fake" || fail quit-kills "$log" "cannot make the fake codex executable"
  printf -v q %q "$bin"
  printf -v f %q "$fake"
  tmux -S "$sock" respawn-pane -k -t v "exec env CODEX_BIN=$f CODEX_LIMITS_TIMEOUT=600 $q" >>"$log" 2>&1 ||
    fail quit-kills "$log" "tmux could not start the app"
  for ((i = 0; i < 80; i++)); do
    [ -s "$pids" ] && break
    sleep 0.25
  done
  [ -s "$pids" ] || fail quit-kills "$log" "the fake codex never started (no $pids after 20 s)"
  read -r leader job <"$pids"
  echo "fake codex: group leader $leader, background job $job" >>"$log"
  valid_pid "$leader" || fail quit-kills "$log" "bad group leader pid '$leader' in $pids"
  valid_pid "$job" || qk_fail "bad job pid '$job' in $pids"
  wait_for_screen "$out/screen-quit-kills.txt" "limits" "Claude" "Codex" "Devin" "q quit" >>"$log" ||
    qk_fail "first frame incomplete"
  tmux -S "$sock" send-keys -t v q
  wait_pane_dead || qk_fail "still running 5 s after q"
  status=$(dead_status)
  [ "$status" = 0 ] || qk_fail "exit status $status after q"
  for ((i = 0; i < 30; i++)); do
    left=""
    for pid in "$leader" "$job"; do
      if alive_in_group "$pid" "$leader"; then left="$left $pid"; fi
    done
    [ -z "$left" ] && break
    sleep 0.1
  done
  [ -z "$left" ] || qk_fail "the fake codex's group outlived q (alive 3 s after exit:$left)"
  echo "PASS quit-kills ($((SECONDS - t0))s)"
}

# cargo build; bin = the limits-tui executable named in its compiler-artifact message, so a
# target dir set anywhere (CARGO_TARGET_DIR, build.target-dir, build.target) is honoured.
build() {
  local json="$out/build.json" line re='"executable":"([^"\]*)"'
  cargo build --message-format=json-render-diagnostics >"$json" || return
  line=$(grep -F '"reason":"compiler-artifact"' "$json" | grep -F '"kind":["bin"]' | grep -F '"name":"limits-tui"')
  [[ $line =~ $re ]] && bin=${BASH_REMATCH[1]}
  [ -n "$bin" ] && [ -x "$bin" ] || {
    echo "cargo reported no limits-tui executable (see $json)"
    return 1
  }
}

run_tier() {
  need cargo
  need tmux
  step build build
  once
  tui
  quit_kills
}

case "$tiers" in
  quick) quick ;;
  run) run_tier ;;
  all) quick && run_tier ;;
esac
echo "verify: $tiers passed"
