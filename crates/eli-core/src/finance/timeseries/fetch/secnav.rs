/// Fund NAV-per-share provider (SEC EDGAR XBRL) and price-to-NAV.
///
/// Tickers (case-insensitive prefix):
///   NAV:<TICKER>   reported NAV per share (us-gaap NetAssetValuePerShare,
///                  unit USD/shares) as a sparse step series of observations.
///   PNAV:<TICKER>  Yahoo close (or intraday candle) / latest NAV known at
///                  that candle. "Public market vs private marks" as a series.
///
/// Endpoints (no key, SEC requires a descriptive User-Agent with contact):
///   https://www.sec.gov/files/company_tickers.json              ticker -> CIK
///   https://data.sec.gov/api/xbrl/companyconcept/CIK##########/us-gaap/NetAssetValuePerShare.json
///
/// NO LOOK-AHEAD: every NAV value is stamped at 00:00 UTC of its FILED date
/// (the date the 10-Q / 10-K / N-CSR reached EDGAR), never at the period end.
/// Only periodic-report rows are used (10-Q, 10-K, 10-KT, N-CSR, N-CSRS and /A
/// amendments); prospectus/proxy rows (424B2, N-2, DEF 14A) are ignored.
/// Filings repeat prior-period comparatives; repeated (end, val) pairs keep the
/// earliest filing, and a row whose period end is older than one already known
/// is dropped so the step series never jumps back to a stale mark. An amended
/// value for the latest known period end is emitted as a new step.
///
/// Caveat: EDGAR's `filed` is a date with no time. Many BDCs file after the
/// US close, so on the filed day itself the PNAV divisor is the new NAV even
/// though the close printed slightly before it was public (PNAV uses the most
/// recent NAV filed on or before each candle's timestamp, per spec).
///
/// Cadence: quarterly (BDCs) or semiannual (closed-end funds via N-CSR/N-CSRS).
/// History: XBRL back to ~2010-2012 for most filers. Carry-forward between
/// observations is left to the caller (NAV:) - only observations are returned.
/// Rate limit: SEC asks for <=10 req/s; requests here are spaced >=150ms and
/// retried with backoff on 429/5xx.

const SECNAV_TICKERS_URL: &str = "https://www.sec.gov/files/company_tickers.json";
const SECNAV_CONCEPT_BASE: &str = "https://data.sec.gov/api/xbrl/companyconcept";
const SECNAV_MIN_SPACING: StdDuration = StdDuration::from_millis(150);
const SECNAV_MAX_RETRIES: usize = 3;
const SECNAV_FORMS: &[&str] = &[
    "10-Q", "10-Q/A", "10-K", "10-K/A", "10-KT", "10-KT/A", "N-CSR", "N-CSR/A", "N-CSRS",
    "N-CSRS/A",
];

static SECNAV_GATE: std::sync::LazyLock<tokio::sync::Mutex<Option<std::time::Instant>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));
/// In-process cache of the SEC ticker -> CIK map (it is ~1MB; fetch once).
static SECNAV_CIK_MAP: std::sync::LazyLock<tokio::sync::Mutex<Option<HashMap<String, u64>>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecNavKind {
    Nav,
    PriceToNav,
}

/// Split `NAV:OBDC` / `pnav:fsk` into (kind, upper-case symbol).
fn secnav_parse_ticker(ticker: &str) -> Option<(SecNavKind, String)> {
    let t = ticker.trim();
    let (kind, rest) = if t.len() > 5 && t[..5].eq_ignore_ascii_case("PNAV:") {
        (SecNavKind::PriceToNav, &t[5..])
    } else if t.len() > 4 && t[..4].eq_ignore_ascii_case("NAV:") {
        (SecNavKind::Nav, &t[4..])
    } else {
        return None;
    };
    let sym = rest.trim().to_ascii_uppercase();
    if sym.is_empty() {
        None
    } else {
        Some((kind, sym))
    }
}

/// Returns true for `NAV:<TICKER>` and `PNAV:<TICKER>` (SEC XBRL NAV provider).
pub fn is_secnav_ticker(ticker: &str) -> bool {
    secnav_parse_ticker(ticker).is_some()
}

/// One NAV observation as published (step-series point).
#[derive(Clone, Debug, PartialEq)]
struct SecNavObs {
    /// Period end the NAV refers to.
    end: NaiveDate,
    /// Date the filing reached EDGAR (the stamp we use).
    filed: NaiveDate,
    val: f64,
    form: String,
}

/// Parse a companyconcept JSON body into the no-look-ahead step series:
/// sorted by filed date, one point per newly published (period end, value).
fn secnav_parse_concept(body: &str) -> std::result::Result<Vec<SecNavObs>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("sec xbrl json parse failed: {e}"))?;
    let rows = v
        .pointer("/units/USD~1shares")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "sec xbrl concept has no USD/shares unit".to_string())?;

    let mut raw: Vec<SecNavObs> = Vec::new();
    for r in rows {
        let form = r.get("form").and_then(|x| x.as_str()).unwrap_or("");
        if !SECNAV_FORMS.iter().any(|f| f.eq_ignore_ascii_case(form)) {
            continue;
        }
        let date = |k: &str| {
            r.get(k)
                .and_then(|x| x.as_str())
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        };
        let (Some(end), Some(filed), Some(val)) =
            (date("end"), date("filed"), r.get("val").and_then(|x| x.as_f64()))
        else {
            continue;
        };
        if !(val.is_finite() && val > 0.0) {
            continue;
        }
        raw.push(SecNavObs { end, filed, val, form: form.to_string() });
    }
    // Earliest filing first; within a filing date, newest period first so the
    // freshest mark wins that day.
    raw.sort_by(|a, b| a.filed.cmp(&b.filed).then(b.end.cmp(&a.end)));

    let mut out: Vec<SecNavObs> = Vec::new();
    let mut seen: HashSet<(NaiveDate, u64)> = HashSet::new();
    for o in raw {
        if !seen.insert((o.end, o.val.to_bits())) {
            continue; // repeated comparative: keep earliest filing
        }
        match out.last() {
            Some(last) if o.end < last.end => continue, // stale comparative
            Some(last) if o.end == last.end && o.filed == last.filed => continue,
            Some(last) if o.end == last.end && (o.val - last.val).abs() < 1e-12 => continue,
            _ => {}
        }
        out.push(o);
    }
    Ok(out)
}

fn secnav_filed_ts(d: NaiveDate) -> DateTime<Utc> {
    d.and_hms_opt(0, 0, 0).map(|n| n.and_utc()).unwrap_or_else(Utc::now)
}

/// Latest NAV whose filed stamp is <= t (steps sorted by filed).
fn secnav_nav_at(steps: &[SecNavObs], t: DateTime<Utc>) -> Option<&SecNavObs> {
    let idx = steps.partition_point(|o| secnav_filed_ts(o.filed) <= t);
    idx.checked_sub(1).map(|i| &steps[i])
}

/// Divide each price candle by the NAV known at its timestamp. Candles before
/// the first known NAV are dropped.
fn secnav_price_to_nav(prices: &[Candle], steps: &[SecNavObs]) -> Vec<Candle> {
    prices
        .iter()
        .filter_map(|c| {
            let nav = secnav_nav_at(steps, c.t)?.val;
            Some(Candle {
                t: c.t,
                o: c.o / nav,
                h: c.h / nav,
                l: c.l / nav,
                c: c.c / nav,
                v: None,
                kind: None,
            })
        })
        .collect()
}

fn secnav_user_agent(override_ua: Option<&str>) -> std::result::Result<String, String> {
    if let Some(s) = override_ua.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(s.to_string());
    }
    let ua = std::env::var("ELI_SEC_USER_AGENT").unwrap_or_default();
    let ua = ua.trim();
    if ua.is_empty() {
        return Err("SEC EDGAR requires a User-Agent with contact email (no signup, no key). Set `ELI_SEC_USER_AGENT=\"Jane Doe jane@example.com\"` or run `market-search config --set sec_user_agent --value \"Jane Doe jane@example.com\"`.".to_string());
    }
    Ok(ua.to_string())
}

/// GET with SEC spacing + backoff on 429/5xx. Ok(None) on 404.
async fn secnav_get(
    client: &reqwest::Client,
    ua: &str,
    url: &str,
) -> std::result::Result<Option<String>, String> {
    let mut last = String::new();
    for attempt in 0..=SECNAV_MAX_RETRIES {
        {
            let mut gate = SECNAV_GATE.lock().await;
            if let Some(prev) = *gate {
                let el = prev.elapsed();
                if el < SECNAV_MIN_SPACING {
                    tokio::time::sleep(SECNAV_MIN_SPACING - el).await;
                }
            }
            *gate = Some(std::time::Instant::now());
        }
        let resp = match client
            .get(url)
            .header(reqwest::header::USER_AGENT, ua)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(StdDuration::from_secs(25))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = format!("request failed: {e}");
                tokio::time::sleep(StdDuration::from_millis(500 * (attempt as u64 + 1))).await;
                continue;
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return resp.text().await.map(Some).map_err(|e| format!("read failed: {e}"));
        }
        last = format!("http {status}");
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(StdDuration::from_secs)
                .unwrap_or_else(|| StdDuration::from_millis(1000 * (attempt as u64 + 1)));
            tokio::time::sleep(wait).await;
            continue;
        }
        break;
    }
    Err(format!("sec fetch failed: {last} ({url})"))
}

async fn secnav_lookup_cik(
    client: &reqwest::Client,
    ua: &str,
    symbol: &str,
) -> std::result::Result<u64, String> {
    let mut guard = SECNAV_CIK_MAP.lock().await;
    if guard.is_none() {
        let body = secnav_get(client, ua, SECNAV_TICKERS_URL)
            .await?
            .ok_or_else(|| "sec company_tickers.json not found".to_string())?;
        *guard = Some(secnav_parse_cik_map(&body)?);
    }
    guard
        .as_ref()
        .and_then(|m| m.get(&symbol.to_ascii_uppercase()).copied())
        .ok_or_else(|| format!("unknown ticker '{symbol}' in SEC company_tickers.json"))
}

fn secnav_parse_cik_map(body: &str) -> std::result::Result<HashMap<String, u64>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("sec ticker map parse failed: {e}"))?;
    let obj = v.as_object().ok_or_else(|| "sec ticker map is not an object".to_string())?;
    let mut m = HashMap::new();
    for e in obj.values() {
        if let (Some(t), Some(cik)) = (
            e.get("ticker").and_then(|x| x.as_str()),
            e.get("cik_str").and_then(|x| x.as_u64()),
        ) {
            // First listing wins (company_tickers.json is ordered by size).
            m.entry(t.trim().to_ascii_uppercase()).or_insert(cik);
        }
    }
    Ok(m)
}

/// Full NAV step history for a symbol (all available XBRL periods).
async fn secnav_nav_history(
    client: &reqwest::Client,
    ua: &str,
    symbol: &str,
) -> std::result::Result<(u64, Vec<SecNavObs>), String> {
    let cik = secnav_lookup_cik(client, ua, symbol).await?;
    let url = format!("{SECNAV_CONCEPT_BASE}/CIK{cik:010}/us-gaap/NetAssetValuePerShare.json");
    let body = secnav_get(client, ua, &url).await?.ok_or_else(|| {
        format!(
            "'{symbol}' (CIK {cik}) has no us-gaap NetAssetValuePerShare XBRL facts; NAV:/PNAV: cover BDCs and closed-end funds that tag NAV per share"
        )
    })?;
    let steps = secnav_parse_concept(&body)?;
    if steps.is_empty() {
        return Err(format!(
            "'{symbol}' (CIK {cik}) has NetAssetValuePerShare facts but none from 10-Q/10-K/N-CSR filings"
        ));
    }
    Ok((cik, steps))
}

/// Fetch NAV:/PNAV: series. Reads the SEC User-Agent from ELI_SEC_USER_AGENT.
/// Same signature/return conventions as `fetch_govt_series`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn fetch_secnav_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    fetch_secnav_series_with_ua(tickers, start, end, granularity, None).await
}

/// As `fetch_secnav_series`, with an explicit SEC User-Agent (e.g. from the
/// `sec_user_agent` config key). `None` falls back to ELI_SEC_USER_AGENT.
pub(crate) async fn fetch_secnav_series_with_ua(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
    sec_user_agent: Option<&str>,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let err = |ticker: &str, stage: &str, message: String| TimeseriesError {
        ticker: ticker.to_string(),
        stage: Some(stage.to_string()),
        message,
    };

    let ua = match secnav_user_agent(sec_user_agent) {
        Ok(ua) => ua,
        Err(m) => {
            for t in tickers {
                errors.push(err(t, "input", m.clone()));
            }
            return Ok((out, errors));
        }
    };

    for ticker in tickers {
        let Some((kind, symbol)) = secnav_parse_ticker(ticker) else {
            errors.push(err(ticker, "input", "expected NAV:<TICKER> or PNAV:<TICKER>".into()));
            continue;
        };
        let (cik, steps) = match secnav_nav_history(client, &ua, &symbol).await {
            Ok(x) => x,
            Err(m) => {
                errors.push(err(ticker, "fetch", format!("sec nav: {m}")));
                continue;
            }
        };

        let candles: Vec<Candle> = match kind {
            SecNavKind::Nav => steps
                .iter()
                .map(|o| (secnav_filed_ts(o.filed), o.val))
                .filter(|(t, _)| *t >= start && *t <= end)
                .map(|(t, v)| Candle { t, o: v, h: v, l: v, c: v, v: None, kind: Some("point".into()) })
                .collect(),
            SecNavKind::PriceToNav => {
                let (prices, perrs) =
                    match fetch_yahoo_series(&[symbol.clone()], start, end, granularity, None).await {
                        Ok(x) => x,
                        Err(e) => {
                            errors.push(err(ticker, "fetch", format!("pnav price (yahoo) failed: {e}")));
                            continue;
                        }
                    };
                if let Some(pe) = perrs.into_iter().next() {
                    errors.push(err(ticker, "fetch", format!("pnav price (yahoo): {}", pe.message)));
                    continue;
                }
                let Some(ps) = prices.into_iter().next() else {
                    errors.push(err(ticker, "fetch", "pnav price (yahoo) returned no series".into()));
                    continue;
                };
                secnav_price_to_nav(&ps.candles, &steps)
                    .into_iter()
                    .filter(|c| c.t >= start && c.t <= end)
                    .collect()
            }
        };

        if candles.is_empty() {
            let latest = steps.last().map(|o| format!("{} (filed {})", o.end, o.filed)).unwrap_or_default();
            errors.push(err(
                ticker,
                "fetch",
                match kind {
                    SecNavKind::Nav => format!(
                        "no NAV filed for '{symbol}' inside the window (NAV is quarterly; latest period {latest}); widen the range"
                    ),
                    SecNavKind::PriceToNav => format!(
                        "no price candles for '{symbol}' with a known NAV in this window (latest NAV period {latest})"
                    ),
                },
            ));
            continue;
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("sec_xbrl".to_string()),
            upstream_id: Some(format!("CIK{cik:010}:us-gaap:NetAssetValuePerShare")),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod secnav_tests {
    use super::*;

    // Trimmed real rows from OBDC (CIK 1655888) companyconcept, 2026-10-30,
    // plus one 424B2 row (ARCC style) that must be ignored and a 10-K that
    // re-reports a prior quarter as a comparative.
    const OBDC_FIXTURE: &str = r#"{"cik":1655888,"taxonomy":"us-gaap","tag":"NetAssetValuePerShare",
      "units":{"USD/shares":[
        {"end":"2025-06-30","val":15.03,"accn":"a","fy":2025,"fp":"Q2","form":"10-Q","filed":"2025-08-06"},
        {"end":"2025-09-30","val":14.89,"accn":"0001655888-25-000024","fy":2025,"fp":"Q3","form":"10-Q","filed":"2025-11-05","frame":"CY2025Q3I"},
        {"end":"2025-09-30","val":14.89,"accn":"0001655888-26-000010","fy":2025,"fp":"FY","form":"10-K","filed":"2026-02-18"},
        {"end":"2025-12-31","val":14.81,"accn":"0001655888-26-000010","fy":2025,"fp":"FY","form":"10-K","filed":"2026-02-18"},
        {"end":"2025-12-31","val":14.81,"accn":"0001655888-26-000033","fy":2026,"fp":"Q1","form":"10-Q","filed":"2026-05-06"},
        {"end":"2025-12-31","val":14.81,"accn":"0001655888-26-000056","fy":2026,"fp":"Q2","form":"10-Q","filed":"2026-08-05","frame":"CY2025Q4I"},
        {"end":"2026-03-31","val":14.41,"accn":"0001655888-26-000033","fy":2026,"fp":"Q1","form":"10-Q","filed":"2026-05-06","frame":"CY2026Q1I"},
        {"end":"2026-04-27","val":14.40,"accn":"x","fy":null,"fp":null,"form":"424B2","filed":"2026-04-29"},
        {"end":"2026-06-30","val":14.26,"accn":"0001655888-26-000056","fy":2026,"fp":"Q2","form":"10-Q","filed":"2026-08-05","frame":"CY2026Q2I"}
      ]}}"#;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn secnav_ticker_grammar() {
        assert_eq!(secnav_parse_ticker("NAV:OBDC"), Some((SecNavKind::Nav, "OBDC".into())));
        assert_eq!(secnav_parse_ticker("pnav:fsk"), Some((SecNavKind::PriceToNav, "FSK".into())));
        assert!(is_secnav_ticker("Nav:ARCC"));
        assert!(!is_secnav_ticker("NAV:"));
        assert!(!is_secnav_ticker("PNAV:"));
        assert!(!is_secnav_ticker("NAVI"));
        assert!(!is_secnav_ticker("SPY"));
    }

    #[test]
    fn secnav_parse_stamps_at_filed_and_dedupes() {
        let steps = secnav_parse_concept(OBDC_FIXTURE).unwrap();
        let got: Vec<(NaiveDate, NaiveDate, f64)> =
            steps.iter().map(|o| (o.filed, o.end, o.val)).collect();
        assert_eq!(
            got,
            vec![
                (d("2025-08-06"), d("2025-06-30"), 15.03),
                (d("2025-11-05"), d("2025-09-30"), 14.89),
                (d("2026-02-18"), d("2025-12-31"), 14.81),
                (d("2026-05-06"), d("2026-03-31"), 14.41),
                (d("2026-08-05"), d("2026-06-30"), 14.26),
            ]
        );
        assert!(steps.iter().all(|o| o.form != "424B2"));
    }

    #[test]
    fn secnav_stale_comparative_never_steps_back() {
        // A later 10-K/A restates an OLD period: must not replace the newer mark.
        let body = r#"{"units":{"USD/shares":[
          {"end":"2026-03-31","val":10.0,"form":"10-Q","filed":"2026-05-01"},
          {"end":"2025-12-31","val":9.5,"form":"10-K/A","filed":"2026-06-01"},
          {"end":"2026-03-31","val":10.1,"form":"10-Q/A","filed":"2026-06-15"}
        ]}}"#;
        let steps = secnav_parse_concept(body).unwrap();
        let vals: Vec<f64> = steps.iter().map(|o| o.val).collect();
        assert_eq!(vals, vec![10.0, 10.1]);
    }

    #[test]
    fn secnav_price_to_nav_uses_latest_filed_nav() {
        let steps = secnav_parse_concept(OBDC_FIXTURE).unwrap();
        let c = |day: &str, px: f64| Candle {
            t: secnav_filed_ts(d(day)),
            o: px,
            h: px,
            l: px,
            c: px,
            v: None,
            kind: None,
        };
        let prices = vec![
            c("2025-08-05", 14.0), // before first NAV -> dropped
            c("2026-08-04", 10.0), // uses 14.41 (Q2 not yet filed)
            c("2026-08-05", 10.0), // filed that day -> 14.26
            c("2026-10-29", 10.4),
        ];
        let out = secnav_price_to_nav(&prices, &steps);
        assert_eq!(out.len(), 3);
        assert!((out[0].c - 10.0 / 14.41).abs() < 1e-12);
        assert!((out[1].c - 10.0 / 14.26).abs() < 1e-12);
        assert!((out[2].c - 10.4 / 14.26).abs() < 1e-12);
    }

    #[test]
    fn secnav_cik_map_parse() {
        let body = r#"{"0":{"cik_str":1287750,"ticker":"ARCC","title":"ARES CAPITAL CORP"},
                       "1":{"cik_str":1655888,"ticker":"OBDC","title":"Blue Owl Capital Corp"}}"#;
        let m = secnav_parse_cik_map(body).unwrap();
        assert_eq!(m.get("OBDC"), Some(&1655888));
        assert_eq!(m.get("ARCC"), Some(&1287750));
    }

    /// Live: `cargo test -p eli-core secnav_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn secnav_live() {
        if std::env::var("ELI_SEC_USER_AGENT").map(|s| s.trim().is_empty()).unwrap_or(true) {
            eprintln!("skipped: set ELI_SEC_USER_AGENT=\"Jane Doe jane@example.com\" to run");
            return;
        }
        let end = Utc::now();
        let start = end - Duration::days(400);
        let day = Span { n: 1, unit: SpanUnit::Day };
        let mut tickers: Vec<String> = Vec::new();
        for s in ["OBDC", "FSK", "GSBD", "BXSL", "ARCC"] {
            tickers.push(format!("NAV:{s}"));
            tickers.push(format!("PNAV:{s}"));
        }
        let (series, errors) = fetch_secnav_series(&tickers, start, end, day).await.unwrap();
        for e in &errors {
            println!("ERR {} {}", e.ticker, e.message);
        }
        assert!(errors.is_empty());
        for s in &series {
            if s.ticker.starts_with("NAV:") {
                let pts: Vec<String> = s
                    .candles
                    .iter()
                    .map(|c| format!("{} {:.2}", c.t.date_naive(), c.c))
                    .collect();
                println!("{} [{}] {}", s.ticker, s.upstream_id.clone().unwrap_or_default(), pts.join(", "));
            } else {
                let last = s.candles.last().unwrap();
                println!("{} n={} latest {} {:.3}", s.ticker, s.candles.len(), last.t.date_naive(), last.c);
                assert!(last.c > 0.2 && last.c < 2.0);
            }
        }
        // Intraday PNAV path.
        let h = Span { n: 1, unit: SpanUnit::Hour };
        let (s2, e2) =
            fetch_secnav_series(&["PNAV:OBDC".to_string()], end - Duration::days(5), end, h).await.unwrap();
        assert!(e2.is_empty(), "{e2:?}");
        let last = s2[0].candles.last().unwrap();
        println!("PNAV:OBDC 1h n={} latest {} {:.3}", s2[0].candles.len(), last.t, last.c);
    }
}
