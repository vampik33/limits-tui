//! Codex (ChatGPT plan) usage: ask the local `codex app-server` over stdio
//! (newline-delimited JSON-RPC): initialize -> initialized ->
//! account/rateLimits/read. Makes no model request and no network call of its
//! own; codex uses and refreshes its own login.
//!
//! Port of herdr-codex-limits' codex_limits.py.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::{ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_f64, json_int, unix_now};

const DEFAULT_TIMEOUT_SECS: f64 = 15.0;
/// Most bytes of login-shell output kept; the rest is ignored.
const SHELL_OUTPUT_CAP: u64 = 64 * 1024;
/// How long the login shell's stdout may stay open after the shell exits
/// before the background jobs it left holding it are killed.
const SHELL_EXIT_GRACE: Duration = Duration::from_millis(100);

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

/// Stdout of `shell -lc "command -v codex"` (at most SHELL_OUTPUT_CAP bytes),
/// or None if the shell could not be started. Returns by `shell_deadline`,
/// apart from reaping a killed shell. Background jobs of the login config
/// that still hold stdout once the shell has exited (or the shell itself, at
/// the deadline) are killed with the shell's process group. Holders outside
/// that group (setsid) can't be reached: the output printed so far is
/// returned and the reader thread lives until they release stdout.
fn login_shell_lookup(shell: &std::ffi::OsStr, shell_deadline: Instant) -> Option<Vec<u8>> {
    let mut child = Command::new(shell)
        .args(["-lc", "command -v codex"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0) // a non-interactive shell keeps its background jobs in this group
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take().expect("stdout is piped").take(SHELL_OUTPUT_CAP);
    let output = Arc::new(Mutex::new(Vec::new()));
    let (closed_tx, closed_rx) = mpsc::channel::<()>();
    let sink = Arc::clone(&output);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => sink.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = closed_tx.send(());
    });
    let until = |t: Instant| t.saturating_duration_since(Instant::now());

    // Wait for the shell itself, not for stdout to close.
    let exited = loop {
        match has_exited(&child) {
            Ok(true) => break true,
            Ok(false) if Instant::now() < shell_deadline => {
                std::thread::sleep(until(shell_deadline).min(Duration::from_millis(25)));
            }
            _ => break false,
        }
    };
    // Let stdout drain and close; something still holding it gets killed.
    let grace_end = (Instant::now() + SHELL_EXIT_GRACE).min(shell_deadline);
    let closed = exited && closed_rx.recv_timeout(until(grace_end)).is_ok();
    if !closed {
        // SAFETY: killpg only sends a signal. The group id is the pid of our
        // child, which leads the group created by process_group(0); the child
        // is not reaped yet (has_exited uses WNOWAIT), so the id cannot have
        // been reused.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
        // Killed holders release stdout at once; give the reader a moment.
        let _ = closed_rx.recv_timeout(until((Instant::now() + SHELL_EXIT_GRACE).min(shell_deadline)));
    }
    let _ = child.wait();
    Some(std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner())))
}

/// CODEX_BIN, else PATH, else the login shell's PATH (desktop launches may
/// lack it) — same order as the Python probe.
fn find_codex(deadline: Instant) -> Result<std::path::PathBuf, ProbeError> {
    if let Ok(override_) = std::env::var("CODEX_BIN") {
        let p = std::path::PathBuf::from(&override_);
        if is_exe(&p) {
            return Ok(p);
        }
        return Err(ProbeError::new(ErrorCode::Missing, "CODEX_BIN is not an executable"));
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::Path::new(dir).join("codex");
            if is_exe(&p) {
                return Ok(p);
            }
        }
    }
    if let Ok(shell) = std::env::var("SHELL") {
        // The login shell may source slow config; bound it like the Python probe.
        let shell_deadline = Instant::now() + (deadline - Instant::now()).min(Duration::from_secs(5));
        if let Some(stdout) = login_shell_lookup(shell.as_ref(), shell_deadline) {
            for line in String::from_utf8_lossy(&stdout).lines().rev() {
                let p = std::path::PathBuf::from(line.trim());
                if p.is_absolute() && is_exe(&p) {
                    return Ok(p);
                }
            }
        }
    }
    Err(ProbeError::new(ErrorCode::Missing, "codex not found on PATH"))
}

/// Errors whose message hints the account isn't signed in.
fn auth_hint(text: &str) -> bool {
    let t = text.to_lowercase();
    ["auth", "log in", "login", "logged", "sign", "api key", "unauthorized", "unauthorised", "401", "chatgpt account"]
        .iter()
        .any(|h| t.contains(h))
}

/// Stop the child and anything it started (the npm shim runs node, which runs
/// codex). The child was spawned in its own process group, so kill the group.
fn kill_tree(child: &mut Child) {
    drop(child.stdin.take());
    let pgid = child.id() as libc::pid_t;
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

struct Rpc {
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<Option<String>>,
    deadline: Instant,
}

impl Rpc {
    fn spawn(child: &mut Child, deadline: Instant) -> Result<Self, ProbeError> {
        let stdin = child.stdin.take().ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no stdout"))?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(Some(line.unwrap_or_default())).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        Ok(Self { stdin, lines, deadline })
    }

    fn send(&mut self, msg: &serde_json::Value) -> Result<(), ProbeError> {
        writeln!(self.stdin, "{msg}")
            .and_then(|()| self.stdin.flush())
            .map_err(|_| ProbeError::new(ErrorCode::Failed, "codex app-server closed its input"))
    }

    /// The response with this id; notifications and other lines are skipped.
    fn wait_for(&self, request_id: i64) -> Result<serde_json::Value, ProbeError> {
        loop {
            let remaining = self.deadline.checked_duration_since(Instant::now());
            let Some(remaining) = remaining else {
                return Err(ProbeError::new(ErrorCode::Timeout, "codex app-server did not answer in time"));
            };
            let line = match self.lines.recv_timeout(remaining) {
                Ok(Some(l)) => l,
                Ok(None) => return Err(ProbeError::new(ErrorCode::Failed, "codex app-server exited")),
                Err(_) => return Err(ProbeError::new(ErrorCode::Timeout, "codex app-server did not answer in time")),
            };
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            if msg.get("id").and_then(|i| i.as_i64()) != Some(request_id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                let text =
                    err.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| err.to_string());
                let code = if auth_hint(&text) { ErrorCode::NotSignedIn } else { ErrorCode::Failed };
                return Err(ProbeError::new(code, text));
            }
            return Ok(msg.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
    }
}

fn normalize(result: &serde_json::Value) -> Result<FetchOk, ProbeError> {
    let limits = result.get("rateLimits");
    let Some(limits) = limits.filter(|l| l.is_object()) else {
        return Err(ProbeError::new(ErrorCode::NotSignedIn, "no rate limits for this account"));
    };
    let mut windows = Vec::new();
    for slot in ["primary", "secondary"] {
        let Some(w) = limits.get(slot) else { continue };
        let label = match json_int(&w["windowDurationMins"]) {
            Some(300) => "5h",
            Some(10080) => "wk",
            _ => continue,
        };
        let (Some(used), Some(resets)) = (json_f64(&w["usedPercent"]), json_int(&w["resetsAt"])) else {
            continue;
        };
        windows.push(UsageWindow { label, used_percent: used, resets_at: resets });
    }
    if windows.is_empty() {
        return Err(ProbeError::new(ErrorCode::NotSignedIn, "no usage windows for this account"));
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
    let timeout = std::env::var("CODEX_LIMITS_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .max(0.001);
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);

    let codex = find_codex(deadline)?;
    let mut child = Command::new(&codex)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0) // own group so kill_tree can take down npm-shim children
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
            use std::os::unix::fs::PermissionsExt;
            let _writing = SCRIPTS.write().unwrap_or_else(|e| e.into_inner());
            let path = self.path("shell");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
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

    /// Whether `pid` is gone (or a zombie awaiting init) within about 2s.
    fn is_gone(pid: libc::pid_t) -> bool {
        let stat = std::path::PathBuf::from(format!("/proc/{pid}/stat"));
        (0..40).any(|_| {
            let alive = std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z "));
            if alive {
                std::thread::sleep(Duration::from_millis(50));
            }
            !alive
        })
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
        assert!(out.len() as u64 <= SHELL_OUTPUT_CAP, "read {} bytes", out.len());
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
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
