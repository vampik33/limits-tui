//! The real binary's hidden `__resolve` mode: the DNS lookup child the Devin
//! probe runs. Unit tests call `resolve_main`'s logic directly; these check
//! that `main` dispatches to it.

use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Exit code and stdout of the binary run with `args`, killed after 10 s
/// (a binary that ignored `__resolve` would start the TUI instead).
fn run(args: &[&str]) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_limits-tui"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn resolve_mode_prints_addresses_for_localhost() {
    let (code, out) = run(&["__resolve", "localhost:443", "5000"]);
    assert_eq!(code, Some(0), "stdout: {out}");
    for line in out.lines() {
        let addr: SocketAddr = line.parse().unwrap();
        assert!(addr.ip().is_loopback() && addr.port() == 443, "{out}");
    }
    assert!(!out.is_empty());
}

#[test]
fn resolve_mode_with_bad_arguments_exits_2() {
    for args in [&["__resolve"][..], &["__resolve", "localhost:443"], &["__resolve", "localhost:443", "soon"]] {
        assert_eq!(run(args), (Some(2), String::new()), "{args:?}");
    }
}

#[test]
fn resolve_mode_reads_flags_as_its_host() {
    // `--once` after `__resolve` is a host to look up, not the --once mode.
    assert_eq!(run(&["__resolve", "--once", "1000"]), (Some(1), String::new()));
}
