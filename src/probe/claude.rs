//! Claude usage: read the capture file written by the claude-limits-tap
//! statusLine wrapper. Pure file read — no network, no credentials.

use super::{ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_f64, unix_now};

pub const STATE_FILE: &str = ".local/state/herdr-claude-limits/rate_limits.json";

fn state_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(STATE_FILE))
}

fn window(v: &serde_json::Value, label: &'static str) -> Option<UsageWindow> {
    let pct = json_f64(v.get("used_percentage")?)?;
    let resets = json_f64(v.get("resets_at")?)? as i64;
    UsageWindow::new(label, pct, resets)
}

pub fn fetch() -> ProbeResult {
    let path = state_path().ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no home dir"))?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProbeError::new(ErrorCode::NoData, "no capture yet"));
        }
        Err(e) => return Err(ProbeError::new(ErrorCode::Failed, e.to_string())),
    };
    let data: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| ProbeError::new(ErrorCode::Failed, e.to_string()))?;

    let mut windows = Vec::new();
    if let Some(w) = window(&data["five_hour"], "5h") {
        windows.push(w);
    }
    if let Some(w) = window(&data["seven_day"], "7d") {
        windows.push(w);
    }
    if windows.is_empty() {
        return Err(ProbeError::new(ErrorCode::NoData, "no windows in capture"));
    }
    let fetched_at = json_f64(data.get("captured_at").unwrap_or(&serde_json::Value::Null))
        .map(|f| f as i64)
        .unwrap_or_else(unix_now);
    Ok(FetchOk { windows, plan: None, overage_micros: None, fetched_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The percent of a window read with this `used_percentage`, as bits so
    /// -0.0 and 0.0 differ.
    fn percent(used: serde_json::Value) -> Option<u64> {
        window(&serde_json::json!({"used_percentage": used, "resets_at": 1_800_000_000}), "5h")
            .map(|w| w.used_percent.to_bits())
    }

    #[test]
    fn non_finite_percent_drops_the_window() {
        for used in ["NaN", "-nan", "inf", "-inf", "infinity", "+Infinity"] {
            assert_eq!(percent(used.into()), None, "{used}");
        }
    }

    #[test]
    fn out_of_range_percent_is_clamped() {
        for (used, want) in [
            (serde_json::json!(-5), 0.0),
            (serde_json::json!(-0.0), 0.0),
            (serde_json::json!("-0"), 0.0),
            (serde_json::json!(101), 100.0),
            (serde_json::json!(150.5), 100.0),
            (serde_json::json!("1e300"), 100.0),
        ] {
            assert_eq!(percent(used.clone()), Some(f64::to_bits(want)), "{used}");
        }
    }

    #[test]
    fn in_range_percent_is_unchanged() {
        for (used, want) in [
            (serde_json::json!(0), 0.0),
            (serde_json::json!(100), 100.0),
            (serde_json::json!(42.5), 42.5),
            (serde_json::json!(" 7.25 "), 7.25),
        ] {
            assert_eq!(percent(used.clone()), Some(f64::to_bits(want)), "{used}");
        }
    }
}
