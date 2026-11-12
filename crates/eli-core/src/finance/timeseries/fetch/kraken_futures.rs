/// Kraken Futures public charts: index, mark and trade candles for its
/// perpetual and fixed-maturity markets. No key: the charts API takes no
/// authentication. Read-only market data.
///
/// Endpoint: GET https://futures.kraken.com/api/charts/v1/{tick}/{symbol}/{res}?to=<unix s>&count=<n>
///   tick  spot  = the index price of the market (volume always "0")
///         mark  = the mark price
///         trade = traded candles, with volume
///   res   1m 5m 15m 30m 1h 4h 12h 1d 1w. Anything else is a 400.
/// Response: {"candles":[{"time":<ms>,"open":"73530.1","high":"..","low":"..",
/// "close":"..","volume":"0"}],"more_candles":false}. Numbers are strings,
/// candles ascend, `time` is the bar open in MILLISECONDS while `to` is in
/// SECONDS and inclusive. Weekly bars open on Thursday 00:00 UTC.
///
/// Paging: `to` with `count` returns the `count` candles that end at `to`
/// (5000 is the page size used here), so the pager walks backward from the
/// end of the window until it reaches the start or a page comes back empty,
/// which is the start of the market's history. An unknown symbol answers 200
/// with an empty array, the same as a window before the first candle; one
/// extra request (`from=0&count=1`, the first candle ever) tells them apart.
///
/// Ticker grammar: `KF:<NAME>[:<KIND>]`, case-insensitive.
///   KIND  INDEX (default; SPOT means the same), MARK, TRADE
///   NAME  a full symbol (PF_XBTUSD, FF_ETHUSD_261225, PI_.., FI_..), an alias
///         (BTC, GOLD, SILVER, OIL, BRENT, NATGAS, COPPER, EUR, GBP, CHF,
///         US100), or a base asset, which becomes PF_<NAME>USD (XRP, DOGE).
///
/// The index is the reference price Kraken Futures publishes for a market.
/// For gold, silver, WTI, Brent, natural gas, copper and the US Tech 100,
/// Kraken's instrument listing names it "... 24/7 Pyth Index"; those keep
/// moving on weekends. FX (EUR, GBP, CHF) is flat on weekends. History
/// starts when each market was listed: 2022-03-22 for PF_XBTUSD, ETH, SOL
/// and XRP, 2025-04 for FX, mid-2026 for the commodities.
///
/// Granularities Kraken does not serve are built from the largest native
/// resolution that divides them (2h from 1h, 10min from 5m) in fixed UTC
/// buckets; months and years are built from daily candles by calendar
/// period. No bar is made up: a bucket exists only if Kraken sent a candle
/// inside it.
///
/// `source` is "kraken_futures" and `upstream_id` is "<tick>/<symbol>"
/// ("spot/PF_XBTUSD"). Volume is set on TRADE candles only.
///
/// Pacing: one request at a time, at least 300 ms apart. The same spacing
/// is shared by every process on the machine (see `venue_http`), which also
/// backs off on a 429 and pauses the host after three refusals in a row.

const KF_CHARTS_URL: &str = "https://futures.kraken.com/api/charts/v1";
const KF_USER_AGENT: &str = "market-search (+https://eliterminal.com)";
/// Candles per request. Asking for 20000 returned 7000 on 2026-11-12, so the
/// pager stays at a size the API serves in full.
const KF_PAGE: i64 = 5000;
/// Pages per ticker per call: 550,000 candles, a year of one-minute bars.
const KF_MAX_PAGES: i64 = 110;
const KF_MIN_GAP_MS: u64 = 300;
/// Native resolutions, finest first.
const KF_RESOLUTIONS: &[(&str, i64)] = &[
    ("1m", 60),
    ("5m", 300),
    ("15m", 900),
    ("30m", 1800),
    ("1h", 3600),
    ("4h", 14400),
    ("12h", 43200),
    ("1d", 86400),
    ("1w", 604800),
];

/// When this process last sent Kraken Futures a request. The shared gate in
/// `venue_http` spaces requests across processes; this keeps the spacing
/// inside one process even if the gate's state directory cannot be written.
static KF_LAST_REQUEST: std::sync::LazyLock<tokio::sync::Mutex<Option<std::time::Instant>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KfTick {
    Index,
    Mark,
    Trade,
}

impl KfTick {
    /// The path segment Kraken uses.
    fn path(self) -> &'static str {
        match self {
            KfTick::Index => "spot",
            KfTick::Mark => "mark",
            KfTick::Trade => "trade",
        }
    }

    fn word(self) -> &'static str {
        match self {
            KfTick::Index => "index",
            KfTick::Mark => "mark",
            KfTick::Trade => "trade",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KfTarget {
    symbol: String,
    tick: KfTick,
}

impl KfTarget {
    fn upstream_id(&self) -> String {
        format!("{}/{}", self.tick.path(), self.symbol)
    }
}

/// Returns true if the ticker should route to the Kraken Futures provider:
/// the "KF:" prefix in any case. A bare "KF:" routes here too, so that it
/// gets this provider's error about the missing name.
pub fn is_kraken_futures_ticker(ticker: &str) -> bool {
    ticker
        .trim()
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("KF:"))
}

/// Names that have a market under a different symbol than PF_<NAME>USD.
fn kf_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        // There is no PF_BTCUSD.
        "BTC" | "XBT" | "BITCOIN" => "PF_XBTUSD",
        "GOLD" | "XAU" => "PF_XAUUSD",
        "SILVER" | "XAG" => "PF_XAGUSD",
        "OIL" | "WTI" | "CRUDE" | "CL" | "USOIL" => "PF_WTIOILUSD",
        "BRENT" | "BZ" | "UKOIL" => "PF_BRENTOILUSD",
        "NATGAS" | "NG" => "PF_NATGASUSD",
        "COPPER" | "HG" => "PF_XCUUSD",
        "EUR" => "PF_EURUSD",
        "GBP" => "PF_GBPUSD",
        // Dollars per franc (about 1.2), the inverse of the usual USD/CHF.
        "CHF" => "PF_CHFUSD",
        "US100" | "NDX" | "NASDAQ" => "PF_US100USD",
        _ => return None,
    })
}

/// The Kraken symbol for the NAME part of a ticker.
fn kf_symbol(name: &str) -> std::result::Result<String, String> {
    let name: String = name
        .trim()
        .to_ascii_uppercase()
        .chars()
        .filter(|c| *c != '/' && *c != '-')
        .collect();
    if name.is_empty()
        || name.len() > 40
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(
            "a Kraken Futures ticker looks like KF:BTC, KF:GOLD:MARK or KF:PF_XRPUSD:TRADE"
                .to_string(),
        );
    }
    if ["PF_", "FF_", "PI_", "FI_"].iter().any(|p| name.starts_with(p)) {
        return Ok(name);
    }
    // Kraken lists no S&P 500 market, and the symbol that looks like one is
    // a meme token. Say so instead of guessing.
    if matches!(name.as_str(), "SP500" | "SPY" | "GSPC" | "US500" | "SP500USD" | "US500USD") {
        return Err(
            "Kraken Futures lists no S&P 500 market (PF_SPXUSD is the SPX6900 token). Use HL:xyz:SP500 for a 24/7 perp or ^GSPC for the index."
                .to_string(),
        );
    }
    if let Some(symbol) = kf_alias(&name) {
        return Ok(symbol.to_string());
    }
    // KF:XRPUSD and KF:BTC/USD name the pair; the market is still PF_<BASE>USD.
    if let Some(base) = name.strip_suffix("USD").filter(|b| !b.is_empty()) {
        return Ok(match kf_alias(base) {
            Some(symbol) => symbol.to_string(),
            None => format!("PF_{base}USD"),
        });
    }
    Ok(format!("PF_{name}USD"))
}

/// Parse `KF:<NAME>[:<KIND>]`. The prefix is optional so that
/// `--provider kraken_futures --tickers BTC` works.
fn kf_parse_ticker(ticker: &str) -> std::result::Result<KfTarget, String> {
    let t = ticker.trim();
    let body = if is_kraken_futures_ticker(t) { &t[3..] } else { t };
    let mut parts = body.split(':');
    let name = parts.next().unwrap_or_default();
    let kind = parts.next().map(|k| k.trim().to_ascii_uppercase());
    if parts.next().is_some() {
        return Err(format!(
            "'{ticker}' has too many parts; write KF:<NAME> or KF:<NAME>:<KIND> with KIND one of INDEX, MARK, TRADE"
        ));
    }
    let tick = match kind.as_deref() {
        None | Some("") | Some("INDEX") | Some("SPOT") => KfTick::Index,
        Some("MARK") => KfTick::Mark,
        Some("TRADE") | Some("TRADES") => KfTick::Trade,
        Some(other) => {
            return Err(format!(
                "unknown kind '{other}' in '{ticker}'; use INDEX (the default), MARK or TRADE"
            ));
        }
    };
    Ok(KfTarget {
        symbol: kf_symbol(name)?,
        tick,
    })
}

/// How the bars of a requested granularity are made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KfBars {
    /// Kraken serves this resolution.
    Native,
    /// A whole multiple of a native resolution, in fixed UTC buckets.
    Fixed,
    /// Calendar months or years, from daily candles.
    Calendar,
}

/// The native resolution to request for a granularity, its length in
/// seconds, and how the requested bars are made from it.
fn kf_plan(granularity: Span) -> (&'static str, i64, KfBars) {
    if matches!(granularity.unit, SpanUnit::Month | SpanUnit::Year) {
        return ("1d", 86_400, KfBars::Calendar);
    }
    let requested = granularity.approx_duration().num_seconds();
    // The largest native resolution that divides the request evenly. One
    // minute divides every span the tool accepts, so there always is one.
    let (res, secs) = KF_RESOLUTIONS
        .iter()
        .rev()
        .find(|(_, secs)| *secs <= requested && requested % *secs == 0)
        .copied()
        .unwrap_or(KF_RESOLUTIONS[0]);
    let bars = if secs == requested { KfBars::Native } else { KfBars::Fixed };
    (res, secs, bars)
}

/// Index of the calendar bucket (n months or n years) that holds `t`.
fn kf_calendar_key(t: DateTime<Utc>, granularity: Span) -> i64 {
    use chrono::Datelike;
    let n = granularity.n.max(1);
    match granularity.unit {
        SpanUnit::Year => i64::from(t.year()).div_euclid(n),
        _ => (i64::from(t.year()) * 12 + i64::from(t.month0())).div_euclid(n),
    }
}

/// Open time of calendar bucket `key`: the first of its month, 00:00 UTC.
fn kf_calendar_open(key: i64, granularity: Span) -> Option<DateTime<Utc>> {
    let n = granularity.n.max(1);
    let first_month = match granularity.unit {
        SpanUnit::Year => key.checked_mul(n)?.checked_mul(12)?,
        _ => key.checked_mul(n)?,
    };
    let year = i32::try_from(first_month.div_euclid(12)).ok()?;
    let month = u32::try_from(first_month.rem_euclid(12)).ok()? + 1;
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single()
}

/// Daily candles folded into calendar months or years.
fn kf_calendar_resample(candles: &[Candle], granularity: Span) -> Vec<Candle> {
    let mut out: Vec<Candle> = Vec::new();
    let mut open_key: Option<i64> = None;
    for c in candles {
        let key = kf_calendar_key(c.t, granularity);
        match out.last_mut() {
            Some(bar) if open_key == Some(key) => {
                bar.h = bar.h.max(c.h);
                bar.l = bar.l.min(c.l);
                bar.c = c.c;
                if let Some(v) = c.v {
                    bar.v = Some(bar.v.unwrap_or(0.0) + v);
                }
            }
            _ => {
                let Some(t) = kf_calendar_open(key, granularity) else { continue };
                open_key = Some(key);
                out.push(Candle { t, o: c.o, h: c.h, l: c.l, c: c.c, v: c.v, kind: None });
            }
        }
    }
    out
}

fn kf_num(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|v| match v {
        serde_json::Value::String(s) => s.parse().ok(),
        other => other.as_f64(),
    })
}

/// Candles of one response, ascending. None when the body has no `candles`
/// array at all.
fn kf_parse_candles(body: &serde_json::Value, with_volume: bool) -> Option<Vec<Candle>> {
    let rows = body.get("candles")?.as_array()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(ms) = row.get("time").and_then(|v| v.as_i64()) else { continue };
        let (Some(o), Some(h), Some(l), Some(c)) = (
            kf_num(row.get("open")),
            kf_num(row.get("high")),
            kf_num(row.get("low")),
            kf_num(row.get("close")),
        ) else {
            continue;
        };
        let Some(t) = DateTime::<Utc>::from_timestamp_millis(ms) else { continue };
        // Index and mark candles always say volume "0": they have no volume,
        // so the field is left out. A trade candle with no trades keeps its 0.
        let v = if with_volume {
            kf_num(row.get("volume")).map(|v| v.max(0.0))
        } else {
            None
        };
        out.push(Candle { t, o, h, l, c, v, kind: None });
    }
    out.sort_by_key(|c| c.t);
    Some(out)
}

struct KfError {
    message: String,
    /// Kraken answered 429: leave it alone for the rest of this call.
    refused: bool,
}

impl KfError {
    fn new(message: impl Into<String>) -> Self {
        KfError { message: message.into(), refused: false }
    }
}

/// Wait until this process may send Kraken Futures another request. The lock
/// is held through the wait, so concurrent callers queue up behind it.
async fn kf_pace() {
    let mut last = KF_LAST_REQUEST.lock().await;
    if let Some(sent) = *last {
        let gap = std::time::Duration::from_millis(KF_MIN_GAP_MS);
        let since = sent.elapsed();
        if since < gap {
            tokio::time::sleep(gap - since).await;
        }
    }
    *last = Some(std::time::Instant::now());
}

/// GET one page of candles. A 5xx or a transport error is retried once after
/// a pause; a 429 has already been waited on and retried by the gate.
async fn kf_get(url: &str, with_volume: bool) -> std::result::Result<Vec<Candle>, KfError> {
    use crate::finance::venue_http::VenueSend;
    let client = crate::finance::venue_http::client();
    let mut last = KfError::new("no response from Kraken Futures");
    for attempt in 0..2u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
        kf_pace().await;
        let resp = match client
            .get(url)
            .header(reqwest::header::USER_AGENT, KF_USER_AGENT)
            .venue_send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = KfError::new(format!("request to Kraken Futures failed: {e}"));
                continue;
            }
        };
        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(KfError {
                message: crate::finance::venue_http::refusal_note(&resp).unwrap_or_else(|| {
                    "Kraken Futures is rate limiting this machine; wait a minute and retry".to_string()
                }),
                refused: true,
            });
        }
        if status.is_server_error() {
            last = KfError::new(format!("Kraken Futures answered http {}", status.as_u16()));
            continue;
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KfError::new(format!(
                "Kraken Futures answered http {}: {}",
                status.as_u16(),
                text.trim().chars().take(160).collect::<String>()
            )));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                last = KfError::new(format!("Kraken Futures sent a response that is not JSON: {e}"));
                continue;
            }
        };
        return kf_parse_candles(&body, with_volume)
            .ok_or_else(|| KfError::new("Kraken Futures sent a response with no candles array"));
    }
    Err(last)
}

struct KfSeries {
    candles: Vec<Candle>,
    notices: Vec<String>,
}

const KF_SKIPPED: &str = "skipped: Kraken Futures rate limited an earlier request in this call";

/// One entry for the response's error list. Stage "data_quality" is a
/// notice about a series that was served; the others are failures.
fn kf_issue(ticker: &str, stage: &str, message: String) -> TimeseriesError {
    TimeseriesError {
        ticker: ticker.to_string(),
        stage: Some(stage.to_string()),
        message,
    }
}

/// Fetch one market for a window, at the requested granularity.
async fn kf_fetch_target(
    target: &KfTarget,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> std::result::Result<KfSeries, KfError> {
    let (res, res_secs, bars) = kf_plan(granularity);
    let step = granularity.approx_duration();
    let step_secs = step.num_seconds().max(1);
    let tick = target.tick.path();
    let kind = target.tick.word();
    let symbol = target.symbol.as_str();
    let base = format!("{KF_CHARTS_URL}/{tick}/{symbol}/{res}");
    let with_volume = target.tick == KfTick::Trade;

    // The window in native candles: every bar that opens inside
    // [start, end], as the other candle sources return. `floor_s` is the
    // open of the first such bar. When bars are aggregated the window runs
    // to the end of the last one, so each bar is built from every candle
    // Kraken has for it (what a native bar that opens at `end` would show).
    let bad_window =
        || KfError::new("the requested window is outside the dates this tool can handle");
    let (floor_s, last_s) = match bars {
        KfBars::Native | KfBars::Fixed => {
            let first = (start.timestamp() + step_secs - 1).div_euclid(step_secs) * step_secs;
            let last = match bars {
                KfBars::Native => end.timestamp(),
                _ => end.timestamp().div_euclid(step_secs) * step_secs + step_secs - 1,
            };
            (first, last)
        }
        KfBars::Calendar => {
            let key = kf_calendar_key(start, granularity);
            let mut open = kf_calendar_open(key, granularity).ok_or_else(bad_window)?;
            if open < start {
                open = kf_calendar_open(key + 1, granularity).ok_or_else(bad_window)?;
            }
            let next = kf_calendar_open(kf_calendar_key(end, granularity) + 1, granularity)
                .ok_or_else(bad_window)?;
            (open.timestamp(), next.timestamp() - 1)
        }
    };
    let last_s = last_s.min(Utc::now().timestamp());
    if last_s < floor_s {
        return Err(KfError::new(format!(
            "no {} bar opens between {} and {}; ask for a longer range or a finer granularity",
            granularity.to_string_compact(),
            start.format("%Y-%m-%d %H:%M UTC"),
            end.format("%Y-%m-%d %H:%M UTC")
        )));
    }
    let needed = (last_s - floor_s) / res_secs + 1;
    if needed > KF_PAGE * KF_MAX_PAGES {
        return Err(KfError::new(format!(
            "this window is about {needed} {res} candles and one call reads at most {} from Kraken Futures; use a coarser --granularity or split the window with --start and --end",
            KF_PAGE * KF_MAX_PAGES
        )));
    }

    let mut candles: Vec<Candle> = Vec::new();
    let mut history_start_reached = false;
    let mut to_s = last_s;
    // The window fits in KF_MAX_PAGES pages; one more covers the empty page
    // that marks the start of the market's history.
    for _page in 0..=KF_MAX_PAGES {
        if to_s < floor_s {
            break;
        }
        let count = ((to_s - floor_s) / res_secs + 1).clamp(1, KF_PAGE);
        let page = kf_get(&format!("{base}?to={to_s}&count={count}"), with_volume).await?;
        let Some(first) = page.first().map(|c| c.t.timestamp()) else {
            history_start_reached = true;
            break;
        };
        candles.extend(page);
        if first <= floor_s || first > to_s {
            break;
        }
        to_s = first - 1;
    }

    candles.retain(|c| (floor_s..=last_s).contains(&c.t.timestamp()));
    candles.sort_by_key(|c| c.t);
    candles.dedup_by_key(|c| c.t);

    let Some(first_candle) = candles.first().map(|c| c.t) else {
        // Nothing in the window. The first candle the market ever printed
        // says whether the symbol exists at all.
        let first_url = format!(
            "{KF_CHARTS_URL}/{tick}/{symbol}/1d?from=0&to={}&count=1",
            Utc::now().timestamp()
        );
        let first = kf_get(&first_url, false).await?;
        return Err(KfError::new(match first.first() {
            None => format!(
                "Kraken Futures has no {kind} candles for '{symbol}'. Its symbols are listed at {KF_CHARTS_URL}/{tick}: perpetuals are PF_<BASE>USD, and bitcoin is PF_XBTUSD (KF:BTC)"
            ),
            Some(c) if c.t.timestamp() > last_s => format!(
                "Kraken Futures' {kind} candles for {symbol} start {}; this window ends before that ({})",
                c.t.format("%Y-%m-%d"),
                end.format("%Y-%m-%d %H:%M UTC")
            ),
            Some(c) => format!(
                "Kraken Futures has no {kind} candles for {symbol} between {} and {} (its history starts {}); the market may have expired or been delisted",
                start.format("%Y-%m-%d %H:%M UTC"),
                end.format("%Y-%m-%d %H:%M UTC"),
                c.t.format("%Y-%m-%d")
            ),
        }));
    };

    let mut notices = Vec::new();
    if history_start_reached {
        notices.push(format!(
            "Kraken Futures' {kind} candles for {symbol} start {}; the window asked for starts {}, and nothing earlier exists.",
            first_candle.format("%Y-%m-%d %H:%M UTC"),
            start.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    if symbol == "PF_SPXUSD" {
        notices.push(
            "PF_SPXUSD is the SPX6900 token, a meme coin, not the S&P 500. For the S&P 500 use HL:xyz:SP500 (24/7 perp) or ^GSPC (the index)."
                .to_string(),
        );
    }
    let candles = match bars {
        KfBars::Native => candles,
        KfBars::Fixed => resample_candles(&candles, start, step),
        KfBars::Calendar => kf_calendar_resample(&candles, granularity),
    };
    Ok(KfSeries { candles, notices })
}

/// Fetch Kraken Futures series. Same signature and return conventions as the
/// other prefix-routed fetchers.
pub(crate) async fn fetch_kraken_futures_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mut refused = false;

    for ticker in tickers {
        let target = match kf_parse_ticker(ticker) {
            Ok(target) => target,
            Err(message) => {
                errors.push(kf_issue(ticker, "input", message));
                continue;
            }
        };
        if refused {
            errors.push(kf_issue(ticker, "fetch", KF_SKIPPED.to_string()));
            continue;
        }
        match kf_fetch_target(&target, start, end, granularity).await {
            Ok(series) => {
                for message in series.notices {
                    errors.push(kf_issue(ticker, "data_quality", message));
                }
                out.push(TickerSeries {
                    ticker: ticker.clone(),
                    candles: series.candles,
                    source: Some("kraken_futures".to_string()),
                    upstream_id: Some(target.upstream_id()),
                });
            }
            Err(e) => {
                refused = refused || e.refused;
                errors.push(kf_issue(ticker, "fetch", e.message));
            }
        }
    }

    Ok((out, errors))
}
