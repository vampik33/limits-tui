//! Devin CLI quota usage: read the credentials TOML and POST GetUserStatus to
//! the configured API server. Port of herdr-devin-limits' devin_limits.py.

use std::time::{Duration, Instant};

use super::{ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_int, unix_now};

const DEFAULT_API_SERVER_URL: &str = "https://server.codeium.com";
const STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const DEFAULT_TIMEOUT_SECS: f64 = 15.0;
const QUOTA_BILLING_STRATEGY: &str = "BILLING_STRATEGY_QUOTA";

struct Credentials {
    api_key: String,
    api_server_url: String,
}

fn load_credentials() -> Result<Credentials, ProbeError> {
    let path = std::env::var("DEVIN_CREDENTIALS")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/devin/credentials.toml")))
        .ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no home dir"))?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| ProbeError::new(ErrorCode::NotSignedIn, "credentials file is missing or invalid"))?;
    let creds: toml::Value = toml::from_str(&text)
        .map_err(|_| ProbeError::new(ErrorCode::NotSignedIn, "credentials file is missing or invalid"))?;
    let api_key = creds
        .get("windsurf_api_key")
        .and_then(|k| k.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ProbeError::new(ErrorCode::NotSignedIn, "credentials file has no API key"))?
        .to_string();
    let api_server_url = creds
        .get("api_server_url")
        .and_then(|u| u.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(DEFAULT_API_SERVER_URL)
        .to_string();
    Ok(Credentials { api_key, api_server_url })
}

/// The GetUserStatus endpoint on the configured server. The request carries
/// the API key, so only https is allowed.
fn status_url(api_server_url: &str) -> Result<String, ProbeError> {
    if !api_server_url.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://")) {
        return Err(ProbeError::new(ErrorCode::Failed, "API server URL must use https"));
    }
    Ok(format!("{}{}", api_server_url.trim_end_matches('/'), STATUS_PATH))
}

/// Server-provided detail, or a fallback; the API key is always redacted.
fn server_detail(body: &str, fallback: &str, key: &str) -> String {
    let mut detail = fallback.to_string();
    if let Ok(data) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(msg) = data.get("message").and_then(|m| m.as_str())
    {
        detail = msg.to_string();
    }
    detail.replace(key, "[redacted]")
}

fn map_error(e: ureq::Error) -> ProbeError {
    match e {
        ureq::Error::Timeout(_) => ProbeError::new(ErrorCode::Timeout, "request timed out"),
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            ProbeError::new(ErrorCode::Timeout, "request timed out")
        }
        ureq::Error::Io(_) => ProbeError::new(ErrorCode::Failed, "could not connect to API server"),
        other => ProbeError::new(ErrorCode::Failed, other.to_string()),
    }
}

pub fn fetch() -> ProbeResult {
    let timeout = std::env::var("DEVIN_LIMITS_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .max(0.001);
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);

    let creds = load_credentials()?;
    let url = status_url(&creds.api_server_url)?;
    let body = serde_json::json!({
        "metadata": {
            "apiKey": creds.api_key,
            "ideName": "devin-cli",
            "ideVersion": "0.1.0",
            "extensionName": "limits-tui",
            "extensionVersion": "0.1.0",
        }
    });

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(deadline - Instant::now()))
        .http_status_as_error(false) // read the body ourselves, like the Python probe
        .https_only(true) // also refuse redirects to plain http
        .build()
        .into();
    let mut response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .send(body.to_string())
        .map_err(map_error)?;

    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|e| ProbeError::new(ErrorCode::Failed, e.to_string()))?;

    if Instant::now() > deadline {
        return Err(ProbeError::new(ErrorCode::Timeout, "request timed out"));
    }
    if status == 401 || status == 403 {
        return Err(ProbeError::new(
            ErrorCode::NotSignedIn,
            server_detail(&text, &format!("HTTP {status}"), &creds.api_key),
        ));
    }
    if status != 200 {
        return Err(ProbeError::new(
            ErrorCode::Failed,
            server_detail(&text, &format!("HTTP {status}"), &creds.api_key),
        ));
    }
    let payload: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| ProbeError::new(ErrorCode::Failed, "API response is not valid JSON"))?;
    normalize(&payload, &creds.api_key)
}

fn normalize(payload: &serde_json::Value, key: &str) -> ProbeResult {
    if let Some(code) = payload.get("code").and_then(|c| c.as_str())
        && (code == "unauthenticated" || code == "permission_denied")
    {
        return Err(ProbeError::new(
            ErrorCode::NotSignedIn,
            server_detail(&payload.to_string(), "authentication failed", key),
        ));
    }
    let plan_status = payload.pointer("/userStatus/planStatus");
    let plan_info = plan_status.and_then(|s| s.get("planInfo")).filter(|p| p.is_object());
    let (Some(plan_status), Some(plan_info)) = (plan_status, plan_info) else {
        return Err(ProbeError::new(ErrorCode::Failed, "API response has no plan information"));
    };

    if let Some(strategy) = plan_info.get("billingStrategy")
        && strategy.as_str() != Some(QUOTA_BILLING_STRATEGY)
    {
        return Err(ProbeError::new(ErrorCode::NoQuota, "account does not use quota billing"));
    }

    let mut windows = Vec::new();
    for (label, remaining_field, reset_field, hidden_field) in [
        ("day", "dailyQuotaRemainingPercent", "dailyQuotaResetAtUnix", "hideDailyQuota"),
        ("wk", "weeklyQuotaRemainingPercent", "weeklyQuotaResetAtUnix", "hideWeeklyQuota"),
    ] {
        match plan_info.get(hidden_field) {
            None | Some(serde_json::Value::Bool(false)) => {}
            Some(serde_json::Value::Bool(true)) => continue,
            _ => {
                return Err(ProbeError::new(ErrorCode::Failed, "API response has an invalid quota visibility flag"));
            }
        }
        let Some(reset) = json_int(&plan_status[reset_field]).filter(|r| *r > 0) else {
            continue;
        };
        let remaining = json_int(&plan_status[remaining_field]).unwrap_or(0);
        let used = (100 - remaining).clamp(0, 100) as f64;
        windows.push(UsageWindow { label, used_percent: used, resets_at: reset });
    }
    if windows.is_empty() {
        return Err(ProbeError::new(ErrorCode::NoQuota, "no quota windows are available"));
    }

    let overage = json_int(&plan_status["overageBalanceMicros"])
        .ok_or_else(|| ProbeError::new(ErrorCode::Failed, "API response has an invalid overage balance"))?;
    let plan = match plan_info.get("planName") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => {
            return Err(ProbeError::new(ErrorCode::Failed, "API response has an invalid plan name"));
        }
    };
    Ok(FetchOk { windows, plan, overage_micros: Some(overage), fetched_at: unix_now() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_url_accepts_https() {
        assert_eq!(
            status_url("https://server.codeium.com").unwrap(),
            format!("https://server.codeium.com{STATUS_PATH}")
        );
    }

    #[test]
    fn status_url_accepts_uppercase_scheme() {
        assert_eq!(status_url("HTTPS://example.com").unwrap(), format!("HTTPS://example.com{STATUS_PATH}"));
    }

    #[test]
    fn status_url_trims_trailing_slashes() {
        assert_eq!(status_url("https://example.com//").unwrap(), format!("https://example.com{STATUS_PATH}"));
    }

    #[test]
    fn status_url_rejects_plain_http() {
        let err = status_url("http://example.com").unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(err.detail.contains("https"), "detail: {}", err.detail);
    }

    #[test]
    fn status_url_rejects_uppercase_plain_http() {
        assert!(status_url("HTTP://example.com").is_err());
    }

    #[test]
    fn status_url_rejects_missing_scheme() {
        assert!(status_url("server.codeium.com").is_err());
    }

    #[test]
    fn status_url_rejects_short_and_empty() {
        assert!(status_url("").is_err());
        assert!(status_url("https:/").is_err());
    }

    #[test]
    fn status_url_rejects_other_schemes_starting_with_https() {
        assert!(status_url("https+unix://example.com").is_err());
        assert!(status_url("httpsx://example.com").is_err());
    }
}
