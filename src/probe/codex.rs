//! Codex (ChatGPT plan) usage: ask the local `codex app-server` over stdio
//! (newline-delimited JSON-RPC): initialize -> initialized ->
//! account/rateLimits/read. Makes no model request and no network call of its
//! own; codex uses and refreshes its own login.
//!
//! Port of herdr-codex-limits' codex_limits.py.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use super::{ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_f64, json_int, timeout_from_env, unix_now};

const DEFAULT_TIMEOUT_SECS: f64 = 15.0;
/// Most bytes of login-shell output kept: the last ones, where the lookup
/// result follows the login config's own output.
const SHELL_OUTPUT_CAP: usize = 64 * 1024;
/// How long the login shell's stdout may stay open after the shell exits
/// before the background jobs it left holding it are killed.
const SHELL_EXIT_GRACE: Duration = Duration::from_millis(100);
/// Printed by the login-shell lookup before its answer, so the login config's
/// own output (printed earlier) can't be mistaken for it.
const LOGIN_MARKER: &str = "@@limits-tui@@";

/// Most bytes one `Pipe::step` reads, so a writer that never pauses can't keep
/// a step going past its deadline.
const PIPE_READ_BURST: usize = 64 * 1024;

/// A child's stdout, read without blocking on the caller's thread, so no read
/// outlasts a deadline and no reader thread is left for a holder of the write
/// end (a descendant outside the child's process group) to pin. Dropping it
/// closes the read end; such a holder's later writes get EPIPE.
struct Pipe {
    stdout: ChildStdout,
    /// EOF or a read error; a closed pipe is no longer polled.
    closed: bool,
}

impl Pipe {
    fn new(stdout: ChildStdout) -> Self {
        let fd = stdout.as_raw_fd();
        // SAFETY: `fd` is the open read end owned by `stdout`; F_GETFL/F_SETFL
        // only change its file status flags.
        let nonblocking = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
        };
        // A blocking read could outlast the deadline, so without O_NONBLOCK
        // the pipe is not read at all.
        Self { stdout, closed: !nonblocking }
    }

    /// Wait up to 25 ms, never past `deadline`, for output and pass what was
    /// read to `sink`, chunk by chunk. Returns false once `deadline` has
    /// passed.
    fn step(&mut self, deadline: Instant, sink: impl FnMut(&[u8])) -> bool {
        let step = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(25));
        if step.is_zero() {
            return false;
        }
        if self.closed {
            std::thread::sleep(step);
            return true;
        }
        let mut fds = libc::pollfd { fd: self.stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: `fds` is one valid pollfd for the duration of the call and
        // its fd is owned by `self.stdout`, which outlives it. The result is
        // not needed: POLLIN, POLLHUP, POLLERR, a timeout and EINTR are all
        // handled by draining.
        unsafe {
            libc::poll(&mut fds, 1, step.as_millis() as libc::c_int);
        }
        self.drain(sink);
        true
    }

    /// Read what the pipe has for now, up to PIPE_READ_BURST bytes.
    fn drain(&mut self, mut sink: impl FnMut(&[u8])) {
        let mut chunk = [0u8; 4096];
        let mut read = 0;
        loop {
            if read >= PIPE_READ_BURST {
                return;
            }
            match self.stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    sink(&chunk[..n]);
                    read += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => break,
            }
        }
        self.closed = true;
    }
}

/// The login shell's stdout and the last SHELL_OUTPUT_CAP bytes it printed.
struct ShellOutput {
    pipe: Pipe,
    tail: Vec<u8>,
}

impl ShellOutput {
    fn new(stdout: ChildStdout) -> Self {
        Self { pipe: Pipe::new(stdout), tail: Vec::new() }
    }

    /// `Pipe::step`, keeping the output's tail.
    fn step(&mut self, deadline: Instant) -> bool {
        let tail = &mut self.tail;
        self.pipe.step(deadline, |chunk| {
            tail.extend_from_slice(chunk);
            let excess = tail.len().saturating_sub(SHELL_OUTPUT_CAP);
            tail.drain(..excess);
        })
    }

    /// Read until stdout closes or SHELL_EXIT_GRACE passes, never past
    /// `deadline`.
    fn settle(&mut self, deadline: Instant) {
        let end = (Instant::now() + SHELL_EXIT_GRACE).min(deadline);
        while !self.pipe.closed && self.step(end) {}
    }

    /// The output kept so far. Consuming `self` closes the read end, so a
    /// holder outside the shell's group keeps nothing of this call alive.
    fn into_tail(self) -> Vec<u8> {
        self.tail
    }
}

/// The codex binary to run and, if it needs one, the PATH to run it with.
struct CodexBin {
    path: std::path::PathBuf,
    /// Set when codex came from the login shell, whose PATH ours lacks: that
    /// PATH, so an npm shim (`#!/usr/bin/env node`) finds node wherever the
    /// login config put it, even in another directory than the shim. Without
    /// the login PATH, codex's directory in front of ours.
    child_path: Option<std::ffi::OsString>,
}

fn is_exe(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file() && p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Whether `child` has exited, without reaping it: its pid stays reserved, so
/// it is still safe to signal as a process group id.
fn has_exited(child: &Child) -> std::io::Result<bool> {
    // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable siginfo_t; WNOWAIT leaves the child
    // to be reaped by `Child::wait`.
    let rc = unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded; with WNOHANG, si_pid stays 0 until the child
    // has exited.
    Ok(unsafe { info.si_pid() } != 0)
}

/// The last SHELL_OUTPUT_CAP bytes of stdout of `shell -lc`, printing
/// LOGIN_MARKER, the exported PATH (an empty line if unset) and `command -v
/// codex`, or None if the shell could not be started. Returns by
/// `shell_deadline`, apart from reaping a killed shell. Background jobs of the
/// login config that still hold stdout once the shell has exited (or the shell
/// itself, at the deadline) are killed with the shell's process group. Holders
/// outside that group (setsid) can't be reached, but the read end is closed on
/// return, so nothing outlives the call: their later writes get EPIPE.
fn login_shell_lookup(shell: &std::ffi::OsStr, shell_deadline: Instant) -> Option<Vec<u8>> {
    // printenv joins PATH with colons in any shell (fish's "$PATH" uses
    // spaces); `|| echo` keeps the PATH line when PATH is unset.
    let lookup = format!("printf '%s\\n' '{LOGIN_MARKER}'; printenv PATH || echo; command -v codex");
    let mut child = Command::new(shell)
        .args(["-lc", &lookup])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0) // a non-interactive shell keeps its background jobs in this group
        .spawn()
        .ok()?;
    let mut output = ShellOutput::new(child.stdout.take().expect("stdout is piped"));

    // (A) Wait for the shell itself, not for stdout to close; failing to
    // check on it counts as not exited.
    let exited = loop {
        match has_exited(&child) {
            Ok(true) => break true,
            Ok(false) if output.step(shell_deadline) => {}
            _ => break false,
        }
    };
    // (B) Let stdout drain and close.
    if exited {
        output.settle(shell_deadline);
    }
    // (C) Kill the shell, or what still holds its stdout.
    if !(exited && output.pipe.closed) {
        // SAFETY: killpg only sends a signal. The group id is the pid of our
        // child, which leads the group created by process_group(0); the child
        // is not reaped yet (has_exited uses WNOWAIT), so the id cannot have
        // been reused.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
        // Killed holders release stdout at once.
        output.settle(shell_deadline);
    }
    let tail = output.into_tail();
    let _ = child.wait();
    Some(tail)
}

/// `dir` followed by `current` (a PATH value), skipping an empty `current`:
/// an empty entry would mean the working directory.
fn path_with(dir: &std::path::Path, current: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let mut path = dir.as_os_str().to_owned();
    if let Some(current) = current.filter(|c| !c.is_empty()) {
        path.push(":");
        path.push(current);
    }
    path
}

/// CODEX_BIN, else PATH, else the login shell's PATH (desktop launches may
/// lack it) — same order as the Python probe.
fn find_codex(deadline: Instant) -> Result<CodexBin, ProbeError> {
    if let Ok(override_) = std::env::var("CODEX_BIN") {
        let p = std::path::PathBuf::from(&override_);
        if is_exe(&p) {
            return Ok(CodexBin { path: p, child_path: None });
        }
        return Err(ProbeError::new(ErrorCode::Missing, "CODEX_BIN is not an executable"));
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::Path::new(dir).join("codex");
            if is_exe(&p) {
                return Ok(CodexBin { path: p, child_path: None });
            }
        }
    }
    if let Ok(shell) = std::env::var("SHELL") {
        // The login shell may source slow config; bound it like the Python probe.
        let shell_deadline = Instant::now() + (deadline - Instant::now()).min(Duration::from_secs(5));
        if let Some(stdout) = login_shell_lookup(shell.as_ref(), shell_deadline)
            && let Some(codex) = codex_from_login_output(&stdout, std::env::var_os("PATH"))
        {
            return Ok(codex);
        }
    }
    Err(ProbeError::new(ErrorCode::Missing, "codex not found on PATH"))
}

/// Codex as found by login_shell_lookup: what follows the last LOGIN_MARKER
/// line is the login PATH, then the lookup's answer; without a marker, the
/// whole output is the answer. Codex is the answer's last absolute, executable
/// path, to be run with the login PATH or, if that is missing or empty, with
/// its directory in front of `current` (our PATH, which lacks it).
fn codex_from_login_output(stdout: &[u8], current: Option<std::ffi::OsString>) -> Option<CodexBin> {
    use std::os::unix::ffi::OsStrExt;
    let lines: Vec<&[u8]> = stdout.split(|&b| b == b'\n').collect();
    let (login_path, answer) = match lines.iter().rposition(|line| line.trim_ascii() == LOGIN_MARKER.as_bytes()) {
        Some(i) => (lines.get(i + 1).filter(|p| !p.is_empty()), lines.get(i + 2..).unwrap_or_default()),
        None => (None, &lines[..]),
    };
    let path = answer
        .iter()
        .rev()
        .map(|line| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(line.trim_ascii())))
        .find(|p| p.is_absolute() && is_exe(p))?;
    let child_path = match login_path {
        Some(login_path) => Some(std::ffi::OsStr::from_bytes(login_path).to_owned()),
        None => path.parent().map(|dir| path_with(dir, current)),
    };
    Some(CodexBin { path, child_path })
}

/// `codex app-server` with piped stdio, in its own process group so kill_tree
/// can take down npm-shim children.
fn app_server_command(codex: &CodexBin) -> Command {
    let mut command = Command::new(&codex.path);
    command.arg("app-server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0);
    if let Some(path) = &codex.child_path {
        command.env("PATH", path);
    }
    command
}

/// Lowercase words of `text`, split at non-alphanumerics and, with `camel`,
/// also where a lowercase letter or digit meets an uppercase one
/// ("NotLoggedIn" -> not, logged, in).
fn words(text: &str, camel: bool) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in text.chars() {
        if (!c.is_alphanumeric() || (camel && prev_lower && c.is_uppercase())) && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// auth, authentication, unauthorized, reauthenticate, authorization...
/// but not author, authored, authority.
fn is_auth_word(word: &str) -> bool {
    let w = word.strip_prefix("un").or_else(|| word.strip_prefix("re")).unwrap_or(word);
    w.starts_with("auth") && (!w.starts_with("author") || w.starts_with("authoriz") || w.starts_with("authoris"))
}

/// Errors whose message hints the account isn't signed in. Matches whole
/// words, so "assign", "author" or "4012" don't count. Words are taken both
/// as written and split at camelCase, so "ChatGPT account" and "NotLoggedIn"
/// both match.
fn auth_hint(text: &str) -> bool {
    const WORDS: &[&str] = &["oauth", "login", "signin", "401"];
    const PHRASES: &[&[&str]] = &[
        &["log", "in"],
        &["logged", "in"],
        &["logged", "out"],
        &["sign", "in"],
        &["signed", "in"],
        &["signed", "out"],
        &["api", "key"],
        &["chatgpt", "account"],
    ];
    [false, true].into_iter().any(|camel| {
        let words = words(text, camel);
        words.iter().any(|w| WORDS.contains(&w.as_str()) || is_auth_word(w))
            || PHRASES.iter().any(|p| words.windows(p.len()).any(|w| w.iter().zip(p.iter()).all(|(a, b)| a == b)))
    })
}

/// A JSON-RPC error response as a ProbeError. Only the server's message is
/// classified: without one, the serialized object (keys like "authMode")
/// would say nothing about sign-in, so it is just the detail.
fn rpc_error(err: &serde_json::Value) -> ProbeError {
    match err.get("message").and_then(|m| m.as_str()) {
        Some(text) => ProbeError::new(if auth_hint(text) { ErrorCode::NotSignedIn } else { ErrorCode::Failed }, text),
        None => ProbeError::new(ErrorCode::Failed, err.to_string()),
    }
}

/// The result of a JSON-RPC response, or its error. A response with neither
/// is a protocol failure, not an empty result.
fn response_result(msg: &serde_json::Value) -> Result<serde_json::Value, ProbeError> {
    if let Some(err) = msg.get("error") {
        return Err(rpc_error(err));
    }
    msg.get("result")
        .cloned()
        .ok_or_else(|| ProbeError::new(ErrorCode::Failed, "codex app-server sent a response without a result"))
}

/// Stop the child and anything it started (the npm shim runs node, which runs
/// codex) by signalling its process group: SIGTERM, up to 1 s for the child to
/// exit, then SIGKILL for what is left of the group, so nothing it started
/// outlives the probe.
fn kill_tree(child: &mut Child) {
    drop(child.stdin.take());
    let pgid = child.id() as libc::pid_t;
    // SAFETY: killpg only sends a signal. The group id is the pid of our child,
    // which leads the group created by process_group(0); the child is not
    // reaped yet, so the id cannot have been reused.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    for _ in 0..20 {
        // has_exited doesn't reap, so the group id stays ours for the SIGKILL.
        if has_exited(child).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: as above; still not reaped.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Newline-delimited JSON-RPC with the app server over its stdio. Its stdout
/// is read on this thread, so dropping the Rpc releases it completely.
struct Rpc {
    stdin: std::process::ChildStdin,
    stdout: Pipe,
    /// Output read but not yet split into lines.
    pending: Vec<u8>,
    deadline: Instant,
}

impl Rpc {
    fn spawn(child: &mut Child, deadline: Instant) -> Result<Self, ProbeError> {
        let stdin = child.stdin.take().ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no stdout"))?;
        let stdout = Pipe::new(stdout);
        if stdout.closed {
            return Err(ProbeError::new(ErrorCode::Failed, "cannot read codex app-server output without blocking"));
        }
        Ok(Self { stdin, stdout, pending: Vec::new(), deadline })
    }

    fn send(&mut self, msg: &serde_json::Value) -> Result<(), ProbeError> {
        writeln!(self.stdin, "{msg}")
            .and_then(|()| self.stdin.flush())
            .map_err(|_| ProbeError::new(ErrorCode::Failed, "codex app-server closed its input"))
    }

    /// The response with this id; notifications and other lines are skipped.
    fn wait_for(&mut self, request_id: i64) -> Result<serde_json::Value, ProbeError> {
        loop {
            // Complete lines, consumed in one drain after the scan.
            let mut start = 0;
            let mut response = None;
            while let Some(len) = self.pending[start..].iter().position(|&b| b == b'\n') {
                response = response_line(&self.pending[start..start + len], request_id);
                start += len + 1;
                if response.is_some() {
                    break;
                }
            }
            self.pending.drain(..start);
            if let Some(response) = response {
                return response;
            }
            if self.stdout.closed {
                if self.pending.is_empty() {
                    return Err(ProbeError::new(ErrorCode::Failed, "codex app-server exited"));
                }
                // EOF ends the last line too.
                self.pending.push(b'\n');
                continue;
            }
            let pending = &mut self.pending;
            if !self.stdout.step(self.deadline, |chunk| pending.extend_from_slice(chunk)) {
                return Err(ProbeError::new(ErrorCode::Timeout, "codex app-server did not answer in time"));
            }
        }
    }
}

/// The outcome of the request with this id, if `line` is its response.
fn response_line(line: &[u8], request_id: i64) -> Option<Result<serde_json::Value, ProbeError>> {
    let msg: serde_json::Value = serde_json::from_slice(line).ok()?;
    (msg.get("id").and_then(|i| i.as_i64()) == Some(request_id)).then(|| response_result(&msg))
}

fn normalize(result: &serde_json::Value) -> Result<FetchOk, ProbeError> {
    let limits = result.get("rateLimits");
    let Some(limits) = limits.filter(|l| l.is_object()) else {
        // The schema requires the object; a signed-out account gets an RPC error.
        return Err(ProbeError::new(ErrorCode::Failed, "response has no rate limits object"));
    };
    let mut windows = Vec::new();
    // A window was sent but can't be shown: a format change, not a lack of quota.
    let mut unrecognized = false;
    for slot in ["primary", "secondary"] {
        let Some(w) = limits.get(slot).filter(|w| !w.is_null()) else { continue };
        let label = match json_int(&w["windowDurationMins"]) {
            Some(300) => "5h",
            Some(10080) => "wk",
            _ => {
                unrecognized = true;
                continue;
            }
        };
        let (Some(used), Some(resets)) = (json_f64(&w["usedPercent"]), json_int(&w["resetsAt"])) else {
            unrecognized = true;
            continue;
        };
        windows.push(UsageWindow { label, used_percent: used, resets_at: resets });
    }
    if windows.is_empty() {
        return Err(if unrecognized {
            ProbeError::new(ErrorCode::Failed, "usage windows in an unrecognized format")
        } else {
            ProbeError::new(ErrorCode::NoQuota, "no usage windows for this account")
        });
    }
    // Order: short window first.
    windows.sort_by_key(|w| w.label != "5h");
    Ok(FetchOk {
        windows,
        plan: limits.get("planType").and_then(|p| p.as_str()).map(String::from),
        overage_micros: None,
        fetched_at: unix_now(),
    })
}

pub fn fetch() -> ProbeResult {
    let deadline = Instant::now() + timeout_from_env("CODEX_LIMITS_TIMEOUT", DEFAULT_TIMEOUT_SECS);

    let codex = find_codex(deadline)?;
    read_rate_limits(&codex, deadline)
}

/// Ask `codex app-server` for the account's rate limits, by `deadline`, then
/// stop it.
fn read_rate_limits(codex: &CodexBin, deadline: Instant) -> ProbeResult {
    let mut child = app_server_command(codex)
        .spawn()
        .map_err(|e| ProbeError::new(ErrorCode::Missing, format!("cannot start codex: {e}")))?;

    let result = (|| {
        let mut rpc = Rpc::spawn(&mut child, deadline)?;
        rpc.send(&serde_json::json!({
            "method": "initialize",
            "id": 0,
            "params": {"clientInfo": {"name": "limits-tui", "title": "Limits TUI", "version": "0.1.0"}},
        }))?;
        rpc.wait_for(0)?;
        rpc.send(&serde_json::json!({"method": "initialized", "params": {}}))?;
        rpc.send(&serde_json::json!({"method": "account/rateLimits/read", "id": 1}))?;
        normalize(&rpc.wait_for(1)?)
    })();

    kill_tree(&mut child);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child forked by one test thread inherits another thread's script
    /// while it is still open for writing, and exec'ing that script then fails
    /// with ETXTBSY. Script writes hold this exclusively, lookups (which fork)
    /// hold it shared, so no fork overlaps an open script.
    static SCRIPTS: std::sync::RwLock<()> = std::sync::RwLock::new(());

    /// Per-test temp dir, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(test: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("limits-tui-{}-{test}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }

        /// Write `body` as an executable `/bin/sh` script and return its path;
        /// the fake "login shell" ignores its arguments.
        fn shell(&self, body: &str) -> std::path::PathBuf {
            self.script("shell", &format!("#!/bin/sh\n{body}\n"))
        }

        /// Write `contents` as executable file `name` (relative, its parent
        /// dirs created) and return its path.
        fn script(&self, name: &str, contents: &str) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let _writing = SCRIPTS.write().unwrap_or_else(|e| e.into_inner());
            let path = self.path(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn lookup(shell: &std::path::Path, budget: Duration) -> (Option<Vec<u8>>, Duration) {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let out = login_shell_lookup(shell.as_os_str(), start + budget);
        (out, start.elapsed())
    }

    fn read_pid(path: &std::path::Path) -> libc::pid_t {
        std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    /// Whether `cond` holds within `tries` checks 50 ms apart.
    fn wait_until(tries: u32, mut cond: impl FnMut() -> bool) -> bool {
        (0..tries).any(|_| {
            let met = cond();
            if !met {
                std::thread::sleep(Duration::from_millis(50));
            }
            met
        })
    }

    /// Whether `pid` is gone (or a zombie awaiting init) within about 2s.
    fn is_gone(pid: libc::pid_t) -> bool {
        let stat = std::path::PathBuf::from(format!("/proc/{pid}/stat"));
        wait_until(40, || !std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z ")))
    }

    #[test]
    fn plain_shell_returns_its_output() {
        let scratch = Scratch::new("plain");
        let (out, elapsed) = lookup(&scratch.shell("echo /bin/sh"), Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some(&b"/bin/sh\n"[..]));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn background_job_holding_stdout_does_not_delay_lookup() {
        let scratch = Scratch::new("bg");
        let (out, elapsed) = lookup(&scratch.shell("sleep 30 &\necho /bin/sh"), Duration::from_secs(5));
        // The shell exits at once; waiting for the job to release stdout
        // would take the whole budget.
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        assert_eq!(out.as_deref(), Some(&b"/bin/sh\n"[..]));
    }

    #[test]
    fn background_job_holding_stdout_is_killed() {
        let scratch = Scratch::new("bgkill");
        let pidfile = scratch.path("pid");
        let shell = scratch.shell(&format!("sleep 30 &\necho $! > {}\necho /bin/sh", pidfile.display()));
        let (_, elapsed) = lookup(&shell, Duration::from_secs(5));
        // Without the bound, a lookup that waits out the job would pass too.
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        let pid = read_pid(&pidfile);
        assert!(is_gone(pid), "background sleep {pid} is still running");
    }

    #[test]
    fn holder_outside_the_process_group_keeps_the_path() {
        let scratch = Scratch::new("setsid");
        let pidfile = scratch.path("pid");
        // setsid moves the job out of the shell's group, beyond killpg.
        let shell = scratch.shell(&format!("setsid sleep 10 &\necho $! > {}\necho /bin/sh", pidfile.display()));
        let (out, elapsed) = lookup(&shell, Duration::from_secs(5));
        let pid = read_pid(&pidfile);
        // SAFETY: plain signal to the test's own background sleep.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        assert_eq!(out.as_deref(), Some(&b"/bin/sh\n"[..]));
    }

    #[test]
    fn output_is_capped() {
        let scratch = Scratch::new("cap");
        let shell = scratch.shell("head -c 100000 /dev/zero | tr '\\0' a\necho\necho /bin/sh");
        let (out, elapsed) = lookup(&shell, Duration::from_secs(5));
        let out = out.unwrap();
        assert!(!out.is_empty());
        assert!(out.len() <= SHELL_OUTPUT_CAP, "read {} bytes", out.len());
        // The path printed after the startup noise survives the cap.
        assert!(
            out.ends_with(b"\n/bin/sh\n"),
            "tail: {:?}",
            String::from_utf8_lossy(&out[out.len().saturating_sub(32)..])
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn read_end_is_closed_on_return() {
        let scratch = Scratch::new("epipe");
        let marker = scratch.path("epipe");
        // The holder leaves the shell's group, so killpg can't reach it; it
        // writes after the lookup returned and sees EPIPE only if nothing
        // still holds the pipe's read end.
        let shell = scratch.shell(&format!(
            "setsid sh -c 'trap \"\" PIPE; sleep 1; echo late || touch {}' &\necho /bin/sh",
            marker.display()
        ));
        let (out, elapsed) = lookup(&shell, Duration::from_secs(5));
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        assert_eq!(out.as_deref(), Some(&b"/bin/sh\n"[..]));
        assert!(
            wait_until(60, || marker.exists()),
            "the holder's late write succeeded: the read end outlived the lookup"
        );
    }

    #[test]
    fn shell_that_never_exits_is_bounded() {
        let scratch = Scratch::new("hang");
        let (out, elapsed) = lookup(&scratch.shell("sleep 30"), Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        assert!(out.unwrap_or_default().is_empty());
    }

    #[test]
    fn shell_that_closes_stdout_but_keeps_running_is_bounded() {
        let scratch = Scratch::new("closed");
        let (out, elapsed) = lookup(&scratch.shell("exec >/dev/null\nsleep 30"), Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        // Some (not None) shows the shell really ran and closed its stdout.
        assert_eq!(out.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn failing_shell_returns_no_path() {
        let scratch = Scratch::new("fail");
        let (out, elapsed) = lookup(&scratch.shell("exit 1"), Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some(&b""[..]));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn deadline_already_passed_returns_promptly() {
        let scratch = Scratch::new("late");
        let (_, elapsed) = lookup(&scratch.shell("sleep 30"), Duration::ZERO);
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn missing_shell_returns_none() {
        let (out, _) = lookup(std::path::Path::new("/nonexistent/shell"), Duration::from_secs(1));
        assert!(out.is_none());
    }

    // --- child PATH for a codex found by the login shell ---

    #[test]
    fn path_with_puts_dir_first() {
        let dir = std::path::Path::new("/home/u/.nvm/versions/node/v22/bin");
        assert_eq!(path_with(dir, Some("/usr/bin:/bin".into())), "/home/u/.nvm/versions/node/v22/bin:/usr/bin:/bin");
    }

    #[test]
    fn path_with_skips_missing_or_empty_path() {
        let dir = std::path::Path::new("/opt/codex/bin");
        assert_eq!(path_with(dir, None), "/opt/codex/bin");
        assert_eq!(path_with(dir, Some("".into())), "/opt/codex/bin");
    }

    /// Run `prog` with PATH set to `path` and return its stdout, or None if it
    /// failed.
    fn run_with_path(prog: &std::path::Path, path: &std::ffi::OsStr) -> Option<String> {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let out = Command::new(prog).env("PATH", path).stderr(Stdio::null()).output().unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn npm_shim_finds_the_node_next_to_it() {
        let scratch = Scratch::new("shim");
        // An npm-style shim and its interpreter in one dir, as nvm installs them.
        scratch.script("fakenode", "#!/bin/sh\necho ran-by-fakenode\n");
        let shim = scratch.script("codex", "#!/usr/bin/env fakenode\n");
        // The process's own PATH lacks the dir: the shim fails...
        assert_eq!(run_with_path(&shim, "/usr/bin:/bin".as_ref()), None);
        // ...and runs once the dir comes first.
        let path = path_with(shim.parent().unwrap(), Some("/usr/bin:/bin".into()));
        assert_eq!(run_with_path(&shim, &path).as_deref(), Some("ran-by-fakenode\n"));
    }

    #[test]
    fn npm_shim_finds_node_in_another_dir_through_the_login_path() {
        let scratch = Scratch::new("shimsplit");
        // An npm global prefix apart from node's own bin dir.
        let shim = scratch.script("npmg/codex", "#!/usr/bin/env fakenode\n");
        scratch.script("node/fakenode", "#!/bin/sh\necho ran-by-fakenode\n");
        let (npmg, node) = (scratch.path("npmg"), scratch.path("node"));
        // A login config that puts both on PATH. It runs the command with a
        // plain `sh -c`: `sh -l` would source this host's real profile.
        let shell = scratch.shell(&format!(
            "PATH={}:{}:/usr/bin:/bin; export PATH\nexec /bin/sh -c \"$2\"",
            npmg.display(),
            node.display()
        ));
        let (out, _) = lookup(&shell, Duration::from_secs(5));
        let found = codex_from_login_output(&out.unwrap(), Some("/usr/bin:/bin".into())).unwrap();
        assert_eq!(found.path, shim);
        let child_path = found.child_path.unwrap();
        assert_eq!(child_path, format!("{}:{}:/usr/bin:/bin", npmg.display(), node.display()).as_str());
        assert_eq!(run_with_path(&shim, &child_path).as_deref(), Some("ran-by-fakenode\n"));
        // The shim's dir in front of our PATH is not enough in this layout.
        assert_eq!(run_with_path(&shim, &path_with(&npmg, Some("/usr/bin:/bin".into()))), None);
    }

    #[test]
    fn login_output_after_marker_runs_codex_with_the_login_path() {
        let scratch = Scratch::new("loginmarker");
        let codex = scratch.script("codex", "#!/bin/sh\n");
        let stdout = format!("welcome!\n{LOGIN_MARKER}\n/a/bin:/b/bin\n{}\n", codex.display());
        let found = codex_from_login_output(stdout.as_bytes(), Some("/usr/bin".into())).unwrap();
        assert_eq!(found.path, codex);
        assert_eq!(found.child_path.unwrap(), "/a/bin:/b/bin");
    }

    #[test]
    fn login_output_uses_only_what_follows_the_last_marker() {
        let scratch = Scratch::new("loginlast");
        let old = scratch.script("old/codex", "#!/bin/sh\n");
        let codex = scratch.script("codex", "#!/bin/sh\n");
        // Login-config noise that looks like our own output comes first.
        let stdout =
            format!("{LOGIN_MARKER}\n/noise/bin\n{}\n{LOGIN_MARKER}\n/login/bin\n{}\n", old.display(), codex.display());
        let found = codex_from_login_output(stdout.as_bytes(), None).unwrap();
        assert_eq!(found.path, codex);
        assert_eq!(found.child_path.unwrap(), "/login/bin");
    }

    #[test]
    fn login_output_with_empty_path_puts_codex_dir_first() {
        let scratch = Scratch::new("loginempty");
        let codex = scratch.script("codex", "#!/bin/sh\n");
        let stdout = format!("{LOGIN_MARKER}\n\n{}\n", codex.display());
        let found = codex_from_login_output(stdout.as_bytes(), Some("/usr/bin".into())).unwrap();
        assert_eq!(found.path, codex);
        assert_eq!(found.child_path.unwrap(), format!("{}:/usr/bin", scratch.0.display()).as_str());
    }

    #[test]
    fn login_output_with_marker_but_no_codex_gives_none() {
        let scratch = Scratch::new("loginnocodex");
        let codex = scratch.script("codex", "#!/bin/sh\n");
        // An executable printed before the marker is login-config noise.
        let stdout = format!("{}\n{LOGIN_MARKER}\n/usr/bin:/bin\n", codex.display());
        assert!(codex_from_login_output(stdout.as_bytes(), None).is_none());
        assert!(codex_from_login_output(format!("{LOGIN_MARKER}\n").as_bytes(), None).is_none());
    }

    #[test]
    fn response_result_is_the_result_the_error_or_failed() {
        let e = response_result(&serde_json::json!({"id": 1})).err().unwrap();
        assert_eq!(e.code, ErrorCode::Failed);
        let ok = response_result(&serde_json::json!({"id": 1, "result": {"rateLimits": {}}})).unwrap();
        assert_eq!(ok, serde_json::json!({"rateLimits": {}}));
        let e = response_result(&serde_json::json!({"id": 1, "error": {"message": "Not logged in"}})).err().unwrap();
        assert_eq!(e.code, ErrorCode::NotSignedIn);
    }

    #[test]
    fn rpc_error_reads_codex_sign_in_errors_as_not_signed_in() {
        for message in [
            "codex account authentication required to read rate limits",
            "chatgpt authentication required to read rate limits",
        ] {
            let e = rpc_error(&serde_json::json!({"code": -32600, "message": message}));
            assert_eq!(e.code, ErrorCode::NotSignedIn, "{message}");
        }
    }

    #[test]
    fn rpc_error_classifies_the_message() {
        let e = rpc_error(&serde_json::json!({"code": -32600, "message": "Not logged in"}));
        assert_eq!((e.code, e.detail.as_str()), (ErrorCode::NotSignedIn, "Not logged in"));
        let e = rpc_error(&serde_json::json!({"code": -32603, "message": "failed to assign thread"}));
        assert_eq!(e.code, ErrorCode::Failed);
    }

    #[test]
    fn rpc_error_without_message_is_failed_whatever_its_keys() {
        let err = serde_json::json!({"code": -32603, "data": {"authMode": "chatgpt"}});
        let e = rpc_error(&err);
        assert_eq!(e.code, ErrorCode::Failed);
        assert_eq!(e.detail, err.to_string());
    }

    #[test]
    fn login_output_gives_last_executable_path_with_its_dir_on_path() {
        let scratch = Scratch::new("loginout");
        let codex = scratch.script("codex", "#!/bin/sh\n");
        let stdout = format!("welcome!\n{}\nrelative/codex\n/nonexistent/codex\n", codex.display());
        let found = codex_from_login_output(stdout.as_bytes(), Some("/usr/bin".into())).unwrap();
        assert_eq!(found.path, codex);
        assert_eq!(found.child_path.unwrap(), format!("{}:/usr/bin", scratch.0.display()).as_str());
    }

    #[test]
    fn login_output_without_an_executable_path_gives_none() {
        let scratch = Scratch::new("loginnone");
        let not_exe = scratch.path("codex");
        std::fs::write(&not_exe, "").unwrap();
        let stdout = format!("{}\nrelative/codex\n/nonexistent/codex\n", not_exe.display());
        assert!(codex_from_login_output(stdout.as_bytes(), None).is_none());
        assert!(codex_from_login_output(b"", None).is_none());
    }

    #[test]
    fn app_server_command_sets_path_only_when_needed() {
        let path_env = |codex: &CodexBin| {
            app_server_command(codex)
                .get_envs()
                .find(|(k, _)| *k == "PATH")
                .map(|(_, v)| v.map(std::ffi::OsStr::to_owned))
        };
        let plain = CodexBin { path: "/usr/bin/codex".into(), child_path: None };
        assert_eq!(path_env(&plain), None);
        let shell = CodexBin { path: "/n/bin/codex".into(), child_path: Some("/n/bin:/usr/bin".into()) };
        assert_eq!(path_env(&shell), Some(Some("/n/bin:/usr/bin".into())));
        let cmd = app_server_command(&shell);
        assert_eq!(cmd.get_program(), "/n/bin/codex");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["app-server"]);
    }

    // --- kill_tree ---

    fn spawn_group(script: &str) -> Child {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn kill_tree_kills_a_group_member_that_ignores_sigterm() {
        let scratch = Scratch::new("killtree");
        let pidfile = scratch.path("pid");
        // The leader dies on SIGTERM; the background holder ignores it.
        let mut child =
            spawn_group(&format!("sh -c 'trap \"\" TERM; echo $$ > {}; exec sleep 30' &\nsleep 30", pidfile.display()));
        assert!(wait_until(40, || std::fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n'))));
        let holder = read_pid(&pidfile);
        let start = Instant::now();
        kill_tree(&mut child);
        assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());
        assert!(is_gone(holder), "holder {holder} survived kill_tree");
    }

    #[test]
    fn kill_tree_kills_a_leader_that_ignores_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let scratch = Scratch::new("killleader");
        let ready = scratch.path("ready");
        let mut child = spawn_group(&format!("trap '' TERM\ntouch {}\nsleep 30", ready.display()));
        assert!(wait_until(40, || ready.exists()), "the shell never installed its trap");
        let start = Instant::now();
        kill_tree(&mut child);
        assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());
        // SIGTERM was ignored, so only the SIGKILL can have ended it.
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn kill_tree_on_an_exited_child_returns_promptly() {
        let mut child = spawn_group("exit 0");
        assert!(wait_until(40, || has_exited(&child).unwrap_or(false)));
        let start = Instant::now();
        kill_tree(&mut child);
        assert!(start.elapsed() < Duration::from_millis(500), "took {:?}", start.elapsed());
    }

    // --- read_rate_limits against a fake app server ---

    const INIT_REPLY: &str = r#"{"id":0,"result":{}}"#;
    const LIMITS_REPLY: &str = r#"{"id":1,"result":{"rateLimits":{"primary":{"windowDurationMins":300,"usedPercent":5,"resetsAt":1800000000}}}}"#;

    /// `read_rate_limits` with the `/bin/sh` script `body` as `codex
    /// app-server`; `$INIT` and `$LIMITS` in it hold the two replies.
    fn rate_limits_from(scratch: &Scratch, body: &str, budget: Duration) -> (ProbeResult, Duration) {
        let codex =
            scratch.script("codex", &format!("#!/bin/sh\nINIT='{INIT_REPLY}'\nLIMITS='{LIMITS_REPLY}'\n{body}\n"));
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let result = read_rate_limits(&CodexBin { path: codex, child_path: None }, start + budget);
        (result, start.elapsed())
    }

    fn expect_ok(result: ProbeResult) -> FetchOk {
        match result {
            Ok(d) => d,
            Err(e) => panic!("expected rate limits, got {e:?}"),
        }
    }

    fn expect_err(result: ProbeResult) -> ProbeError {
        result.err().expect("expected an error, got rate limits")
    }

    #[test]
    fn app_server_replies_are_read_past_noise_and_split_writes() {
        let scratch = Scratch::new("rpcok");
        // Startup noise, a notification first, the init reply in two writes,
        // and a notification sharing a write with the limits reply.
        let (result, elapsed) = rate_limits_from(
            &scratch,
            "echo starting up\nread line\necho '{\"method\":\"remoteControl/status/changed\",\"params\":{}}'\n\
             printf '%s' '{\"id\":0,'\nsleep 0.1\necho '\"result\":{}}'\nread line\nread line\n\
             printf '%s\\n%s\\n' '{\"method\":\"account/updated\",\"params\":{}}' \"$LIMITS\"\nsleep 30",
            Duration::from_secs(10),
        );
        let d = expect_ok(result);
        assert_eq!(d.windows.len(), 1);
        assert_eq!((d.windows[0].label, d.windows[0].used_percent), ("5h", 5.0));
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[test]
    fn last_reply_without_newline_is_read_at_exit() {
        let scratch = Scratch::new("rpceof");
        let (result, _) = rate_limits_from(
            &scratch,
            "read line\necho \"$INIT\"\nread line\nread line\nprintf '%s' \"$LIMITS\"",
            Duration::from_secs(10),
        );
        expect_ok(result);
    }

    #[test]
    fn app_server_that_exits_without_answering_fails_at_once() {
        let scratch = Scratch::new("rpcexit");
        let (result, elapsed) = rate_limits_from(&scratch, "read line\nexit 0", Duration::from_secs(10));
        let err = expect_err(result);
        assert_eq!(err.code, ErrorCode::Failed, "{err:?}");
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[test]
    fn silent_app_server_times_out_by_the_deadline() {
        let scratch = Scratch::new("rpcsilent");
        let (result, elapsed) = rate_limits_from(&scratch, "sleep 30", Duration::from_secs(1));
        assert_eq!(expect_err(result).code, ErrorCode::Timeout);
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[test]
    fn writer_that_never_pauses_cannot_hold_past_the_deadline() {
        let scratch = Scratch::new("rpcflood");
        let (result, elapsed) =
            rate_limits_from(&scratch, "read line\nyes '{\"method\":\"noise\"}'", Duration::from_secs(1));
        assert_eq!(expect_err(result).code, ErrorCode::Timeout);
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[test]
    fn setsid_holder_of_stdout_keeps_nothing_of_the_probe_alive() {
        let scratch = Scratch::new("rpcsetsid");
        let (ready, go, marker, pidfile) =
            (scratch.path("ready"), scratch.path("go"), scratch.path("epipe"), scratch.path("pid"));
        // The holder leaves the app server's process group, beyond kill_tree,
        // with its stdout; the server answers only once it has (else killpg
        // would take the holder down before setsid ran). Once told to, the
        // holder writes: that fails with EPIPE only if nothing of the probe
        // (a reader thread) still holds the pipe's read end.
        let (result, elapsed) = rate_limits_from(
            &scratch,
            &format!(
                "setsid sh -c 'touch {ready}; trap \"\" PIPE; i=0; while [ ! -e {go} ] && [ $i -lt 100 ]; do \
                 sleep 0.1; i=$((i+1)); done; echo late || touch {marker}' &\necho $! > {pid}\n\
                 i=0; while [ ! -e {ready} ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done\n\
                 read line\necho \"$INIT\"\nread line\nread line\necho \"$LIMITS\"\nsleep 30",
                ready = ready.display(),
                go = go.display(),
                marker = marker.display(),
                pid = pidfile.display()
            ),
            Duration::from_secs(10),
        );
        expect_ok(result);
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
        assert!(ready.exists(), "the holder never left the process group");
        std::fs::write(&go, "").unwrap();
        let reached_epipe = wait_until(60, || marker.exists());
        let holder = read_pid(&pidfile);
        // SAFETY: plain signal to the test's own holder, in case it lingers.
        unsafe {
            libc::kill(holder, libc::SIGKILL);
        }
        assert!(reached_epipe, "the holder's late write succeeded: the read end outlived the probe");
    }

    // --- normalize ---

    fn window(mins: i64) -> serde_json::Value {
        serde_json::json!({"windowDurationMins": mins, "usedPercent": 12.5, "resetsAt": 1_800_000_000})
    }

    #[test]
    fn normalize_reads_known_windows_short_first() {
        let d = normalize(&serde_json::json!({
            "rateLimits": {"primary": window(10080), "secondary": window(300), "planType": "plus"}
        }))
        .unwrap();
        let labels: Vec<_> = d.windows.iter().map(|w| w.label).collect();
        assert_eq!(labels, ["5h", "wk"]);
        assert_eq!(d.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn normalize_unknown_window_length_is_failed_not_signed_out() {
        let err = normalize(&serde_json::json!({"rateLimits": {"primary": window(1440)}})).err().unwrap();
        assert_eq!(err.code, ErrorCode::Failed);
    }

    #[test]
    fn normalize_incomplete_window_is_failed_not_signed_out() {
        let err =
            normalize(&serde_json::json!({"rateLimits": {"primary": {"windowDurationMins": 300}}})).err().unwrap();
        assert_eq!(err.code, ErrorCode::Failed);
    }

    #[test]
    fn normalize_keeps_known_window_next_to_unknown_one() {
        let d =
            normalize(&serde_json::json!({"rateLimits": {"primary": window(300), "secondary": window(1440)}})).unwrap();
        assert_eq!(d.windows.len(), 1);
        assert_eq!(d.windows[0].label, "5h");
    }

    #[test]
    fn normalize_without_rate_limits_object_is_failed() {
        for result in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!({"rateLimits": null}),
            serde_json::json!({"rateLimits": []}),
            serde_json::json!({"rateLimits": "x"}),
        ] {
            assert_eq!(normalize(&result).err().unwrap().code, ErrorCode::Failed, "{result}");
        }
    }

    #[test]
    fn normalize_without_windows_is_no_quota() {
        for result in [
            serde_json::json!({"rateLimits": {}}),
            serde_json::json!({"rateLimits": {"primary": null, "secondary": null}}),
        ] {
            assert_eq!(normalize(&result).err().unwrap().code, ErrorCode::NoQuota, "{result}");
        }
    }

    // --- auth_hint ---

    #[test]
    fn auth_hint_matches_sign_in_errors() {
        for msg in [
            "Not logged in",
            "please log in to ChatGPT",
            "Sign in required",
            "sign-in expired",
            "You are signed out",
            "HTTP 401 Unauthorized",
            "unauthorised",
            "auth token expired",
            "OAuth refresh token was rejected: invalid_grant",
            "Authorization failed",
            "not authorized",
            "please reauthenticate",
            "AuthRequired",
            "NotLoggedIn",
            "ChatGPT account required",
            "Authentication failed",
            "missing API key",
            "no ChatGPT account",
            "login required",
        ] {
            assert!(auth_hint(msg), "{msg}");
        }
    }

    #[test]
    fn auth_hint_ignores_lookalike_words() {
        for msg in [
            "failed to assign thread",
            "invalid signal handler",
            "bad signature",
            "design error",
            "unknown field `author`",
            "request id 74012",
            "port 4010 refused",
            "log rotated in /var/log",
            "certificate authority unknown",
            "authored by codex",
            "AuthorName missing",
            "Catalog index in progress",
        ] {
            assert!(!auth_hint(msg), "{msg}");
        }
    }

    #[test]
    fn scratch_dir_is_removed() {
        let dir = {
            let scratch = Scratch::new("cleanup");
            scratch.shell("true");
            scratch.0.clone()
        };
        assert!(!dir.exists());
    }
}
