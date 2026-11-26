//! Child processes bounded by a deadline: output read without blocking on the
//! caller's thread and capped, and the whole process group killed before the
//! call returns, so nothing a probe starts outlives it. Every probe child is
//! recorded in CHILDREN while it lives, so quitting the TUI, which doesn't wait
//! for the detached probe threads, kills them too.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
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
/// Longest `reap_killed` waits for a child sent SIGKILL, and the rest of its
/// process group, to exit before it leaves the reap to a reaper thread.
/// SIGKILL ends a process as soon as it next runs, and even a large one is
/// torn down well within this; one still there is stuck in a kernel call that
/// ignores signals (a hung NFS or FUSE mount, a stuck device), which can last
/// indefinitely, so waiting longer would only hold the probe's result past its
/// deadline. Too short only starts a reaper thread that isn't needed.
pub(super) const REAP_WAIT: Duration = Duration::from_millis(250);
/// How often a reaper thread checks whether the group of the exited leader it
/// waits on has emptied: a stuck member gives no signal when it ends.
const GROUP_POLL: Duration = Duration::from_millis(200);

/// The process groups of every live child the probes started.
pub(super) static CHILDREN: Children = Children::new();

#[cfg(test)]
thread_local! {
    /// Test hook: the hold for the next child this thread spawns, and where
    /// to send its pid (`hold_next_reap`).
    static HOLD_NEXT: std::cell::RefCell<Option<(std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<libc::pid_t>)>> =
        const { std::cell::RefCell::new(None) };
}

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
/// waits at most SPAWN_WAIT, for spawns in flight. While a group SIGKILL
/// couldn't empty waits in a reaper thread, no child of its program is spawned
/// (`spawn`), so retries against a cause that persists (a hung mount) don't
/// add one stuck child each.
pub(super) struct Children {
    groups: Mutex<Groups>,
    /// Signalled at the end of each spawn, for `kill_all` to stop waiting.
    spawn_done: Condvar,
    /// Test hook: reaps held back, by pid, as if SIGKILL couldn't end the
    /// child, until the paired Sender (`hold_reap`) is dropped.
    #[cfg(test)]
    held: Mutex<Vec<(libc::pid_t, std::sync::mpsc::Receiver<()>)>>,
    /// Test hook: groups that look as if a member SIGKILL couldn't end were
    /// still in them, until the paired Sender (`hold_group`) is dropped.
    #[cfg(test)]
    held_groups: Mutex<Vec<(libc::pid_t, std::sync::mpsc::Receiver<()>)>>,
}

struct Groups {
    /// Group ids: the pids of children spawned with `process_group(0)` and not
    /// reaped yet.
    live: Vec<libc::pid_t>,
    /// The leader of each group in `live`.
    leaders: Vec<Leader>,
    /// Set by `kill_all`: no child is spawned after it.
    closed: bool,
    /// Spawns in flight: started, but their child not recorded yet.
    spawning: usize,
}

impl Groups {
    /// Whether a leader running `program` was left to a reaper thread and
    /// is not reaped yet.
    fn is_stuck(&self, program: &OsStr) -> bool {
        self.leaders.iter().any(|leader| leader.stuck && leader.program == program)
    }
}

/// A recorded group's leader.
struct Leader {
    group: libc::pid_t,
    /// What it was spawned to run (`Command::get_program`): each probe's
    /// children run their own (the codex app server, the login shell, this
    /// binary for Devin's lookup).
    program: OsString,
    /// `reap_killed` left it to a reaper thread: SIGKILL hasn't ended it, or
    /// another member of its group.
    stuck: bool,
}

impl Children {
    const fn new() -> Self {
        Self {
            groups: Mutex::new(Groups { live: Vec::new(), leaders: Vec::new(), closed: false, spawning: 0 }),
            spawn_done: Condvar::new(),
            #[cfg(test)]
            held: Mutex::new(Vec::new()),
            #[cfg(test)]
            held_groups: Mutex::new(Vec::new()),
        }
    }

    /// Spawn `cmd`, which must set `process_group(0)`, and record its group.
    /// The spawn runs outside the lock, counted in `spawning`; its child is
    /// recorded under the lock, and if `kill_all` has begun by then, its group
    /// is killed there and then. So a child is spawned before `kill_all` and
    /// killed by it, killed by its own spawn, or never spawned. Only a spawn
    /// still stalled in exec when `kill_all` stops waiting (SPAWN_WAIT) and the
    /// process exits can leave a child that outlives the quit. A killed child is
    /// still returned, for the caller to reap through `reap`. Fails without
    /// spawning while an earlier child of the same program is stuck
    /// (`is_stuck`), with ErrorKind::ResourceBusy: whatever kept SIGKILL from
    /// ending it would most likely catch this one too.
    pub(super) fn spawn(&self, cmd: &mut Command) -> std::io::Result<Child> {
        {
            let mut groups = self.lock();
            if groups.closed {
                return Err(std::io::Error::other("quitting: no new child"));
            }
            if groups.is_stuck(cmd.get_program()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "an earlier child of this program is stuck",
                ));
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
        groups.leaders.push(Leader { group, program: cmd.get_program().to_owned(), stuck: false });
        #[cfg(test)]
        if let Some((held, spawned)) = HOLD_NEXT.take() {
            self.held.lock().unwrap_or_else(PoisonError::into_inner).push((group, held));
            let _ = spawned.send(group);
        }
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
        {
            let mut groups = self.lock();
            groups.live.retain(|&group| group != pid);
            groups.leaders.retain(|leader| leader.group != pid);
        }
        child.wait()
    }

    /// Reap `child`, whose group was just sent SIGKILL, and return its status,
    /// if it exits within REAP_WAIT and no other member of its group is left
    /// alive by then. SIGKILL can't end a process in uninterruptible sleep (a
    /// hung mount) until its kernel call returns, and that may be a member the
    /// leader started (the native codex under the npm shim's node, a command
    /// the login config runs) rather than the leader. So a child that hasn't
    /// exited by then, or whose group hasn't emptied, is left to a reaper
    /// thread, which waits without a bound for its exit and then for its group
    /// to empty, and reaps it through `reap`, and None is returned. Its group
    /// stays recorded until then, so `kill_all` still signals it, and its pid,
    /// held by the unreaped leader, can't be reused while recorded, as a pid
    /// or as a group id. Until then the leader counts as stuck, and `spawn`
    /// refuses its program. If the thread can't be started, the leader is never
    /// reaped and its group stays recorded: it holds its pid, and its program
    /// stays refused, until we exit. A descendant that left the group
    /// (`setsid`) is not waited for: nothing can tell it from an unrelated
    /// process. Outside Linux, or with a /proc from another pid namespace, the
    /// group isn't checked, only the leader. The group scan reads all of
    /// /proc, which a loaded host can slow: one begun before REAP_WAIT ends
    /// may still end after it, so this returns by REAP_WAIT plus that scan.
    pub(super) fn reap_killed(&'static self, mut child: Child) -> Option<ExitStatus> {
        let group = child.id() as libc::pid_t;
        let end = Instant::now() + REAP_WAIT;
        loop {
            // Failing to check on it counts as exited: `wait` then fails at
            // once (no such child) rather than blocking. The group is checked
            // only once the leader has exited (until then it is a live member
            // itself), and not once REAP_WAIT is over: the reaper thread
            // checks it then.
            if has_exited(&child).unwrap_or(true)
                && !self.is_held(&child)
                && Instant::now() < end
                && self.group_is_empty(group)
            {
                return self.reap(&mut child).ok();
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Marked before the reaper starts: once it reaps the leader,
                // its pid may be reused by a child this would mark instead.
                if let Some(leader) = self.lock().leaders.iter_mut().find(|leader| leader.group == group) {
                    leader.stuck = true;
                }
                let _ = std::thread::Builder::new().name("reaper".into()).spawn(move || {
                    #[cfg(test)]
                    self.wait_until_released(&child);
                    wait_exited(&child);
                    // The leader is exited but unreaped, so `group` is still
                    // its group: no other can take the id.
                    while !self.group_is_empty(group) {
                        std::thread::sleep(GROUP_POLL);
                    }
                    let _ = self.reap(&mut child);
                });
                return None;
            }
            std::thread::sleep(left.min(Duration::from_millis(10)));
        }
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

    /// Whether a child of `program` was left to a reaper thread, SIGKILL not
    /// having ended it, and is not reaped yet: `spawn` refuses `program`.
    pub(super) fn is_stuck(&self, program: &OsStr) -> bool {
        self.lock().is_stuck(program)
    }

    /// The lock, even if a thread panicked holding it: `live`, `leaders`,
    /// `closed` and `spawning` are each changed in one step (`live` and
    /// `leaders` together), and quitting must still kill.
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
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        // Released holds go: their pids may be reused.
        held.retain(|(_, held)| matches!(held.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        held.iter().any(|(pid, _)| *pid == child.id() as libc::pid_t)
    }

    #[cfg(not(test))]
    fn is_held(&self, _child: &Child) -> bool {
        false
    }

    /// Whether no process is left alive in `group`, whose leader has exited
    /// unreaped (zombies don't count).
    fn group_is_empty(&self, group: libc::pid_t) -> bool {
        !group_has_live_member(Path::new("/proc"), group) && !self.is_group_held(group)
    }

    /// Test hook: make `group` look as if a member SIGKILL couldn't end were
    /// still in it, until the returned Sender is dropped.
    #[cfg(test)]
    pub(super) fn hold_group(&self, group: u32) -> std::sync::mpsc::Sender<()> {
        let (release, held) = std::sync::mpsc::channel();
        self.held_groups.lock().unwrap_or_else(PoisonError::into_inner).push((group as libc::pid_t, held));
        release
    }

    /// Test hook: whether `group` is held as not empty.
    #[cfg(test)]
    fn is_group_held(&self, group: libc::pid_t) -> bool {
        let mut held = self.held_groups.lock().unwrap_or_else(PoisonError::into_inner);
        // Released holds go: their group ids may be reused.
        held.retain(|(_, held)| matches!(held.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        held.iter().any(|(id, _)| *id == group)
    }

    #[cfg(not(test))]
    fn is_group_held(&self, _group: libc::pid_t) -> bool {
        false
    }

    /// Test hook: wait until the reap of `child` is no longer held back.
    #[cfg(test)]
    fn wait_until_released(&self, child: &Child) {
        let held = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            // Released holds go, as in `is_held`: one left for a reused pid
            // must not stand in for this child's live hold.
            held.retain(|(_, held)| matches!(held.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
            let Some(i) = held.iter().position(|(pid, _)| *pid == child.id() as libc::pid_t) else { return };
            held.swap_remove(i).1
        };
        // Not under the lock: other reaps and holds go on meanwhile.
        let _ = held.recv();
    }
}

/// What `run_bounded_timed` got from a child.
pub(super) struct BoundedRun {
    /// The last OUTPUT_CAP bytes of its stdout.
    pub(super) stdout: Vec<u8>,
    /// The deadline came before its output was read to the end: the child was
    /// still running then and was killed, or had only just exited. So
    /// `stdout` may lack what it would have printed last.
    pub(super) timed_out: bool,
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

/// `run_bounded`, also telling whether the deadline cut the child short
/// (`BoundedRun::timed_out`), so a caller can tell that from a child that
/// finished without printing what it looks for.
pub(super) fn run_bounded_timed(cmd: &mut Command, deadline: Instant) -> Option<BoundedRun> {
    run_bounded_timed_with(&CHILDREN, cmd, deadline)
}

/// Test hook: `Children::hold_reap` for the next child this thread spawns,
/// through whichever `Children` spawns it, held from before `spawn` returns
/// it, so it is held even if it is killed before its pid could be read.
/// Returns the release and the receiver of that child's pid.
#[cfg(test)]
pub(super) fn hold_next_reap() -> (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<libc::pid_t>) {
    let (release, held) = std::sync::mpsc::channel();
    let (spawned, pid) = std::sync::mpsc::channel();
    HOLD_NEXT.set(Some((held, spawned)));
    (release, pid)
}

/// `run_bounded`, recording the child in `children`.
fn run_bounded_with(children: &'static Children, cmd: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    run_bounded_timed_with(children, cmd, deadline).map(|run| run.stdout)
}

/// `run_bounded_timed`, recording the child in `children`.
fn run_bounded_timed_with(children: &'static Children, cmd: &mut Command, deadline: Instant) -> Option<BoundedRun> {
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
    // Taken before the reap, which may take REAP_WAIT more: a child that
    // finished on time is not reported cut short because its reap was slow.
    let timed_out = Instant::now() >= deadline;
    let stdout = output.into_tail();
    children.reap_killed(child);
    Some(BoundedRun { stdout, timed_out })
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

/// Whether a process in process group `group` is alive, as procfs mounted at
/// `proc` shows it: one with a thread that isn't a zombie, so a zombie leader
/// whose other threads are stuck counts as alive. A procfs of another pid
/// namespace (`unshare --pid` without remounting /proc) numbers processes
/// differently: our children aren't found in it, and a process there whose
/// group id happens to equal our child's pid is not its member, so none is
/// seen, and only the leader is waited for. A process getpgid places in
/// another group is passed over (`in_other_group`); for the rest it reads
/// only `stat` files, which don't block on a stuck process (`cmdline` can).
/// What they hold is parsed without trusting it: a process name may hold
/// anything, and an entry that can't be read or parsed (gone meanwhile)
/// counts as not a member.
#[cfg(target_os = "linux")]
fn group_has_live_member(proc: &Path, group: libc::pid_t) -> bool {
    if !is_our_procfs(proc) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(proc) else { return false };
    entries.filter_map(Result::ok).any(|entry| {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
        else {
            return false;
        };
        if in_other_group(pid, group) {
            return false;
        }
        let path = entry.path();
        match stat_state_and_group(&path.join("stat")) {
            Some((state, pgrp)) if pgrp == group => is_live_state(&state) || has_live_thread(&path),
            _ => false,
        }
    })
}

/// Whether procfs at `proc` numbers processes as we do: its `self` names our
/// pid.
#[cfg(target_os = "linux")]
fn is_our_procfs(proc: &Path) -> bool {
    std::fs::read_link(proc.join("self"))
        .is_ok_and(|pid| pid.to_str().and_then(|pid| pid.parse::<u32>().ok()) == Some(std::process::id()))
}

/// Whether process `pid` (a /proc entry's name) is in a process group other
/// than `group`, as getpgid tells: one call, where reading its `stat` makes
/// the kernel build the whole file, so a scan that reads every process's
/// takes about ten times as long, and on a loaded host can outlast
/// REAP_WAIT. Only called for a procfs of our pid namespace, whose pids are
/// the ones getpgid takes. A pid it can't place (gone, never ours, or out
/// of range) is not known to be elsewhere: its `stat` decides.
#[cfg(target_os = "linux")]
fn in_other_group(pid: &str, group: libc::pid_t) -> bool {
    // Never 0: getpgid(0) is our own group.
    let Some(pid) = pid.parse::<libc::pid_t>().ok().filter(|&pid| pid > 0) else { return false };
    // SAFETY: getpgid only reads the process group id of `pid`.
    let pgid = unsafe { libc::getpgid(pid) };
    pgid >= 0 && pgid != group
}

/// Outside Linux there is no procfs to read: no member is seen, and only the
/// leader is waited for.
#[cfg(not(target_os = "linux"))]
fn group_has_live_member(_proc: &Path, _group: libc::pid_t) -> bool {
    false
}

/// The state and process group in a `/proc/<pid>/stat` (or task) file:
/// "pid (comm) state ppid pgrp ...". The comm may hold anything, `)` and
/// spaces included, so fields are counted after its last `)`.
#[cfg(target_os = "linux")]
fn stat_state_and_group(path: &Path) -> Option<(String, libc::pid_t)> {
    let stat = std::fs::read_to_string(path).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?.to_owned();
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((state, pgrp))
}

/// A state other than zombie (Z) or dead (X, briefly, while being reaped).
#[cfg(target_os = "linux")]
fn is_live_state(state: &str) -> bool {
    !matches!(state, "Z" | "X")
}

/// Whether a thread of the process at `path` (`/proc/<pid>`) is alive.
#[cfg(target_os = "linux")]
fn has_live_thread(path: &Path) -> bool {
    let Ok(tasks) = std::fs::read_dir(path.join("task")) else { return false };
    tasks
        .filter_map(Result::ok)
        .any(|task| stat_state_and_group(&task.path().join("stat")).is_some_and(|(state, _)| is_live_state(&state)))
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
    /// recorded in `children` and its reap held from its spawn on: a busy
    /// machine can pass the deadline, and kill and reap the child, before this
    /// thread could read its group. Returns the group, the hold, the output's
    /// receiver and when the run started.
    fn run_held(
        children: &'static Children,
        script: &str,
        budget: Duration,
    ) -> (libc::pid_t, mpsc::Sender<()>, mpsc::Receiver<Option<Vec<u8>>>, Instant) {
        let (tx, rx) = mpsc::channel();
        let (hold_tx, hold_rx) = mpsc::channel();
        let mut cmd = sh(script);
        std::thread::spawn(move || {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            // The start is taken here, not before the thread runs: how late a
            // busy machine runs it is no part of how long the run takes.
            let start = Instant::now();
            let _ = hold_tx.send((hold_next_reap(), start));
            let _ = tx.send(run_bounded_with(children, &mut cmd, start + budget));
        });
        let ((release, spawned), start) =
            hold_rx.recv_timeout(Duration::from_secs(10)).expect("the bounded run never started");
        let group = spawned.recv_timeout(Duration::from_secs(10)).expect("the bounded run spawned no child");
        assert!(group > 1, "bad group {group}");
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
        // Exited, however long a busy machine takes SIGKILL to end it, and
        // still unreaped while held.
        let unreaped = wait_until(200, || is_our_zombie(group));
        drop(release);
        assert!(recorded, "group {group} was forgotten before its leader was reaped");
        assert!(unreaped, "leader {group} was reaped while held, or never killed");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
        assert!(wait_until(40, || !is_our_zombie(group)), "the reaper never reaped {group}");
    }

    /// A child killed at once, before its group could be read on another
    /// thread, is still held from its spawn: run_bounded gives up on its reap
    /// after REAP_WAIT, and its group stays recorded until it is released.
    #[test]
    fn hold_next_reap_holds_a_child_killed_before_its_group_is_read() {
        static CHILDREN: Children = Children::new();
        let (group, release, done, start) = run_held(&CHILDREN, "exec sleep 30", Duration::ZERO);
        let out = done.recv_timeout(REAP_WAIT + Duration::from_secs(5)).expect("the bounded run never returned");
        let elapsed = start.elapsed();
        // Checked before the release, asserted after: a failed check must not
        // leave the reaper waiting.
        let recorded = CHILDREN.contains(group as u32);
        // Exited, however long a busy machine takes SIGKILL to end it, and
        // still unreaped while held.
        let unreaped = wait_until(200, || is_our_zombie(group));
        drop(release);
        assert_eq!(out.as_deref(), Some(&b""[..]));
        assert!(elapsed >= REAP_WAIT, "gave up on the reap after {elapsed:?}");
        assert!(recorded, "group {group} was forgotten before its leader was reaped");
        assert!(unreaped, "leader {group} was reaped while held, or never killed");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
        assert!(wait_until(40, || !is_our_zombie(group)), "the reaper never reaped {group}");
    }

    /// `hold_next_reap` holds the next child its thread spawns, and only
    /// that one: not the one after it, nor one another thread spawns.
    #[test]
    fn hold_next_reap_holds_only_the_next_child_of_its_own_thread() {
        static CHILDREN: Children = Children::new();
        let spawn = || {
            let mut cmd = sh("exec sleep 30");
            cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            CHILDREN.spawn(&mut cmd).expect("cannot spawn")
        };
        let (release, spawned) = hold_next_reap();
        let other = std::thread::spawn(spawn).join().unwrap();
        let next = spawn();
        let next_group = next.id() as libc::pid_t;
        let after = spawn();
        let held = [&other, &next, &after].map(|child| CHILDREN.is_held(child));
        let sent = spawned.try_recv().ok();
        let ended = spawned.try_recv();
        drop(release);
        for mut child in [other, next, after] {
            let group = child.id() as libc::pid_t;
            assert!(group > 1, "bad group {group}");
            // SAFETY: killpg only sends a signal. `group` leads the group
            // process_group(0) created, and only the reap below reaps it.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
            let _ = CHILDREN.reap(&mut child);
        }
        assert_eq!(held, [false, true, false], "held (other thread, next, after)");
        assert_eq!(sent, Some(next_group), "the held child's group was not sent");
        assert!(ended.is_err(), "a second group was sent");
        assert!(CHILDREN.lock().live.is_empty());
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
    /// whether it ended by itself or by the SIGKILL at its deadline. How
    /// promptly SIGKILL ends it is up to the machine: one a busy machine takes
    /// longer than REAP_WAIT to end is left to a reaper thread, which must
    /// reap it and forget its group.
    #[test]
    fn run_bounded_reaps_a_child_that_exits_before_it_returns() {
        static CHILDREN: Children = Children::new();
        for (script, budget, killed) in
            [("echo $$", Duration::from_secs(5), false), ("echo $$; exec sleep 30", Duration::from_millis(300), true)]
        {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let start = Instant::now();
            let out = run_bounded_with(&CHILDREN, &mut sh(script), start + budget).expect("not started");
            let elapsed = start.elapsed();
            let pid: libc::pid_t = String::from_utf8_lossy(&out).trim().parse().expect("no pid printed");
            // The reap is left to a reaper thread only once REAP_WAIT has
            // passed after the kill, which comes at the deadline at the
            // earliest: a run back sooner reaped the child itself. One that
            // ended by itself is reaped before the call, whenever it returns.
            if !killed || elapsed < budget + REAP_WAIT {
                assert!(!is_our_zombie(pid), "{script:?}: {pid} was left unreaped");
                assert!(!CHILDREN.contains(pid as u32), "{script:?}: {pid} is still recorded");
                assert!(elapsed < budget + REAP_WAIT, "{script:?}: took {elapsed:?}");
            } else {
                assert!(elapsed < budget + REAP_WAIT + Duration::from_secs(1), "{script:?}: took {elapsed:?}");
                assert!(
                    wait_until(200, || !is_our_zombie(pid) && !CHILDREN.contains(pid as u32)),
                    "{script:?}: {pid} was never reaped and forgotten"
                );
            }
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

    /// A child that SIGKILL has ended is reaped at once, its status returned
    /// and its group forgotten.
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
        // However long a busy machine takes to end it, it has exited before
        // the call, so the reap has nothing to wait for.
        assert!(wait_until(200, || has_exited(&child).unwrap_or(false)), "{group} never exited on SIGKILL");
        let start = Instant::now();
        let status = CHILDREN.reap_killed(child);
        let elapsed = start.elapsed();
        assert_eq!(status.and_then(|s| s.signal()), Some(libc::SIGKILL));
        assert!(elapsed < REAP_WAIT, "took {elapsed:?}");
        assert!(!CHILDREN.contains(group as u32));
        assert!(!is_our_zombie(group));
    }

    /// A child that can be reaped partway through REAP_WAIT (here: its hold is
    /// released then) is noticed within a poll and reaped, not left to the end
    /// of the wait. The hold makes the timing the test's own: the child has
    /// exited before the call, and only the release, at a time the test
    /// records, lets it go. When reap_killed noticed is when its check dropped
    /// the released hold (`is_held`): the group scan that follows reads all
    /// of /proc, which a loaded host can slow past REAP_WAIT, so the return
    /// time can't show it.
    #[test]
    fn reap_killed_reaps_a_child_freed_during_its_wait_promptly() {
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
        let release = CHILDREN.hold_reap(child.id());
        // SAFETY: killpg only sends a signal. `group` leads the group
        // process_group(0) created, and nothing reaps it before reap_killed.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        assert!(wait_until(200, || is_our_zombie(group)), "leader {group} never died");
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = started_tx.send(Instant::now());
            let status = CHILDREN.reap_killed(child);
            let _ = done_tx.send((status, Instant::now()));
        });
        let start = started_rx.recv_timeout(Duration::from_secs(5)).expect("the reap never started");
        // Well inside the wait, which ends REAP_WAIT after the call at the
        // earliest, and past its first check.
        std::thread::sleep((start + REAP_WAIT / 5).saturating_duration_since(Instant::now()));
        drop(release);
        let released = Instant::now();
        let held = || CHILDREN.held.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|(pid, _)| *pid == group);
        while held() && released.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let noticed = released.elapsed();
        let (status, _) = done_rx.recv_timeout(Duration::from_secs(10)).expect("the reap never returned");
        assert!(released < start + REAP_WAIT, "released {:?} into the wait", released - start);
        assert!(noticed < Duration::from_millis(100), "the release was noticed {noticed:?} after it");
        assert_eq!(status.and_then(|s| s.signal()), Some(libc::SIGKILL), "not reaped once released");
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

    /// A child of `cmd` recorded in `children` and left stuck: sent SIGKILL
    /// with its reap held, as if in uninterruptible sleep, so reap_killed
    /// leaves it to a reaper thread. Returns its group and the hold.
    fn stuck_child(children: &'static Children, cmd: &mut Command) -> (libc::pid_t, mpsc::Sender<()>) {
        cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
        let child = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            children.spawn(cmd).expect("cannot spawn")
        };
        let group = child.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        let release = children.hold_reap(child.id());
        // SAFETY: killpg only sends a signal. `group` leads the group
        // process_group(0) created, and nothing reaps it before reap_killed.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        // On another thread: a reap that blocks fails the test instead of
        // hanging it, and the release, dropped as the test unwinds, frees it.
        let status = promptly(move || children.reap_killed(child));
        assert_eq!(status, None, "a held child was reaped");
        (group, release)
    }

    /// `cmd` run bounded (5 s) and recorded in `children`.
    fn run_briefly(children: &'static Children, cmd: &mut Command) -> Option<Vec<u8>> {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        run_bounded_with(children, cmd, Instant::now() + Duration::from_secs(5))
    }

    /// Issue #40: while a child SIGKILL couldn't end waits in a reaper thread,
    /// its program isn't started again, so retries can't pile up stuck
    /// children; another program still starts, and once the stuck child is
    /// reaped, its program starts again.
    #[test]
    fn a_program_is_not_spawned_again_while_its_earlier_child_is_stuck() {
        static CHILDREN: Children = Children::new();
        let budget = Duration::from_millis(500);
        let (group, release, done, _) = run_held(&CHILDREN, "exec sleep 30", budget);
        let first =
            done.recv_timeout(budget + REAP_WAIT + Duration::from_secs(3)).expect("the bounded run never returned");
        let (refused, elapsed) = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            let start = Instant::now();
            (run_bounded_with(&CHILDREN, &mut sh("echo ran"), start + Duration::from_secs(5)), start.elapsed())
        };
        let (live, spawning) = {
            let groups = CHILDREN.lock();
            (groups.live.clone(), groups.spawning)
        };
        let other = run_briefly(&CHILDREN, Command::new("echo").arg("other"));
        drop(release);
        assert_eq!(first, Some(Vec::new()));
        assert_eq!(refused, None, "a second /bin/sh child was spawned while the first was stuck");
        assert!(elapsed < Duration::from_millis(500), "the refusal took {elapsed:?}");
        assert_eq!(live, [group], "the refused spawn changed what is recorded");
        assert_eq!(spawning, 0, "the refused spawn is still counted in flight");
        assert_eq!(other.as_deref(), Some(&b"other\n"[..]), "another program was held up");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
        assert_eq!(run_briefly(&CHILDREN, &mut sh("echo ran")).as_deref(), Some(&b"ran\n"[..]));
        assert!(CHILDREN.lock().live.is_empty());
    }

    /// A stuck child counts as stuck exactly while its group is recorded.
    #[test]
    fn a_child_left_to_the_reaper_is_stuck_until_it_is_reaped() {
        static CHILDREN: Children = Children::new();
        let sh_program = OsStr::new("/bin/sh");
        assert!(!CHILDREN.is_stuck(sh_program));
        let (group, release) = stuck_child(&CHILDREN, &mut sh("exec sleep 30"));
        let stuck = CHILDREN.is_stuck(sh_program);
        let recorded = CHILDREN.contains(group as u32);
        // Exited, however long a busy machine takes SIGKILL to end it, and
        // still unreaped while held.
        let unreaped = wait_until(200, || is_our_zombie(group));
        drop(release);
        assert!(stuck, "a child left to the reaper is not stuck");
        assert!(recorded, "group {group} was forgotten before its leader was reaped");
        assert!(unreaped, "leader {group} was reaped while held, or never killed");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once released");
        assert!(!CHILDREN.is_stuck(sh_program), "still stuck once reaped");
        assert!(CHILDREN.lock().leaders.is_empty());
    }

    /// Each stuck program is refused and freed on its own.
    #[test]
    fn stuck_programs_are_refused_and_freed_each_on_its_own() {
        static CHILDREN: Children = Children::new();
        let sleep_program = OsStr::new("sleep");
        let sh_program = OsStr::new("/bin/sh");
        let (sh_group, sh_release) = stuck_child(&CHILDREN, &mut sh("exec sleep 30"));
        let (sleep_group, sleep_release) = stuck_child(&CHILDREN, Command::new("sleep").arg("30"));
        assert!(CHILDREN.is_stuck(sh_program) && CHILDREN.is_stuck(sleep_program));
        assert_eq!(run_briefly(&CHILDREN, Command::new("sleep").arg("0")), None);
        drop(sleep_release);
        assert!(
            wait_until(40, || !CHILDREN.contains(sleep_group as u32)),
            "group {sleep_group} is still recorded once released"
        );
        assert!(!CHILDREN.is_stuck(sleep_program), "sleep is still stuck once reaped");
        assert!(CHILDREN.is_stuck(sh_program), "freeing sleep freed /bin/sh too");
        assert_eq!(run_briefly(&CHILDREN, Command::new("sleep").arg("0")), Some(Vec::new()));
        assert_eq!(run_briefly(&CHILDREN, &mut sh("echo ran")), None);
        drop(sh_release);
        assert!(
            wait_until(40, || !CHILDREN.contains(sh_group as u32)),
            "group {sh_group} is still recorded once released"
        );
        assert!(!CHILDREN.is_stuck(sh_program));
        assert_eq!(run_briefly(&CHILDREN, &mut sh("echo ran")).as_deref(), Some(&b"ran\n"[..]));
    }

    /// Only a child left to the reaper is stuck: one that ends by itself or
    /// on the SIGKILL at its deadline leaves its program free.
    #[test]
    fn a_child_reaped_before_its_run_returns_leaves_its_program_free() {
        static CHILDREN: Children = Children::new();
        for (script, budget) in [("echo $$", Duration::from_secs(5)), ("exec sleep 30", Duration::from_millis(300))] {
            let out = {
                let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
                run_bounded_with(&CHILDREN, &mut sh(script), Instant::now() + budget)
            };
            assert!(out.is_some(), "{script:?}: not started");
            assert!(!CHILDREN.is_stuck(OsStr::new("/bin/sh")), "{script:?}: left /bin/sh stuck");
            assert!(CHILDREN.lock().leaders.is_empty(), "{script:?}: its leader is still recorded");
            assert_eq!(run_briefly(&CHILDREN, &mut sh("echo ran")).as_deref(), Some(&b"ran\n"[..]), "{script:?}");
        }
    }

    /// A child spawned outside `children` (kill_tree's callers may pass one)
    /// marks no program stuck when its reap is left to a reaper thread.
    #[test]
    fn reap_killed_marks_no_program_for_a_child_it_did_not_record() {
        static CHILDREN: Children = Children::new();
        let child = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            sh("exec sleep 30")
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("cannot spawn")
        };
        let group = child.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        let release = CHILDREN.hold_reap(child.id());
        // SAFETY: as in stuck_child.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        let status = promptly(move || CHILDREN.reap_killed(child));
        let stuck = CHILDREN.is_stuck(OsStr::new("/bin/sh"));
        let ran = run_briefly(&CHILDREN, &mut sh("echo ran"));
        drop(release);
        assert_eq!(status, None, "a held child was reaped");
        assert!(!stuck, "an unrecorded child marked /bin/sh stuck");
        assert_eq!(ran.as_deref(), Some(&b"ran\n"[..]));
        assert!(wait_until(40, || !is_our_zombie(group)), "the reaper never reaped {group}");
    }

    /// `sleep 30` joined to `group`, whose leader may already be dead: the
    /// group lives while its unreaped leader holds the id.
    fn join_group(group: libc::pid_t) -> Child {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        Command::new("sleep")
            .arg("30")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .process_group(group)
            .spawn()
            .expect("cannot join the group")
    }

    /// Review of #40: a leader that dies on SIGKILL at once doesn't make its
    /// group done while a member lives on (as the native codex under the npm
    /// shim's node, stuck on a hung mount, would): the reap is left to a
    /// reaper thread, the program counts as stuck, and only once the group
    /// has emptied is the leader reaped and its group forgotten.
    #[test]
    fn reap_killed_waits_for_the_group_to_empty_not_just_the_leader() {
        static CHILDREN: Children = Children::new();
        let mut cmd = sh("exec sleep 30");
        cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
        let child = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            CHILDREN.spawn(&mut cmd).expect("cannot spawn")
        };
        let group = child.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        // SAFETY: killpg only sends a signal. `group` leads the group
        // process_group(0) created, and nothing reaps it before reap_killed.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        assert!(wait_until(40, || is_our_zombie(group)), "leader {group} never died");
        // Joined after the SIGKILL, so it lives on, as a member SIGKILL
        // couldn't end would.
        let mut member = join_group(group);
        let (status, elapsed) = promptly(move || {
            let start = Instant::now();
            (CHILDREN.reap_killed(child), start.elapsed())
        });
        let stuck = CHILDREN.is_stuck(OsStr::new("/bin/sh"));
        let recorded = CHILDREN.contains(group as u32);
        let unreaped = is_our_zombie(group);
        let reaped_with_a_member = wait_until(6, || !CHILDREN.contains(group as u32));
        // The group empties.
        let _ = member.kill();
        let _ = member.wait();
        assert_eq!(status, None, "the leader was reaped while its group had a live member");
        assert!(elapsed >= REAP_WAIT, "gave up after {elapsed:?}");
        assert!(stuck, "a group with a live member left its program free");
        assert!(recorded, "group {group} was forgotten while it had a live member");
        assert!(unreaped, "leader {group} was reaped while its group had a live member");
        assert!(!reaped_with_a_member, "the reaper reaped {group} while its group had a live member");
        assert!(wait_until(40, || !CHILDREN.contains(group as u32)), "group {group} is still recorded once empty");
        assert!(!CHILDREN.is_stuck(OsStr::new("/bin/sh")), "still stuck once reaped");
        assert!(wait_until(40, || !is_our_zombie(group)), "the reaper never reaped {group}");
    }

    /// The group check on the real /proc: a live leader, then a live member
    /// once the leader is a zombie, count; zombies alone don't.
    #[cfg(target_os = "linux")]
    #[test]
    fn group_has_live_member_sees_live_members_and_not_zombies() {
        let proc = Path::new("/proc");
        let mut leader = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            Command::new("sleep")
                .arg("30")
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("cannot spawn")
        };
        let group = leader.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        let with_leader = group_has_live_member(proc, group);
        let mut member = join_group(group);
        let _ = leader.kill();
        let leader_died = wait_until(40, || is_our_zombie(group));
        let with_member = group_has_live_member(proc, group);
        let _ = member.kill();
        let _ = member.wait();
        let with_zombie_leader = group_has_live_member(proc, group);
        let _ = leader.wait();
        assert!(with_leader, "the live leader of {group} was not seen");
        assert!(leader_died, "leader {group} never died");
        assert!(with_member, "the live member of {group} was not seen");
        assert!(!with_zombie_leader, "the zombie leader of {group} counted as live");
    }

    /// A made-up procfs entry's name: a number as `pid` is moved past
    /// PID_MAX_LIMIT (2^22), where no process is, so getpgid can't place it
    /// in a group and its made-up `stat` decides; any other name is kept.
    #[cfg(target_os = "linux")]
    fn past_pid_max(pid: &str) -> String {
        pid.parse::<u32>().map_or_else(|_| pid.to_owned(), |pid| (pid + (1 << 22)).to_string())
    }

    /// A process getpgid places in another group is passed over without its
    /// `stat` being read; one it places in the group, or can't place, is
    /// judged by its `stat`. Here the `stat` files are made up and say
    /// otherwise, so they show which was read.
    #[cfg(target_os = "linux")]
    #[test]
    fn group_has_live_member_reads_stat_only_where_getpgid_finds_no_other_group() {
        use crate::probe::tests::Scratch;
        let scratch = Scratch::new("procpgid");
        let mut leader = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            Command::new("sleep")
                .arg("30")
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("cannot spawn")
        };
        let group = leader.id() as libc::pid_t;
        assert!(group > 1, "bad group {group}");
        // `stat` of a live member of `group`, or of a zombie one.
        let live = format!("1 (sleep) S 1 {group} {group} 0");
        let zombie = format!("1 (sleep) Z 1 {group} {group} 0");
        let ours = std::process::id().to_string();
        let unplaced = past_pid_max("100");
        // Each case: the entry's name, its stat, and whether a live member is
        // seen.
        let cases: [(&str, &str, &str, bool); 5] = [
            ("in another group (this test)", &ours, &live, false),
            ("in the group, live", &group.to_string(), &live, true),
            ("in the group, a zombie", &group.to_string(), &zombie, false),
            ("not placed, live", &unplaced, &live, true),
            ("not placed, a zombie", &unplaced, &zombie, false),
        ];
        let seen: Vec<bool> = cases
            .iter()
            .enumerate()
            .map(|(i, (_, name, stat, _))| {
                let proc = scratch.path(&format!("proc{i}"));
                std::fs::create_dir_all(proc.join(name).join("task")).unwrap();
                std::fs::write(proc.join(name).join("stat"), stat).unwrap();
                std::os::unix::fs::symlink(&ours, proc.join("self")).unwrap();
                group_has_live_member(&proc, group)
            })
            .collect();
        let _ = leader.kill();
        let _ = leader.wait();
        for ((case, _, _, want), seen) in cases.iter().zip(seen) {
            assert_eq!(seen, *want, "{case}");
        }
    }

    /// The group check on a made-up procfs: what counts as a live member,
    /// and stat files that are odd, malformed or missing.
    #[cfg(target_os = "linux")]
    #[test]
    fn group_has_live_member_reads_procfs_without_trusting_it() {
        use crate::probe::tests::Scratch;
        let scratch = Scratch::new("procfs");
        let group = 4242;
        // A /proc entry: its name, its stat, and its tasks' stats. Each case:
        // entries, and whether group 4242 has a live member.
        type Entry<'a> = (&'a str, Option<&'a str>, &'a [&'a str]);
        let cases: [(&str, &[Entry], bool); 14] = [
            ("empty", &[], false),
            ("running", &[("100", Some("100 (sleep) R 1 4242 4242 0"), &[])], true),
            ("sleeping", &[("100", Some("100 (sleep) S 1 4242 4242 0"), &[])], true),
            ("uninterruptible", &[("100", Some("100 (codex) D 1 4242 4242 0"), &[])], true),
            ("other group", &[("100", Some("100 (sleep) R 1 4243 4243 0"), &[])], false),
            ("zombie", &[("100", Some("100 (sh) Z 1 4242 4242 0"), &["100 (sh) Z 1 4242"])], false),
            ("dead", &[("100", Some("100 (sh) X 1 4242 4242 0"), &[])], false),
            (
                "zombie leader, stuck thread",
                &[("100", Some("100 (node) Z 1 4242 4242 0"), &["100 (node) Z 1 4242", "101 (node) D 1 4242"])],
                true,
            ),
            // A name that looks like the fields: they come after its last ')'.
            ("name mimics a live member", &[("100", Some("100 (x) R 1 4242 (y) Z 1 99 99 0"), &[])], false),
            ("name mimics a zombie", &[("100", Some("100 (a) Z 1 1 (b) S 1 4242 4242 0"), &[])], true),
            (
                "malformed",
                &[
                    ("100", Some("garbage"), &[]),
                    ("101", Some("101 (x)"), &[]),
                    ("102", Some("102 (x) S 1 not-a-group 0"), &[]),
                    ("103", Some(""), &[]),
                ],
                false,
            ),
            ("no stat", &[("100", None, &[])], false),
            (
                "not a pid",
                &[("sys", Some("1 (x) R 1 4242 4242 0"), &[]), ("12a", Some("1 (x) R 1 4242 4242 0"), &[])],
                false,
            ),
            (
                "one live among others",
                &[
                    ("100", Some("100 (sh) Z 1 4242 4242 0"), &["100 (sh) Z 1 4242"]),
                    ("101", Some("101 (vim) S 1 7 7 0"), &[]),
                    ("102", Some("102 (sleep) S 100 4242 4242 0"), &[]),
                ],
                true,
            ),
        ];
        for (i, (name, entries, live)) in cases.iter().enumerate() {
            let proc = scratch.path(&format!("proc{i}"));
            std::fs::create_dir(&proc).unwrap();
            // Ours, as /proc/self says.
            std::os::unix::fs::symlink(std::process::id().to_string(), proc.join("self")).unwrap();
            for (pid, stat, tasks) in entries.iter() {
                let dir = proc.join(past_pid_max(pid));
                std::fs::create_dir_all(dir.join("task")).unwrap();
                if let Some(stat) = stat {
                    std::fs::write(dir.join("stat"), stat).unwrap();
                }
                for (t, task) in tasks.iter().enumerate() {
                    let task_dir = dir.join("task").join(t.to_string());
                    std::fs::create_dir(&task_dir).unwrap();
                    std::fs::write(task_dir.join("stat"), task).unwrap();
                }
            }
            assert_eq!(group_has_live_member(&proc, group), *live, "{name}");
        }
        assert!(!group_has_live_member(&scratch.path("missing"), group), "a missing procfs");
    }

    /// A procfs whose `self` isn't our pid (one of another pid namespace) is
    /// not scanned: only the leader is waited for, so an unrelated process
    /// that looks like a member there marks nothing stuck.
    #[cfg(target_os = "linux")]
    #[test]
    fn group_has_live_member_skips_a_procfs_of_another_pid_namespace() {
        use crate::probe::tests::Scratch;
        let scratch = Scratch::new("procns");
        let group = 4242;
        let ours = std::process::id().to_string();
        let other = (std::process::id() + 1).to_string();
        // What `self` is in each case, and whether the live member is seen.
        let cases: [(&str, Option<&str>, bool); 5] = [
            ("ours", Some(&ours), true),
            ("another namespace's", Some(&other), false),
            ("not a pid", Some("self"), false),
            ("an absolute path", Some(&format!("/proc/{ours}")), false),
            ("missing", None, false),
        ];
        for (i, (name, self_link, seen)) in cases.iter().enumerate() {
            let proc = scratch.path(&format!("proc{i}"));
            let dir = proc.join(past_pid_max("100"));
            std::fs::create_dir_all(dir.join("task")).unwrap();
            std::fs::write(dir.join("stat"), "100 (sleep) S 1 4242 4242 0").unwrap();
            if let Some(target) = self_link {
                std::os::unix::fs::symlink(target, proc.join("self")).unwrap();
            }
            assert_eq!(group_has_live_member(&proc, group), *seen, "self {name}");
        }
        assert!(is_our_procfs(Path::new("/proc")), "this test's own /proc is not its pid namespace's");
    }

    /// `script` run by run_bounded_timed_with with `budget`, and how long it took.
    fn run_timed(children: &'static Children, script: &str, budget: Duration) -> (BoundedRun, Duration) {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let run = run_bounded_timed_with(children, &mut sh(script), start + budget).expect("not started");
        (run, start.elapsed())
    }

    /// A child that exits by itself in time is not timed out, whatever it
    /// printed and however it exited.
    #[test]
    fn run_bounded_timed_child_that_exits_by_itself_is_not_timed_out() {
        static CHILDREN: Children = Children::new();
        let cases: [(&str, &[u8]); 5] = [
            ("echo ran", b"ran\n"),
            ("printf partial", b"partial"),
            ("exit 1", b""),
            ("exec >&-; exit 0", b""),
            ("kill -9 $$", b""),
        ];
        for (script, stdout) in cases {
            let (run, elapsed) = run_timed(&CHILDREN, script, Duration::from_secs(5));
            assert_eq!(run.stdout, stdout, "{script:?}");
            assert!(!run.timed_out, "{script:?}: timed out after {elapsed:?}");
            assert!(elapsed < Duration::from_secs(2), "{script:?}: took {elapsed:?}");
        }
    }

    /// Jobs left holding stdout once the child has exited, in its group or
    /// out of it (setsid), hold it only for the exit grace, which is no
    /// timeout: the child's own output is complete.
    #[test]
    fn run_bounded_timed_child_whose_jobs_hold_stdout_is_not_timed_out() {
        static CHILDREN: Children = Children::new();
        for script in ["sleep 30 &\necho ran", "setsid sleep 2 &\necho ran"] {
            let (run, elapsed) = run_timed(&CHILDREN, script, Duration::from_secs(5));
            assert_eq!(run.stdout, b"ran\n", "{script:?}");
            assert!(!run.timed_out, "{script:?}: timed out after {elapsed:?}");
            assert!(elapsed < Duration::from_secs(2), "{script:?}: took {elapsed:?}");
        }
    }

    /// A child still running at its deadline is timed out, keeping what it
    /// printed before: silent, after some output, with stdout closed, with a
    /// background job in its group, or flooding stdout.
    #[test]
    fn run_bounded_timed_child_still_running_at_its_deadline_is_timed_out() {
        static CHILDREN: Children = Children::new();
        let budget = Duration::from_millis(500);
        let cases: [(&str, Option<&[u8]>); 5] = [
            ("exec sleep 30", Some(b"")),
            ("echo started; exec sleep 30", Some(b"started\n")),
            ("exec >&-; exec sleep 30", Some(b"")),
            ("sleep 30 &\necho started; sleep 30", Some(b"started\n")),
            ("exec yes", None),
        ];
        for (script, stdout) in cases {
            let (run, elapsed) = run_timed(&CHILDREN, script, budget);
            assert!(run.timed_out, "{script:?}: not timed out after {elapsed:?}");
            match stdout {
                Some(stdout) => assert_eq!(run.stdout, stdout, "{script:?}"),
                None => assert!(!run.stdout.is_empty() && run.stdout.len() <= OUTPUT_CAP, "{script:?}"),
            }
            assert!(elapsed >= budget, "{script:?}: returned before its deadline, after {elapsed:?}");
            assert!(elapsed < budget + REAP_WAIT + Duration::from_secs(1), "{script:?}: took {elapsed:?}");
        }
    }

    /// With its deadline already passed, nothing is read: timed out, even for
    /// a child that would have exited at once.
    #[test]
    fn run_bounded_timed_with_its_deadline_passed_is_timed_out() {
        static CHILDREN: Children = Children::new();
        for script in ["echo ran", "exec sleep 30"] {
            let (run, elapsed) = run_timed(&CHILDREN, script, Duration::ZERO);
            assert!(run.timed_out, "{script:?}");
            assert!(elapsed < REAP_WAIT + Duration::from_secs(1), "{script:?}: took {elapsed:?}");
        }
    }

    /// A child that can't be started is None, not a run that timed out.
    #[test]
    fn run_bounded_timed_child_that_cannot_start_is_none() {
        static CHILDREN: Children = Children::new();
        let mut cmd = Command::new("/nonexistent/program");
        assert!(run_bounded_timed_with(&CHILDREN, &mut cmd, Instant::now() + Duration::from_secs(5)).is_none());
        CHILDREN.kill_all();
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            run_bounded_timed_with(&CHILDREN, &mut sh("echo ran"), Instant::now() + Duration::from_secs(5)).is_none()
        );
    }
}
