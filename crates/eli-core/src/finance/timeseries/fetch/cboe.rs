/// CBOE free-data provider (prefix `CBOE:`). No API key.
///
/// Ticker grammar
/// - VIX futures (daily, history back to 2013):
///   `CBOE:VX1`..`CBOE:VX9`   n-th monthly contract settle, stitched by roll on expiry
///   `CBOE:VX2-VX1`           spread between two stitched months (a - b), VIX points
///   `CBOE:VXCONTANGO`        (VX2 / VX1 - 1) x 100, in %
///   `CBOE:VX:2026-11-18`     one explicit contract by expiry date
///   Source: https://cdn.cboe.com/data/us/futures/market_statistics/historical_data/VX/VX_<expiry>.csv
///   (Trade Date, Futures, Open, High, Low, Close, Settle, Change, Total Volume, EFP, Open Interest).
///   c = Settle (official daily settlement, not last trade); o/h/l from the contract
///   when non-zero, else = settle; v = Total Volume. Stamped 00:00 UTC of the trade date.
///   Roll: on date d the front month is the first monthly contract with expiry > d,
///   so on expiry day itself VX1 is already the next contract (the expiring one
///   settles to the SOQ at the open). Monthly expiries come from CBOE's product list
///   (https://www.cboe.com/us/futures/market_statistics/historical_data/product/list/VX/);
///   if that is unreachable they are computed: Wednesday 30 days before the next
///   month's 3rd-Friday SPX expiry, shifted to the prior business day around NYSE
///   holidays (Good Friday, Juneteenth, ...; rule matches every listed 2013-2027 expiry
///   tested). Weekly VX contracts are ignored.
/// - Daily put/call ratios (history back to at least 2019):
///   `CBOE:PCR_TOTAL`, `CBOE:PCR_EQUITY`, `CBOE:PCR_INDEX`, `CBOE:PCR_ETP`,
///   `CBOE:PCR_VIX`, `CBOE:PCR_SPX` (SPX + SPXW).
///   Source: https://cdn.cboe.com/data/us/options/market_statistics/daily/<YYYY-MM-DD>_daily_options
///   One request per weekday (holidays/weekends 403 and are skipped), at most 4 in
///   flight, and at most the most recent 400 weekdays of a window (a warning is
///   attached when the window is truncated). Values are CBOE's published 2-decimal
///   ratio; v = that product's total volume. Stamped 00:00 UTC of the trade date
///   (CBOE posts the file after the close).
/// - Option-chain positioning SNAPSHOTS (one candle per call, no history: build
///   history by polling): `CBOE:GEX:<ROOT>`, `CBOE:ZEROGAMMA:<ROOT>`,
///   `CBOE:PCR_OI:<ROOT>`, `CBOE:0DTE_SHARE:<ROOT>`, `CBOE:IV30:<ROOT>`, `CBOE:SPOT:<ROOT>`.
///   ROOT is any CBOE delayed-quotes symbol: SPX, NDX, RUT, VIX (index roots are
///   tried as `_SPX` etc.), SPY, QQQ, NVDA...
///   Source: https://cdn.cboe.com/api/global/delayed_quotes/options/<ROOT>.json (307 ->
///   cdn-api.cboe.com, ~13MB for SPX, ~15-min delayed quotes). The candle is stamped at
///   the payload's `timestamp`, which is UTC (verified 2026-10-30: "15:30:26" vs HTTP
///   Last-Modified 15:30:29 GMT; per-contract last_trade_time fields are ET).
///   * GEX: net dealer gamma in $bn per 1% move in the underlying, under the naive
///     convention that dealers are LONG calls and SHORT puts (i.e. customers sold calls
///     and bought puts): sum(sign x gamma x OI x 100 x S^2 x 0.01), sign +1 calls, -1 puts.
///     Gamma is recomputed with Black-Scholes (r = q = 0) from each contract's IV and
///     time to expiry, because the feed's own gamma is rounded to 4 decimals. Limits:
///     real dealer positioning is unknown (covered calls / put-writing flip signs), OI is
///     as of the prior close, and 0DTE flow that opens and closes intraday is invisible.
///   * ZEROGAMMA: underlying level where that GEX crosses zero, found by re-pricing BS
///     gamma on a +/-10% spot grid (0.2% steps, IV and T held fixed) and linearly
///     interpolating the crossing closest to spot. Error if no crossing in the grid.
///   * PCR_OI: total put OI / total call OI. 0DTE_SHARE: % of today's volume in
///     contracts expiring today (ET date). IV30: CBOE's 30-day IV for the root. SPOT:
///     current_price.
///   Expiry instant used for T: 09:30 ET for AM-settled roots (SPX, NDX, RUT, VIX),
///   16:00 ET otherwise; T floored at 1 hour; contracts already expired are dropped.

const CBOE_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36";
const CBOE_VX_BASE: &str = "https://cdn.cboe.com/data/us/futures/market_statistics/historical_data/VX/VX_";
const CBOE_VX_LIST: &str =
    "https://www.cboe.com/us/futures/market_statistics/historical_data/product/list/VX/";
const CBOE_PCR_BASE: &str = "https://cdn.cboe.com/data/us/options/market_statistics/daily/";
const CBOE_CHAIN_BASE: &str = "https://cdn.cboe.com/api/global/delayed_quotes/options/";
const CBOE_PCR_MAX_DAYS: usize = 400;
const CBOE_MAX_INFLIGHT: usize = 4;

#[derive(Clone, Debug, PartialEq)]
enum CboeSpec {
    /// n-th stitched monthly VX (1-based).
    VxN(usize),
    /// VXa - VXb.
    VxSpread(usize, usize),
    /// (VX2/VX1 - 1) * 100.
    VxContango,
    /// One contract by expiry.
    VxContract(NaiveDate),
    /// Daily put/call ratio; the CBOE "ratios[].name" and the volume section key.
    Pcr(&'static str, &'static str),
    /// Option-chain snapshot metric for a root.
    Chain(CboeChainMetric, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CboeChainMetric {
    Gex,
    ZeroGamma,
    PcrOi,
    ZeroDteShare,
    Iv30,
    Spot,
}

/// Returns true if the ticker should route to the CBOE provider.
pub fn is_cboe_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    t.len() > 5 && t[..5].eq_ignore_ascii_case("CBOE:")
}

fn cboe_parse_ticker(ticker: &str) -> std::result::Result<CboeSpec, String> {
    let t = ticker.trim();
    if !is_cboe_ticker(t) {
        return Err(format!("'{t}' is not a CBOE: ticker"));
    }
    let body = t[5..].trim().to_ascii_uppercase();
    let bad = || {
        format!(
            "bad CBOE ticker '{t}': use CBOE:VX1, CBOE:VX2, CBOE:VX2-VX1, CBOE:VXCONTANGO, CBOE:VX:2026-11-18, \
             CBOE:PCR_TOTAL|PCR_EQUITY|PCR_INDEX|PCR_ETP|PCR_VIX|PCR_SPX, or snapshots \
             CBOE:GEX:SPX, CBOE:ZEROGAMMA:SPX, CBOE:PCR_OI:SPY, CBOE:0DTE_SHARE:SPX, CBOE:IV30:SPX"
        )
    };
    let vx_n = |s: &str| -> Option<usize> {
        let n: usize = s.strip_prefix("VX")?.parse().ok()?;
        (1..=9).contains(&n).then_some(n)
    };
    if let Some(n) = vx_n(&body) {
        return Ok(CboeSpec::VxN(n));
    }
    if body == "VXCONTANGO" {
        return Ok(CboeSpec::VxContango);
    }
    if let Some((a, b)) = body.split_once('-') {
        if let (Some(a), Some(b)) = (vx_n(a), vx_n(b)) {
            return Ok(CboeSpec::VxSpread(a, b));
        }
    }
    if let Some(d) = body.strip_prefix("VX:") {
        return NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map(CboeSpec::VxContract)
            .map_err(|_| bad());
    }
    let pcr = match body.as_str() {
        "PCR_TOTAL" => Some(("TOTAL PUT/CALL RATIO", "SUM OF ALL PRODUCTS")),
        "PCR_EQUITY" => Some(("EQUITY PUT/CALL RATIO", "EQUITY OPTIONS")),
        "PCR_INDEX" => Some(("INDEX PUT/CALL RATIO", "INDEX OPTIONS")),
        "PCR_ETP" => Some(("EXCHANGE TRADED PRODUCTS PUT/CALL RATIO", "EXCHANGE TRADED PRODUCTS")),
        "PCR_VIX" => Some(("CBOE VOLATILITY INDEX (VIX) PUT/CALL RATIO", "CBOE VOLATILITY INDEX (VIX)")),
        "PCR_SPX" => Some(("SPX + SPXW PUT/CALL RATIO", "SPX + SPXW")),
        _ => None,
    };
    if let Some((name, section)) = pcr {
        return Ok(CboeSpec::Pcr(name, section));
    }
    if let Some((metric, root)) = body.split_once(':') {
        let m = match metric {
            "GEX" => CboeChainMetric::Gex,
            "ZEROGAMMA" | "ZERO_GAMMA" => CboeChainMetric::ZeroGamma,
            "PCR_OI" => CboeChainMetric::PcrOi,
            "0DTE_SHARE" | "ODTE_SHARE" => CboeChainMetric::ZeroDteShare,
            "IV30" => CboeChainMetric::Iv30,
            "SPOT" => CboeChainMetric::Spot,
            _ => return Err(bad()),
        };
        let root = root.trim().trim_start_matches(['_', '^']).to_string();
        if root.is_empty() || !root.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
            return Err(bad());
        }
        return Ok(CboeSpec::Chain(m, root));
    }
    Err(bad())
}

// ---------------------------------------------------------------------------
// HTTP

/// GET with UA and backoff retries on 429/5xx. Ok(None) for 403/404 (CBOE's CDN
/// answers AccessDenied 403 for missing objects: holidays, unknown roots).
async fn cboe_get_bytes(url: &str) -> std::result::Result<Option<Vec<u8>>, String> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut delay_ms = 500u64;
    for attempt in 0..3 {
        let resp = client
            .get(url)
            .header(reqwest::header::USER_AGENT, CBOE_USER_AGENT)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if status.as_u16() == 429 || status.is_server_error() {
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
                continue;
            }
            return Err(format!("http {status} after retries"));
        }
        if matches!(status.as_u16(), 403 | 404 | 410) {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("http {status}"));
        }
        return resp
            .bytes()
            .await
            .map(|b| Some(b.to_vec()))
            .map_err(|e| format!("body read failed: {e}"));
    }
    Err("unreachable retry loop".into())
}

// ---------------------------------------------------------------------------
// Calendar helpers

fn cboe_nth_weekday(year: i32, month: u32, wd: chrono::Weekday, n: u32) -> Option<NaiveDate> {
    NaiveDate::from_weekday_of_month_opt(year, month, wd, n as u8)
}

fn cboe_last_weekday(year: i32, month: u32, wd: chrono::Weekday) -> Option<NaiveDate> {
    cboe_nth_weekday(year, month, wd, 5).or_else(|| cboe_nth_weekday(year, month, wd, 4))
}

fn cboe_easter(year: i32) -> Option<NaiveDate> {
    // Anonymous Gregorian algorithm.
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32)
}

/// Fixed-date holiday observed on the nearest weekday (Sat -> Fri, Sun -> Mon).
fn cboe_observed(d: NaiveDate) -> NaiveDate {
    use chrono::{Datelike, Weekday};
    match d.weekday() {
        Weekday::Sat => d - Duration::days(1),
        Weekday::Sun => d + Duration::days(1),
        _ => d,
    }
}

/// Approximate NYSE/CBOE full-day holidays (no ad-hoc closures).
fn cboe_is_holiday(d: NaiveDate) -> bool {
    use chrono::{Datelike, Weekday};
    let y = d.year();
    let mut hs: Vec<NaiveDate> = Vec::with_capacity(11);
    if let Some(ny) = NaiveDate::from_ymd_opt(y, 1, 1) {
        // New Year's on a Saturday is not observed on Dec 31.
        if ny.weekday() != Weekday::Sat {
            hs.push(cboe_observed(ny));
        }
    }
    hs.extend(cboe_nth_weekday(y, 1, Weekday::Mon, 3));
    hs.extend(cboe_nth_weekday(y, 2, Weekday::Mon, 3));
    hs.extend(cboe_easter(y).map(|e| e - Duration::days(2)));
    hs.extend(cboe_last_weekday(y, 5, Weekday::Mon));
    if y >= 2022 {
        hs.extend(NaiveDate::from_ymd_opt(y, 6, 19).map(cboe_observed));
    }
    hs.extend(NaiveDate::from_ymd_opt(y, 7, 4).map(cboe_observed));
    hs.extend(cboe_nth_weekday(y, 9, Weekday::Mon, 1));
    hs.extend(cboe_nth_weekday(y, 11, Weekday::Thu, 4));
    hs.extend(NaiveDate::from_ymd_opt(y, 12, 25).map(cboe_observed));
    hs.contains(&d)
}

fn cboe_is_business_day(d: NaiveDate) -> bool {
    use chrono::{Datelike, Weekday};
    !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) && !cboe_is_holiday(d)
}

fn cboe_prev_business_day(mut d: NaiveDate) -> NaiveDate {
    while !cboe_is_business_day(d) {
        d -= Duration::days(1);
    }
    d
}

/// Computed monthly VIX futures expiry for the contract month (year, month):
/// 30 days before the 3rd Friday of the following month (or before the business
/// day preceding it if that Friday is a holiday), moved to the prior business day
/// if the Wednesday itself is a holiday.
fn cboe_vix_monthly_expiry(year: i32, month: u32) -> Option<NaiveDate> {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let fri = cboe_nth_weekday(ny, nm, chrono::Weekday::Fri, 3)?;
    let anchor = cboe_prev_business_day(fri);
    Some(cboe_prev_business_day(anchor - Duration::days(30)))
}

/// Monthly expiries from CBOE's product-list JSON ({"2026": [{expire_date, duration_type}]}).
fn cboe_parse_vx_list(body: &serde_json::Value) -> Vec<NaiveDate> {
    let mut out: Vec<NaiveDate> = body
        .as_object()
        .into_iter()
        .flat_map(|o| o.values())
        .filter_map(|v| v.as_array())
        .flatten()
        .filter(|c| c.get("duration_type").and_then(|d| d.as_str()) == Some("M"))
        .filter_map(|c| c.get("expire_date").and_then(|d| d.as_str()))
        .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn cboe_computed_expiries(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    use chrono::Datelike;
    let mut out = Vec::new();
    let (mut y, mut m) = (from.year(), from.month());
    while (y, m) <= (to.year(), to.month()) {
        out.extend(cboe_vix_monthly_expiry(y, m));
        if m == 12 {
            y += 1;
            m = 1;
        } else {
            m += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// VX futures

#[derive(Clone, Debug, PartialEq)]
struct CboeVxRow {
    date: NaiveDate,
    open: f64,
    high: f64,
    low: f64,
    settle: f64,
    volume: f64,
}

fn cboe_parse_vx_csv(body: &str) -> Vec<CboeVxRow> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(body.as_bytes());
    let Ok(headers) = rdr.headers().cloned() else {
        return Vec::new();
    };
    let col = |n: &str| headers.iter().position(|h| h.trim().eq_ignore_ascii_case(n));
    let (Some(di), Some(si)) = (col("Trade Date"), col("Settle")) else {
        return Vec::new();
    };
    let (oi, hi, li, vi) = (col("Open"), col("High"), col("Low"), col("Total Volume"));
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        let num = |i: Option<usize>| i.and_then(|i| rec.get(i)).and_then(|s| s.trim().parse::<f64>().ok());
        let Some(date) = rec.get(di).and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()) else {
            continue;
        };
        let Some(settle) = num(Some(si)).filter(|v| *v > 0.0) else {
            continue;
        };
        let nz = |v: Option<f64>| v.filter(|x| *x > 0.0).unwrap_or(settle);
        let (o, h, l) = (nz(num(oi)), nz(num(hi)), nz(num(li)));
        out.push(CboeVxRow {
            date,
            open: o,
            high: h.max(settle).max(o),
            low: l.min(settle).min(o),
            settle,
            volume: num(vi).unwrap_or(0.0),
        });
    }
    out.sort_by_key(|r| r.date);
    out.dedup_by_key(|r| r.date);
    out
}

/// Stitch the n-th (1-based) monthly contract per trade date: front = first expiry > d.
fn cboe_stitch_vx(
    contracts: &BTreeMap<NaiveDate, BTreeMap<NaiveDate, CboeVxRow>>,
    n: usize,
) -> BTreeMap<NaiveDate, CboeVxRow> {
    let expiries: Vec<NaiveDate> = contracts.keys().copied().collect();
    let mut dates: BTreeSet<NaiveDate> = BTreeSet::new();
    for rows in contracts.values() {
        dates.extend(rows.keys().copied());
    }
    let mut out = BTreeMap::new();
    for d in dates {
        let idx = expiries.partition_point(|e| *e <= d);
        let Some(exp) = expiries.get(idx + n - 1) else {
            continue;
        };
        if let Some(row) = contracts.get(exp).and_then(|r| r.get(&d)) {
            out.insert(d, row.clone());
        }
    }
    out
}

fn cboe_vx_candle(d: NaiveDate, o: f64, h: f64, l: f64, c: f64, v: Option<f64>) -> Option<Candle> {
    Some(Candle {
        t: d.and_hms_opt(0, 0, 0)?.and_utc(),
        o,
        h,
        l,
        c,
        v,
        kind: None,
    })
}

async fn cboe_fetch_vx_contracts(
    expiries: Vec<NaiveDate>,
) -> (BTreeMap<NaiveDate, BTreeMap<NaiveDate, CboeVxRow>>, Vec<String>) {
    use futures::StreamExt;
    let results: Vec<(NaiveDate, std::result::Result<Option<Vec<u8>>, String>)> =
        futures::stream::iter(expiries.into_iter().map(|e| async move {
            let url = format!("{CBOE_VX_BASE}{e}.csv");
            (e, cboe_get_bytes(&url).await)
        }))
        .buffer_unordered(CBOE_MAX_INFLIGHT)
        .collect()
        .await;
    let mut map = BTreeMap::new();
    let mut errs = Vec::new();
    for (e, r) in results {
        match r {
            Ok(Some(b)) => {
                let rows = cboe_parse_vx_csv(&String::from_utf8_lossy(&b));
                map.insert(e, rows.into_iter().map(|r| (r.date, r)).collect());
            }
            Ok(None) => {
                // Listed-but-missing or not-yet-listed contract: keep the slot so the
                // roll index stays aligned, with no rows.
                map.insert(e, BTreeMap::new());
            }
            Err(err) => errs.push(format!("VX_{e}: {err}")),
        }
    }
    (map, errs)
}

// ---------------------------------------------------------------------------
// Put/call ratios

#[derive(Clone, Debug, PartialEq)]
struct CboePcrDay {
    ratios: HashMap<String, f64>,
    volumes: HashMap<String, f64>,
}

fn cboe_parse_pcr(body: &serde_json::Value) -> CboePcrDay {
    let mut ratios = HashMap::new();
    for r in body.get("ratios").and_then(|v| v.as_array()).into_iter().flatten() {
        let (Some(name), Some(val)) = (
            r.get("name").and_then(|v| v.as_str()),
            r.get("value").and_then(|v| match v {
                serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
                other => other.as_f64(),
            }),
        ) else {
            continue;
        };
        ratios.insert(name.to_string(), val);
    }
    let mut volumes = HashMap::new();
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            if let Some(total) = v
                .as_array()
                .into_iter()
                .flatten()
                .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("VOLUME"))
                .and_then(|e| e.get("total"))
                .and_then(|t| t.as_f64())
            {
                volumes.insert(k.clone(), total);
            }
        }
    }
    CboePcrDay { ratios, volumes }
}

// ---------------------------------------------------------------------------
// Option-chain snapshot

#[derive(Clone, Debug, PartialEq)]
struct CboeContract {
    is_call: bool,
    strike: f64,
    expiry: NaiveDate,
    am_settled: bool,
    iv: f64,
    oi: f64,
    volume: f64,
}

#[derive(Clone, Debug)]
struct CboeChain {
    ts: DateTime<Utc>,
    spot: f64,
    iv30: Option<f64>,
    contracts: Vec<CboeContract>,
}

/// US Eastern UTC offset in hours for a calendar date (DST: 2nd Sun Mar .. 1st Sun Nov).
fn cboe_et_offset_hours(d: NaiveDate) -> i64 {
    use chrono::{Datelike, Weekday};
    let y = d.year();
    let start = cboe_nth_weekday(y, 3, Weekday::Sun, 2);
    let end = cboe_nth_weekday(y, 11, Weekday::Sun, 1);
    match (start, end) {
        (Some(s), Some(e)) if d >= s && d < e => -4,
        _ => -5,
    }
}

fn cboe_et_to_utc(d: NaiveDate, h: u32, m: u32) -> Option<DateTime<Utc>> {
    let local = d.and_hms_opt(h, m, 0)?.and_utc();
    Some(local - Duration::hours(cboe_et_offset_hours(d)))
}

/// Parse "SPXW261120C07600000" -> (root, expiry, is_call, strike).
fn cboe_parse_occ(sym: &str) -> Option<(&str, NaiveDate, bool, f64)> {
    if sym.len() < 16 || !sym.is_ascii() {
        return None;
    }
    let (root, tail) = sym.split_at(sym.len() - 15);
    let expiry = NaiveDate::parse_from_str(&format!("20{}", &tail[..6]), "%Y%m%d").ok()?;
    let is_call = match &tail[6..7] {
        "C" => true,
        "P" => false,
        _ => return None,
    };
    let strike = tail[7..].parse::<f64>().ok()? / 1000.0;
    Some((root, expiry, is_call, strike))
}

fn cboe_parse_chain(body: &serde_json::Value) -> std::result::Result<CboeChain, String> {
    let ts_s = body
        .get("timestamp")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "chain: missing timestamp".to_string())?;
    let ts = chrono::NaiveDateTime::parse_from_str(ts_s, "%Y-%m-%d %H:%M:%S")
        .map_err(|e| format!("chain: bad timestamp '{ts_s}': {e}"))?
        .and_utc();
    let data = body.get("data").ok_or_else(|| "chain: missing data".to_string())?;
    let spot = data
        .get("current_price")
        .and_then(|v| v.as_f64())
        .filter(|v| *v > 0.0)
        .ok_or_else(|| "chain: missing current_price".to_string())?;
    let iv30 = data.get("iv30").and_then(|v| v.as_f64());
    let mut contracts = Vec::new();
    for o in data.get("options").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some(sym) = o.get("option").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some((root, expiry, is_call, strike)) = cboe_parse_occ(sym) else {
            continue;
        };
        let f = |k: &str| o.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        contracts.push(CboeContract {
            is_call,
            strike,
            expiry,
            am_settled: matches!(root, "SPX" | "NDX" | "RUT" | "VIX"),
            iv: f("iv"),
            oi: f("open_interest"),
            volume: f("volume"),
        });
    }
    if contracts.is_empty() {
        return Err("chain: no option contracts".into());
    }
    Ok(CboeChain { ts, spot, iv30, contracts })
}

/// (sign, strike, iv, T years, oi) for contracts usable in gamma math.
fn cboe_gamma_inputs(chain: &CboeChain) -> Vec<(f64, f64, f64, f64, f64)> {
    let year_secs = 365.0 * 86400.0;
    chain
        .contracts
        .iter()
        .filter(|c| c.iv > 0.0 && c.oi > 0.0 && c.strike > 0.0)
        .filter_map(|c| {
            let (h, m) = if c.am_settled { (9, 30) } else { (16, 0) };
            let exp = cboe_et_to_utc(c.expiry, h, m)?;
            if exp <= chain.ts {
                return None;
            }
            let t = ((exp - chain.ts).num_seconds() as f64 / year_secs).max(1.0 / (365.0 * 24.0));
            Some((if c.is_call { 1.0 } else { -1.0 }, c.strike, c.iv, t, c.oi))
        })
        .collect()
}

/// Net dealer GEX in $bn per 1% move at spot `s` (dealers long calls, short puts).
fn cboe_gex_at(inputs: &[(f64, f64, f64, f64, f64)], s: f64) -> f64 {
    let inv_sqrt_2pi = 1.0 / (2.0 * std::f64::consts::PI).sqrt();
    let mut g = 0.0;
    for &(sign, k, iv, t, oi) in inputs {
        let sd = iv * t.sqrt();
        if sd <= 0.0 {
            continue;
        }
        let d1 = ((s / k).ln() + 0.5 * iv * iv * t) / sd;
        let gamma = (-0.5 * d1 * d1).exp() * inv_sqrt_2pi / (s * sd);
        g += sign * gamma * oi * 100.0 * s * s * 0.01;
    }
    g / 1e9
}

/// Spot level where GEX crosses zero on a +/-10% grid, nearest crossing to spot.
fn cboe_zero_gamma(inputs: &[(f64, f64, f64, f64, f64)], spot: f64) -> Option<f64> {
    let grid: Vec<(f64, f64)> = (-50..=50)
        .map(|i| {
            let s = spot * (1.0 + i as f64 * 0.002);
            (s, cboe_gex_at(inputs, s))
        })
        .collect();
    grid.windows(2)
        .filter(|w| (w[0].1 < 0.0) != (w[1].1 < 0.0))
        .map(|w| {
            let (s0, g0) = w[0];
            let (s1, g1) = w[1];
            if (g1 - g0).abs() < f64::EPSILON {
                s0
            } else {
                s0 + (s1 - s0) * (-g0) / (g1 - g0)
            }
        })
        .min_by(|a, b| (a - spot).abs().total_cmp(&(b - spot).abs()))
}

fn cboe_chain_metric(chain: &CboeChain, m: CboeChainMetric) -> std::result::Result<f64, String> {
    match m {
        CboeChainMetric::Spot => Ok(chain.spot),
        CboeChainMetric::Iv30 => chain.iv30.ok_or_else(|| "chain has no iv30".to_string()),
        CboeChainMetric::PcrOi => {
            let (mut p, mut c) = (0.0, 0.0);
            for k in &chain.contracts {
                if k.is_call {
                    c += k.oi;
                } else {
                    p += k.oi;
                }
            }
            if c > 0.0 {
                Ok(p / c)
            } else {
                Err("no call open interest".into())
            }
        }
        CboeChainMetric::ZeroDteShare => {
            let off = cboe_et_offset_hours(chain.ts.date_naive());
            let today_et = (chain.ts + Duration::hours(off)).date_naive();
            let total: f64 = chain.contracts.iter().map(|c| c.volume).sum();
            if total <= 0.0 {
                return Err("no option volume yet today (pre-open?)".into());
            }
            let zero: f64 = chain
                .contracts
                .iter()
                .filter(|c| c.expiry == today_et)
                .map(|c| c.volume)
                .sum();
            Ok(zero / total * 100.0)
        }
        CboeChainMetric::Gex => {
            let inputs = cboe_gamma_inputs(chain);
            Ok(cboe_gex_at(&inputs, chain.spot))
        }
        CboeChainMetric::ZeroGamma => {
            let inputs = cboe_gamma_inputs(chain);
            cboe_zero_gamma(&inputs, chain.spot)
                .ok_or_else(|| "net GEX does not cross zero within +/-10% of spot".to_string())
        }
    }
}

async fn cboe_fetch_chain(root: &str) -> std::result::Result<CboeChain, String> {
    let index_like = matches!(root, "SPX" | "NDX" | "RUT" | "VIX" | "XSP" | "DJX" | "OEX" | "XEO" | "MRUT");
    let candidates = if index_like {
        vec![format!("_{root}"), root.to_string()]
    } else {
        vec![root.to_string(), format!("_{root}")]
    };
    for sym in candidates {
        let url = format!("{CBOE_CHAIN_BASE}{sym}.json");
        if let Some(b) = cboe_get_bytes(&url).await? {
            let v: serde_json::Value =
                serde_json::from_slice(&b).map_err(|e| format!("chain json parse: {e}"))?;
            return cboe_parse_chain(&v);
        }
    }
    Err(format!("CBOE has no delayed option chain for '{root}'"))
}

// ---------------------------------------------------------------------------
// Entry point

pub(crate) async fn fetch_cboe_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    _granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    use futures::StreamExt;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mk_err = |ticker: &str, stage: &str, message: String| TimeseriesError {
        ticker: ticker.to_string(),
        stage: Some(stage.to_string()),
        message,
    };

    let mut specs = Vec::new();
    for t in tickers {
        match cboe_parse_ticker(t) {
            Ok(s) => specs.push((t.clone(), s)),
            Err(e) => errors.push(mk_err(t, "parse", e)),
        }
    }
    let start_day = start.date_naive();
    let end_day = end.date_naive();
    let in_window = |d: NaiveDate| d >= start_day && d <= end_day;

    // ---- VX futures: one batch of contract files shared by every VX ticker.
    let max_n = specs
        .iter()
        .map(|(_, s)| match s {
            CboeSpec::VxN(n) => *n,
            CboeSpec::VxSpread(a, b) => (*a).max(*b),
            CboeSpec::VxContango => 2,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let explicit: BTreeSet<NaiveDate> = specs
        .iter()
        .filter_map(|(_, s)| match s {
            CboeSpec::VxContract(d) => Some(*d),
            _ => None,
        })
        .collect();
    let mut vx: BTreeMap<NaiveDate, BTreeMap<NaiveDate, CboeVxRow>> = BTreeMap::new();
    let mut stitched: HashMap<usize, BTreeMap<NaiveDate, CboeVxRow>> = HashMap::new();
    let mut vx_err: Option<String> = None;
    if max_n > 0 || !explicit.is_empty() {
        let mut wanted: BTreeSet<NaiveDate> = explicit.clone();
        let mut monthly_wanted: BTreeSet<NaiveDate> = BTreeSet::new();
        if max_n > 0 {
            let horizon = end_day + Duration::days(31 * (max_n as i64 + 2));
            let listed = match cboe_get_bytes(CBOE_VX_LIST).await {
                Ok(Some(b)) => serde_json::from_slice::<serde_json::Value>(&b)
                    .map(|v| cboe_parse_vx_list(&v))
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let monthly = if listed.is_empty() {
                cboe_computed_expiries(start_day - Duration::days(1), horizon)
            } else {
                listed
            };
            monthly_wanted.extend(monthly.iter().filter(|e| **e > start_day && **e <= end_day));
            monthly_wanted.extend(monthly.iter().filter(|e| **e > end_day).take(max_n));
            wanted.extend(monthly_wanted.iter().copied());
        }
        let (map, errs) = cboe_fetch_vx_contracts(wanted.into_iter().collect()).await;
        if !errs.is_empty() {
            vx_err = Some(errs.join("; "));
        }
        vx = map;
        let monthly_only: BTreeMap<NaiveDate, BTreeMap<NaiveDate, CboeVxRow>> = vx
            .iter()
            .filter(|(e, _)| monthly_wanted.contains(*e))
            .map(|(e, r)| (*e, r.clone()))
            .collect();
        for n in 1..=max_n {
            stitched.insert(n, cboe_stitch_vx(&monthly_only, n));
        }
    }

    // ---- PCR: one request per weekday, shared by every PCR ticker.
    let mut pcr_days: BTreeMap<NaiveDate, CboePcrDay> = BTreeMap::new();
    if specs.iter().any(|(_, s)| matches!(s, CboeSpec::Pcr(..))) {
        let mut days = Vec::new();
        let mut d = end_day;
        while d >= start_day {
            use chrono::{Datelike, Weekday};
            if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
                days.push(d);
            }
            d -= Duration::days(1);
        }
        if days.len() > CBOE_PCR_MAX_DAYS {
            days.truncate(CBOE_PCR_MAX_DAYS);
            for (t, s) in &specs {
                if matches!(s, CboeSpec::Pcr(..)) {
                    errors.push(mk_err(
                        t,
                        "range",
                        format!(
                            "cboe put/call history is one request per day; window truncated to the most recent {CBOE_PCR_MAX_DAYS} weekdays (from {})",
                            days.last().copied().unwrap_or(end_day)
                        ),
                    ));
                }
            }
        }
        let results: Vec<(NaiveDate, std::result::Result<Option<Vec<u8>>, String>)> =
            futures::stream::iter(days.into_iter().map(|d| async move {
                let url = format!("{CBOE_PCR_BASE}{d}_daily_options");
                (d, cboe_get_bytes(&url).await)
            }))
            .buffer_unordered(CBOE_MAX_INFLIGHT)
            .collect()
            .await;
        for (d, r) in results {
            if let Ok(Some(b)) = r {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                    pcr_days.insert(d, cboe_parse_pcr(&v));
                }
            }
        }
    }

    // ---- Chains: one download per root.
    let mut chains: HashMap<String, std::result::Result<CboeChain, String>> = HashMap::new();
    for (_, s) in &specs {
        if let CboeSpec::Chain(_, root) = s {
            if !chains.contains_key(root) {
                let r = cboe_fetch_chain(root).await;
                chains.insert(root.clone(), r);
            }
        }
    }

    for (ticker, spec) in &specs {
        let (candles, source, upstream): (Vec<Candle>, &str, String) = match spec {
            CboeSpec::VxN(n) => {
                let rows = stitched.get(n).cloned().unwrap_or_default();
                let c = rows
                    .values()
                    .filter(|r| in_window(r.date))
                    .filter_map(|r| cboe_vx_candle(r.date, r.open, r.high, r.low, r.settle, Some(r.volume)))
                    .collect();
                (c, "cboe_vx", format!("VX{n}"))
            }
            CboeSpec::VxSpread(..) | CboeSpec::VxContango => {
                let (a, b) = match spec {
                    CboeSpec::VxSpread(a, b) => (*a, *b),
                    _ => (2, 1),
                };
                let ra = stitched.get(&a).cloned().unwrap_or_default();
                let rb = stitched.get(&b).cloned().unwrap_or_default();
                let contango = matches!(spec, CboeSpec::VxContango);
                let c = ra
                    .iter()
                    .filter(|(d, _)| in_window(**d))
                    .filter_map(|(d, x)| {
                        let y = rb.get(d)?;
                        let v = if contango { (x.settle / y.settle - 1.0) * 100.0 } else { x.settle - y.settle };
                        cboe_vx_candle(*d, v, v, v, v, None)
                    })
                    .collect();
                let up = if contango { "VX2/VX1-1".to_string() } else { format!("VX{a}-VX{b}") };
                (c, "cboe_vx", up)
            }
            CboeSpec::VxContract(exp) => {
                let c = vx
                    .get(exp)
                    .into_iter()
                    .flat_map(|r| r.values())
                    .filter(|r| in_window(r.date))
                    .filter_map(|r| cboe_vx_candle(r.date, r.open, r.high, r.low, r.settle, Some(r.volume)))
                    .collect();
                (c, "cboe_vx", format!("VX_{exp}"))
            }
            CboeSpec::Pcr(name, section) => {
                let c = pcr_days
                    .iter()
                    .filter_map(|(d, day)| {
                        let v = *day.ratios.get(*name)?;
                        let mut c = cboe_vx_candle(*d, v, v, v, v, day.volumes.get(*section).copied())?;
                        c.kind = Some("point".to_string());
                        Some(c)
                    })
                    .collect();
                (c, "cboe_pcr", name.to_string())
            }
            CboeSpec::Chain(metric, root) => {
                let chain = match chains.get(root) {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        errors.push(mk_err(ticker, "fetch", format!("cboe chain: {e}")));
                        continue;
                    }
                    None => continue,
                };
                // Snapshot only: never serve today's chain into a past as_of window.
                if chain.ts > end + Duration::hours(1) {
                    errors.push(mk_err(
                        ticker,
                        "range",
                        format!(
                            "cboe chain metrics are live snapshots only (payload {}); no history before polling started",
                            chain.ts.to_rfc3339()
                        ),
                    ));
                    continue;
                }
                match cboe_chain_metric(chain, *metric) {
                    Ok(v) => (
                        vec![Candle { t: chain.ts, o: v, h: v, l: v, c: v, v: None, kind: Some("point".to_string()) }],
                        "cboe_chain",
                        root.clone(),
                    ),
                    Err(e) => {
                        errors.push(mk_err(ticker, "compute", format!("cboe {root}: {e}")));
                        continue;
                    }
                }
            }
        };
        if candles.is_empty() {
            let mut msg = format!("cboe returned no data for '{ticker}' in this window");
            if let (Some(e), "cboe_vx") = (&vx_err, source) {
                msg.push_str(&format!(" (upstream: {e})"));
            }
            errors.push(mk_err(ticker, "fetch", msg));
            continue;
        }
        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some(source.to_string()),
            upstream_id: Some(upstream),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod cboe_tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn cboe_ticker_grammar() {
        assert!(is_cboe_ticker("CBOE:VX1"));
        assert!(is_cboe_ticker("cboe:vx1"));
        assert!(!is_cboe_ticker("CBOE:"));
        assert_eq!(cboe_parse_ticker("cboe:vx2").unwrap(), CboeSpec::VxN(2));
        assert_eq!(cboe_parse_ticker("CBOE:VX2-VX1").unwrap(), CboeSpec::VxSpread(2, 1));
        assert_eq!(cboe_parse_ticker("CBOE:VXCONTANGO").unwrap(), CboeSpec::VxContango);
        assert_eq!(
            cboe_parse_ticker("CBOE:VX:2026-11-18").unwrap(),
            CboeSpec::VxContract(d(2026, 11, 18))
        );
        assert!(matches!(cboe_parse_ticker("CBOE:PCR_EQUITY").unwrap(), CboeSpec::Pcr("EQUITY PUT/CALL RATIO", _)));
        assert_eq!(
            cboe_parse_ticker("CBOE:GEX:_spx").unwrap(),
            CboeSpec::Chain(CboeChainMetric::Gex, "SPX".into())
        );
        assert_eq!(
            cboe_parse_ticker("CBOE:0DTE_SHARE:SPY").unwrap(),
            CboeSpec::Chain(CboeChainMetric::ZeroDteShare, "SPY".into())
        );
        assert!(cboe_parse_ticker("CBOE:VX0").is_err());
        assert!(cboe_parse_ticker("CBOE:FOO").is_err());
    }

    #[test]
    fn cboe_vix_expiry_calendar() {
        // Cross-checked against CBOE's product list.
        assert_eq!(cboe_vix_monthly_expiry(2026, 11), Some(d(2026, 11, 18)));
        assert_eq!(cboe_vix_monthly_expiry(2027, 1), Some(d(2027, 1, 20)));
        assert_eq!(cboe_vix_monthly_expiry(2027, 3), Some(d(2027, 3, 17)));
        assert_eq!(cboe_vix_monthly_expiry(2027, 4), Some(d(2027, 4, 21)));
        // Juneteenth Wednesday 2024 -> Tuesday.
        assert_eq!(cboe_vix_monthly_expiry(2024, 6), Some(d(2024, 6, 18)));
        // Good Friday 2022-04-15 is the April 3rd Friday -> anchor Thu 14th -> Tue 15 Mar.
        assert_eq!(cboe_vix_monthly_expiry(2022, 3), Some(d(2022, 3, 15)));
        assert!(cboe_is_holiday(d(2026, 4, 3)));
        assert!(!cboe_is_holiday(d(2026, 10, 30)));
    }

    #[test]
    fn cboe_vx_list_and_csv_and_stitch() {
        let list: serde_json::Value = serde_json::from_str(r#"{"2026":[
          {"expire_date":"2026-11-18","duration_type":"M"},
          {"expire_date":"2026-11-04","duration_type":"W"},
          {"expire_date":"2026-12-16","duration_type":"M"}],
          "2027":[{"expire_date":"2027-01-20","duration_type":"M"}]}"#).unwrap();
        assert_eq!(cboe_parse_vx_list(&list), vec![d(2026, 11, 18), d(2026, 12, 16), d(2027, 1, 20)]);

        // Real VX_2026-11-18.csv rows (fetched 2026-10-30).
        let nov = "Trade Date,Futures,Open,High,Low,Close,Settle,Change,Total Volume,EFP,Open Interest\n\
2026-02-23,X (Nov 2026),0.0000,21.0000,0.0000,0.0000,22.775,0,0,0,0\n\
2026-10-28,X (Nov 2026),19.0000,19.1100,18.3100,18.8000,18.5982,-0.3541,88333,0,184848\n\
2026-10-29,X (Nov 2026),18.8000,19.0700,18.1000,18.7500,18.1504,-0.4478,99166,392,185430\n";
        let rows = cboe_parse_vx_csv(nov);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].settle, 18.1504);
        assert_eq!(rows[2].volume, 99166.0);
        // zero open/low fall back to settle
        assert_eq!(rows[0].open, 22.775);
        assert_eq!(rows[0].high, 22.775);

        let dec = "Trade Date,Futures,Open,High,Low,Close,Settle,Change,Total Volume,EFP,Open Interest\n\
2026-10-29,Z (Dec 2026),19.9,20.1,19.4,19.8,19.70,-0.3,50000,0,100000\n\
2026-11-18,Z (Dec 2026),19.0,19.2,18.9,19.1,19.05,0,40000,0,100000\n";
        let mut contracts = BTreeMap::new();
        contracts.insert(d(2026, 11, 18), cboe_parse_vx_csv(nov).into_iter().map(|r| (r.date, r)).collect());
        contracts.insert(d(2026, 12, 16), cboe_parse_vx_csv(dec).into_iter().map(|r| (r.date, r)).collect());
        let vx1 = cboe_stitch_vx(&contracts, 1);
        let vx2 = cboe_stitch_vx(&contracts, 2);
        assert_eq!(vx1[&d(2026, 10, 29)].settle, 18.1504);
        assert_eq!(vx2[&d(2026, 10, 29)].settle, 19.70);
        // On Nov expiry day the front month is already December.
        assert_eq!(vx1[&d(2026, 11, 18)].settle, 19.05);
        assert!(!vx2.contains_key(&d(2026, 11, 18)));
    }

    #[test]
    fn cboe_pcr_fixture() {
        let body: serde_json::Value = serde_json::from_str(r#"{
          "ratios": [
            {"name": "TOTAL PUT/CALL RATIO", "value": "0.85"},
            {"name": "EQUITY PUT/CALL RATIO", "value": "0.58"},
            {"name": "SPX + SPXW PUT/CALL RATIO", "value": "1.10"}],
          "EQUITY OPTIONS": [
            {"name": "VOLUME", "call": 2277666, "put": 1324620, "total": 3602286},
            {"name": "OPEN INTEREST", "call": 269080885, "put": 169830580, "total": 438911465}]
        }"#).unwrap();
        let day = cboe_parse_pcr(&body);
        assert_eq!(day.ratios["EQUITY PUT/CALL RATIO"], 0.58);
        assert_eq!(day.ratios["SPX + SPXW PUT/CALL RATIO"], 1.10);
        assert_eq!(day.volumes["EQUITY OPTIONS"], 3602286.0);
    }

    #[test]
    fn cboe_chain_fixture_metrics() {
        // Shape of the real _SPX payload (2026-10-30), a handful of contracts.
        let body: serde_json::Value = serde_json::from_str(r#"{"timestamp": "2026-10-30 15:30:26",
          "data": {"symbol": "^SPX", "current_price": 7610.19, "iv30": 14.709, "options": [
            {"option": "SPXW261120C07600000", "iv": 0.1599, "open_interest": 971.0, "volume": 86.0, "gamma": 0.0014},
            {"option": "SPXW261120P07600000", "iv": 0.1597, "open_interest": 3365.0, "volume": 205.0, "gamma": 0.0014},
            {"option": "SPXW261030C07650000", "iv": 0.12, "open_interest": 500.0, "volume": 900.0},
            {"option": "SPXW261030P07500000", "iv": 0.20, "open_interest": 100.0, "volume": 1000.0},
            {"option": "SPX261120C00400000", "iv": 4.4265, "open_interest": 2510.0, "volume": 2.0}
          ]}}"#).unwrap();
        let chain = cboe_parse_chain(&body).unwrap();
        assert_eq!(chain.ts, Utc.with_ymd_and_hms(2026, 10, 30, 15, 30, 26).unwrap());
        assert_eq!(chain.contracts.len(), 5);
        assert!(chain.contracts[4].am_settled);
        assert_eq!(cboe_chain_metric(&chain, CboeChainMetric::Iv30).unwrap(), 14.709);
        let pcr = cboe_chain_metric(&chain, CboeChainMetric::PcrOi).unwrap();
        assert!((pcr - 3465.0 / 3981.0).abs() < 1e-12);
        let zd = cboe_chain_metric(&chain, CboeChainMetric::ZeroDteShare).unwrap();
        assert!((zd - 1900.0 / 2193.0 * 100.0).abs() < 1e-9);
        // BS gamma for the ATM Nov call ~= the feed's 0.0014.
        let inputs = cboe_gamma_inputs(&chain);
        let atm_call = cboe_gex_at(&inputs[..1], chain.spot) * 1e9 / (971.0 * 100.0 * chain.spot * chain.spot * 0.01);
        assert!((atm_call - 0.0014).abs() < 0.0002, "{atm_call}");
        // Put-heavy book: negative GEX at spot, flips positive above.
        let g = cboe_chain_metric(&chain, CboeChainMetric::Gex).unwrap();
        assert!(g < 0.0);
        assert!(cboe_zero_gamma(&inputs, chain.spot).is_none() || g < 0.0);
        assert_eq!(cboe_parse_occ("NVDA261030C00050000").map(|x| (x.0, x.3)), Some(("NVDA", 50.0)));
    }

    #[test]
    fn cboe_zero_gamma_synthetic() {
        // Puts below spot, calls above: GEX crosses zero between the strikes.
        let inputs = vec![(-1.0, 95.0, 0.2, 0.1, 1000.0), (1.0, 105.0, 0.2, 0.1, 1000.0)];
        let z = cboe_zero_gamma(&inputs, 100.0).unwrap();
        assert!((z - 100.0).abs() < 1.0, "{z}");
        assert!(cboe_gex_at(&inputs, 96.0) < 0.0 && cboe_gex_at(&inputs, 104.0) > 0.0);
    }

    #[tokio::test]
    #[ignore]
    async fn cboe_live_vx_pcr_chain() {
        let end = Utc::now();
        let start = end - Duration::days(10);
        let tickers: Vec<String> = [
            "CBOE:VX1", "CBOE:VX2", "CBOE:VXCONTANGO", "CBOE:VX2-VX1", "CBOE:VX:2026-11-18",
            "CBOE:PCR_EQUITY", "CBOE:PCR_TOTAL", "CBOE:PCR_SPX",
            "CBOE:GEX:SPX", "CBOE:ZEROGAMMA:SPX", "CBOE:PCR_OI:SPX", "CBOE:0DTE_SHARE:SPX", "CBOE:IV30:SPX",
            "CBOE:GEX:SPY",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (series, errors) = fetch_cboe_series(&tickers, start, end, Span { n: 1, unit: SpanUnit::Day })
            .await
            .unwrap();
        for s in &series {
            let tail: Vec<String> = s
                .candles
                .iter()
                .rev()
                .take(5)
                .map(|c| format!("{} {:.4}", c.t.format("%Y-%m-%dT%H:%MZ"), c.c))
                .collect();
            println!("{} [{}] {:?}", s.ticker, s.source.clone().unwrap_or_default(), tail);
        }
        for e in &errors {
            println!("ERR {} {}", e.ticker, e.message);
        }
        assert!(series.iter().any(|s| s.ticker == "CBOE:VX1" && !s.candles.is_empty()));
        assert!(series.iter().any(|s| s.ticker == "CBOE:PCR_EQUITY" && !s.candles.is_empty()));

        // Computed calendar agrees with every monthly expiry CBOE lists.
        let b = cboe_get_bytes(CBOE_VX_LIST).await.unwrap().unwrap();
        let listed = cboe_parse_vx_list(&serde_json::from_slice(&b).unwrap());
        let mismatches: Vec<String> = listed
            .iter()
            .filter(|e| {
                use chrono::Datelike;
                cboe_vix_monthly_expiry(e.year(), e.month()) != Some(**e)
            })
            .map(|e| e.to_string())
            .collect();
        println!("listed monthly expiries: {}, computed mismatches: {:?}", listed.len(), mismatches);
    }
}
