//! Codex (ChatGPT plan) usage: ask the local `codex app-server` over stdio
//! (newline-delimited JSON-RPC): initialize -> initialized ->
//! account/rateLimits/read. Makes no model request and no network call of its
//! own; codex uses and refreshes its own login.
//!
//! Port of herdr-codex-limits' codex_limits.py.

use std::io::{BufRead, BufReader, Write};
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
    if let Ok(shell) = std::env::var("SHELL")
        && let Ok(mut child) = Command::new(&shell)
            .args(["-lc", "command -v codex"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    {
        // The login shell may source slow config; bound it like the Python probe.
        let shell_deadline = Instant::now() + (deadline - Instant::now()).min(Duration::from_secs(5));
        let out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break child.wait_with_output().ok(),
                Ok(None) if Instant::now() < shell_deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Err(_) => break None,
            }
        };
        if let Some(out) = out {
            for line in String::from_utf8_lossy(&out.stdout).lines().rev() {
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
                let text = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| err.to_string());
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
