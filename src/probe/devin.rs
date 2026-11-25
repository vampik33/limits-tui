//! Devin CLI quota usage: read the credentials TOML and POST GetUserStatus to
//! the configured API server. Port of herdr-devin-limits' devin_limits.py.

use std::ffi::OsString;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use super::child::run_bounded;
use super::{
    ErrorCode, FetchOk, ProbeError, ProbeResult, UsageWindow, json_int, read_small_file, timeout_from_env, unix_now,
};

/// argv[1] that runs limits-tui as the Devin probe's DNS lookup child:
/// `limits-tui __resolve <host:port> <timeout_ms>`. Internal, not in `--help`.
pub const RESOLVE_ARG: &str = "__resolve";

const DEFAULT_API_SERVER_URL: &str = "https://server.codeium.com";
const STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const DEFAULT_TIMEOUT_SECS: f64 = 15.0;
const QUOTA_BILLING_STRATEGY: &str = "BILLING_STRATEGY_QUOTA";
/// Most addresses kept from one lookup, as ureq's own resolver keeps.
const MAX_ADDRS: usize = 16;

struct Credentials {
    api_key: String,
    api_server_url: String,
}

/// Looks the API server up in a child process (limits-tui `__resolve`) that is
/// killed with its process group at the probe's deadline. getaddrinfo can't be
/// cancelled, so ureq's own resolver, which runs it on a thread and stops
/// waiting at the deadline, leaves that thread running past the probe.
#[derive(Debug)]
struct ChildResolver {
    program: PathBuf,
    /// Put before `RESOLVE_ARG`: tests run `/bin/sh -c <script>` as the child.
    args: Vec<String>,
    deadline: Instant,
}

impl Resolver for ChildResolver {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host_port = uri
            .scheme()
            .zip(uri.authority())
            .and_then(|(scheme, authority)| DefaultResolver::host_and_port(scheme, authority))
            .ok_or_else(|| ureq::Error::BadUri(uri.to_string()))?;
        let now = Instant::now();
        // ureq's "no timeout" is u64::MAX seconds: `now + after` would panic.
        let end = now.checked_add(*timeout.after).map_or(self.deadline, |t| t.min(self.deadline));
        let left = end.saturating_duration_since(now);
        if left.is_zero() {
            return Err(ureq::Error::Timeout(timeout.reason));
        }
        // Rounded up, so the child never gives up before `end`: a lookup cut
        // off by the deadline then reads as a timeout below.
        let millis = u64::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).args([RESOLVE_ARG, &host_port, &millis.to_string()]);
        die_with_parent(&mut cmd);
        let output = run_bounded(&mut cmd, end).unwrap_or_default();
        if Instant::now() >= end {
            return Err(ureq::Error::Timeout(timeout.reason));
        }
        let found: Vec<SocketAddr> =
            String::from_utf8_lossy(&output).lines().filter_map(|l| l.trim().parse().ok()).collect();
        if found.is_empty() {
            // What a failed `to_socket_addrs` gives in ureq's own resolver.
            return Err(ureq::Error::Io(std::io::Error::other(format!("could not resolve {host_port}"))));
        }
        let mut addrs = self.empty();
        for addr in config.ip_family().keep_wanted(found.into_iter()).take(MAX_ADDRS) {
            addrs.push(addr);
        }
        if addrs.is_empty() { Err(ureq::Error::HostNotFound) } else { Ok(addrs) }
    }
}

fn load_credentials() -> Result<Credentials, ProbeError> {
    let path = std::env::var("DEVIN_CREDENTIALS")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/devin/credentials.toml")))
        .ok_or_else(|| ProbeError::new(ErrorCode::Failed, "no home dir"))?;
    read_credentials(&path)
}

fn read_credentials(path: &std::path::Path) -> Result<Credentials, ProbeError> {
    let text = read_small_file(path)
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
    let deadline = Instant::now() + timeout_from_env("DEVIN_LIMITS_TIMEOUT", DEFAULT_TIMEOUT_SECS);

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

    let program = self_exe().ok_or_else(|| {
        ProbeError::new(ErrorCode::Failed, "cannot find limits-tui's own executable for the DNS lookup")
    })?;
    // ureq's default proxy setting: tests pass None to stay off the environment's.
    let proxy = ureq::Proxy::try_from_env();
    let mut response = agent(ChildResolver { program, args: Vec::new(), deadline }, proxy)
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

/// The lookup child: `args` are what follows RESOLVE_ARG. Prints up to
/// MAX_ADDRS addresses of `<host:port>`, one per line, and returns the exit
/// code: 0 with addresses, 1 without (none, lookup error, `<timeout_ms>`
/// passed), 2 for bad arguments. The process exits right after, which ends a
/// lookup still running, so the child can't outlive `<timeout_ms>` even when
/// the probe that started it is gone (the TUI quit mid-lookup).
pub fn resolve_main(args: &[OsString]) -> i32 {
    resolve_with(args, |host_port| host_port.to_socket_addrs().map(Iterator::collect), &mut std::io::stdout())
}

/// `resolve_main` with the lookup and the output as parameters, for tests.
fn resolve_with(
    args: &[OsString],
    lookup: impl FnOnce(&str) -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
    out: &mut impl Write,
) -> i32 {
    let [host_port, timeout_ms] = args else { return 2 };
    let (Some(host_port), Some(timeout_ms)) =
        (host_port.to_str(), timeout_ms.to_str().and_then(|ms| ms.parse::<u64>().ok()))
    else {
        return 2;
    };
    let host_port = host_port.to_string();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(lookup(&host_port));
    });
    let Ok(Ok(addrs)) = rx.recv_timeout(Duration::from_millis(timeout_ms)) else { return 1 };
    let text: String = addrs.iter().take(MAX_ADDRS).map(|a| format!("{a}\n")).collect();
    if text.is_empty() || out.write_all(text.as_bytes()).and_then(|()| out.flush()).is_err() {
        return 1;
    }
    0
}

/// Makes the child of `cmd` get SIGKILL when the thread that spawns it exits
/// (Linux: PR_SET_PDEATHSIG). That thread waits in run_bounded until the child
/// is reaped, so the signal only fires when the whole process goes, the TUI
/// quitting mid-lookup; without it the child would run on to its own timeout.
fn die_with_parent(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: the closure runs in the forked child before exec. It calls
        // only prctl and getppid, which are async-signal-safe, and builds its
        // errors from errno without allocating.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before prctl, so the signal will never come.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// This binary, to run as the lookup child. `/proc/self/exe` still runs it
/// after `cargo install` replaced the file during a long TUI session.
fn self_exe() -> Option<PathBuf> {
    let proc_exe = Path::new("/proc/self/exe");
    if proc_exe.exists() { Some(proc_exe.into()) } else { std::env::current_exe().ok() }
}

/// The agent for the GetUserStatus request, bounded by `resolver.deadline`.
fn agent(resolver: ChildResolver, proxy: Option<ureq::Proxy>) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .proxy(proxy)
        .timeout_global(Some(resolver.deadline - Instant::now()))
        .http_status_as_error(false) // read the body ourselves, like the Python probe
        .https_only(true) // also refuse redirects to plain http
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::default(), resolver)
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
        // Connect's JSON omits zero-valued fields, so a missing percentage is 0% remaining.
        let remaining = match plan_status.get(remaining_field) {
            None | Some(serde_json::Value::Null) => 0,
            Some(v) => json_int(v)
                .ok_or_else(|| ProbeError::new(ErrorCode::Failed, "API response has an invalid quota percentage"))?,
        }
        .clamp(0, 100);
        let used = (100 - remaining) as f64;
        windows.extend(UsageWindow::new(label, used, reset));
    }
    if windows.is_empty() {
        return Err(ProbeError::new(ErrorCode::NoQuota, "no quota windows are available"));
    }

    // Connect's JSON omits zero-valued fields, so a missing balance is not an error.
    let overage = match plan_status.get("overageBalanceMicros") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            json_int(v)
                .ok_or_else(|| ProbeError::new(ErrorCode::Failed, "API response has an invalid overage balance"))?,
        ),
    };
    let plan = match plan_info.get("planName") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => {
            return Err(ProbeError::new(ErrorCode::Failed, "API response has an invalid plan name"));
        }
    };
    Ok(FetchOk { windows, plan, overage_micros: overage, fetched_at: unix_now() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MAX_FILE_BYTES;
    use crate::probe::tests::{Scratch, promptly};
    use crate::probe::{SCRIPTS, wait_until};
    use ureq::config::IpFamily;

    // --- the lookup child and its resolver ---

    /// A resolver that runs `script` with /bin/sh as its child, which gets
    /// RESOLVE_ARG, host:port and the milliseconds left as $1, $2 and $3.
    fn sh_resolver(script: &str, budget: Duration) -> ChildResolver {
        ChildResolver {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into()],
            deadline: Instant::now() + budget,
        }
    }

    fn resolve_uri(resolver: &ChildResolver, uri: &str, config: &Config) -> Result<Vec<SocketAddr>, ureq::Error> {
        let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
        let uri: Uri = uri.parse().unwrap();
        // ureq's "no timeout": the resolver must fall back to its own deadline.
        let timeout =
            NextTimeout { after: ureq::unversioned::transport::time::Duration::NotHappening, ..Default::default() };
        resolver.resolve(&uri, config, timeout).map(|a| a.to_vec())
    }

    fn resolve(script: &str) -> Result<Vec<SocketAddr>, ureq::Error> {
        resolve_uri(&sh_resolver(script, Duration::from_secs(5)), "https://example.com/", &Config::default())
    }

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    #[test]
    fn child_resolver_returns_the_addresses_in_order() {
        let got = resolve("printf '%s\\n' 192.0.2.1:443 '[2001:db8::1]:443'").unwrap();
        assert_eq!(got, addrs(&["192.0.2.1:443", "[2001:db8::1]:443"]));
    }

    #[test]
    fn child_resolver_skips_lines_that_are_not_addresses() {
        let got = resolve("echo junk; echo 192.0.2.1:443; echo 192.0.2.2; echo").unwrap();
        assert_eq!(got, addrs(&["192.0.2.1:443"]));
    }

    #[test]
    fn child_resolver_keeps_at_most_16_addresses() {
        let got = resolve("i=1; while [ $i -le 20 ]; do echo 192.0.2.$i:443; i=$((i+1)); done").unwrap();
        assert_eq!(got.len(), MAX_ADDRS);
        assert_eq!(got[0], "192.0.2.1:443".parse().unwrap());
    }

    #[test]
    fn child_resolver_passes_host_port_and_the_time_left() {
        // An IPv6 literal keeps its brackets, so the child can parse it back.
        let script = format!("[ \"$1\" = {RESOLVE_ARG} ] && [ \"$3\" -gt 0 ] && [ \"$3\" -le 5000 ] && echo \"$2\"");
        let resolver = sh_resolver(&script, Duration::from_secs(5));
        let got = resolve_uri(&resolver, "https://[2001:db8::1]:8443/x", &Config::default()).unwrap();
        assert_eq!(got, addrs(&["[2001:db8::1]:8443"]));
        let got = resolve_uri(&resolver, "https://192.0.2.7/x", &Config::default()).unwrap();
        assert_eq!(got, addrs(&["192.0.2.7:443"]), "the https default port");
    }

    #[test]
    fn child_resolver_keeps_the_wanted_ip_family() {
        let script = "printf '%s\\n' '[2001:db8::1]:443' 192.0.2.1:443";
        let v4_only = ureq::Agent::config_builder().ip_family(IpFamily::Ipv4Only).build();
        let resolver = sh_resolver(script, Duration::from_secs(5));
        assert_eq!(resolve_uri(&resolver, "https://example.com/", &v4_only).unwrap(), addrs(&["192.0.2.1:443"]));
        let resolver = sh_resolver("echo '[2001:db8::1]:443'", Duration::from_secs(5));
        let err = resolve_uri(&resolver, "https://example.com/", &v4_only).unwrap_err();
        assert!(matches!(err, ureq::Error::HostNotFound), "{err}");
    }

    #[test]
    fn child_resolver_failing_lookup_could_not_connect() {
        for script in ["exit 1", "echo nothing-here"] {
            let err = resolve(script).unwrap_err();
            assert!(matches!(err, ureq::Error::Io(_)), "{script}: {err}");
            let err = map_error(err);
            assert_eq!((err.code, err.detail.as_str()), (ErrorCode::Failed, "could not connect to API server"));
        }
    }

    #[test]
    fn child_resolver_missing_program_could_not_connect() {
        let resolver = ChildResolver {
            program: "/nonexistent/limits-tui".into(),
            args: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(5),
        };
        let err = resolve_uri(&resolver, "https://example.com/", &Config::default()).unwrap_err();
        assert!(matches!(err, ureq::Error::Io(_)), "{err}");
    }

    /// A fresh sleep argument to find a test's processes by: GNU sleep adds its
    /// arguments, so the fraction changes nothing.
    fn sleep_token(test: &str) -> String {
        use std::hash::BuildHasher;
        let random = std::collections::hash_map::RandomState::new().hash_one(test) % 1_000_000_000;
        format!("0.000{random:09}")
    }

    /// Pids of live (not zombie) processes whose command line has `token`.
    fn live_with(token: &str) -> Vec<String> {
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
    fn hung_lookup_times_out_by_the_deadline_and_leaves_nothing_running() {
        // The token marks the child's background job, which killpg must reach.
        let token = sleep_token("hung");
        let resolver = sh_resolver(&format!("sleep 30 {token} & wait"), Duration::from_millis(500));
        let start = Instant::now();
        let err = resolve_uri(&resolver, "https://example.com/", &Config::default()).unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(err, ureq::Error::Timeout(_)), "{err}");
        assert_eq!(map_error(err).code, ErrorCode::Timeout);
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        let gone = wait_until(40, || live_with(&token).is_empty());
        assert!(gone, "lookup child's job is still running: {:?}", live_with(&token));
    }

    #[test]
    fn lookup_job_that_released_stdout_is_killed() {
        // The job sends its output elsewhere, so the child exits and stdout
        // closes while the job still runs in the child's group.
        let token = sleep_token("quiet");
        let got = resolve(&format!("sleep 30 {token} >/dev/null 2>&1 &\necho 192.0.2.1:443")).unwrap();
        assert_eq!(got, addrs(&["192.0.2.1:443"]));
        let gone = wait_until(40, || live_with(&token).is_empty());
        assert!(gone, "lookup child's job is still running: {:?}", live_with(&token));
    }

    #[test]
    fn addresses_printed_by_a_lookup_cut_off_at_the_deadline_time_out() {
        let resolver = sh_resolver("echo 127.0.0.1:443; exec sleep 30", Duration::from_millis(500));
        let start = Instant::now();
        let result = resolve_uri(&resolver, "https://example.com/", &Config::default());
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(ureq::Error::Timeout(_))), "{result:?}");
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    }

    /// Set only for the helper run by `lookup_child_dies_with_its_parent`: the
    /// token its lookup child carries.
    const PARENT_HELPER_ENV: &str = "LIMITS_TUI_TEST_PDEATH_TOKEN";

    /// Not a test of its own: the parent process for
    /// `lookup_child_dies_with_its_parent`, which runs this test binary with
    /// PARENT_HELPER_ENV set and kills it mid-lookup. A plain run returns at once.
    #[test]
    fn lookup_child_parent_helper() {
        let Ok(token) = std::env::var(PARENT_HELPER_ENV) else { return };
        let resolver = sh_resolver(&format!("exec sleep 30 {token}"), Duration::from_secs(30));
        let _ = resolve_uri(&resolver, "https://example.com/", &Config::default());
    }

    #[test]
    fn lookup_child_dies_with_its_parent() {
        let token = sleep_token("pdeath");
        let mut helper = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "probe::devin::tests::lookup_child_parent_helper", "--test-threads=1"])
                .env(PARENT_HELPER_ENV, &token)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let started = wait_until(100, || !live_with(&token).is_empty());
        // SIGKILL: the parent gets no chance to clean up, as when the TUI dies.
        let _ = helper.kill();
        let _ = helper.wait();
        assert!(started, "the helper's lookup child never started");
        assert!(
            wait_until(100, || live_with(&token).is_empty()),
            "lookup child outlived its parent: {:?}",
            live_with(&token)
        );
    }

    #[test]
    fn deadline_already_passed_times_out_at_once() {
        let resolver = sh_resolver("sleep 30", Duration::ZERO);
        let start = Instant::now();
        let err = resolve_uri(&resolver, "https://example.com/", &Config::default()).unwrap_err();
        assert!(matches!(err, ureq::Error::Timeout(_)), "{err}");
        assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
    }

    #[test]
    fn probe_requests_go_through_the_child_resolver() {
        // Only the fake child knows where this name is: ureq's own resolver
        // can't find a .invalid name, so a connection here proves the agent
        // asked the child.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let accepted = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end {
                if listener.accept().is_ok() {
                    return true; // dropping the stream fails the TLS handshake
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        });
        let resolver = sh_resolver(&format!("echo 127.0.0.1:{port}"), Duration::from_secs(5));
        let result = {
            let _forking = SCRIPTS.read().unwrap_or_else(|e| e.into_inner());
            agent(resolver, None).post("https://devin-probe.invalid/").send("")
        };
        assert!(accepted.join().unwrap(), "the request never reached the child's address: {result:?}");
    }

    #[test]
    fn agent_still_refuses_plain_http() {
        let resolver = sh_resolver("echo 127.0.0.1:9", Duration::from_secs(5));
        let err = agent(resolver, None).post("http://127.0.0.1:9/").send("").unwrap_err();
        assert!(matches!(err, ureq::Error::RequireHttpsOnly(_)), "{err}");
    }

    // --- resolve_main / resolve_with ---

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn run_child(
        args: &[&str],
        lookup: impl FnOnce(&str) -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
    ) -> (i32, String) {
        let mut out = Vec::new();
        let code = resolve_with(&os_args(args), lookup, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn lookup_child_prints_one_address_per_line() {
        let (code, out) = run_child(&["example.com:443", "1000"], |host| {
            assert_eq!(host, "example.com:443");
            Ok(addrs(&["192.0.2.1:443", "[2001:db8::1]:443"]))
        });
        assert_eq!((code, out.as_str()), (0, "192.0.2.1:443\n[2001:db8::1]:443\n"));
    }

    #[test]
    fn lookup_child_prints_at_most_16() {
        let many: Vec<SocketAddr> = (1..=20).map(|i| SocketAddr::from(([192, 0, 2, i], 443))).collect();
        let (code, out) = run_child(&["h:443", "1000"], move |_| Ok(many));
        assert_eq!((code, out.lines().count()), (0, MAX_ADDRS));
    }

    #[test]
    fn lookup_child_with_bad_arguments_exits_2() {
        for args in [&[][..], &["h:443"], &["h:443", "1000", "extra"], &["h:443", "soon"], &["h:443", "-1"]] {
            let (code, out) = run_child(args, |_| panic!("no lookup for bad arguments"));
            assert_eq!((code, out.as_str()), (2, ""), "{args:?}");
        }
    }

    #[test]
    fn lookup_child_treats_flag_like_text_as_the_host() {
        let (code, _) = run_child(&["--once", "1000"], |host| {
            assert_eq!(host, "--once");
            Err(std::io::Error::other("no such host"))
        });
        assert_eq!(code, 1);
    }

    #[test]
    fn lookup_child_without_addresses_exits_1() {
        assert_eq!(run_child(&["h:443", "1000"], |_| Err(std::io::Error::other("nxdomain"))), (1, String::new()));
        assert_eq!(run_child(&["h:443", "1000"], |_| Ok(Vec::new())), (1, String::new()));
    }

    #[test]
    fn lookup_child_stops_waiting_at_its_timeout() {
        // The lookup blocks until the test lets it go, after the assertions
        // (or 5 s, so a child that waits it out fails instead of hanging).
        let (release, blocked) = mpsc::channel::<()>();
        let start = Instant::now();
        let (code, out) = run_child(&["h:443", "100"], move |_| {
            let _ = blocked.recv_timeout(Duration::from_secs(5));
            Ok(addrs(&["192.0.2.1:443"]))
        });
        let elapsed = start.elapsed();
        drop(release);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(elapsed < Duration::from_millis(600), "took {elapsed:?}");
    }

    #[test]
    fn lookup_child_resolves_localhost_for_real() {
        let (code, out) = run_child(&["localhost:443", "5000"], |h| h.to_socket_addrs().map(Iterator::collect));
        assert_eq!(code, 0);
        let first: SocketAddr = out.lines().next().unwrap().parse().unwrap();
        assert!(first.ip().is_loopback() && first.port() == 443, "{out}");
    }

    const CREDENTIALS: &str = "windsurf_api_key = \"test-key\"\n";

    #[test]
    fn credentials_are_read() {
        let scratch = Scratch::new("creds");
        let path = scratch.path("credentials.toml");
        std::fs::write(&path, CREDENTIALS).unwrap();
        assert_eq!(read_credentials(&path).ok().unwrap().api_key, "test-key");
    }

    #[test]
    fn credentials_through_a_symlink_are_read() {
        let scratch = Scratch::new("credslink");
        let (target, link) = (scratch.path("target.toml"), scratch.path("credentials.toml"));
        std::fs::write(&target, CREDENTIALS).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_credentials(&link).ok().unwrap().api_key, "test-key");
    }

    #[test]
    fn fifo_credentials_fail_without_blocking() {
        let scratch = Scratch::new("credsfifo");
        let fifo = scratch.fifo("credentials.toml");
        let err = promptly(move || read_credentials(&fifo).err()).expect("should fail");
        assert_eq!(err.code, ErrorCode::NotSignedIn, "{}", err.detail);
    }

    #[test]
    fn oversized_credentials_fail() {
        let scratch = Scratch::new("credsbig");
        let path = scratch.path("credentials.toml");
        std::fs::write(&path, format!("{CREDENTIALS}{}", "\n".repeat(MAX_FILE_BYTES as usize))).unwrap();
        let err = read_credentials(&path).err().expect("should fail");
        assert_eq!(err.code, ErrorCode::NotSignedIn, "{}", err.detail);
    }

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

    /// A quota-plan payload with a daily window and the given overage field
    /// (`None` leaves the field out).
    fn payload(overage: Option<serde_json::Value>) -> serde_json::Value {
        let mut status = serde_json::json!({
            "planInfo": {"billingStrategy": QUOTA_BILLING_STRATEGY, "planName": "Pro"},
            "dailyQuotaRemainingPercent": 70,
            "dailyQuotaResetAtUnix": "1800000000",
        });
        if let Some(v) = overage {
            status["overageBalanceMicros"] = v;
        }
        serde_json::json!({"userStatus": {"planStatus": status}})
    }

    #[test]
    fn missing_or_null_overage_keeps_the_windows() {
        for overage in [None, Some(serde_json::Value::Null)] {
            let d = normalize(&payload(overage.clone()), "key").unwrap();
            assert_eq!(d.overage_micros, None, "{overage:?}");
            assert_eq!(d.windows.len(), 1, "{overage:?}");
            assert_eq!(d.windows[0].used_percent, 30.0, "{overage:?}");
        }
    }

    #[test]
    fn overage_as_number_or_string_is_read() {
        assert_eq!(normalize(&payload(Some(2_500_000.into())), "key").unwrap().overage_micros, Some(2_500_000));
        assert_eq!(normalize(&payload(Some("2500000".into())), "key").unwrap().overage_micros, Some(2_500_000));
    }

    #[test]
    fn extreme_remaining_percent_is_clamped_without_overflow() {
        for (remaining, used) in [
            (serde_json::json!(-1e300), 100.0),
            (serde_json::json!("-9223372036854775808"), 100.0),
            (serde_json::json!(1e300), 0.0),
            (serde_json::json!(-20), 100.0),
        ] {
            let mut p = payload(None);
            p["userStatus"]["planStatus"]["dailyQuotaRemainingPercent"] = remaining.clone();
            assert_eq!(normalize(&p, "key").unwrap().windows[0].used_percent, used, "{remaining}");
        }
    }

    #[test]
    fn remaining_percent_edge_values_give_a_percent_in_range() {
        for (remaining, used) in [
            (serde_json::json!(0), 100.0),
            (serde_json::json!(100), 0.0),
            (serde_json::json!(150), 0.0),
            (serde_json::json!("70"), 30.0),
        ] {
            let mut p = payload(None);
            p["userStatus"]["planStatus"]["dailyQuotaRemainingPercent"] = remaining.clone();
            assert_eq!(normalize(&p, "key").unwrap().windows[0].used_percent, used, "{remaining}");
        }
    }

    #[test]
    fn unreadable_remaining_percent_is_an_error() {
        let bad =
            [serde_json::json!("NaN"), "inf".into(), "-inf".into(), "abc".into(), true.into(), serde_json::json!({})];
        for (field, reset_field) in [
            ("dailyQuotaRemainingPercent", "dailyQuotaResetAtUnix"),
            ("weeklyQuotaRemainingPercent", "weeklyQuotaResetAtUnix"),
        ] {
            for remaining in &bad {
                let mut p = payload(None);
                p["userStatus"]["planStatus"][reset_field] = "1800000000".into();
                p["userStatus"]["planStatus"][field] = remaining.clone();
                let err = normalize(&p, "key").err().expect("should fail");
                assert_eq!(err.code, ErrorCode::Failed, "{field}: {remaining}");
                assert!(err.detail.contains("quota percentage"), "{field}: {remaining}: {}", err.detail);
            }
        }
    }

    #[test]
    fn absent_or_null_remaining_percent_is_fully_used() {
        for remaining in [None, Some(serde_json::Value::Null)] {
            let mut p = payload(None);
            let status = p["userStatus"]["planStatus"].as_object_mut().unwrap();
            status.remove("dailyQuotaRemainingPercent");
            if let Some(v) = remaining.clone() {
                status.insert("dailyQuotaRemainingPercent".into(), v);
            }
            assert_eq!(normalize(&p, "key").unwrap().windows[0].used_percent, 100.0, "{remaining:?}");
        }
    }

    #[test]
    fn invalid_overage_is_still_an_error() {
        for bad in [serde_json::json!("lots"), serde_json::json!(true), serde_json::json!({})] {
            let err = normalize(&payload(Some(bad.clone())), "key").err().expect("should fail");
            assert_eq!(err.code, ErrorCode::Failed, "{bad}");
            assert!(err.detail.contains("overage"), "{bad}: {}", err.detail);
        }
    }
}
