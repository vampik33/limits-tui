//! Codex (ChatGPT plan) usage: ask the local `codex app-server` over stdio
//! (newline-delimited JSON-RPC): initialize -> initialized ->
//! account/rateLimits/read. Makes no model request and no network call of its
//! own; codex uses and refreshes its own login.
//!
//! Port of herdr-codex-limits' codex_limits.py.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_f64, json_int, unix_now};

const DEFAULT_TIMEOUT_SECS: f64 = 15.0;

fn is_exe(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file() && p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Stdout of `shell -lc "command -v codex"`, or None if the shell could not
/// be started or printed nothing readable. Returns by `shell_deadline` even
/// when a background job of the login config keeps stdout open: the shell's
/// process group is then killed and what was printed so far is returned.
fn login_shell_lookup(shell: &std::ffi::OsStr, shell_deadline: Instant) -> Option<Vec<u8>> {
    let mut child = Command::new(shell)
        .args(["-lc", "command -v codex"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0) // a non-interactive shell keeps its background jobs in this group
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let out = rx.recv_timeout(shell_deadline.saturating_duration_since(Instant::now())).ok();
    let exited = out.is_some()
        && loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < shell_deadline => std::thread::sleep(Duration::from_millis(25)),
                _ => break false,
            }
        };
    if exited {
        return out;
    }
    // SAFETY: killpg only sends a signal. The group id is the pid of our
    // child, which leads the group created by process_group(0); the child is
    // not reaped yet, so the id cannot have been reused.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
    out.or_else(|| rx.recv_timeout(Duration::from_millis(100)).ok())
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

    /// Write `body` as an executable `/bin/sh` script in a per-test temp dir
    /// and return its path; the fake "login shell" ignores its arguments.
    fn fake_shell(test: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let _writing = SCRIPTS.write().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("limits-tui-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shell");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn lookup(shell: &std::path::Path, budget: Duration) -> (Option<Vec<u8>>, Duration) {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let out = login_shell_lookup(shell.as_os_str(), start + budget);
        (out, start.elapsed())
    }

    #[test]
    fn plain_shell_returns_its_output() {
        let shell = fake_shell("plain", "echo /bin/sh");
        let (out, elapsed) = lookup(&shell, Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some(&b"/bin/sh\n"[..]));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn background_job_holding_stdout_does_not_outlive_deadline() {
        let shell = fake_shell("bg", "sleep 30 &\necho /bin/sh");
        let (out, elapsed) = lookup(&shell, Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        // The path printed before the deadline is still used.
        assert_eq!(out.as_deref(), Some(&b"/bin/sh\n"[..]));
    }

    #[test]
    fn background_job_holding_stdout_is_killed() {
        let dir = std::env::temp_dir().join(format!("limits-tui-{}-bgkill", std::process::id()));
        let pidfile = dir.join("pid");
        let shell = fake_shell("bgkill", &format!("sleep 30 &\necho $! > {}\necho /bin/sh", pidfile.display()));
        let (_, elapsed) = lookup(&shell, Duration::from_secs(1));
        // Without the bound, a lookup that waits out the job would pass too.
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        // Reparented to init after the kill; allow a moment to be reaped.
        let stat = std::path::PathBuf::from(format!("/proc/{}/stat", pid.trim()));
        let gone = (0..40).any(|_| {
            let alive = std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z "));
            if alive {
                std::thread::sleep(Duration::from_millis(50));
            }
            !alive
        });
        assert!(gone, "background sleep {} is still running", pid.trim());
    }

    #[test]
    fn shell_that_never_exits_is_bounded() {
        let shell = fake_shell("hang", "sleep 30");
        let (out, elapsed) = lookup(&shell, Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        assert!(out.unwrap_or_default().is_empty());
    }

    #[test]
    fn shell_that_closes_stdout_but_keeps_running_is_bounded() {
        let shell = fake_shell("closed", "exec >/dev/null\nsleep 30");
        let (out, elapsed) = lookup(&shell, Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        // Some (not None) shows the shell really ran and closed its stdout.
        assert_eq!(out.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn failing_shell_returns_no_path() {
        let shell = fake_shell("fail", "exit 1");
        let (out, elapsed) = lookup(&shell, Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some(&b""[..]));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn deadline_already_passed_returns_promptly() {
        let shell = fake_shell("late", "sleep 30");
        let (_, elapsed) = lookup(&shell, Duration::ZERO);
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn missing_shell_returns_none() {
        let (out, _) = lookup(std::path::Path::new("/nonexistent/shell"), Duration::from_secs(1));
        assert!(out.is_none());
    }
}
