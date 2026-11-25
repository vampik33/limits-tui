mod child;
pub mod claude;
pub mod codex;
pub mod devin;

use std::time::Duration;

/// Upper bound for the *_LIMITS_TIMEOUT env vars. It keeps every deadline
/// built from them in range: `Instant + Duration` panics on overflow, here and
/// inside ureq.
const MAX_TIMEOUT_SECS: f64 = 24.0 * 3600.0;

/// A child forked by one test thread inherits another thread's script while it
/// is still open for writing, and exec'ing that script then fails with
/// ETXTBSY. Script writes hold this exclusively, forks hold it shared, so no
/// fork overlaps an open script. Shared by every probe's tests.
#[cfg(test)]
static SCRIPTS: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Largest file a probe reads. The Claude capture and the Devin credentials
/// are well under a kilobyte, so a file past this is not one of them.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// A child's PATH when the one it would get has no absolute entry (or none is
/// set): `getconf PATH`. An empty PATH is itself an empty entry, the child's
/// working directory, and with PATH unset the program the child runs picks its
/// own default (upstream bash's ends in `.`).
const FALLBACK_PATH: &str = "/bin:/usr/bin";

/// One quota/rate-limit window as the UI needs it, normalized across sources.
pub struct UsageWindow {
    /// Short label for the window: "5h", "day", "wk".
    pub label: &'static str,
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: i64,
}

impl UsageWindow {
    /// `None` when `used_percent` isn't finite. Otherwise it is clamped to
    /// 0..=100, so the meter, the TUI label and `--once` show the same number.
    pub fn new(label: &'static str, used_percent: f64, resets_at: i64) -> Option<Self> {
        // `+ 0.0` turns -0.0 into 0.0, which `--once` would print as "-0".
        used_percent.is_finite().then(|| Self { label, used_percent: used_percent.clamp(0.0, 100.0) + 0.0, resets_at })
    }
}

/// A successful fetch from any source.
pub struct FetchOk {
    pub windows: Vec<UsageWindow>,
    pub plan: Option<String>,
    /// Devin only: overage balance in micro-dollars.
    pub overage_micros: Option<i64>,
    /// Unix seconds. For Claude this is the tap's `captured_at` (last seen),
    /// for Codex/Devin the moment the probe answered.
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Credentials/CLI absent or the account isn't signed in.
    NotSignedIn,
    /// Tool is not installed (codex binary missing).
    Missing,
    /// Account has no quota windows: non-quota billing (devin) or no usage
    /// windows (codex).
    NoQuota,
    Timeout,
    /// Claude only: no capture file yet — Claude Code hasn't run here.
    NoData,
    Failed,
}

impl ErrorCode {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotSignedIn => "not signed in",
            Self::Missing => "not installed",
            Self::NoQuota => "no quota",
            Self::Timeout => "timeout",
            Self::NoData => "no data",
            Self::Failed => "unavailable",
        }
    }
}

#[derive(Debug)]
pub struct ProbeError {
    pub code: ErrorCode,
    #[allow(dead_code)] // surfaced via --verbose later if needed
    pub detail: String,
}

impl ProbeError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        detail.truncate(detail.floor_char_boundary(200));
        Self { code, detail }
    }
}

pub type ProbeResult = Result<FetchOk, ProbeError>;

pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Loose integer parse like the Python probes: real ints, or numeric strings
/// (the devin API mixes both).
pub fn json_int(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn json_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// The UTF-8 text of the regular file at `path` (symlinks followed), at most
/// MAX_FILE_BYTES. It is opened with O_NONBLOCK, so a FIFO there can't block
/// the caller, and O_NOCTTY, so a terminal there can't become ours; the type
/// is checked on the open file, so the path can't be swapped after the check.
pub fn read_small_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::new(ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(Error::new(ErrorKind::FileTooLarge, "file is larger than 1 MiB"));
    }
    String::from_utf8(bytes).map_err(|e| Error::new(ErrorKind::InvalidData, e))
}

/// Probe timeout from env var `var` (seconds, may be fractional), else
/// `default_secs`.
pub fn timeout_from_env(var: &str, default_secs: f64) -> Duration {
    parse_timeout(std::env::var(var).ok().as_deref(), default_secs)
}

/// Unparsable values and NaN fall back to `default_secs`; the rest, infinity
/// included, is clamped to 1 ms ..= MAX_TIMEOUT_SECS, so this never panics.
fn parse_timeout(raw: Option<&str>, default_secs: f64) -> Duration {
    let secs = raw.and_then(|s| s.parse::<f64>().ok()).filter(|t| !t.is_nan()).unwrap_or(default_secs);
    Duration::from_secs_f64(secs.clamp(0.001, MAX_TIMEOUT_SECS))
}

/// The absolute entries of `path` (a PATH value). Empty and relative entries
/// are resolved against the working directory: ours may be an untrusted
/// checkout, and under a child's (`/`) `tmp/bin` is `/tmp/bin`, which any
/// local user can create.
fn absolute_dirs(path: &std::ffi::OsStr) -> impl Iterator<Item = std::path::PathBuf> + '_ {
    std::env::split_paths(path).filter(|dir| dir.is_absolute())
}

/// The PATH to give a child for `path` (a PATH value): its absolute entries,
/// or FALLBACK_PATH if it has none.
fn safe_path(path: &std::ffi::OsStr) -> std::ffi::OsString {
    std::env::join_paths(absolute_dirs(path)).ok().filter(|p| !p.is_empty()).unwrap_or_else(|| FALLBACK_PATH.into())
}

/// Leave `cmd` to inherit `ours` (our PATH, None if unset) if safe_path keeps
/// it as it is; otherwise set PATH to what safe_path makes of it.
fn inherit_safe_path(cmd: &mut std::process::Command, ours: Option<&std::ffi::OsStr>) {
    let safe = safe_path(ours.unwrap_or_default());
    if ours != Some(safe.as_os_str()) {
        cmd.env("PATH", safe);
    }
}

/// Whether `cond` holds within `tries` checks 50 ms apart. Shared by every
/// probe's tests.
#[cfg(test)]
fn wait_until(tries: u32, mut cond: impl FnMut() -> bool) -> bool {
    (0..tries).any(|_| {
        let met = cond();
        if !met {
            std::thread::sleep(Duration::from_millis(50));
        }
        met
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test temp dir, removed on drop.
    pub(super) struct Scratch(pub(super) std::path::PathBuf);

    impl Scratch {
        /// A fresh dir only we can use: created here (never one that already
        /// exists, which another user could have planted in a shared /tmp),
        /// mode 0700, with a random suffix so planting can't block it either.
        pub(super) fn new(test: &str) -> Self {
            use std::hash::BuildHasher;
            use std::os::unix::fs::DirBuilderExt;
            let random = std::collections::hash_map::RandomState::new().hash_one(test);
            let dir = std::env::temp_dir().join(format!("limits-tui-{}-{test}-{random:016x}", std::process::id()));
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            Self(dir)
        }

        pub(super) fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }

        /// Make a FIFO `name` and return its path.
        pub(super) fn fifo(&self, name: &str) -> std::path::PathBuf {
            use std::os::unix::ffi::OsStrExt;
            let path = self.path(name);
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c_path` is a valid NUL-terminated string for the call.
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo {}", path.display());
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `f`'s result, or a panic if it takes more than 5 s (a read that blocks
    /// fails the test instead of hanging it).
    pub(super) fn promptly<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(5)).expect("blocked for more than 5 s")
    }

    #[test]
    fn small_file_is_read() {
        let scratch = Scratch::new("readok");
        let path = scratch.path("f");
        std::fs::write(&path, "héllo\n").unwrap();
        assert_eq!(read_small_file(&path).unwrap(), "héllo\n");
    }

    #[test]
    fn symlink_to_a_regular_file_is_read() {
        let scratch = Scratch::new("readlink");
        let (target, link) = (scratch.path("target"), scratch.path("link"));
        std::fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_small_file(&link).unwrap(), "{}");
    }

    #[test]
    fn missing_file_is_not_found() {
        let scratch = Scratch::new("readmissing");
        assert_eq!(read_small_file(&scratch.path("nope")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn fifo_is_refused_without_blocking() {
        let scratch = Scratch::new("readfifo");
        let fifo = scratch.fifo("fifo");
        let err = promptly(move || read_small_file(&fifo)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    }

    #[test]
    fn symlink_to_a_fifo_is_refused_without_blocking() {
        let scratch = Scratch::new("readfifolink");
        let link = scratch.path("link");
        std::os::unix::fs::symlink(scratch.fifo("fifo"), &link).unwrap();
        let err = promptly(move || read_small_file(&link)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    }

    #[test]
    fn directory_is_refused() {
        let scratch = Scratch::new("readdir");
        assert!(read_small_file(&scratch.0).is_err());
    }

    #[test]
    fn character_device_is_refused() {
        let err = promptly(|| read_small_file(std::path::Path::new("/dev/null"))).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    }

    #[test]
    fn file_at_the_size_cap_is_read() {
        let scratch = Scratch::new("readcap");
        let path = scratch.path("f");
        std::fs::write(&path, "a".repeat(MAX_FILE_BYTES as usize)).unwrap();
        assert_eq!(read_small_file(&path).unwrap().len() as u64, MAX_FILE_BYTES);
    }

    #[test]
    fn file_over_the_size_cap_is_refused() {
        let scratch = Scratch::new("readbig");
        let path = scratch.path("f");
        std::fs::write(&path, "a".repeat(MAX_FILE_BYTES as usize + 1)).unwrap();
        assert_eq!(read_small_file(&path).unwrap_err().kind(), std::io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let scratch = Scratch::new("readutf8");
        let path = scratch.path("f");
        std::fs::write(&path, b"\xff\xfe").unwrap();
        assert_eq!(read_small_file(&path).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn detail_two_byte_char_crossing_limit_is_dropped() {
        // "é" occupies bytes 199..201, so byte 200 is not a char boundary.
        let e = ProbeError::new(ErrorCode::Failed, format!("{}é{}", "a".repeat(199), "b".repeat(50)));
        assert_eq!(e.detail, "a".repeat(199));
    }

    #[test]
    fn detail_four_byte_char_crossing_limit_is_dropped() {
        // "🦀" occupies bytes 198..202.
        let e = ProbeError::new(ErrorCode::Failed, format!("{}🦀tail", "a".repeat(198)));
        assert_eq!(e.detail, "a".repeat(198));
    }

    #[test]
    fn detail_multibyte_char_ending_at_limit_is_kept() {
        // "é" occupies bytes 198..200, so it fits exactly.
        let e = ProbeError::new(ErrorCode::Failed, format!("{}é{}", "a".repeat(198), "b".repeat(50)));
        assert_eq!(e.detail, format!("{}é", "a".repeat(198)));
    }

    #[test]
    fn detail_long_ascii_is_cut_to_200_bytes() {
        let e = ProbeError::new(ErrorCode::Failed, "a".repeat(250));
        assert_eq!(e.detail, "a".repeat(200));
    }

    #[test]
    fn detail_exactly_200_bytes_is_unchanged() {
        let e = ProbeError::new(ErrorCode::Failed, "a".repeat(200));
        assert_eq!(e.detail, "a".repeat(200));
    }

    #[test]
    fn detail_short_and_empty_are_unchanged() {
        assert_eq!(ProbeError::new(ErrorCode::Failed, "héllo").detail, "héllo");
        assert_eq!(ProbeError::new(ErrorCode::Failed, "").detail, "");
    }

    const MAX: Duration = Duration::from_secs(24 * 3600);

    #[test]
    fn timeout_unset_or_unparsable_uses_default() {
        assert_eq!(parse_timeout(None, 15.0), Duration::from_secs(15));
        assert_eq!(parse_timeout(Some(""), 15.0), Duration::from_secs(15));
        assert_eq!(parse_timeout(Some("soon"), 15.0), Duration::from_secs(15));
        assert_eq!(parse_timeout(Some("NaN"), 15.0), Duration::from_secs(15));
    }

    #[test]
    fn timeout_plain_values_are_kept() {
        assert_eq!(parse_timeout(Some("2.5"), 15.0), Duration::from_millis(2500));
        assert_eq!(parse_timeout(Some("30"), 15.0), Duration::from_secs(30));
    }

    #[test]
    fn timeout_zero_and_negative_become_one_millisecond() {
        assert_eq!(parse_timeout(Some("0"), 15.0), Duration::from_millis(1));
        assert_eq!(parse_timeout(Some("-5"), 15.0), Duration::from_millis(1));
        assert_eq!(parse_timeout(Some("-inf"), 15.0), Duration::from_millis(1));
    }

    #[test]
    fn usage_window_rejects_non_finite_percent() {
        for pct in [f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(UsageWindow::new("5h", pct, 0).is_none(), "{pct}");
        }
    }

    #[test]
    fn usage_window_clamps_percent_to_0_100() {
        for (pct, want) in [
            (-5.0, 0.0),
            (-0.0, 0.0),
            (-f64::MIN_POSITIVE, 0.0),
            (f64::MIN, 0.0),
            (100.4, 100.0),
            (101.0, 100.0),
            (f64::MAX, 100.0),
        ] {
            let w = UsageWindow::new("5h", pct, 0).unwrap();
            assert_eq!(w.used_percent.to_bits(), f64::to_bits(want), "{pct:e}");
        }
    }

    #[test]
    fn usage_window_keeps_in_range_percent_label_and_reset() {
        for pct in [0.0, 5e-324, 0.4, 12.5, 42.0, 99.5, 100.0] {
            let w = UsageWindow::new("wk", pct, 1_800_000_000).unwrap();
            assert_eq!(
                (w.label, w.used_percent.to_bits(), w.resets_at),
                ("wk", pct.to_bits(), 1_800_000_000),
                "{pct:e}"
            );
        }
    }

    /// xorshift64*: fixed seed, so a failing value is reproducible.
    fn next_u64(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    #[test]
    fn usage_window_percent_is_always_finite_and_in_range() {
        let mut values = vec![
            0.0,
            -0.0,
            100.0,
            -100.0,
            0.5,
            -0.5,
            99.5,
            100.5,
            f64::EPSILON,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            f64::MAX,
            f64::MIN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        for edge in [0.0f64, 100.0] {
            values.extend([edge.next_up(), edge.next_down()]);
        }
        // Subnormals, NaN payloads (quiet, signalling, negative), the largest finite.
        for bits in [
            1u64,
            0x000F_FFFF_FFFF_FFFF,
            0x8000_0000_0000_0001,
            0x7FF0_0000_0000_0001,
            0x7FF8_0000_0000_0001,
            0xFFF8_0000_0000_0000,
            0xFFFF_FFFF_FFFF_FFFF,
            0x7FEF_FFFF_FFFF_FFFF,
        ] {
            values.push(f64::from_bits(bits));
        }
        let mut state = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..100_000 {
            // Any bit pattern, then a value near the 0..=100 range.
            values.push(f64::from_bits(next_u64(&mut state)));
            values.push((next_u64(&mut state) >> 11) as f64 / (1u64 << 53) as f64 * 200.0 - 50.0);
        }
        for pct in values {
            let Some(w) = UsageWindow::new("5h", pct, 7) else {
                assert!(!pct.is_finite(), "{pct:e} was rejected");
                continue;
            };
            let p = w.used_percent;
            assert!(pct.is_finite(), "{pct:e} was kept");
            assert!((0.0..=100.0).contains(&p) && p.is_sign_positive(), "{pct:e} -> {p:e}");
            if pct > 0.0 && pct <= 100.0 {
                assert_eq!(p.to_bits(), pct.to_bits(), "{pct:e}");
            }
            // The TUI label (`round() as i64`) and `--once` (`round()` printed) agree.
            assert_eq!(p.round().to_string(), (p.round() as i64).to_string(), "{pct:e}");
        }
    }

    #[test]
    fn timeout_infinite_or_huge_is_capped_without_panicking() {
        for raw in ["inf", "infinity", "1e20", "1e300", "86401"] {
            assert_eq!(parse_timeout(Some(raw), 15.0), MAX, "{raw}");
        }
        // The cap leaves room to build a deadline from it.
        let _ = std::time::Instant::now() + parse_timeout(Some("inf"), 15.0);
    }

    // --- PATH for children ---

    #[test]
    fn safe_path_keeps_only_absolute_entries() {
        for (path, want) in [
            ("/usr/bin:/bin", "/usr/bin:/bin"),
            (":/usr/bin", "/usr/bin"),
            ("/usr/bin:", "/usr/bin"),
            ("/usr/bin::/bin", "/usr/bin:/bin"),
            (".:/usr/bin:tmp/bin:/bin:..", "/usr/bin:/bin"),
            ("~/bin:./bin:/opt/x/", "/opt/x/"),
        ] {
            assert_eq!(safe_path(path.as_ref()), want, "{path:?}");
        }
    }

    #[test]
    fn safe_path_without_an_absolute_entry_falls_back() {
        for path in ["", ":", "::", ".", "tmp/bin", ".:tmp/bin:", "bin::.."] {
            assert_eq!(safe_path(path.as_ref()), FALLBACK_PATH, "{path:?}");
        }
    }

    #[test]
    fn safe_path_keeps_non_utf8_entries() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::OsStr::from_bytes(b"/opt/\xff/bin:rel\xfe:/bin");
        assert_eq!(safe_path(path).as_bytes(), b"/opt/\xff/bin:/bin");
        assert_eq!(safe_path(std::ffi::OsStr::from_bytes(b"\xff:.")), FALLBACK_PATH);
    }

    /// Every PATH of one to four entries from a small pool: safe_path keeps
    /// exactly the entries that start with `/`, in order, falls back when
    /// there are none, and changes nothing the second time.
    #[test]
    fn safe_path_matches_a_naive_filter() {
        let pool = ["", ".", "..", "tmp/bin", "~/bin", "/usr/bin", "/a b/", "/"];
        let mut paths: Vec<Vec<&str>> = pool.iter().map(|e| vec![*e]).collect();
        for _ in 1..4 {
            let longer: Vec<Vec<&str>> = paths
                .iter()
                .filter(|p| p.len() == paths.last().unwrap().len())
                .flat_map(|p| pool.iter().map(move |e| [p.as_slice(), &[*e]].concat()))
                .collect();
            paths.extend(longer);
        }
        assert_eq!(paths.len(), 8 + 64 + 512 + 4096);
        for entries in paths {
            let path = entries.join(":");
            let kept: Vec<&str> = entries.iter().copied().filter(|e| e.starts_with('/')).collect();
            let want = if kept.is_empty() { FALLBACK_PATH.to_string() } else { kept.join(":") };
            let safe = safe_path(path.as_ref());
            assert_eq!(safe, want.as_str(), "{path:?}");
            assert_eq!(safe_path(&safe), safe, "{path:?}");
        }
    }

    #[test]
    fn inherit_safe_path_sets_path_only_when_needed() {
        let set = |ours: Option<&str>| {
            let mut cmd = std::process::Command::new("/bin/true");
            inherit_safe_path(&mut cmd, ours.map(std::ffi::OsStr::new));
            cmd.get_envs().find(|(k, _)| *k == "PATH").map(|(_, v)| v.map(std::ffi::OsStr::to_owned))
        };
        assert_eq!(set(Some("/usr/bin:/bin")), None);
        assert_eq!(set(Some("/usr/bin:tmp/bin")), Some(Some("/usr/bin".into())));
        assert_eq!(set(Some("/usr/bin:")), Some(Some("/usr/bin".into())));
        assert_eq!(set(Some(".:/usr/bin")), Some(Some("/usr/bin".into())));
        for ours in [Some(""), Some("tmp/bin"), Some(":."), None] {
            assert_eq!(set(ours), Some(Some(FALLBACK_PATH.into())), "{ours:?}");
        }
    }
}
