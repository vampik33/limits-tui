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
/// Longest `reap_killed` waits for a child sent SIGKILL to exit before it
/// leaves the reap to a reaper thread. SIGKILL ends a process as soon as it
/// next runs, and even a large one is torn down well within this; one still
/// there is stuck in a kernel call that ignores signals (a hung NFS or FUSE
/// mount, a stuck device), which can last indefinitely, so waiting longer
/// would only hold the probe's result past its deadline. Too short only
/// starts a reaper thread that isn't needed.
pub(super) const REAP_WAIT: Duration = Duration::from_millis(250);

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
    pub(super) fn drain(&mut self, mut sink: impl FnMut(&[u8])) {
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
/// reaped, by its probe thread or, if it doesn't exit promptly on SIGKILL, by
/// a reaper thread (`reap_killed`). The lock is never held across a spawn
/// (fork and exec, which can stall on a hung mount) or a wait on a child or its
/// output, so one probe's spawn never waits for another's, and `kill_all`
/// waits at most SPAWN_WAIT, for spawns in flight.
pub(super) struct Children {
    groups: Mutex<Groups>,
    /// Signalled at the end of each spawn, for `kill_all` to stop waiting.
    spawn_done: Condvar,
    /// Test hook: reaps held back, by pid, as if SIGKILL couldn't end the
    /// child, until the paired Sender (`hold_reap`) is dropped.
    #[cfg(test)]
    held: Mutex<Vec<(libc::pid_t, std::sync::mpsc::Receiver<()>)>>,
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
        Self {
            groups: Mutex::new(Groups { live: Vec::new(), closed: false, spawning: 0 }),
            spawn_done: Condvar::new(),
            #[cfg(test)]
            held: Mutex::new(Vec::new()),
        }
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

    /// Reap `child`, whose group was just sent SIGKILL, and return its status,
    /// if it exits within REAP_WAIT. SIGKILL can't end a leader in
    /// uninterruptible sleep (a hung mount) until its kernel call returns, so
    /// one that hasn't exited by then is left to a reaper thread, which waits
    /// for its exit without a bound and reaps it through `reap`, and None is
    /// returned. Its group stays recorded until then, so `kill_all` still
    /// signals it, and its pid, held by the unreaped leader, can't be reused
    /// while recorded. If the thread can't be started, the leader is never
    /// reaped and its group stays recorded: it holds its pid until we exit.
    pub(super) fn reap_killed(&'static self, mut child: Child) -> Option<ExitStatus> {
        let end = Instant::now() + REAP_WAIT;
        // Failing to check on it counts as exited: `wait` then fails at once
        // (no such child) rather than blocking.
        while !(has_exited(&child).unwrap_or(true) && !self.is_held(&child)) {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = std::thread::Builder::new().name("reaper".into()).spawn(move || {
                    #[cfg(test)]
                    self.wait_until_released(&child);
                    wait_exited(&child);
                    let _ = self.reap(&mut child);
                });
                return None;
            }
            std::thread::sleep(left.min(Duration::from_millis(10)));
        }
        self.reap(&mut child).ok()
    }

    /// Refuse new children, wait up to SPAWN_WAIT for spawns in flight to
    /// record theirs, then SIGKILL every recorded group. A spawn that ends
    /// after the wait kills its own child (see `spawn`). Doesn't wait for
    /// anything to exit: the probe and reaper threads (or init, once we exit)
    /// reap them.
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

    /// Test hook: hold back the reap of child `pid` as if SIGKILL couldn't end
    /// it, until the returned Sender is dropped.
    #[cfg(test)]
    pub(super) fn hold_reap(&self, pid: u32) -> std::sync::mpsc::Sender<()> {
        let (release, held) = std::sync::mpsc::channel();
        self.held.lock().unwrap_or_else(PoisonError::into_inner).push((pid as libc::pid_t, held));
        release
    }

    /// Test hook: whether the reap of `child` is held back.
    #[cfg(test)]
    fn is_held(&self, child: &Child) -> bool {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.iter().any(|(pid, held)| {
            *pid == child.id() as libc::pid_t && matches!(held.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty))
        })
    }

    #[cfg(not(test))]
    fn is_held(&self, _child: &Child) -> bool {
        false
    }

    /// Test hook: wait until the reap of `child` is no longer held back.
    #[cfg(test)]
    fn wait_until_released(&self, child: &Child) {
        let held = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(i) = held.iter().position(|(pid, _)| *pid == child.id() as libc::pid_t) else { return };
            held.swap_remove(i).1
        };
        // Not under the lock: other reaps and holds go on meanwhile.
        let _ = held.recv();
    }
}

/// The last OUTPUT_CAP bytes of stdout of `cmd`, or None if it could not be
/// started. Returns by `deadline`, plus up to REAP_WAIT to reap the killed
/// child (`Children::reap_killed`). Before it returns, the child's process
/// group is killed: the child itself if it is
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
fn run_bounded_with(children: &'static Children, cmd: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
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
    // `children.reap_killed` below reaps the child (has_exited uses WNOWAIT),
    // so its pid, and with it the group id, cannot have been reused yet.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    // Killed holders release stdout at once.
    output.settle(deadline);
    let tail = output.into_tail();
    children.reap_killed(child);
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

/// Block until `child` has exited, without reaping it: `has_exited` without
/// WNOHANG. Returns early only if it can't wait on `child` at all.
fn wait_exited(child: &Child) {
    loop {
        // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill in.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable siginfo_t; WNOWAIT leaves the
        // child to be reaped by `Child::wait`.
        let rc = unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;

    use crate::probe::tests::{is_our_zombie, live_with, promptly, sleep_token};
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

    /// `script` run bounded with `budget` on another thread, its group
    /// recorded in `children` and its reap held from when it is recorded.
    /// Returns the group, the hold, the output's receiver and when it started.
    fn run_held(
        children: &'static Children,
        script: &str,
        budget: Duration,
    ) -> (libc::pid_t, mpsc::Sender<()>, mpsc::Receiver<Option<Vec<u8>>>, Instant) {
        let (tx, rx) = mpsc::channel();
        let mut cmd = sh(script);
        let start = Instant::now();
        std::thread::spawn(move || {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let _ = tx.send(run_bounded_with(children, &mut cmd, Instant::now() + budget));
        });
        let group = recorded_group(children);
        let release = children.hold_reap(group as u32);
        (group, release, rx, start)
    }

    /// A child SIGKILL can't end (held as if in uninterruptible sleep) holds
    /// run_bounded only REAP_WAIT past its deadline. Its group stays recorded,
    /// its leader unreaped, until a reaper thread reaps it once it exits.
    #[test]
    fn run_bounded_returns_by_its_deadline_when_its_child_cannot_be_reaped() {
        static CHILDREN: Children = Children::new();
        let budget = Duration::from_millis(1500);
        let (group, release, done, start) = run_held(&CHILDREN, "echo started; exec sleep 30", budget);
        let out =
            done.recv_timeout(budget + REAP_WAIT + Duration::from_secs(3)).expect("the bounded run never returned");
        let elapsed = start.elapsed();
        assert_eq!(
            out.as_deref(),
            Some(
                &b"started
"[..]
            )
        );
        assert!(elapsed >= budget + REAP_WAIT, "gave up on the reap after {elapsed:?}");
        assert!(elapsed < budget + REAP_WAIT + Duration::from_secs(1), "took {elapsed:?}");
        // Checked before the release, asserted after: a failed check must not
        // leave the reaper waiting.
        let recorded = CHILDREN.contains(group as u32);
        let unreaped = is_our_zombie(group);
        drop(release);
        assert!(recorded, "group {group} was forgotten before its leader was reaped");
        assert!(unreaped, "leader {group} was reaped while held, or never killed");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
        assert!(wait_until(40, || !is_our_zombie(group)), "the reaper never reaped {group}");
    }

    /// A quit while the reaper waits still signals the group: here a process
    /// that joined it after run_bounded's SIGKILL.
    #[test]
    fn kill_all_signals_a_group_whose_reap_is_held() {
        use std::os::unix::process::ExitStatusExt;
        static CHILDREN: Children = Children::new();
        let budget = Duration::from_millis(1500);
        let (group, release, done, _) = run_held(&CHILDREN, "exec sleep 30", budget);
        done.recv_timeout(budget + REAP_WAIT + Duration::from_secs(3)).expect("the bounded run never returned");
        assert!(group > 1, "bad group {group}");
        // The leader is a zombie, but its group lives on until it is reaped.
        let joined = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            Command::new("sleep")
                .arg("30")
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .process_group(group)
                .spawn()
        };
        let mut joined = match joined {
            Ok(joined) => joined,
            Err(e) => {
                drop(release);
                panic!("cannot join group {group}: {e}");
            }
        };
        CHILDREN.kill_all();
        let mut status = None;
        let signalled = wait_until(40, || {
            status = joined.try_wait().ok().flatten();
            status.is_some()
        });
        if !signalled {
            let _ = joined.kill();
        }
        let _ = joined.wait();
        drop(release);
        assert!(signalled, "kill_all did not signal held group {group}");
        assert_eq!(status.and_then(|s| s.signal()), Some(libc::SIGKILL));
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
    }

    /// A child that exits promptly is reaped before run_bounded returns,
    /// whether it ended by itself or by the SIGKILL at its deadline.
    #[test]
    fn run_bounded_reaps_a_child_that_exits_before_it_returns() {
        static CHILDREN: Children = Children::new();
        for (script, budget) in
            [("echo $$", Duration::from_secs(5)), ("echo $$; exec sleep 30", Duration::from_millis(300))]
        {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let start = Instant::now();
            let out = run_bounded_with(&CHILDREN, &mut sh(script), start + budget).expect("not started");
            let elapsed = start.elapsed();
            let pid: libc::pid_t = String::from_utf8_lossy(&out).trim().parse().expect("no pid printed");
            assert!(!is_our_zombie(pid), "{script:?}: {pid} was left unreaped");
            assert!(!CHILDREN.contains(pid as u32), "{script:?}: {pid} is still recorded");
            assert!(elapsed < budget + REAP_WAIT, "{script:?}: took {elapsed:?}");
        }
    }

    /// The reap of a held child waits out REAP_WAIT and returns None; once
    /// released, the reaper thread reaps it with nothing more to do.
    #[test]
    fn reap_killed_leaves_a_held_child_to_a_reaper_thread() {
        static CHILDREN: Children = Children::new();
        let mut cmd = sh("exec sleep 30");
        cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
        let child = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            CHILDREN.spawn(&mut cmd).expect("cannot spawn")
        };
        let group = child.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        let release = CHILDREN.hold_reap(child.id());
        // SAFETY: killpg only sends a signal. `group` leads the group
        // process_group(0) created, and nothing reaps it before reap_killed.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        // On another thread: a reap that blocks fails the test instead of
        // hanging it, and the release, dropped as the test unwinds, frees it.
        let (status, elapsed) = promptly(move || {
            let start = Instant::now();
            (CHILDREN.reap_killed(child), start.elapsed())
        });
        let recorded = CHILDREN.contains(group as u32);
        drop(release);
        assert_eq!(status, None, "a held child was reaped");
        assert!(elapsed >= REAP_WAIT, "gave up after {elapsed:?}");
        assert!(elapsed < REAP_WAIT + Duration::from_millis(500), "took {elapsed:?}");
        assert!(recorded, "group {group} was forgotten before its leader was reaped");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
        assert!(wait_until(40, || !is_our_zombie(group)), "the reaper never reaped {group}");
    }

    /// A child that exits promptly on SIGKILL is reaped at once, its status
    /// returned and its group forgotten.
    #[test]
    fn reap_killed_reaps_a_killed_child_at_once() {
        use std::os::unix::process::ExitStatusExt;
        static CHILDREN: Children = Children::new();
        let mut cmd = sh("exec sleep 30");
        cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
        let child = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            CHILDREN.spawn(&mut cmd).expect("cannot spawn")
        };
        let group = child.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        // SAFETY: as in reap_killed_leaves_a_held_child_to_a_reaper_thread.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        let start = Instant::now();
        let status = CHILDREN.reap_killed(child);
        let elapsed = start.elapsed();
        assert_eq!(status.and_then(|s| s.signal()), Some(libc::SIGKILL));
        assert!(elapsed < REAP_WAIT, "took {elapsed:?}");
        assert!(!CHILDREN.contains(group as u32));
        assert!(!is_our_zombie(group));
    }

    /// The reaper's wait leaves the exited child unreaped, so `reap` can still
    /// forget its group before its pid is freed.
    #[test]
    fn wait_exited_returns_once_the_child_exits_without_reaping_it() {
        use std::os::unix::process::ExitStatusExt;
        let child = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            sh("exec sleep 30")
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .expect("cannot spawn")
        };
        let pid = child.id() as libc::pid_t;
        assert!(pid > 1, "bad pid {pid}");
        let (tx, rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || {
            wait_exited(&child);
            let _ = tx.send(());
            child
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "wait_exited returned while the child ran");
        // SAFETY: kill only sends a signal. `pid` is our child, which only
        // the wait below reaps, so the id is still its own.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        rx.recv_timeout(Duration::from_secs(5)).expect("wait_exited never returned after the kill");
        let mut child = waiting.join().unwrap();
        assert!(is_our_zombie(pid), "wait_exited reaped {pid}");
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
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
