use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::probe::unix_now;
use crate::{App, CLAUDE_STALE_SECS, SourceKind, SourceState};

const METER_W: usize = 12;
const NAME_W: usize = 8;

/// "48m", "4h 48m", "2d 2h" — same shapes as the mods.
fn countdown(secs: i64) -> String {
    let m = secs.max(0) / 60;
    if m < 60 {
        format!("{m}m")
    } else if m < 1440 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{}d {}h", m / 1440, (m % 1440) / 60)
    }
}

/// "5m", "3h", "2d" — age of a fetch/capture.
fn age(secs: i64) -> String {
    let m = (secs / 60).max(1);
    if m < 60 {
        format!("{m}m")
    } else if m < 1440 {
        format!("{}h", m / 60)
    } else {
        format!("{}d", m / 1440)
    }
}

fn level_color(pct: f64) -> Option<Color> {
    if pct >= 95.0 {
        Some(Color::Red)
    } else if pct >= 80.0 {
        Some(Color::Yellow)
    } else {
        None
    }
}

fn meter(pct: f64) -> String {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * METER_W as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(METER_W - filled))
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

struct Brand {
    icon: &'static str,
    name: &'static str,
    color: Color,
}

fn brand(kind: SourceKind) -> Brand {
    match kind {
        SourceKind::Claude => Brand { icon: "✻", name: "Claude", color: Color::Rgb(217, 119, 87) },
        SourceKind::Codex => Brand { icon: "✱", name: "Codex", color: Color::Rgb(116, 185, 155) },
        SourceKind::Devin => Brand { icon: "◆", name: "Devin", color: Color::Rgb(139, 124, 246) },
    }
}

fn source_row(kind: SourceKind, state: &SourceState) -> Line<'static> {
    let b = brand(kind);
    let now = unix_now();

    // Claude dims when its capture is old; codex/devin dim on stale-after-error
    // or while a manual refresh is in flight.
    let stale = match kind {
        SourceKind::Claude => state.last.as_ref().is_some_and(|d| now - d.fetched_at > CLAUDE_STALE_SECS),
        _ => state.error.is_some() && state.last.is_some(),
    };
    let dimmed = stale || (state.updating && state.busy);
    let fg = if dimmed { Color::DarkGray } else { Color::White };

    let mut spans = vec![
        Span::styled(format!(" {} ", b.icon), Style::default().fg(if dimmed { Color::DarkGray } else { b.color })),
        Span::styled(
            format!("{:<width$}", b.name, width = NAME_W),
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        ),
    ];

    match (&state.last, &state.error) {
        (Some(data), _) => {
            let live: Vec<_> = data.windows.iter().filter(|w| w.resets_at > now).collect();
            if live.is_empty() {
                spans.push(Span::styled("…", dim_style()));
            }
            for (i, w) in live.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled("  ·  ", dim_style()));
                }
                let pct = w.used_percent;
                let level = if dimmed { Some(Color::DarkGray) } else { level_color(pct) };
                let bar_color = level.unwrap_or(if dimmed { Color::DarkGray } else { Color::Gray });
                spans.push(Span::styled(format!("{} ", w.label), dim_style()));
                spans.push(Span::styled(meter(pct), Style::default().fg(bar_color)));
                spans.push(Span::styled(
                    format!(" {:>3}%", pct.round() as i64),
                    Style::default().fg(level.unwrap_or(fg)),
                ));
                spans.push(Span::styled(format!(" {}", countdown(w.resets_at - now)), Style::default().fg(fg)));
            }
            if let Some(micros) = data.overage_micros.filter(|m| *m > 0) {
                spans.push(Span::styled("  ·  ", dim_style()));
                spans.push(Span::styled(format!("${:.2} over", micros as f64 / 1e6), dim_style()));
            }
            if let Some(plan) = &data.plan {
                spans.push(Span::styled("  ·  ", dim_style()));
                spans.push(Span::styled(plan.clone(), dim_style()));
            }
        }
        (None, Some(e)) => {
            let msg = match (kind, e.code) {
                (SourceKind::Claude, crate::probe::ErrorCode::NoData) => "… (start a Claude Code session)",
                _ => e.code.label(),
            };
            spans.push(Span::styled(msg, dim_style()));
        }
        (None, None) => spans.push(Span::styled("…", dim_style())),
    }

    if let Some(d) = &state.last {
        // The last refresh failed: say why, so a sign-in problem doesn't look
        // like ordinary staleness.
        if let Some(e) = &state.error {
            let msg = if e.code == crate::probe::ErrorCode::Failed { "update failed" } else { e.code.label() };
            spans.push(Span::styled(format!("  ·  {msg}"), dim_style()));
        }
        if stale {
            spans.push(Span::styled(format!("  ·  {} ago", age(now - d.fetched_at)), dim_style()));
        }
    } else if state.busy {
        spans.push(Span::styled("  updating…", dim_style()));
    }

    Line::from(spans)
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let outer = Block::default().borders(Borders::ALL).title(" limits ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // padding
        Constraint::Length(1), // claude
        Constraint::Length(1), // codex
        Constraint::Length(1), // devin
        Constraint::Min(1),    // gap
        Constraint::Length(1), // help
    ])
    .split(inner);

    for (kind, row) in [(SourceKind::Claude, rows[1]), (SourceKind::Codex, rows[2]), (SourceKind::Devin, rows[3])] {
        f.render_widget(Paragraph::new(source_row(kind, app.source_ref(kind))), row);
    }

    let help = Line::from(vec![
        Span::styled(" r", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(" refresh   ", dim_style()),
        Span::styled("q", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(" quit", dim_style()),
    ]);
    f.render_widget(Paragraph::new(help), rows[5]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{ErrorCode, FetchOk, ProbeError, UsageWindow};

    fn cached(resets_in: i64, fetched_ago: i64) -> FetchOk {
        let now = unix_now();
        FetchOk {
            windows: vec![UsageWindow { label: "5h", used_percent: 42.0, resets_at: now + resets_in }],
            plan: None,
            overage_micros: None,
            fetched_at: now - fetched_ago,
        }
    }

    fn row_text(kind: SourceKind, last: Option<FetchOk>, error: Option<ErrorCode>) -> String {
        let mut state = SourceState::new();
        state.last = last;
        state.error = error.map(|code| ProbeError::new(code, "detail"));
        source_row(kind, &state).spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn codex_cached_with_auth_error_shows_not_signed_in() {
        let text = row_text(SourceKind::Codex, Some(cached(3600, 60)), Some(ErrorCode::NotSignedIn));
        assert!(text.contains("42%"), "{text}");
        assert!(text.contains("not signed in"), "{text}");
        assert!(text.contains("ago"), "{text}");
    }

    #[test]
    fn codex_cached_with_generic_error_shows_update_failed() {
        let text = row_text(SourceKind::Codex, Some(cached(3600, 60)), Some(ErrorCode::Failed));
        assert!(text.contains("update failed"), "{text}");
        assert!(text.contains("ago"), "{text}");
    }

    #[test]
    fn devin_cached_with_timeout_shows_timeout() {
        let text = row_text(SourceKind::Devin, Some(cached(3600, 60)), Some(ErrorCode::Timeout));
        assert!(text.contains("timeout"), "{text}");
    }

    #[test]
    fn codex_expired_windows_with_auth_error_shows_not_signed_in() {
        let text = row_text(SourceKind::Codex, Some(cached(-10, 600)), Some(ErrorCode::NotSignedIn));
        assert!(text.contains('…'), "{text}");
        assert!(text.contains("not signed in"), "{text}");
    }

    #[test]
    fn claude_fresh_with_error_shows_update_failed_without_age() {
        let text = row_text(SourceKind::Claude, Some(cached(3600, 60)), Some(ErrorCode::Failed));
        assert!(text.contains("update failed"), "{text}");
        assert!(!text.contains("ago"), "{text}");
    }

    #[test]
    fn claude_stale_with_error_shows_label_and_age() {
        let text = row_text(SourceKind::Claude, Some(cached(3600, CLAUDE_STALE_SECS + 60)), Some(ErrorCode::Failed));
        assert!(text.contains("update failed"), "{text}");
        assert!(text.contains("ago"), "{text}");
    }

    #[test]
    fn cached_without_error_shows_no_error_label() {
        for kind in [SourceKind::Claude, SourceKind::Codex, SourceKind::Devin] {
            let text = row_text(kind, Some(cached(3600, 60)), None);
            assert!(text.contains("42%"), "{text}");
            for label in ["update failed", "not signed in", "timeout", "ago"] {
                assert!(!text.contains(label), "{text}");
            }
        }
    }

    #[test]
    fn no_cache_with_auth_error_shows_not_signed_in() {
        let text = row_text(SourceKind::Codex, None, Some(ErrorCode::NotSignedIn));
        assert!(text.contains("not signed in"), "{text}");
    }
}
