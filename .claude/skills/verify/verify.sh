#!/usr/bin/env bash
# limits-tui verify: the checks to run before committing a change.
#
#   .claude/skills/verify/verify.sh [quick|run|all]   (default: all)
#
#   quick  cargo fmt --check, clippy (-D warnings), cargo test
#   run    build, then run the real binary: --once must print the three source lines, and the
#          TUI in a private tmux server must draw its frame, survive "r", and exit 0 on "q".
#          The captured screens are the evidence: target/verify/screen-*.txt
#
# One PASS/FAIL line per step; the first FAIL stops the run with the tail of that step's log.
# Exit 0 only when every step passed. Lines a source reports as an error (logged out, offline)
# are WARN, not FAIL: they depend on accounts, not on the code.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
out="$repo/target/verify"
bin="" # set by build(): the executable cargo reports, wherever the target dir is
sock="${TMPDIR:-/tmp}/limits-tui-verify-$$.sock"
tiers="${1:-all}"

case "$tiers" in quick | run | all) ;; *)
  echo "usage: verify.sh [quick|run|all]" >&2
  exit 2
  ;;
esac

cd "$repo" || exit 2
mkdir -p "$out"

cleanup() {
  tmux -S "$sock" kill-server 2>/dev/null || true
  rm -f "$sock"
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
  local log="$out/tui.log" t0=$SECONDS i status q
  : >"$log"
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
  for ((i = 0; i < 20; i++)); do
    [ "$(pane '#{pane_dead}')" = 1 ] && break
    sleep 0.25
  done
  [ "$(pane '#{pane_dead}')" = 1 ] || fail tui "$log" "still running 5 s after q"
  tmux -S "$sock" capture-pane -p -t v >"$out/screen-exit.txt" 2>/dev/null || true
  status=$(dead_status)
  if [ "$status" != 0 ]; then
    cat "$out/screen-exit.txt" >>"$log"
    fail tui "$log" "exit status $status after q"
  fi
  if grep -q "panicked" "$out/screen-exit.txt"; then fail tui "$out/screen-exit.txt" "panic message on screen"; fi
  echo "PASS tui ($((SECONDS - t0))s) screens: $out/screen-{start,refresh,exit}.txt"
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
}

case "$tiers" in
  quick) quick ;;
  run) run_tier ;;
  all) quick && run_tier ;;
esac
echo "verify: $tiers passed"
