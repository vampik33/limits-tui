//! Child processes bounded by a deadline: output read without blocking on the
//! caller's thread and capped, and the whole process group killed when the
//! deadline passes, so nothing a probe starts outlives it.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
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

/// The last OUTPUT_CAP bytes of stdout of `cmd`, or None if it could not be
/// started. Returns by `deadline`, apart from reaping a killed child. Before it
/// returns, the child's process group is killed: the child itself if it is
/// still running at the deadline, and every background job left in the group,
/// whether or not it still holds stdout. Holders outside that group (setsid)
/// can't be reached, but the read end is closed on return, so nothing outlives
/// the call: their later writes get EPIPE. The child runs in
/// `/` with stdin and stderr closed, so a planted file in an untrusted working
/// directory can't be run.
pub(super) fn run_bounded(cmd: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    let mut child = cmd
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0) // a non-interactive shell keeps its background jobs in this group
        .spawn()
        .ok()?;
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
    // `child.wait()` below reaps the child (has_exited uses WNOWAIT), so its
    // pid, and with it the group id, cannot have been reused yet.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    // Killed holders release stdout at once.
    output.settle(deadline);
    let tail = output.into_tail();
    let _ = child.wait();
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
