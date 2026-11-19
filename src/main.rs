mod probe;
mod ui;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use probe::{FetchOk, ProbeError, ProbeResult, unix_now};

/// Claude's capture file is a cheap local read.
const CLAUDE_POLL: Duration = Duration::from_secs(15);
/// Codex/Devin spawn a process or hit the network — poll like the mods do.
const PROBE_POLL: Duration = Duration::from_secs(5 * 60);
const PROBE_RETRY: Duration = Duration::from_secs(60);
/// A running probe answers through a channel that `event::poll` can't watch,
/// so check it this often until the probe is done.
const PROBE_CHECK: Duration = Duration::from_millis(100);
/// Refetch just after a reset passes so the new window shows up promptly.
const AFTER_RESET: i64 = 15;
/// Claude data older than this renders dimmed.
const CLAUDE_STALE_SECS: i64 = 15 * 60;

#[derive(Clone, Copy)]
enum SourceKind {
    Claude,
    Codex,
    Devin,
}

/// Result of one probe fetch, tagged with its source.
struct FetchMsg {
    kind: SourceKind,
    result: ProbeResult,
}

struct SourceState {
    last: Option<FetchOk>,
    error: Option<ProbeError>,
    busy: bool,
    /// A manual refresh is in flight (for the "updating" dim).
    updating: bool,
    failures: u32,
    next_at: Instant,
}

impl SourceState {
    fn new() -> Self {
        Self { last: None, error: None, busy: false, updating: false, failures: 0, next_at: Instant::now() }
    }

    fn apply(&mut self, result: ProbeResult) {
        self.busy = false;
        self.updating = false;
        match result {
            Ok(data) => {
                // Refetch soon after the next reset, but never later than the poll interval.
                let mut next = Instant::now() + PROBE_POLL;
                let now = unix_now();
                for w in &data.windows {
                    let until = w.resets_at + AFTER_RESET - now;
                    if until > 0 {
                        next = next.min(Instant::now() + Duration::from_secs(until as u64));
                    }
                }
                self.next_at = next;
                self.last = Some(data);
                self.error = None;
                self.failures = 0;
            }
            Err(e) => {
                self.error = Some(e);
                self.failures += 1;
                let backoff = PROBE_RETRY.saturating_mul(1u32 << self.failures.saturating_sub(1).min(3));
                self.next_at = Instant::now() + backoff.min(PROBE_POLL);
            }
        }
    }
}

struct App {
    claude: SourceState,
    codex: SourceState,
    devin: SourceState,
}

impl App {
    fn source(&mut self, kind: SourceKind) -> &mut SourceState {
        match kind {
            SourceKind::Claude => &mut self.claude,
            SourceKind::Codex => &mut self.codex,
            SourceKind::Devin => &mut self.devin,
        }
    }
    fn source_ref(&self, kind: SourceKind) -> &SourceState {
        match kind {
            SourceKind::Claude => &self.claude,
            SourceKind::Codex => &self.codex,
            SourceKind::Devin => &self.devin,
        }
    }
}

fn spawn_probe(kind: SourceKind, tx: mpsc::Sender<FetchMsg>) {
    std::thread::spawn(move || {
        let result = match kind {
            SourceKind::Codex => probe::codex::fetch(),
            SourceKind::Devin => probe::devin::fetch(),
            SourceKind::Claude => unreachable!(),
        };
        let _ = tx.send(FetchMsg { kind, result });
    });
}

fn refresh_due(app: &mut App, tx: &mpsc::Sender<FetchMsg>, manual: bool) {
    for kind in [SourceKind::Claude, SourceKind::Codex, SourceKind::Devin] {
        let due = manual || app.source(kind).next_at <= Instant::now();
        if !due {
            continue;
        }
        match kind {
            SourceKind::Claude => {
                // Cheap file read: no worker needed.
                if !app.claude.busy {
                    app.claude.busy = true;
                    let result = probe::claude::fetch();
                    app.claude.apply(result);
                    app.claude.next_at = Instant::now() + CLAUDE_POLL;
                }
            }
            kind @ (SourceKind::Codex | SourceKind::Devin) => {
                let src = app.source(kind);
                if src.busy {
                    if manual {
                        src.updating = true;
                    }
                    continue;
                }
                src.busy = true;
                src.updating = manual;
                spawn_probe(kind, tx.clone());
            }
        }
    }
}

/// Earliest due fetch, ignoring sources with a probe in flight: their
/// `next_at` is already past and only `apply` moves it, so counting them would
/// spin the loop at its 10 ms floor until the probe answers.
fn next_wakeup(app: &App) -> Option<Instant> {
    [&app.claude, &app.codex, &app.devin].into_iter().filter(|s| !s.busy).map(|s| s.next_at).min()
}

/// How long to wait for a key: until the next due fetch, but at most 1 s (for
/// the countdown text), or PROBE_CHECK while a probe runs; at least 10 ms.
fn poll_wait(app: &App, now: Instant) -> Duration {
    let cap = if app.codex.busy || app.devin.busy { PROBE_CHECK } else { Duration::from_secs(1) };
    next_wakeup(app)
        .map_or(Duration::MAX, |at| at.saturating_duration_since(now))
        .min(cap)
        .max(Duration::from_millis(10))
}

/// One non-interactive snapshot for `--once` / unscripted checks (and for
/// terminals that can't host a TUI). Prints one line per source.
fn run_once() {
    for (name, result) in
        [("claude", probe::claude::fetch()), ("codex", probe::codex::fetch()), ("devin", probe::devin::fetch())]
    {
        println!("{}", once_line(name, &result));
    }
}

fn once_line(name: &str, result: &ProbeResult) -> String {
    match result {
        Ok(d) => {
            let windows: Vec<String> = d
                .windows
                .iter()
                .map(|w| format!("{}={}% (resets {})", w.label, w.used_percent.round(), w.resets_at))
                .collect();
            let mut extra = String::new();
            if let Some(p) = &d.plan {
                extra.push_str(&format!(" plan={p}"));
            }
            if let Some(m) = d.overage_micros {
                extra.push_str(&format!(" overage=${:.2}", m as f64 / 1e6));
            }
            format!("{name}: {}{}", windows.join(" "), extra)
        }
        Err(e) => format!("{name}: error {:?} {}", e.code, e.detail),
    }
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--once" || a == "-1") {
        run_once();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "limits-tui — Claude / Codex / Devin usage limits in one TUI\n\
             \n\
             Usage: limits-tui [--once]\n\
             \n\
             Keys: r refresh all · q / Esc / Ctrl-C quit\n\
             \n\
             Runs fine over SSH. Env: CODEX_BIN, CODEX_LIMITS_TIMEOUT,\n\
             DEVIN_CREDENTIALS, DEVIN_LIMITS_TIMEOUT, CLAUDE_CONFIG_DIR."
        );
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<FetchMsg>();
    let mut app = App { claude: SourceState::new(), codex: SourceState::new(), devin: SourceState::new() };
    refresh_due(&mut app, &tx, false);

    loop {
        // Drain finished fetches.
        while let Ok(msg) = rx.try_recv() {
            app.source(msg.kind).apply(msg.result);
        }
        refresh_due(&mut app, &tx, false);

        terminal.draw(|f| ui::draw(f, &app))?;

        if !event::poll(poll_wait(&app, Instant::now()))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('r') => refresh_due(&mut app, &tx, true),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe::{ErrorCode, UsageWindow};

    fn app() -> App {
        App { claude: SourceState::new(), codex: SourceState::new(), devin: SourceState::new() }
    }

    #[test]
    fn next_wakeup_skips_busy_sources() {
        let mut app = app();
        let now = Instant::now();
        app.claude.next_at = now + Duration::from_secs(15);
        // Codex is due and its probe is running: the past next_at must not count.
        app.codex.next_at = now - Duration::from_secs(1);
        app.codex.busy = true;
        app.devin.next_at = now + Duration::from_secs(300);
        assert_eq!(next_wakeup(&app), Some(now + Duration::from_secs(15)));
    }

    #[test]
    fn next_wakeup_counts_idle_due_sources() {
        let mut app = app();
        let now = Instant::now();
        app.claude.next_at = now + Duration::from_secs(15);
        app.codex.next_at = now - Duration::from_secs(1);
        app.devin.next_at = now + Duration::from_secs(300);
        assert_eq!(next_wakeup(&app), Some(now - Duration::from_secs(1)));
    }

    #[test]
    fn next_wakeup_is_none_when_all_busy() {
        let mut app = app();
        for s in [&mut app.claude, &mut app.codex, &mut app.devin] {
            s.busy = true;
        }
        assert_eq!(next_wakeup(&app), None);
    }

    #[test]
    fn poll_wait_checks_often_while_a_probe_runs() {
        let mut app = app();
        let now = Instant::now();
        app.claude.next_at = now + Duration::from_secs(15);
        app.devin.next_at = now + Duration::from_secs(300);
        app.codex.next_at = now - Duration::from_secs(1);
        app.codex.busy = true;
        assert_eq!(poll_wait(&app, now), PROBE_CHECK);
    }

    #[test]
    fn poll_wait_is_one_second_when_idle() {
        let mut app = app();
        let now = Instant::now();
        for s in [&mut app.claude, &mut app.codex, &mut app.devin] {
            s.next_at = now + Duration::from_secs(60);
        }
        assert_eq!(poll_wait(&app, now), Duration::from_secs(1));
    }

    #[test]
    fn poll_wait_is_short_for_a_due_fetch_but_never_zero() {
        let mut app = app();
        let now = Instant::now();
        for s in [&mut app.claude, &mut app.codex, &mut app.devin] {
            s.next_at = now + Duration::from_secs(60);
        }
        app.claude.next_at = now + Duration::from_millis(40);
        assert_eq!(poll_wait(&app, now), Duration::from_millis(40));
        app.claude.next_at = now - Duration::from_secs(1);
        assert_eq!(poll_wait(&app, now), Duration::from_millis(10));
    }

    fn ok(plan: Option<&str>, overage_micros: Option<i64>) -> ProbeResult {
        Ok(FetchOk {
            windows: vec![UsageWindow { label: "day", used_percent: 30.4, resets_at: 1_800_000_000 }],
            plan: plan.map(String::from),
            overage_micros,
            fetched_at: 0,
        })
    }

    #[test]
    fn once_line_shows_plan_and_overage_independently() {
        let base = "devin: day=30% (resets 1800000000)";
        assert_eq!(once_line("devin", &ok(Some("Pro"), Some(2_500_000))), format!("{base} plan=Pro overage=$2.50"));
        assert_eq!(once_line("devin", &ok(Some("Pro"), None)), format!("{base} plan=Pro"));
        assert_eq!(once_line("devin", &ok(None, Some(2_500_000))), format!("{base} overage=$2.50"));
        assert_eq!(once_line("devin", &ok(None, None)), base);
    }

    #[test]
    fn once_line_shows_errors() {
        let err = Err(ProbeError::new(ErrorCode::Timeout, "slow"));
        assert_eq!(once_line("codex", &err), "codex: error Timeout slow");
    }
}
