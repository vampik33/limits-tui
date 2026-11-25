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

/// One quota/rate-limit window as the UI needs it, normalized across sources.
pub struct UsageWindow {
    /// Short label for the window: "5h", "day", "wk".
    pub label: &'static str,
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: i64,
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
    fn timeout_infinite_or_huge_is_capped_without_panicking() {
        for raw in ["inf", "infinity", "1e20", "1e300", "86401"] {
            assert_eq!(parse_timeout(Some(raw), 15.0), MAX, "{raw}");
        }
        // The cap leaves room to build a deadline from it.
        let _ = std::time::Instant::now() + parse_timeout(Some("inf"), 15.0);
    }
}
