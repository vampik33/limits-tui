pub mod claude;
pub mod codex;
pub mod devin;

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
    /// Account uses non-quota billing (devin).
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
}
