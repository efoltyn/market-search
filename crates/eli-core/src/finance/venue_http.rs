//! One HTTP path for the prediction-market venues (Kalshi, Polymarket). The
//! Kraken Futures charts host is spaced here too, so that several processes
//! paging long candle histories at once still add up to one polite client.
//!
//! `VenueSend::venue_send()` is a drop-in for `RequestBuilder::send()`:
//!
//! - Requests to a venue host are spaced. The schedule lives in small files
//!   under the cache dir, so the spacing holds across concurrent processes
//!   (the MCP server runs every tool call as its own process).
//! - A 429, or a 503 that carries Retry-After, pushes the host's next slot
//!   out for every process and is retried here a couple of times. Kalshi
//!   sends no Retry-After, so the wait doubles per refusal, with jitter.
//! - Three refusals in a row pause the host. While it is paused, requests
//!   fail at once with a message that says when to come back, instead of
//!   queueing more traffic at a venue that is already saying no.
//! - `MARKET_SEARCH_HTTP_TRACE=<file>` appends one JSON line per request
//!   (host, path, status, wait, time to headers, body time, decoded bytes).
//!
//! Requests to any other host pass straight through.

use reqwest::ResponseBuilderExt;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Append one JSON line per venue request to this file.
pub const TRACE_ENV: &str = "MARKET_SEARCH_HTTP_TRACE";
/// Override the spacing between requests to one venue host, in milliseconds.
pub const MIN_INTERVAL_ENV: &str = "MARKET_SEARCH_HTTP_MIN_INTERVAL_MS";
/// Override how long a host stays paused after three refusals in a row.
pub const PAUSE_ENV: &str = "MARKET_SEARCH_HTTP_PAUSE_SECS";

/// Kalshi documents 20 reads a second for its basic tier and nothing for
/// anonymous callers; 8 a second is what this tool has always used.
const KALSHI_SPACING_MS: u64 = 125;
/// Polymarket's tightest documented read limit is 20 a second per IP
/// (data-api prices-history); 10 a second per host stays well inside it.
const POLYMARKET_SPACING_MS: u64 = 100;
/// Kraken Futures' charts API documents no limit and sends no rate-limit
/// headers. 92,000 one-minute candles were read at 600 ms spacing without a
/// refusal; 300 ms is the floor the candle pager keeps in any case.
const KRAKEN_FUTURES_SPACING_MS: u64 = 300;
const MIN_SPACING_MS: u64 = 50;
const DEFAULT_PAUSE_SECS: u64 = 60;
const REFUSALS_BEFORE_PAUSE: u32 = 3;
/// A wait longer than this is reported to the caller instead of slept through.
const FAIL_FAST_MS: u64 = 20_000;
const MAX_GATE_RETRIES: u32 = 2;
const MAX_COOLDOWN_MS: u64 = 600_000;
/// A slot further out than this is a leftover from a dead process or a clock
/// change, not a queue.
const MAX_QUEUE_MS: u64 = 300_000;
const PAUSED_HEADER: &str = "x-market-search-paused";

/// Pooled client shared by every venue request in this process: one TLS
/// handshake per host per command, HTTP/2 where the venue offers it.
pub fn client() -> &'static reqwest::Client {
    &crate::finance::shared_client::GENERAL
}

#[derive(Clone, Copy)]
struct HostRule {
    key: &'static str,
    label: &'static str,
    spacing_ms: u64,
}

fn host_rule(host: &str) -> Option<HostRule> {
    let (key, label, default_ms) = match host {
        "api.elections.kalshi.com" | "external-api.kalshi.com" => {
            ("kalshi", "Kalshi", KALSHI_SPACING_MS)
        }
        "gamma-api.polymarket.com" => ("polymarket-gamma", "Polymarket", POLYMARKET_SPACING_MS),
        "clob.polymarket.com" => ("polymarket-clob", "Polymarket", POLYMARKET_SPACING_MS),
        "data-api.polymarket.com" => ("polymarket-data", "Polymarket", POLYMARKET_SPACING_MS),
        "futures.kraken.com" => ("kraken-futures", "Kraken Futures", KRAKEN_FUTURES_SPACING_MS),
        _ => return None,
    };
    Some(HostRule {
        key,
        label,
        spacing_ms: spacing_override().unwrap_or(default_ms),
    })
}

fn spacing_override() -> Option<u64> {
    static VALUE: OnceLock<Option<u64>> = OnceLock::new();
    *VALUE.get_or_init(|| {
        std::env::var(MIN_INTERVAL_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .map(|ms| ms.max(MIN_SPACING_MS))
    })
}

fn pause_ms() -> u64 {
    static VALUE: OnceLock<u64> = OnceLock::new();
    *VALUE.get_or_init(|| {
        std::env::var(PAUSE_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_PAUSE_SECS)
            .clamp(1, 3600)
            .saturating_mul(1000)
    })
}

fn trace_path() -> Option<&'static Path> {
    static VALUE: OnceLock<Option<PathBuf>> = OnceLock::new();
    VALUE
        .get_or_init(|| {
            std::env::var_os(TRACE_ENV)
                .filter(|raw| !raw.is_empty())
                .map(PathBuf::from)
        })
        .as_deref()
}

/// True when a request trace is being written.
pub fn trace_enabled() -> bool {
    trace_path().is_some()
}

/// Add a line to the request trace (cache hits and the like). No-op unless
/// the trace is on.
pub fn trace_event(kind: &str, detail: serde_json::Value) {
    if trace_enabled() {
        trace_line(serde_json::json!({
            "ts": now_ms(),
            "pid": std::process::id(),
            "event": kind,
            "detail": detail,
        }));
    }
}

fn trace_line(value: serde_json::Value) {
    let Some(path) = trace_path() else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{value}");
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

// ── Per-host schedule, shared across processes ──────────────────────────────

#[derive(Clone, Copy, Default, PartialEq)]
struct HostState {
    /// Earliest time the next request may go out.
    next_ms: u64,
    /// No request before this time (set by a refusal).
    cool_until_ms: u64,
    /// Refusals in a row, across processes.
    refusals: u32,
}

fn limit_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds").join("provider_limits"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-provider-limits"))
}

/// Directory lock, the same protocol 0.5.0 uses for `kalshi.lock`, so an old
/// binary and a new one running side by side still take turns.
struct DirLock {
    path: PathBuf,
}

impl DirLock {
    fn acquire(dir: &Path, key: &str) -> Option<Self> {
        let path = dir.join(format!("{key}.lock"));
        for _ in 0..200 {
            match std::fs::create_dir(&path) {
                Ok(()) => return Some(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|elapsed| elapsed > Duration::from_secs(10));
                    if stale {
                        let _ = std::fs::remove_dir(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn read_state(dir: &Path, key: &str) -> HostState {
    let next_ms = std::fs::read_to_string(dir.join(format!("{key}.next_ms")))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or_default();
    let cooldown = std::fs::read_to_string(dir.join(format!("{key}.cooldown"))).unwrap_or_default();
    let mut parts = cooldown.split_whitespace();
    let cool_until_ms = parts.next().and_then(|v| v.parse().ok()).unwrap_or_default();
    let refusals = parts.next().and_then(|v| v.parse().ok()).unwrap_or_default();
    HostState {
        next_ms,
        cool_until_ms,
        refusals,
    }
}

fn write_state(dir: &Path, key: &str, before: HostState, after: HostState) {
    if before.next_ms != after.next_ms {
        let _ = std::fs::write(dir.join(format!("{key}.next_ms")), after.next_ms.to_string());
    }
    if before.cool_until_ms != after.cool_until_ms || before.refusals != after.refusals {
        let _ = std::fs::write(
            dir.join(format!("{key}.cooldown")),
            format!("{} {}", after.cool_until_ms, after.refusals),
        );
    }
}

/// Run `f` on the host's schedule under the cross-process lock. When the
/// cache dir cannot be used, the schedule still holds inside this process.
fn with_state<R>(rule: HostRule, f: impl FnOnce(&mut HostState, u64) -> R) -> R {
    static LOCAL: Mutex<Option<HashMap<&'static str, HostState>>> = Mutex::new(None);
    let mut local = LOCAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let local = local.get_or_insert_with(HashMap::new);

    let dir = limit_dir();
    let lock = std::fs::create_dir_all(&dir)
        .ok()
        .and_then(|_| DirLock::acquire(&dir, rule.key));
    let before = if lock.is_some() {
        read_state(&dir, rule.key)
    } else {
        local.get(rule.key).copied().unwrap_or_default()
    };
    let mut state = before;
    let out = f(&mut state, now_ms());
    if lock.is_some() {
        write_state(&dir, rule.key, before, state);
    }
    local.insert(rule.key, state);
    out
}

enum Slot {
    Go { wait_ms: u64, refusals: u32 },
    Paused { wait_ms: u64 },
}

fn reserve_slot(rule: HostRule) -> Slot {
    with_state(rule, |state, now| {
        if state.cool_until_ms > now.saturating_add(FAIL_FAST_MS) {
            return Slot::Paused {
                wait_ms: state.cool_until_ms - now,
            };
        }
        let queued = if state.next_ms > now.saturating_add(MAX_QUEUE_MS) {
            now
        } else {
            state.next_ms
        };
        let scheduled = now.max(queued).max(state.cool_until_ms);
        state.next_ms = scheduled.saturating_add(rule.spacing_ms);
        Slot::Go {
            wait_ms: scheduled - now,
            refusals: state.refusals,
        }
    })
}

/// Record a refusal. Returns true when the caller should stop retrying (the
/// host is now paused, or the venue asked for a long wait).
fn note_refusal(rule: HostRule, retry_after_ms: Option<u64>, counts: bool) -> bool {
    let jitter = 1.0 + rand::random::<f64>() * 0.25;
    with_state(rule, |state, now| {
        if counts {
            state.refusals = state.refusals.saturating_add(1);
        }
        let doublings = state.refusals.saturating_sub(1).min(5);
        let backoff_ms = ((1000u64 << doublings) as f64 * jitter) as u64;
        let mut delay_ms = retry_after_ms.unwrap_or(0).max(backoff_ms);
        if counts && state.refusals >= REFUSALS_BEFORE_PAUSE {
            delay_ms = delay_ms.max(pause_ms());
            state.refusals = 0;
        }
        let delay_ms = delay_ms.min(MAX_COOLDOWN_MS);
        state.cool_until_ms = state.cool_until_ms.max(now.saturating_add(delay_ms));
        delay_ms > FAIL_FAST_MS
    })
}

fn note_success(rule: HostRule) {
    with_state(rule, |state, _| state.refusals = 0);
}

// ── Sending ─────────────────────────────────────────────────────────────────

fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = raw.parse::<f64>() {
        if secs.is_finite() && secs >= 0.0 {
            return Some((secs * 1000.0) as u64);
        }
        return None;
    }
    let when = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    let delta_ms = when.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    Some(delta_ms.max(0) as u64)
}

fn paused_message(label: &str, wait_ms: u64) -> String {
    let secs = wait_ms.div_ceil(1000);
    let until = chrono::Utc::now() + chrono::Duration::milliseconds(wait_ms as i64);
    format!(
        "{label} is rate limiting this machine, so requests to it are paused for {secs}s (until {} UTC). Retry after that. Fewer tickers or a shorter range needs fewer requests.",
        until.format("%H:%M:%S")
    )
}

fn paused_response(url: &reqwest::Url, rule: HostRule, wait_ms: u64) -> reqwest::Response {
    let body = serde_json::json!({
        "error": paused_message(rule.label, wait_ms),
        "code": "paused_after_rate_limit",
    })
    .to_string();
    let built = http::Response::builder()
        .status(reqwest::StatusCode::TOO_MANY_REQUESTS)
        .header(reqwest::header::RETRY_AFTER, wait_ms.div_ceil(1000).to_string())
        .header(PAUSED_HEADER, rule.label)
        .url(url.clone())
        .body(body)
        .expect("static response parts are valid");
    reqwest::Response::from(built)
}

/// What to tell the user about a 429 response: how long to wait and why.
/// None for any other status.
pub fn refusal_note(resp: &reqwest::Response) -> Option<String> {
    if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let wait_ms = retry_after_ms(resp.headers());
    if let Some(label) = resp
        .headers()
        .get(PAUSED_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        return Some(paused_message(label, wait_ms.unwrap_or(pause_ms())));
    }
    let host = resp.url().host_str().unwrap_or("the venue");
    Some(match wait_ms {
        Some(ms) => format!(
            "{host} is rate limiting this machine; retry in {}s.",
            ms.div_ceil(1000)
        ),
        None => format!("{host} is rate limiting this machine; wait a few seconds and retry."),
    })
}

fn http_version_label(version: reqwest::Version) -> &'static str {
    match version {
        reqwest::Version::HTTP_09 => "0.9",
        reqwest::Version::HTTP_10 => "1.0",
        reqwest::Version::HTTP_11 => "1.1",
        reqwest::Version::HTTP_2 => "2",
        reqwest::Version::HTTP_3 => "3",
        _ => "?",
    }
}

pub trait VenueSend {
    /// `send()`, with venue hosts spaced, refusals honored and the request
    /// traced when the trace is on.
    fn venue_send(
        self,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send;
}

impl VenueSend for reqwest::RequestBuilder {
    async fn venue_send(self) -> reqwest::Result<reqwest::Response> {
        let (client, request) = self.build_split();
        let request = request?;
        let rule = request.url().host_str().and_then(host_rule);
        let tracing = trace_enabled();
        if rule.is_none() && !tracing {
            return client.execute(request).await;
        }

        let method = request.method().to_string();
        let url = request.url().clone();
        let trace = |extra: serde_json::Value| {
            if !tracing {
                return;
            }
            let mut line = serde_json::json!({
                "ts": now_ms(),
                "pid": std::process::id(),
                "method": method,
                "host": url.host_str().unwrap_or_default(),
                "path": url.path(),
                "query_len": url.query().map(str::len).unwrap_or(0),
            });
            // The query is logged only for the venue hosts, whose read URLs
            // carry nothing private; it shows cursors, offsets and batch sizes.
            if let (Some(line), true) = (line.as_object_mut(), rule.is_some()) {
                line.insert(
                    "query".to_string(),
                    serde_json::Value::String(url.query().unwrap_or_default().to_string()),
                );
            }
            if let (Some(line), Some(extra)) = (line.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    line.insert(key.clone(), value.clone());
                }
            }
            trace_line(line);
        };

        let mut pending = Some(request);
        let mut attempt = 0u32;
        loop {
            let Some(request) = pending.take() else {
                unreachable!("a request is always pending at the top of the loop");
            };
            let mut wait_ms = 0u64;
            let mut refusals_seen = 0u32;
            if let Some(rule) = rule {
                let slot = tokio::task::spawn_blocking(move || reserve_slot(rule))
                    .await
                    .unwrap_or(Slot::Go {
                        wait_ms: 0,
                        refusals: 0,
                    });
                match slot {
                    Slot::Paused { wait_ms } => {
                        trace(serde_json::json!({
                            "status": 429, "paused": true, "retry_in_ms": wait_ms, "attempt": attempt,
                        }));
                        return Ok(paused_response(&url, rule, wait_ms));
                    }
                    Slot::Go { wait_ms: wait, refusals } => {
                        if wait > 0 {
                            tokio::time::sleep(Duration::from_millis(wait)).await;
                        }
                        wait_ms = wait;
                        refusals_seen = refusals;
                    }
                }
            }

            let retry_copy = if rule.is_some() && attempt < MAX_GATE_RETRIES {
                request.try_clone()
            } else {
                None
            };
            let started = Instant::now();
            let resp = match client.execute(request).await {
                Ok(resp) => resp,
                Err(err) => {
                    trace(serde_json::json!({
                        "wait_ms": wait_ms, "ttfb_ms": started.elapsed().as_millis() as u64,
                        "attempt": attempt, "error": err.to_string(),
                    }));
                    return Err(err);
                }
            };
            let ttfb_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            let version = http_version_label(resp.version());

            if let Some(rule) = rule {
                let asked_wait = retry_after_ms(resp.headers());
                let too_many = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                let unavailable =
                    status == reqwest::StatusCode::SERVICE_UNAVAILABLE && asked_wait.is_some();
                if too_many || unavailable {
                    let stop = tokio::task::spawn_blocking(move || {
                        note_refusal(rule, asked_wait, too_many)
                    })
                    .await
                    .unwrap_or(true);
                    trace(serde_json::json!({
                        "status": status.as_u16(), "http": version, "wait_ms": wait_ms,
                        "ttfb_ms": ttfb_ms, "attempt": attempt, "retry_after_ms": asked_wait,
                    }));
                    if !stop {
                        if let Some(copy) = retry_copy {
                            attempt += 1;
                            pending = Some(copy);
                            continue;
                        }
                    }
                    return Ok(resp);
                }
                if refusals_seen > 0 && status.is_success() {
                    let _ = tokio::task::spawn_blocking(move || note_success(rule)).await;
                }
            }

            if !tracing {
                return Ok(resp);
            }
            // Tracing: read the body here so the line can carry its size and
            // transfer time, then hand the caller an equivalent response.
            let headers = resp.headers().clone();
            let body_started = Instant::now();
            let body = match resp.bytes().await {
                Ok(body) => body,
                Err(err) => {
                    trace(serde_json::json!({
                        "status": status.as_u16(), "http": version, "wait_ms": wait_ms,
                        "ttfb_ms": ttfb_ms, "attempt": attempt, "error": err.to_string(),
                    }));
                    return Err(err);
                }
            };
            trace(serde_json::json!({
                "status": status.as_u16(), "http": version, "wait_ms": wait_ms,
                "ttfb_ms": ttfb_ms, "body_ms": body_started.elapsed().as_millis() as u64,
                "bytes": body.len(), "attempt": attempt,
            }));
            let mut builder = http::Response::builder().status(status).url(url.clone());
            if let Some(target) = builder.headers_mut() {
                *target = headers;
            }
            return Ok(reqwest::Response::from(
                builder.body(body).expect("copied response parts are valid"),
            ));
        }
    }
}
