//! Claude usage: read the capture file written by the claude-limits-tap
//! statusLine wrapper. Pure file read — no network, no credentials.

use super::{ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_f64, read_small_file, unix_now};

pub const STATE_FILE: &str = ".local/state/herdr-claude-limits/rate_limits.json";

/// How far past now a capture's `captured_at` may be. The tap stamps it with
/// jq's `now` from this machine's clock, the one `unix_now` reads, so a real
/// capture is never ahead unless the clock stepped back since it was written
/// (an NTP correction: seconds, rarely more). A minute covers that and delays
/// the `CLAUDE_STALE_SECS` dimming by at most a minute; a later timestamp
/// (milliseconds, `1e300`) would keep a capture fresh forever.
const CAPTURE_CLOCK_SKEW_SECS: i64 = 60;

fn state_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(STATE_FILE))
}

/// The number at `key` in `v`: `None` when absent or null, Failed with
/// `invalid` when present but not a finite number.
fn number(v: &serde_json::Value, key: &str, invalid: &'static str) -> Result<Option<f64>, ProbeError> {
    match v.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(n) => match json_f64(n) {
            Some(f) if f.is_finite() => Ok(Some(f)),
            _ => Err(ProbeError::new(ErrorCode::Failed, invalid)),
        },
    }
}

/// The window `v`, if the capture has one with both fields. A window or field
/// that is present but unreadable makes the capture malformed: Failed, not a
/// silently dropped window.
fn window(v: Option<&serde_json::Value>, label: &'static str) -> Result<Option<UsageWindow>, ProbeError> {
    let v = match v {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(v) if v.is_object() => v,
        Some(_) => return Err(ProbeError::new(ErrorCode::Failed, "capture has an invalid usage window")),
    };
    let pct = number(v, "used_percentage", "capture has an invalid usage percentage")?;
    let resets = number(v, "resets_at", "capture has an invalid reset time")?;
    Ok(pct.zip(resets).and_then(|(pct, resets)| UsageWindow::new(label, pct, resets as i64)))
}

pub fn fetch() -> ProbeResult {
    let path = state_path().ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no home dir"))?;
    fetch_from(&path, unix_now())
}

fn fetch_from(path: &std::path::Path, now: i64) -> ProbeResult {
    let text = match read_small_file(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProbeError::new(ErrorCode::NoData, "no capture yet"));
        }
        Err(e) => return Err(ProbeError::new(ErrorCode::Failed, e.to_string())),
    };
    let data: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| ProbeError::new(ErrorCode::Failed, e.to_string()))?;
    // Valid JSON that isn't an object has no window keys: malformed, not NoData.
    if !data.is_object() {
        return Err(ProbeError::new(ErrorCode::Failed, "capture is not a JSON object"));
    }

    let mut windows = Vec::new();
    if let Some(w) = window(data.get("five_hour"), "5h")? {
        windows.push(w);
    }
    if let Some(w) = window(data.get("seven_day"), "7d")? {
        windows.push(w);
    }
    // Only an absent or null timestamp means now: an unreadable one read as now
    // would show an old capture as fresh.
    let fetched_at = number(&data, "captured_at", "capture has an invalid timestamp")?.map_or(now, |f| f as i64);
    // A capture from the future would never go stale.
    if fetched_at > now.saturating_add(CAPTURE_CLOCK_SKEW_SECS) {
        return Err(ProbeError::new(ErrorCode::Failed, "capture has an invalid timestamp"));
    }
    if windows.is_empty() {
        return Err(ProbeError::new(ErrorCode::NoData, "no windows in capture"));
    }
    Ok(FetchOk { windows, plan: None, overage_micros: None, fetched_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MAX_FILE_BYTES;
    use crate::probe::tests::{Scratch, promptly};

    /// The percent of a window read with this `used_percentage`, as bits so
    /// -0.0 and 0.0 differ.
    fn percent(used: serde_json::Value) -> Option<u64> {
        window(Some(&serde_json::json!({"used_percentage": used, "resets_at": 1_800_000_000})), "5h")
            .expect("a finite percent is readable")
            .map(|w| w.used_percent.to_bits())
    }

    #[test]
    fn non_finite_percent_fails() {
        for used in ["NaN", "-nan", "inf", "-inf", "infinity", "+Infinity"] {
            let v = serde_json::json!({"used_percentage": used, "resets_at": 1_800_000_000});
            let err = window(Some(&v), "5h").err().expect(used);
            assert_eq!(err.code, ErrorCode::Failed, "{used}: {}", err.detail);
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

    const CAPTURE: &str = r#"{"five_hour":{"used_percentage":1,"resets_at":2}}"#;

    #[test]
    fn capture_is_read() {
        let scratch = Scratch::new("capture");
        let path = scratch.path("rate_limits.json");
        std::fs::write(&path, CAPTURE).unwrap();
        assert_eq!(fetch_from(&path, unix_now()).unwrap().windows.len(), 1);
    }

    #[test]
    fn capture_through_a_symlink_is_read() {
        let scratch = Scratch::new("capturelink");
        let (target, link) = (scratch.path("target.json"), scratch.path("rate_limits.json"));
        std::fs::write(&target, CAPTURE).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(fetch_from(&link, unix_now()).unwrap().windows.len(), 1);
    }

    #[test]
    fn missing_capture_is_no_data() {
        let scratch = Scratch::new("capturemissing");
        assert_eq!(
            fetch_from(&scratch.path("rate_limits.json"), unix_now()).err().expect("should fail").code,
            ErrorCode::NoData
        );
    }

    #[test]
    fn fifo_capture_fails_without_blocking() {
        let scratch = Scratch::new("capturefifo");
        let fifo = scratch.fifo("rate_limits.json");
        let err = promptly(move || fetch_from(&fifo, unix_now()).err()).expect("should fail");
        assert_eq!(err.code, ErrorCode::Failed, "{}", err.detail);
    }

    #[test]
    fn oversized_capture_fails() {
        let scratch = Scratch::new("capturebig");
        let path = scratch.path("rate_limits.json");
        std::fs::write(&path, format!("{CAPTURE}{}", " ".repeat(MAX_FILE_BYTES as usize))).unwrap();
        let err = fetch_from(&path, unix_now()).err().expect("should fail");
        assert_eq!(err.code, ErrorCode::Failed, "{}", err.detail);
    }

    /// fetch_from on a capture file holding `text`.
    fn fetch_text(text: &str) -> ProbeResult {
        fetch_text_at(text, unix_now())
    }

    /// fetch_from on a capture file holding `text`, read at `now`.
    fn fetch_text_at(text: &str, now: i64) -> ProbeResult {
        let scratch = Scratch::new("capturetext");
        let path = scratch.path("rate_limits.json");
        std::fs::write(&path, text).unwrap();
        fetch_from(&path, now)
    }

    /// The detail of the Failed error fetch_from gives for `text`.
    fn failure(text: &str) -> String {
        let err = fetch_text(text).err().unwrap_or_else(|| panic!("should fail: {text}"));
        assert_eq!(err.code, ErrorCode::Failed, "{text}: {}", err.detail);
        err.detail
    }

    const WINDOW: &str = r#"{"used_percentage":1,"resets_at":2}"#;

    /// Present values that aren't a finite number, as JSON text.
    const UNREADABLE: [&str; 14] = [
        r#""x""#,
        r#""""#,
        r#""NaN""#,
        r#""inf""#,
        r#""-infinity""#,
        r#""1e400""#,
        "{}",
        r#"{"n":1}"#,
        "[]",
        "[1700000000]",
        "true",
        "false",
        "1e400",
        "-1e400",
    ];

    #[test]
    fn unreadable_captured_at_fails() {
        for at in UNREADABLE {
            for windows in [format!(r#""five_hour":{WINDOW},"#), String::new()] {
                let capture = format!(r#"{{{windows}"captured_at":{at}}}"#);
                assert_eq!(failure(&capture), "capture has an invalid timestamp", "{capture}");
            }
        }
    }

    #[test]
    fn absent_or_null_captured_at_is_now() {
        for capture in
            [format!(r#"{{"five_hour":{WINDOW}}}"#), format!(r#"{{"five_hour":{WINDOW},"captured_at":null}}"#)]
        {
            let before = unix_now();
            let at = fetch_text(&capture).unwrap().fetched_at;
            assert!((before..=unix_now()).contains(&at), "{capture}: {at}");
        }
    }

    #[test]
    fn readable_captured_at_is_kept() {
        for (at, want) in [
            ("1764130000", 1_764_130_000),
            // jq's `now`, which the tap writes, is fractional.
            ("1764130000.789", 1_764_130_000),
            ("1.7641300007e9", 1_764_130_000),
            (r#"" 1764130000.5 ""#, 1_764_130_000),
            ("0", 0),
            ("-0.5", 0),
            ("-1.5", -1),
            // Finite but past i64: saturates, never wraps or panics.
            ("-1e300", i64::MIN),
        ] {
            let capture = format!(r#"{{"five_hour":{WINDOW},"captured_at":{at}}}"#);
            assert_eq!(fetch_text(&capture).unwrap().fetched_at, want, "{capture}");
        }
    }

    /// A fixed now for captures read at a known time.
    const NOW: i64 = 1_764_130_000;

    #[test]
    fn future_captured_at_fails() {
        for at in [
            "1764130061",
            "1764130061.5",
            r#"" 1764130061 ""#,
            "1764130120",
            "1764216400",
            // A writer that stores milliseconds.
            "1764130000000",
            "1764130000000.789",
            // Finite but past i64: saturates to i64::MAX, still in the future.
            "1e300",
            "9223372036854775807",
            "1e19",
        ] {
            for windows in [format!(r#""five_hour":{WINDOW},"#), String::new()] {
                let capture = format!(r#"{{{windows}"captured_at":{at}}}"#);
                let err = fetch_text_at(&capture, NOW).err().unwrap_or_else(|| panic!("should fail: {capture}"));
                assert_eq!(err.code, ErrorCode::Failed, "{capture}: {}", err.detail);
                assert_eq!(err.detail, "capture has an invalid timestamp", "{capture}");
            }
        }
    }

    #[test]
    fn captured_at_within_the_skew_is_kept() {
        for (at, want) in [
            ("1764130000", NOW),
            ("1764130000.9", NOW),
            ("1764130001", NOW + 1),
            ("1764130059", NOW + 59),
            ("1764130060", NOW + CAPTURE_CLOCK_SKEW_SECS),
            // Truncated to whole seconds like jq's fractional `now`.
            ("1764130060.999", NOW + CAPTURE_CLOCK_SKEW_SECS),
            (r#""1764130060""#, NOW + CAPTURE_CLOCK_SKEW_SECS),
            ("1764129999", NOW - 1),
            ("0", 0),
        ] {
            let capture = format!(r#"{{"five_hour":{WINDOW},"captured_at":{at}}}"#);
            let got = fetch_text_at(&capture, NOW).unwrap_or_else(|e| panic!("{capture}: {}", e.detail));
            assert_eq!(got.fetched_at, want, "{capture}");
        }
    }

    #[test]
    fn captured_at_is_kept_exactly_up_to_the_skew() {
        for offset in -2 * CAPTURE_CLOCK_SKEW_SECS..=2 * CAPTURE_CLOCK_SKEW_SECS {
            let capture = format!(r#"{{"five_hour":{WINDOW},"captured_at":{}}}"#, NOW + offset);
            match fetch_text_at(&capture, NOW) {
                Ok(got) => {
                    assert!(offset <= CAPTURE_CLOCK_SKEW_SECS, "{capture}: kept");
                    assert_eq!(got.fetched_at, NOW + offset, "{capture}");
                }
                Err(e) => {
                    assert!(offset > CAPTURE_CLOCK_SKEW_SECS, "{capture}: {}", e.detail);
                    assert_eq!((e.code, e.detail.as_str()), (ErrorCode::Failed, "capture has an invalid timestamp"));
                }
            }
        }
    }

    #[test]
    fn absent_or_null_captured_at_is_the_read_time() {
        for now in [NOW, 0, i64::MAX] {
            for capture in
                [format!(r#"{{"five_hour":{WINDOW}}}"#), format!(r#"{{"five_hour":{WINDOW},"captured_at":null}}"#)]
            {
                assert_eq!(fetch_text_at(&capture, now).unwrap().fetched_at, now, "{capture}");
            }
        }
    }

    #[test]
    fn skew_check_saturates_at_an_extreme_now() {
        // now + skew past i64 must not overflow; i64::MAX is then not ahead.
        for at in ["1e300", "9223372036854775807"] {
            let capture = format!(r#"{{"five_hour":{WINDOW},"captured_at":{at}}}"#);
            assert_eq!(fetch_text_at(&capture, i64::MAX).unwrap().fetched_at, i64::MAX, "{capture}");
        }
    }

    #[test]
    fn non_object_capture_fails() {
        for capture in [
            "null",
            " null ",
            "[]",
            "[{}]",
            &format!(r#"[{{"five_hour":{WINDOW}}}]"#),
            r#""x""#,
            r#""""#,
            "42",
            "0",
            "-1.5",
            "1764130000",
            "true",
            "false",
        ] {
            assert_eq!(failure(capture), "capture is not a JSON object", "{capture}");
        }
    }

    #[test]
    fn unreadable_window_field_fails() {
        for bad in UNREADABLE {
            for (field, detail) in [
                (format!(r#"{{"used_percentage":{bad},"resets_at":2}}"#), "capture has an invalid usage percentage"),
                (format!(r#"{{"used_percentage":{bad}}}"#), "capture has an invalid usage percentage"),
                (format!(r#"{{"used_percentage":1,"resets_at":{bad}}}"#), "capture has an invalid reset time"),
                (format!(r#"{{"resets_at":{bad}}}"#), "capture has an invalid reset time"),
                (format!(r#"{{"used_percentage":null,"resets_at":{bad}}}"#), "capture has an invalid reset time"),
            ] {
                // Alone, or beside a readable window that must not hide it.
                for capture in [
                    format!(r#"{{"five_hour":{field}}}"#),
                    format!(r#"{{"five_hour":{field},"seven_day":{WINDOW}}}"#),
                    format!(r#"{{"five_hour":{WINDOW},"seven_day":{field}}}"#),
                    format!(r#"{{"five_hour":{WINDOW},"seven_day":{field},"captured_at":1764130000}}"#),
                ] {
                    assert_eq!(failure(&capture), detail, "{capture}");
                }
            }
        }
    }

    #[test]
    fn non_object_window_fails() {
        for bad in ["1", "0", r#""x""#, r#""""#, "[]", "[1,2]", "true", "false"] {
            for capture in [
                format!(r#"{{"five_hour":{bad}}}"#),
                format!(r#"{{"seven_day":{bad}}}"#),
                format!(r#"{{"five_hour":{WINDOW},"seven_day":{bad}}}"#),
                format!(r#"{{"five_hour":{bad},"seven_day":{WINDOW}}}"#),
            ] {
                assert_eq!(failure(&capture), "capture has an invalid usage window", "{capture}");
            }
        }
    }

    #[test]
    fn absent_or_null_window_or_field_is_skipped() {
        for (five_hour, windows) in [
            ("null", vec!["7d"]),
            (r#"{"used_percentage":null,"resets_at":2}"#, vec!["7d"]),
            (r#"{"used_percentage":1,"resets_at":null}"#, vec!["7d"]),
            (r#"{"resets_at":2}"#, vec!["7d"]),
            (r#"{"used_percentage":1}"#, vec!["7d"]),
            ("{}", vec!["7d"]),
            (WINDOW, vec!["5h", "7d"]),
        ] {
            let capture = format!(r#"{{"five_hour":{five_hour},"seven_day":{WINDOW},"captured_at":null}}"#);
            let labels: Vec<_> = fetch_text(&capture).unwrap().windows.iter().map(|w| w.label).collect();
            assert_eq!(labels, windows, "{capture}");
        }
        let labels: Vec<_> =
            fetch_text(&format!(r#"{{"seven_day":{WINDOW}}}"#)).unwrap().windows.iter().map(|w| w.label).collect();
        assert_eq!(labels, ["7d"]);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let capture = format!(r#"{{"five_hour":{WINDOW},"ten_day":"x","other":{{"resets_at":"x"}}}}"#);
        assert_eq!(fetch_text(&capture).unwrap().windows.len(), 1);
    }

    #[test]
    fn capture_without_readable_windows_is_no_data() {
        for capture in [
            "{}",
            r#"{"captured_at":1764130000}"#,
            r#"{"five_hour":null,"seven_day":null}"#,
            r#"{"five_hour":{"resets_at":2},"seven_day":{}}"#,
        ] {
            let err = fetch_text(capture).err().unwrap_or_else(|| panic!("should fail: {capture}"));
            assert_eq!(err.code, ErrorCode::NoData, "{capture}: {}", err.detail);
        }
    }

    #[test]
    fn fractional_window_fields_are_read() {
        let w = fetch_text(r#"{"five_hour":{"used_percentage":"42.5","resets_at":1800000000.9}}"#).unwrap();
        assert_eq!((w.windows[0].used_percent, w.windows[0].resets_at), (42.5, 1_800_000_000));
    }
}
