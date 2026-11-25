//! Child processes bounded by a deadline: output read without blocking on the
//! caller's thread and capped, and the whole process group killed before the
//! call returns, so nothing a probe starts outlives it. Every probe child is
//! recorded in CHILDREN while it lives, so quitting the TUI, which doesn't wait
//! for the detached probe threads, kills them too.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Most bytes of a bounded child's output kept: the last ones, where the
/// answer follows anything printed before it (a login config's own output).
pub(super) const OUTPUT_CAP: usize = 64 * 1024;
/// How long a bounded child's stdout may stay open after the child exits
/// before the background jobs it left holding it are killed.
const EXIT_GRACE: Duration = Duration::from_millis(100);
/// Most bytes one `Pipe::step` reads, so a writer that never pauses can't keep
/// a step going past its deadline.
const PIPE_READ_BURST: usize = 64 * 1024;
/// Longest `kill_all` waits for spawns in flight to record their child: a
/// spawn returns only once the child has exec'd, which can stall on a hung
/// mount, and quitting must still restore the terminal.
const SPAWN_WAIT: Duration = Duration::from_millis(500);

/// The process groups of every live child the probes started.
pub(super) static CHILDREN: Children = Children::new();

/// A child's stdout, read without blocking on the caller's thread, so no read
/// outlasts a deadline and no reader thread is left for a holder of the write
/// end (a descendant outside the child's process group) to pin. Dropping it
/// closes the read end; such a holder's later writes get EPIPE.
pub(super) struct Pipe {
    stdout: ChildStdout,
    /// EOF or a read error; a closed pipe is no longer polled.
    pub(super) closed: bool,
}

impl Pipe {
    pub(super) fn new(stdout: ChildStdout) -> Self {
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
    pub(super) fn step(&mut self, deadline: Instant, sink: impl FnMut(&[u8])) -> bool {
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

/// A bounded child's stdout and the last OUTPUT_CAP bytes it printed.
struct BoundedOutput {
    pipe: Pipe,
    tail: Vec<u8>,
}

impl BoundedOutput {
    fn new(stdout: ChildStdout) -> Self {
        Self { pipe: Pipe::new(stdout), tail: Vec::new() }
    }

    /// `Pipe::step`, keeping the output's tail.
    fn step(&mut self, deadline: Instant) -> bool {
        let tail = &mut self.tail;
        self.pipe.step(deadline, |chunk| {
            tail.extend_from_slice(chunk);
            let excess = tail.len().saturating_sub(OUTPUT_CAP);
            tail.drain(..excess);
        })
    }

    /// Read until stdout closes or EXIT_GRACE passes, never past
    /// `deadline`.
    fn settle(&mut self, deadline: Instant) {
        let end = (Instant::now() + EXIT_GRACE).min(deadline);
        while !self.pipe.closed && self.step(end) {}
    }

    /// The output kept so far. Consuming `self` closes the read end, so a
    /// holder outside the child's group keeps nothing of this call alive.
    fn into_tail(self) -> Vec<u8> {
        self.tail
    }
}

/// Process groups of live children, for `kill_all` to kill when the TUI quits:
/// the probe threads that would kill them are detached and die with the
/// process. A group is recorded from its spawn until just before its leader is
/// reaped. The lock is never held across a spawn (fork and exec, which can
/// stall on a hung mount) or a wait on a child or its output, so one probe's
/// spawn never waits for another's, and `kill_all` waits at most SPAWN_WAIT,
/// for spawns in flight.
pub(super) struct Children {
    groups: Mutex<Groups>,
    /// Signalled at the end of each spawn, for `kill_all` to stop waiting.
    spawn_done: Condvar,
}

struct Groups {
    /// Group ids: the pids of children spawned with `process_group(0)` and not
    /// reaped yet.
    live: Vec<libc::pid_t>,
    /// Set by `kill_all`: no child is spawned after it.
    closed: bool,
    /// Spawns in flight: started, but their child not recorded yet.
    spawning: usize,
}

impl Children {
    const fn new() -> Self {
        Self { groups: Mutex::new(Groups { live: Vec::new(), closed: false, spawning: 0 }), spawn_done: Condvar::new() }
    }

    /// Spawn `cmd`, which must set `process_group(0)`, and record its group.
    /// The spawn runs outside the lock, counted in `spawning`; its child is
    /// recorded under the lock, and if `kill_all` has begun by then, its group
    /// is killed there and then. So a child is spawned before `kill_all` and
    /// killed by it, killed by its own spawn, or never spawned. Only a spawn
    /// still stalled in exec when `kill_all` stops waiting (SPAWN_WAIT) and the
    /// process exits can leave a child that outlives the quit. A killed child is
    /// still returned, for the caller to reap through `reap`.
    pub(super) fn spawn(&self, cmd: &mut Command) -> std::io::Result<Child> {
        {
            let mut groups = self.lock();
            if groups.closed {
                return Err(std::io::Error::other("quitting: no new child"));
            }
            groups.spawning += 1;
        }
        let spawned = cmd.spawn();
        let mut groups = self.lock();
        groups.spawning -= 1;
        self.spawn_done.notify_all();
        let child = spawned?;
        let group = child.id() as libc::pid_t;
        groups.live.push(group);
        if groups.closed {
            // SAFETY: killpg only sends a signal. `group` is the pid of the
            // child just spawned, which leads the group process_group(0)
            // created, and nothing has reaped it yet: the id cannot have been
            // reused.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
        Ok(child)
    }

    /// Forget `child`'s group, then reap it. Forgetting comes first, so
    /// `kill_all` never signals a group id whose leader was reaped and whose
    /// pid may have been reused.
    pub(super) fn reap(&self, child: &mut Child) -> std::io::Result<ExitStatus> {
        let pid = child.id() as libc::pid_t;
        self.lock().live.retain(|&group| group != pid);
        child.wait()
    }

    /// Refuse new children, wait up to SPAWN_WAIT for spawns in flight to
    /// record theirs, then SIGKILL every recorded group. A spawn that ends
    /// after the wait kills its own child (see `spawn`). Doesn't wait for
    /// anything to exit: the probe threads (or init, once we exit) reap them.
    pub(super) fn kill_all(&self) {
        let mut groups = self.lock();
        groups.closed = true;
        let deadline = Instant::now() + SPAWN_WAIT;
        while groups.spawning > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            // Not wait_timeout_while: its `?` would end the wait on a
            // poisoned lock.
            groups = self.spawn_done.wait_timeout(groups, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        for &group in &groups.live {
            // SAFETY: killpg only sends a signal. `group` is the pid of a
            // child that leads the group process_group(0) created, and it is
            // still recorded, so `reap` (which forgets it under this lock
            // first) hasn't reaped it: the id cannot have been reused.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
    }

    /// The lock, even if a thread panicked holding it: `live`, `closed` and
    /// `spawning` are each changed in one step, and quitting must still kill.
    fn lock(&self) -> MutexGuard<'_, Groups> {
        self.groups.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn contains(&self, pid: u32) -> bool {
        self.lock().live.contains(&(pid as libc::pid_t))
    }
}

/// The last OUTPUT_CAP bytes of stdout of `cmd`, or None if it could not be
/// started. Returns by `deadline`, apart from reaping a killed child. Before it
/// returns, the child's process group is killed: the child itself if it is
/// still running at the deadline, and every background job left in the group,
/// whether or not it still holds stdout. Holders outside that group (setsid)
/// can't be reached, but the read end is closed on return, so nothing outlives
/// the call: their later writes get EPIPE. The child runs in
/// `/` with stdin and stderr closed, so a planted file in an untrusted working
/// directory can't be run. Its group is in CHILDREN while it runs.
pub(super) fn run_bounded(cmd: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    run_bounded_with(&CHILDREN, cmd, deadline)
}

/// `run_bounded`, recording the child in `children`.
fn run_bounded_with(children: &Children, cmd: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    // A non-interactive shell keeps its background jobs in this group.
    cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0);
    let mut child = children.spawn(cmd).ok()?;
    let mut output = BoundedOutput::new(child.stdout.take().expect("stdout is piped"));

    // (A) Wait for the child itself, not for stdout to close; failing to
    // check on it counts as not exited.
    let exited = loop {
        match has_exited(&child) {
            Ok(true) => break true,
            Ok(false) if output.step(deadline) => {}
            _ => break false,
        }
    };
    // (B) Let stdout drain and close.
    if exited {
        output.settle(deadline);
    }
    // (C) Kill the whole group, even once the child has exited and stdout
    // has closed: a background job that sent its output elsewhere still runs
    // in it.
    // SAFETY: killpg only sends a signal. The group id is the pid of our
    // child, which leads the group created by process_group(0). Only
    // `children.reap` below reaps the child (has_exited uses WNOWAIT), so its
    // pid, and with it the group id, cannot have been reused yet.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    // Killed holders release stdout at once.
    output.settle(deadline);
    let tail = output.into_tail();
    let _ = children.reap(&mut child);
    Some(tail)
}

/// Whether `child` has exited, without reaping it: its pid stays reserved, so
/// it is still safe to signal as a process group id.
pub(super) fn has_exited(child: &Child) -> std::io::Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;

    use crate::probe::tests::{live_with, sleep_token};
    use crate::probe::{SCRIPTS, wait_until};

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd
    }

    /// `script` run bounded (8 s) on another thread with its group recorded in
    /// `children`; the output arrives on the receiver.
    fn run_in_thread(children: &'static Children, script: &str) -> mpsc::Receiver<Option<Vec<u8>>> {
        let (tx, rx) = mpsc::channel();
        let mut cmd = sh(script);
        std::thread::spawn(move || {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let _ = tx.send(run_bounded_with(children, &mut cmd, Instant::now() + Duration::from_secs(8)));
        });
        rx
    }

    /// The group of the one child recorded in `children`, once there is one.
    /// Never 0 or negative: killpg would read those as our own group.
    fn recorded_group(children: &Children) -> libc::pid_t {
        let mut group = None;
        assert!(wait_until(40, || {
            group = children.lock().live.first().copied();
            group.is_some()
        }));
        let group = group.unwrap();
        assert!(group > 1, "bad group {group}");
        group
    }

    /// Live (not zombie) processes in process group `group`.
    fn live_in_group(group: libc::pid_t) -> usize {
        let Ok(dir) = std::fs::read_dir("/proc") else { return 0 };
        dir.filter_map(|e| e.ok())
            .filter(|e| {
                let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
                // "pid (comm) state ppid pgrp ...": comm may hold anything, so
                // count fields after its last ')'.
                let fields: Vec<&str> = stat.rsplit_once(')').map_or("", |(_, rest)| rest).split_whitespace().collect();
                fields.first().is_some_and(|&state| state != "Z")
                    && fields.get(2).and_then(|g| g.parse().ok()) == Some(group)
            })
            .count()
    }

    /// A child hung like a slow login shell, with a background job in its
    /// group, is killed whole by `kill_all` long before its deadline.
    fn assert_kill_all_kills_a_running_group(children: &'static Children) {
        let done = run_in_thread(children, "sleep 30 &\nsleep 30");
        let group = recorded_group(children);
        assert!(wait_until(40, || live_in_group(group) >= 2), "the background job never started");
        children.kill_all();
        let out = done.recv_timeout(Duration::from_secs(4)).expect("the bounded run outlived kill_all");
        assert!(out.is_some());
        // Zombies count as gone: the orphaned job waits for init to reap it.
        assert!(
            wait_until(60, || live_in_group(group) == 0),
            "group {group} has {} live members",
            live_in_group(group)
        );
        assert!(children.lock().live.is_empty());
    }

    #[test]
    fn kill_all_kills_the_whole_group_of_a_running_child() {
        static CHILDREN: Children = Children::new();
        assert_kill_all_kills_a_running_group(&CHILDREN);
    }

    #[test]
    fn kill_all_still_kills_after_a_panic_poisoned_the_lock() {
        static CHILDREN: Children = Children::new();
        let _ = std::thread::spawn(|| {
            let _held = CHILDREN.groups.lock();
            panic!("a probe thread panics holding the lock");
        })
        .join();
        assert!(CHILDREN.groups.is_poisoned());
        assert_kill_all_kills_a_running_group(&CHILDREN);
    }

    #[test]
    fn a_child_is_recorded_while_it_runs_and_forgotten_once_reaped() {
        static CHILDREN: Children = Children::new();
        let done = run_in_thread(&CHILDREN, "sleep 0.3; echo done");
        recorded_group(&CHILDREN);
        let out = done.recv_timeout(Duration::from_secs(10)).expect("the bounded run never returned");
        assert_eq!(out.as_deref(), Some(&b"done\n"[..]));
        assert!(CHILDREN.lock().live.is_empty());
    }

    #[test]
    fn kill_all_with_no_children_signals_nothing() {
        static CHILDREN: Children = Children::new();
        // A stray killpg(0) would take down this test process's own group.
        CHILDREN.kill_all();
        assert!(CHILDREN.lock().live.is_empty());
    }

    #[test]
    fn no_child_is_spawned_after_kill_all() {
        static CHILDREN: Children = Children::new();
        CHILDREN.kill_all();
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let out = run_bounded_with(&CHILDREN, &mut sh("echo ran"), Instant::now() + Duration::from_secs(5));
        assert_eq!(out, None, "a probe that starts after quitting began must not spawn");
        assert!(CHILDREN.lock().live.is_empty());
    }

    /// A spawn stalled before exec, as on a hung mount, holds up neither
    /// another probe's spawn nor `kill_all` past SPAWN_WAIT, and once it ends,
    /// it kills its own child.
    #[test]
    fn a_stalled_spawn_blocks_neither_other_spawns_nor_kill_all() {
        use std::os::unix::process::ExitStatusExt;
        static CHILDREN: Children = Children::new();
        // Held until the stalled child has forked, not through its stall: no
        // script is open for writing while it is held, so the child inherits
        // none, and other tests' forks and script writes wait only briefly.
        let forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let (mut forked, forked_tx) = std::io::pipe().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut cmd = sh("exec sleep 30");
            cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
            let fd = forked_tx.as_raw_fd();
            // SAFETY: the hook runs in the forked child before exec and only
            // makes async-signal-safe calls (write, nanosleep); it allocates
            // nothing. `fd` stays open in the child: `forked_tx` outlives
            // the spawn.
            unsafe {
                cmd.pre_exec(move || {
                    libc::write(fd, b"f".as_ptr().cast(), 1);
                    std::thread::sleep(Duration::from_secs(2));
                    Ok(())
                });
            }
            let spawned = CHILDREN.spawn(&mut cmd);
            drop(forked_tx);
            let _ = tx.send(spawned.and_then(|mut child| CHILDREN.reap(&mut child)));
        });
        let mut byte = [0u8; 1];
        forked.read_exact(&mut byte).expect("the stalled spawn never forked");
        drop(forking);
        assert!(wait_until(40, || CHILDREN.lock().spawning == 1), "the stalled spawn is not in flight");

        let out = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let start = Instant::now();
            let out = run_bounded_with(&CHILDREN, &mut sh("echo ran"), Instant::now() + Duration::from_secs(5));
            let elapsed = start.elapsed();
            assert!(elapsed < Duration::from_secs(1), "a spawn waited {elapsed:?} for another probe's stalled spawn");
            out
        };
        assert_eq!(out.as_deref(), Some(&b"ran\n"[..]));

        let start = Instant::now();
        CHILDREN.kill_all();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(1500), "kill_all waited {elapsed:?} for a stalled spawn");

        let status = rx.recv_timeout(Duration::from_secs(10)).expect("the stalled spawn never returned");
        let status = status.expect("the stalled spawn failed");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "the stalled spawn's child ended with {status:?}");
        assert!(CHILDREN.lock().live.is_empty());
    }

    #[test]
    fn run_bounded_records_its_child_in_children() {
        // A fresh sleep argument to find the child by.
        let token = sleep_token("record");
        let (tx, rx) = mpsc::channel();
        let mut cmd = sh(&format!("exec sleep 30 {token}"));
        std::thread::spawn(move || {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let _ = tx.send(run_bounded(&mut cmd, Instant::now() + Duration::from_secs(30)));
        });
        let mut pid = None;
        assert!(wait_until(40, || {
            pid = live_with(&token).first().and_then(|p| p.parse().ok());
            pid.is_some()
        }));
        let pid: u32 = pid.unwrap();
        // Checked before the kill, asserted after: a failed check must not
        // leave the run holding SCRIPTS for its 30 s.
        let recorded = CHILDREN.contains(pid);
        let group = pid as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        // SAFETY: killpg only sends a signal. The child leads the group
        // process_group(0) created and runs until this signal or its 30 s
        // deadline; run_bounded reaps it only after that, so the id is still
        // its own.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        assert!(recorded, "run_bounded's child {pid} is not in CHILDREN");
        rx.recv_timeout(Duration::from_secs(10)).expect("the bounded run never returned");
        assert!(!CHILDREN.contains(pid));
    }
}
