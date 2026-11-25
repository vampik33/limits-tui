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
/// (the devin API mixes both). A number must denote a whole value in the i64
/// range (`1.0` and `1e3` are ints); a fraction like `0.5` or a huge value
/// like `1e30` is `None`, so the caller reports it instead of reading a
/// plausible int. The check reads the number's exact decimal text, which
/// serde_json's `arbitrary_precision` keeps, not a rounded f64: so
/// `300.00000000000001` and `-9223372036854775809` are `None`, and
/// `9.223372036854775807e18` is i64::MAX.
pub fn json_int(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => exact_int(n.as_str()),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// The i64 that the JSON number `token` denotes exactly, if it is a whole
/// number in range. Linear in the token's length with no allocation, so an
/// exponent like `1e999999999` costs only its digits.
fn exact_int(token: &str) -> Option<i64> {
    let (negative, rest) = token.strip_prefix('-').map_or((false, token), |r| (true, r));
    let (mantissa, exponent) = rest.split_once(['e', 'E']).map_or((rest, None), |(m, e)| (m, Some(e)));
    // JSON's grammar: digits before the point, and after it if there is one.
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (int, frac) = match mantissa.split_once('.') {
        Some((int, frac)) if all_digits(frac) => (int, frac),
        Some(_) => return None,
        None => (mantissa, ""),
    };
    if !all_digits(int) {
        return None;
    }
    // Saturating: an exponent past i64 is far larger than any token's length,
    // so the outcome below (zero, a fraction or out of range) still holds.
    let exp = match exponent {
        None => 0,
        Some(e) => {
            let (neg, digits) = match e.strip_prefix('-') {
                Some(digits) => (true, digits),
                None => (false, e.strip_prefix('+').unwrap_or(e)),
            };
            if !all_digits(digits) {
                return None;
            }
            let mag = digits.bytes().fold(0i64, |acc, d| acc.saturating_mul(10).saturating_add(i64::from(d - b'0')));
            if neg { -mag } else { mag }
        }
    };
    // The value is the digit string int + frac times 10^(exp - frac.len()).
    let digits = || int.bytes().chain(frac.bytes());
    let total = int.len() + frac.len();
    let lead = digits().take_while(|&d| d == b'0').count();
    if lead == total {
        return Some(0);
    }
    let trail = digits().rev().take_while(|&d| d == b'0').count();
    let significant = total - lead - trail;
    // The significant digits, ending in a nonzero one, times 10^scale: whole only
    // if scale >= 0, and within i64 (19 digits) only if it has at most 19.
    let scale = exp.saturating_sub(frac.len() as i64).saturating_add(trail as i64);
    if scale < 0 || (significant as i64).saturating_add(scale) > 19 {
        return None;
    }
    let mut magnitude = 0u64;
    for d in digits().skip(lead).take(significant) {
        magnitude = magnitude.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
    }
    for _ in 0..scale {
        magnitude = magnitude.checked_mul(10)?;
    }
    if negative { 0i64.checked_sub_unsigned(magnitude) } else { i64::try_from(magnitude).ok() }
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

/// SIGKILL the process group of every child a probe still runs, and start no
/// more. For quitting the TUI: its probe threads, which would kill them at
/// their deadlines, die with the process. Doesn't wait for the probes, only
/// (half a second at most) for spawns in flight.
pub fn kill_children() {
    child::CHILDREN.kill_all();
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

    /// A fresh sleep argument to find a test's processes by: GNU sleep adds its
    /// arguments, so the fraction changes nothing.
    pub(super) fn sleep_token(test: &str) -> String {
        use std::hash::BuildHasher;
        let random = std::collections::hash_map::RandomState::new().hash_one(test) % 1_000_000_000;
        format!("0.000{random:09}")
    }

    /// Pids of live (not zombie) processes whose command line has `token`.
    pub(super) fn live_with(token: &str) -> Vec<String> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        dir.filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                let cmdline = std::fs::read(path.join("cmdline")).unwrap_or_default();
                let stat = std::fs::read_to_string(path.join("stat")).unwrap_or_default();
                String::from_utf8_lossy(&cmdline).contains(token) && !stat.contains(") Z ")
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
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

    /// JSON number tokens an f64 can't tell from a nearby int (a rounded
    /// fraction, one past either i64 bound) and the i64 bounds written with
    /// exponents, each with what json_int reads from it. For the raw-reply
    /// tests of every json_int caller.
    pub(super) const EDGE_INT_TOKENS: [(&str, Option<i64>); 7] = [
        ("1800000000.00000000000000001", None),
        ("-9223372036854775809", None),
        ("9223372036854775808", None),
        ("-9.223372036854775809e18", None),
        ("9.223372036854775808e18", None),
        ("9.223372036854775807e18", Some(i64::MAX)),
        ("-9.223372036854775808e18", Some(i64::MIN)),
    ];

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

    // --- json_int ---

    #[test]
    fn json_int_reads_whole_numbers() {
        for (v, want) in [
            (serde_json::json!(0), 0),
            (serde_json::json!(-1), -1),
            (serde_json::json!(1.0), 1),
            (serde_json::json!(-1.0), -1),
            (serde_json::json!(-0.0), 0),
            (serde_json::json!(1.8e9), 1_800_000_000),
            (serde_json::json!(i64::MAX), i64::MAX),
            (serde_json::json!(i64::MIN), i64::MIN),
        ] {
            assert_eq!(json_int(&v), Some(want), "{v}");
        }
        for (text, want) in [
            ("1.0", 1),
            ("-1.0", -1),
            ("-0", 0),
            ("1800000000.0", 1_800_000_000),
            ("-9.223372036854775808e18", i64::MIN),
        ] {
            assert_eq!(json_int(&serde_json::from_str(text).unwrap()), Some(want), "{text}");
        }
    }

    #[test]
    fn json_int_rejects_fractions_and_out_of_range_numbers() {
        for v in [
            serde_json::json!(0.5),
            serde_json::json!(-0.5),
            serde_json::json!(1_800_000_000.5),
            serde_json::json!(5e-324),
            serde_json::json!(1e30),
            serde_json::json!(-1e30),
            serde_json::json!(f64::MAX),
            serde_json::json!(f64::MIN),
            // 2^63, and i64::MAX written as a float, which rounds to 2^63.
            serde_json::json!(9_223_372_036_854_775_808.0),
            serde_json::json!(i64::MAX as f64),
            serde_json::json!((i64::MIN as f64).next_down()),
            serde_json::json!(i64::MAX as u64 + 1),
            serde_json::json!(u64::MAX),
            // A Value made from a float holds its shortest decimal,
            // -9.223372036854776e18, which is below i64::MIN.
            serde_json::json!(i64::MIN as f64),
        ] {
            assert_eq!(json_int(&v), None, "{v}");
        }
        for text in ["0.5", "-0.5", "1e30", "-1e30", "9223372036854775808", "18446744073709551615"] {
            assert_eq!(json_int(&serde_json::from_str(text).unwrap()), None, "{text}");
        }
    }

    /// A number is judged by its exact decimal text, not the f64 it rounds
    /// to: a fraction or a value past i64 that an f64 can't tell apart from
    /// an int is `None`, and the bounds written with exponents are ints.
    #[test]
    fn json_int_judges_the_exact_decimal() {
        for (text, want) in [
            ("1.0", Some(1)),
            ("1e3", Some(1000)),
            ("1E3", Some(1000)),
            ("1e+3", Some(1000)),
            ("300.000", Some(300)),
            ("3000e-1", Some(300)),
            ("0.003e5", Some(300)),
            ("-0.0e7", Some(0)),
            ("9.223372036854775807e18", Some(i64::MAX)),
            ("-9.223372036854775808e18", Some(i64::MIN)),
            ("92233720368547758070e-1", Some(i64::MAX)),
            ("-0.9223372036854775808e19", Some(i64::MIN)),
            ("300.00000000000001", None),
            ("1800000000.00000000000000001", None),
            ("-9223372036854775809", None),
            ("9223372036854775808", None),
            ("9.223372036854775808e18", None),
            ("-9.223372036854775809e18", None),
            ("9.2233720368547758071e18", None),
            ("1e19", None),
            ("1e-1", None),
        ] {
            assert_eq!(json_int(&serde_json::from_str(text).unwrap()), want, "{text}");
        }
    }

    /// Exponents far past any int stay cheap: no allocation or loop grows
    /// with the exponent's value, and saturating it keeps the outcome exact.
    #[test]
    fn json_int_handles_huge_exponents_promptly() {
        let got = promptly(|| {
            let huge = "9".repeat(1_000_000);
            let zeros = "0".repeat(1_000_000);
            [
                "1e999999999".to_string(),
                "-1e999999999".to_string(),
                "1e-999999999".to_string(),
                "0e999999999".to_string(),
                "1e99999999999999999999999999999".to_string(),
                "1e-99999999999999999999999999999".to_string(),
                "0.0e-99999999999999999999999999999".to_string(),
                format!("1e{huge}"),
                format!("1e-{huge}"),
                // A million trailing zeros and an exponent that takes them back.
                format!("1{zeros}e-1000000"),
                format!("-0.{zeros}5e1000001"),
                format!("1{zeros}e-999980"),
            ]
            .map(|text| json_int(&serde_json::from_str(&text).unwrap()))
        });
        assert_eq!(got, [None, None, None, Some(0), None, None, Some(0), None, None, Some(1), Some(-5), None]);
    }

    /// What the JSON number `token` denotes, worked out on digit strings by
    /// moving the decimal point: the oracle for json_int. `Some` holds the
    /// int's decimal text when the token is a whole number within i64.
    fn int_oracle(token: &str) -> Option<String> {
        let (negative, rest) = token.strip_prefix('-').map_or((false, token), |r| (true, r));
        let (mantissa, exponent) = rest.split_once(['e', 'E']).unwrap_or((rest, "0"));
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = format!("{int}{frac}");
        assert!(digits.len() < 1000, "the oracle reads exponents for short mantissas only: {token}");
        let exponent = exponent.strip_prefix('+').unwrap_or(exponent);
        let (exp_negative, exp_digits) = exponent.strip_prefix('-').map_or((false, exponent), |e| (true, e));
        let exp_digits = exp_digits.trim_start_matches('0');
        if digits.bytes().all(|d| d == b'0') {
            return Some("0".into());
        }
        // A nonzero mantissa of under 1000 digits times 10^±10000 or more is
        // far past i64, or a nonzero value far below 1.
        if exp_digits.len() > 4 {
            return None;
        }
        let exp: i32 = exp_digits.parse().unwrap_or(0);
        let point = int.len() as i32 + if exp_negative { -exp } else { exp };
        let (whole, fraction) = if point <= 0 {
            (String::new(), format!("{}{digits}", "0".repeat(-point as usize)))
        } else if point as usize >= digits.len() {
            (format!("{digits}{}", "0".repeat(point as usize - digits.len())), String::new())
        } else {
            (digits[..point as usize].to_string(), digits[point as usize..].to_string())
        };
        if !fraction.bytes().all(|d| d == b'0') {
            return None;
        }
        let whole = whole.trim_start_matches('0');
        if whole.is_empty() {
            return Some("0".into());
        }
        let bound = if negative { "9223372036854775808" } else { "9223372036854775807" };
        (whole.len() < bound.len() || (whole.len() == bound.len() && whole <= bound))
            .then(|| format!("{}{whole}", if negative { "-" } else { "" }))
    }

    /// Over generated JSON number tokens (random digits, the i64 bounds with
    /// the decimal point moved and the exponent making up for it or missing
    /// by one, rounded fractions, huge exponents, and the decimals floats
    /// print as), json_int agrees with int_oracle.
    #[test]
    fn json_int_agrees_with_a_digit_string_oracle() {
        const BOUNDS: [&str; 6] = [
            "9223372036854775807",
            "9223372036854775808",
            "9223372036854775806",
            "9223372036854775809",
            "1800000000",
            "300",
        ];
        let mut state = 0x2545_F491_4F6C_DD1D;
        let mut rand = |n: u64| next_u64(&mut state) % n;
        // A third of the digits are zeros, so runs of them come up.
        fn random_digits(len: u64, rand: &mut impl FnMut(u64) -> u64) -> String {
            (0..len).map(|_| if rand(3) == 0 { '0' } else { char::from(b'0' + rand(10) as u8) }).collect()
        }
        let (mut accepted, mut rejected, mut max_seen, mut min_seen) = (0, 0, false, false);
        for i in 0..200_000 {
            let negative = rand(2) == 0;
            let token = if i % 5 == 4 {
                // The decimal a float prints as.
                let f = f64::from_bits(rand(u64::MAX)) * if rand(2) == 0 { 1.0 } else { 1e-300 };
                let f = if rand(2) == 0 { f } else { f.trunc() };
                match serde_json::Value::from(f) {
                    serde_json::Value::Number(n) => n.as_str().to_string(),
                    _ => continue,
                }
            } else if i % 5 < 2 {
                // A bound's digits with the point moved and the exponent making up for it.
                let bound = if rand(3) == 0 {
                    format!("{}{}", 1 + rand(9), random_digits(rand(22), &mut rand))
                } else {
                    BOUNDS[rand(BOUNDS.len() as u64) as usize].to_string()
                };
                let k = rand(bound.len() as u64 + 1) as usize;
                let leading = rand(4) as usize;
                let (mut mantissa, mut exp) = if k == 0 {
                    (format!("0.{}{bound}", "0".repeat(leading)), (leading + bound.len()) as i64)
                } else if k == bound.len() {
                    (bound.clone(), 0)
                } else {
                    (format!("{}.{}", &bound[..k], &bound[k..]), (bound.len() - k) as i64)
                };
                if rand(2) == 0 {
                    // Trailing zeros, which change nothing.
                    let zeros = "0".repeat(1 + rand(20) as usize);
                    mantissa = if mantissa.contains('.') { mantissa + &zeros } else { format!("{mantissa}.{zeros}") };
                }
                if rand(4) == 0 {
                    // A rounded fraction: a 1 past what an f64 keeps.
                    let sep = if mantissa.contains('.') { "" } else { "." };
                    mantissa = format!("{mantissa}{sep}{}1", "0".repeat(rand(25) as usize));
                }
                exp += [-1, 0, 0, 0, 1][rand(5) as usize];
                if exp == 0 && rand(2) == 0 { mantissa } else { format!("{mantissa}e{exp}") }
            } else {
                // Random digits, a fraction, and an exponent small or huge.
                let int = if rand(4) == 0 {
                    "0".to_string()
                } else {
                    format!("{}{}", 1 + rand(9), random_digits(rand(25), &mut rand))
                };
                let frac = if rand(2) == 0 {
                    String::new()
                } else {
                    format!(".{}{}", random_digits(1 + rand(25), &mut rand), "0".repeat(rand(5) as usize))
                };
                let exp = match rand(6) {
                    0 => String::new(),
                    1 => format!(
                        "e{}",
                        ["999999999999999999999", "18446744073709551616", "100000", "0000"][rand(4) as usize]
                    ),
                    _ => format!("e{}", rand(61) as i64 - 30),
                };
                let exp = match (exp.strip_prefix('e'), rand(4)) {
                    (Some(e), 0) if !e.starts_with('-') => format!("E+{e}"),
                    (Some(e), 1) if !e.starts_with('-') && rand(2) == 0 => format!("e-{e}"),
                    _ => exp,
                };
                format!("{int}{frac}{exp}")
            };
            let token = if negative && !token.starts_with('-') { format!("-{token}") } else { token };
            let value: serde_json::Value = serde_json::from_str(&token).unwrap_or_else(|e| panic!("{token}: {e}"));
            let got = json_int(&value);
            assert_eq!(got.map(|n| n.to_string()), int_oracle(&token), "{token}");
            match got {
                Some(n) => {
                    accepted += 1;
                    max_seen |= n == i64::MAX;
                    min_seen |= n == i64::MIN;
                }
                None => rejected += 1,
            }
        }
        // The generator reaches both outcomes and both bounds.
        assert!(accepted > 10_000 && rejected > 10_000, "{accepted} accepted, {rejected} rejected");
        assert!(max_seen && min_seen, "max {max_seen}, min {min_seen}");
    }

    #[test]
    fn json_int_reads_only_integer_strings() {
        for (s, want) in [
            ("0", 0),
            ("-1", -1),
            (" 42 ", 42),
            ("1800000000", 1_800_000_000),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
        ] {
            assert_eq!(json_int(&s.into()), Some(want), "{s:?}");
        }
        for s in [
            "0.5",
            "-0.5",
            "1.0",
            "1e30",
            "-1e30",
            "1e3",
            "NaN",
            "inf",
            "-inf",
            "infinity",
            "9223372036854775808",
            "",
            "abc",
        ] {
            assert_eq!(json_int(&s.into()), None, "{s:?}");
        }
        for v in [serde_json::Value::Null, true.into(), serde_json::json!({}), serde_json::json!([1])] {
            assert_eq!(json_int(&v), None, "{v}");
        }
    }

    /// Over edge and random floats, written as their exact decimal, json_int
    /// keeps exactly the finite whole values in the i64 range (checked against
    /// a naive i128 oracle), and the int it gives is that value.
    #[test]
    fn json_int_keeps_exactly_whole_floats_in_range() {
        const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
        let mut values = vec![
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.0,
            -1.0,
            1e30,
            -1e30,
            TWO_POW_63,
            -TWO_POW_63,
            i64::MAX as f64,
            f64::MAX,
            f64::MIN,
            f64::EPSILON,
            f64::MIN_POSITIVE,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        for edge in [0.0f64, 1.0, -1.0, 4_503_599_627_370_496.0, 9_007_199_254_740_992.0, TWO_POW_63, -TWO_POW_63] {
            values.extend([edge.next_up(), edge.next_down(), -edge.next_up(), -edge.next_down()]);
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
            // Any bit pattern; a whole value inside the range, then one that
            // is past it half the time; a value with a fraction.
            values.push(f64::from_bits(next_u64(&mut state)));
            values.push(next_u64(&mut state) as i64 as f64);
            values.push(next_u64(&mut state) as i64 as f64 * 2.0);
            values.push((next_u64(&mut state) as i64 >> 11) as f64 / 1024.0);
        }
        for f in values {
            // `as i128` saturates only far outside the i64 range.
            let fits = f.is_finite() && f.trunc() == f && (i64::MIN as i128..=i64::MAX as i128).contains(&(f as i128));
            // A whole float's exact decimal; a Value made from any other
            // float holds its shortest decimal, which keeps a fraction, or
            // null for a non-finite one.
            let value = if f.is_finite() && f.trunc() == f {
                serde_json::from_str(&format!("{f:.0}")).unwrap()
            } else {
                serde_json::Value::from(f)
            };
            let got = json_int(&value);
            assert_eq!(got.is_some(), fits, "{f:e}");
            if let Some(n) = got {
                assert_eq!(n as f64, f, "{f:e}");
                assert_eq!(n as i128, f as i128, "{f:e}");
            }
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
        let mut paths: Vec<Vec<&str>> = Vec::new();
        let mut longest: Vec<Vec<&str>> = vec![Vec::new()];
        for _ in 0..4 {
            longest = longest.iter().flat_map(|p| pool.iter().map(move |e| [p.as_slice(), &[*e]].concat())).collect();
            paths.extend(longest.iter().cloned());
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
