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
    Some(UsageWindow { label, used_percent: pct, resets_at: resets })
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
