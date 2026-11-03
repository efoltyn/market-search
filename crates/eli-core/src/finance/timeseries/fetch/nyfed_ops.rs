/// NY Fed Markets API as timeseries: repo / reverse-repo operations and reference rates.
///
/// Endpoints (no key):
///   ops:   https://markets.newyorkfed.org/api/rp/results/search.json?startDate=YYYY-MM-DD&endDate=YYYY-MM-DD
///          (returns BOTH repo and reverse-repo operations; history back to 2010-08 in one call)
///   rates: https://markets.newyorkfed.org/api/rates/{secured|unsecured}/{sofr|bgcr|tgcr|effr|obfr|sofrai}/search.json?startDate=..&endDate=..
///          (SOFR from 2018-04, EFFR from 2000-07)
/// The non-timeseries `finance_nyfed` tool (finance/nyfed) only hits the /latest and
/// /last/N endpoints; these date-range search endpoints are what make history possible.
///
/// Ticker grammar: `NYFED:<SERIES>` (case-insensitive)
///   Operations ($bn unless noted; stamped at the operation close time, US/Eastern -> UTC,
///   which is when results are released):
///     `SRF`            Standing Repo Facility (all "Repo" operations) total accepted, $bn
///     `SRF_SUBMITTED`  repo propositions submitted, $bn
///     `RRP`            overnight reverse repo (ON RRP) total accepted, $bn
///     `RRP_SUBMITTED`  reverse repo submitted, $bn
///     `RRP_CPTY`       accepted counterparties, count
///     `RRP_RATE`       ON RRP award rate, percent
///   Reference rates (percent unless noted):
///     `SOFR`, `BGCR`, `TGCR`, `EFFR`, `OBFR`                 the published rate
///     `<RATE>_P1`, `_P25`, `_P75`, `_P99`                     volume-weighted percentiles
///     `<RATE>_VOL`                                           underlying volume, $bn
///     `EFFR_TARGET_LO`, `EFFR_TARGET_HI`                     FOMC target range bounds
///     `SOFR_30D`, `SOFR_90D`, `SOFR_180D`, `SOFR_INDEX`      SOFR averages / index (SOFRAI)
///   Examples: NYFED:SRF, NYFED:RRP, NYFED:SOFR_P99, NYFED:EFFR, NYFED:TGCR_VOL
///
/// Timestamps / no look-ahead:
///   * Operations: stamped at closeTime (ET) on operationDate. The SRF runs twice a day
///     (08:15-08:30 and 13:30-13:45 ET), so hourly requests see two prints per day; daily
///     and coarser buckets SUM the amounts (SRF/RRP/_SUBMITTED) and keep the last value
///     for RRP_CPTY / RRP_RATE.
///   * Rates for effective date D are published the NEXT business day (SOFR/BGCR/TGCR at
///     ~08:00 ET, EFFR/OBFR at ~09:00 ET). They are stamped at that publication time, so
///     the daily candle dated D+1 carries D's rate (FRED stamps the same value at D).
///     "Next business day" = next weekday; Fed holidays are not modeled, so after a
///     holiday the stamp is one day early.
///   * SOFR averages/index for effective date D are published on D at ~08:00 ET.
///   Daily and coarser rate candles are resampled with last-value-wins.

const NYFED_API_BASE: &str = "https://markets.newyorkfed.org/api";

/// Returns true if the ticker routes to the NY Fed timeseries provider.
pub fn is_nyfed_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    t.len() > 6 && t[..6].eq_ignore_ascii_case("NYFED:")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NyfOpsField {
    Accepted,
    Submitted,
    Counterparties,
    AwardRate,
}

#[derive(Clone, Debug, PartialEq)]
enum NyfSeries {
    /// (is_repo [SRF] vs reverse repo, field)
    Ops(bool, NyfOpsField),
    /// (endpoint group, endpoint rate name, json field, publication hour ET, next business day?)
    Rate(&'static str, &'static str, &'static str, u32, bool),
}

impl NyfSeries {
    /// Upstream endpoint path for this series (tickers sharing a path share one request).
    fn path(&self) -> String {
        match self {
            NyfSeries::Ops(..) => "rp/results/search.json".to_string(),
            NyfSeries::Rate(group, rate, ..) => format!("rates/{group}/{rate}/search.json"),
        }
    }
    fn is_flow(&self) -> bool {
        matches!(self, NyfSeries::Ops(_, NyfOpsField::Accepted | NyfOpsField::Submitted))
    }
}

fn nyf_parse_ticker(ticker: &str) -> std::result::Result<NyfSeries, String> {
    let t = ticker.trim();
    if !is_nyfed_ticker(t) {
        return Err(format!("not a NYFED: ticker: '{ticker}'"));
    }
    let s = t[6..].trim().to_ascii_uppercase().replace('-', "_");
    let ops = match s.as_str() {
        "SRF" | "REPO" | "SRF_ACCEPTED" => Some(NyfSeries::Ops(true, NyfOpsField::Accepted)),
        "SRF_SUBMITTED" | "REPO_SUBMITTED" => Some(NyfSeries::Ops(true, NyfOpsField::Submitted)),
        "RRP" | "ONRRP" | "RRP_ACCEPTED" => Some(NyfSeries::Ops(false, NyfOpsField::Accepted)),
        "RRP_SUBMITTED" => Some(NyfSeries::Ops(false, NyfOpsField::Submitted)),
        "RRP_CPTY" | "RRP_COUNTERPARTIES" => Some(NyfSeries::Ops(false, NyfOpsField::Counterparties)),
        "RRP_RATE" => Some(NyfSeries::Ops(false, NyfOpsField::AwardRate)),
        "SOFR_30D" => Some(NyfSeries::Rate("secured", "sofrai", "average30day", 8, false)),
        "SOFR_90D" => Some(NyfSeries::Rate("secured", "sofrai", "average90day", 8, false)),
        "SOFR_180D" => Some(NyfSeries::Rate("secured", "sofrai", "average180day", 8, false)),
        "SOFR_INDEX" | "SOFRINDEX" => Some(NyfSeries::Rate("secured", "sofrai", "index", 8, false)),
        _ => None,
    };
    if let Some(o) = ops {
        return Ok(o);
    }
    let (rate, suffix) = match s.split_once('_') {
        Some((r, f)) => (r, Some(f)),
        None => (s.as_str(), None),
    };
    let (group, name, hour) = match rate {
        "SOFR" => ("secured", "sofr", 8),
        "BGCR" => ("secured", "bgcr", 8),
        "TGCR" => ("secured", "tgcr", 8),
        "EFFR" => ("unsecured", "effr", 9),
        "OBFR" => ("unsecured", "obfr", 9),
        _ => {
            return Err(format!(
                "unknown NYFED series '{s}' (SRF, SRF_SUBMITTED, RRP, RRP_SUBMITTED, RRP_CPTY, RRP_RATE, SOFR, BGCR, TGCR, EFFR, OBFR, <RATE>_P1/_P25/_P75/_P99/_VOL, EFFR_TARGET_LO/HI, SOFR_30D/90D/180D/INDEX)"
            ))
        }
    };
    let field = match suffix {
        None => "percentRate",
        Some("P1") => "percentPercentile1",
        Some("P25") => "percentPercentile25",
        Some("P75") => "percentPercentile75",
        Some("P99") => "percentPercentile99",
        Some("VOL") | Some("VOLUME") => "volumeInBillions",
        Some("TARGET_LO") | Some("TARGET_FROM") if name == "effr" => "targetRateFrom",
        Some("TARGET_HI") | Some("TARGET_TO") if name == "effr" => "targetRateTo",
        Some(other) => {
            return Err(format!(
                "unknown NYFED field '{other}' for {rate} (use _P1, _P25, _P75, _P99, _VOL; EFFR also _TARGET_LO/_TARGET_HI)"
            ))
        }
    };
    Ok(NyfSeries::Rate(group, name, field, hour, true))
}

fn nyf_num(v: Option<&serde_json::Value>) -> Option<f64> {
    match v? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Local US/Eastern wall time -> UTC (DST via bb_eastern_offset_hours in ust_buyback.rs).
fn nyf_et_to_utc(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    let local = date.and_hms_opt(hour, minute, 0)?;
    Some((local + Duration::hours(bb_eastern_offset_hours(local))).and_utc())
}

fn nyf_next_weekday(d: NaiveDate) -> NaiveDate {
    use chrono::Datelike;
    let mut n = d + Duration::days(1);
    while matches!(n.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
        n += Duration::days(1);
    }
    n
}

/// Parse /api/rp/results/search.json into (stamp, value) points for one ops series.
fn nyf_parse_ops(body: &serde_json::Value, repo: bool, field: NyfOpsField) -> Vec<(DateTime<Utc>, f64)> {
    let want = if repo { "repo" } else { "reverse repo" };
    let ops = body.pointer("/repo/operations").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for op in &ops {
        let ty = op.get("operationType").and_then(|v| v.as_str()).unwrap_or("");
        if !ty.eq_ignore_ascii_case(want) {
            continue;
        }
        if op.get("auctionStatus").and_then(|v| v.as_str()).is_some_and(|s| !s.eq_ignore_ascii_case("Results")) {
            continue;
        }
        let Some(date) = op
            .get("operationDate")
            .and_then(|v| v.as_str())
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        else {
            continue;
        };
        let (h, m) = op
            .get("closeTime")
            .and_then(|v| v.as_str())
            .and_then(|s| s.split_once(':'))
            .and_then(|(h, m)| Some((h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?)))
            .unwrap_or((0, 0));
        let Some(t) = (if (h, m) == (0, 0) {
            date.and_hms_opt(0, 0, 0).map(|n| n.and_utc())
        } else {
            nyf_et_to_utc(date, h, m)
        }) else {
            continue;
        };
        let v = match field {
            NyfOpsField::Accepted => nyf_num(op.get("totalAmtAccepted")).map(|v| v / 1e9),
            NyfOpsField::Submitted => nyf_num(op.get("totalAmtSubmitted")).map(|v| v / 1e9),
            NyfOpsField::Counterparties => nyf_num(op.get("acceptedCpty")),
            NyfOpsField::AwardRate => op
                .get("details")
                .and_then(|d| d.as_array())
                .and_then(|d| d.iter().find_map(|x| nyf_num(x.get("percentAwardRate")))),
        };
        if let Some(v) = v {
            out.push((t, v));
        }
    }
    out.sort_by_key(|p| p.0);
    out
}

/// Parse /api/rates/.../search.json (refRates) into publication-stamped points.
fn nyf_parse_rates(
    body: &serde_json::Value,
    field: &str,
    pub_hour_et: u32,
    next_day: bool,
) -> Vec<(DateTime<Utc>, f64)> {
    let rows = body.get("refRates").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in &rows {
        let Some(date) = r
            .get("effectiveDate")
            .and_then(|v| v.as_str())
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        else {
            continue;
        };
        let Some(v) = nyf_num(r.get(field)) else { continue };
        let pub_date = if next_day { nyf_next_weekday(date) } else { date };
        if let Some(t) = nyf_et_to_utc(pub_date, pub_hour_et, 0) {
            out.push((t, v));
        }
    }
    out.sort_by_key(|p| p.0);
    out
}

/// Bucket points into candles. Flows sum per bucket; levels keep OHLC (last = close).
fn nyf_bucket(points: &[(DateTime<Utc>, f64)], step_secs: i64, flow: bool) -> Vec<Candle> {
    let mut out: Vec<Candle> = Vec::new();
    for &(t, v) in points {
        let key = t.timestamp().div_euclid(step_secs) * step_secs;
        let Some(bt) = DateTime::<Utc>::from_timestamp(key, 0) else { continue };
        match out.last_mut() {
            Some(c) if c.t == bt => {
                if flow {
                    let s = c.c + v;
                    c.o = s;
                    c.h = s;
                    c.l = s;
                    c.c = s;
                } else {
                    c.h = c.h.max(v);
                    c.l = c.l.min(v);
                    c.c = v;
                }
            }
            _ => out.push(Candle { t: bt, o: v, h: v, l: v, c: v, v: None, kind: None }),
        }
    }
    for c in &mut out {
        for x in [&mut c.o, &mut c.h, &mut c.l, &mut c.c] {
            *x = (*x * 1e6).round() / 1e6;
        }
    }
    out
}

/// Fetch NY Fed series for `NYFED:<SERIES>` tickers. Tickers that share an upstream
/// endpoint (e.g. SOFR and SOFR_P99, or SRF and RRP) share one request.
pub(crate) async fn fetch_nyfed_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let step_secs = granularity.approx_duration().num_seconds().max(1);
    let mut out = Vec::new();
    let mut errors = Vec::new();

    let mut parsed: Vec<(String, NyfSeries)> = Vec::new();
    for ticker in tickers {
        match nyf_parse_ticker(ticker) {
            Ok(s) => parsed.push((ticker.clone(), s)),
            Err(message) => errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message,
            }),
        }
    }

    // Publication stamps trail effective dates by up to ~4 days (Fri -> Mon, holidays).
    let q_start = (start - Duration::days(5)).date_naive().format("%Y-%m-%d").to_string();
    let q_end = (end + Duration::days(1)).date_naive().format("%Y-%m-%d").to_string();

    let mut bodies: HashMap<String, std::result::Result<serde_json::Value, String>> = HashMap::new();
    for (_, series) in &parsed {
        let path = series.path();
        if bodies.contains_key(&path) {
            continue;
        }
        let url = format!("{NYFED_API_BASE}/{path}?startDate={q_start}&endDate={q_end}");
        let res = match plumb_http_get(&url).await {
            Ok((200, body)) => serde_json::from_str::<serde_json::Value>(&body)
                .map_err(|e| format!("nyfed json parse failed: {e}")),
            Ok((status, body)) => Err(format!(
                "nyfed returned http {status} for {path}: {}",
                body.chars().take(160).collect::<String>()
            )),
            Err(e) => Err(format!("nyfed request failed: {e}")),
        };
        bodies.insert(path, res);
    }

    for (ticker, series) in parsed {
        let path = series.path();
        let body = match bodies.get(&path) {
            Some(Ok(b)) => b,
            Some(Err(e)) => {
                errors.push(TimeseriesError {
                    ticker,
                    stage: Some("fetch".to_string()),
                    message: e.clone(),
                });
                continue;
            }
            None => continue,
        };
        let points = match &series {
            NyfSeries::Ops(repo, field) => nyf_parse_ops(body, *repo, *field),
            NyfSeries::Rate(_, _, field, hour, next_day) => nyf_parse_rates(body, field, *hour, *next_day),
        };
        let points: Vec<_> = points.into_iter().filter(|(t, _)| *t >= start && *t <= end).collect();
        let candles = nyf_bucket(&points, step_secs, series.is_flow());
        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("insufficient_data".to_string()),
                message: format!(
                    "NY Fed returned no observations for '{ticker}' in this window (rates publish next business day ~08:00-09:00 ET; ops only on business days; SOFR history starts 2018-04)"
                ),
            });
            continue;
        }
        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("nyfed".to_string()),
            upstream_id: Some(path),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod nyfed_ops_tests {
    use super::*;

    const OPS_FIXTURE: &str = r#"{ "repo": { "operations": [
      { "operationId": "RP 103026 25", "auctionStatus": "Results", "operationDate": "2026-10-30", "operationType": "Repo", "term": "Overnight", "closeTime": "08:30", "totalAmtSubmitted": 0 ,"totalAmtAccepted": 0, "details": [] },
      { "operationId": "RP 102926 25", "auctionStatus": "Results", "operationDate": "2026-10-29", "operationType": "Repo", "term": "Overnight", "closeTime": "08:30", "totalAmtSubmitted": 2000000000 ,"totalAmtAccepted": 1500000000, "details": [] },
      { "operationId": "RP 102926 26", "auctionStatus": "Results", "operationDate": "2026-10-29", "operationType": "Reverse Repo", "term": "Overnight", "closeTime": "13:15", "participatingCpty": 4 ,"acceptedCpty": 4 ,"totalAmtSubmitted": 353000000 ,"totalAmtAccepted": 353000000 , "details": [ { "securityType": "Treasury", "amtSubmitted": 353000000, "amtAccepted": 353000000 ,"percentOfferingRate": 3.75 ,"percentAwardRate": 3.75 } ] },
      { "operationId": "RP 102926 27", "auctionStatus": "Results", "operationDate": "2026-10-29", "operationType": "Repo", "term": "Overnight", "closeTime": "13:45", "totalAmtSubmitted": 31000000 ,"totalAmtAccepted": 31000000 , "details": [] }
    ] } }"#;

    const SOFR_FIXTURE: &str = r#"{ "refRates": [
      { "effectiveDate": "2026-10-30", "type": "SOFR" ,"percentRate": 3.93 ,"percentPercentile99": 4.01 ,"volumeInBillions": 3100 },
      { "effectiveDate": "2026-10-29", "type": "SOFR" ,"percentRate": 3.94 ,"percentPercentile1": 3.89 ,"percentPercentile25": 3.92 ,"percentPercentile75": 3.99 ,"percentPercentile99": 4.02 ,"volumeInBillions": 3145 ,"revisionIndicator": "" },
      { "effectiveDate": "2026-10-28", "type": "SOFR" ,"percentRate": 3.94 ,"percentPercentile1": 3.89 ,"percentPercentile25": 3.92 ,"percentPercentile75": 3.99 ,"percentPercentile99": 4.02 ,"volumeInBillions": 3074 ,"revisionIndicator": "" }
    ] }"#;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn nyfed_ticker_grammar() {
        assert!(is_nyfed_ticker("NYFED:SRF"));
        assert!(is_nyfed_ticker("nyfed:sofr_p99"));
        assert!(!is_nyfed_ticker("NYFED:"));
        assert_eq!(nyf_parse_ticker("NYFED:SRF").unwrap(), NyfSeries::Ops(true, NyfOpsField::Accepted));
        assert_eq!(nyf_parse_ticker("nyfed:rrp_cpty").unwrap(), NyfSeries::Ops(false, NyfOpsField::Counterparties));
        assert_eq!(
            nyf_parse_ticker("NYFED:SOFR_P99").unwrap(),
            NyfSeries::Rate("secured", "sofr", "percentPercentile99", 8, true)
        );
        assert_eq!(
            nyf_parse_ticker("NYFED:EFFR").unwrap(),
            NyfSeries::Rate("unsecured", "effr", "percentRate", 9, true)
        );
        assert_eq!(
            nyf_parse_ticker("NYFED:EFFR_TARGET_HI").unwrap(),
            NyfSeries::Rate("unsecured", "effr", "targetRateTo", 9, true)
        );
        assert_eq!(nyf_parse_ticker("NYFED:SOFR_30D").unwrap().path(), "rates/secured/sofrai/search.json");
        assert!(nyf_parse_ticker("NYFED:SOFR_TARGET_HI").is_err());
        assert!(nyf_parse_ticker("NYFED:LIBOR").is_err());
    }

    #[test]
    fn nyfed_ops_parse_and_bucket() {
        let body: serde_json::Value = serde_json::from_str(OPS_FIXTURE).unwrap();
        let srf = nyf_parse_ops(&body, true, NyfOpsField::Accepted);
        assert_eq!(srf.len(), 3);
        // 08:30 EDT -> 12:30 UTC
        assert_eq!(srf[0], (ts("2026-10-29T12:30:00Z"), 1.5));
        assert_eq!(srf[1], (ts("2026-10-29T17:45:00Z"), 0.031));
        // Daily: two SRF ops on Oct 29 sum.
        let daily = nyf_bucket(&srf, 86400, true);
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].t, ts("2026-10-29T00:00:00Z"));
        assert!((daily[0].c - 1.531).abs() < 1e-9);
        assert_eq!(daily[1].c, 0.0);
        // Hourly keeps them apart.
        assert_eq!(nyf_bucket(&srf, 3600, true).len(), 3);
        let rrp = nyf_parse_ops(&body, false, NyfOpsField::Accepted);
        assert_eq!(rrp, vec![(ts("2026-10-29T17:15:00Z"), 0.353)]);
        assert_eq!(nyf_parse_ops(&body, false, NyfOpsField::AwardRate)[0].1, 3.75);
        assert_eq!(nyf_parse_ops(&body, false, NyfOpsField::Counterparties)[0].1, 4.0);
        assert_eq!(nyf_parse_ops(&body, true, NyfOpsField::Submitted)[0].1, 2.0);
    }

    #[test]
    fn nyfed_rates_publication_stamp() {
        let body: serde_json::Value = serde_json::from_str(SOFR_FIXTURE).unwrap();
        let pts = nyf_parse_rates(&body, "percentRate", 8, true);
        // Wed Oct 28 rate -> published Thu Oct 29 08:00 EDT = 12:00 UTC
        assert_eq!(pts[0], (ts("2026-10-29T12:00:00Z"), 3.94));
        // Fri Oct 30 rate -> published Mon Nov 2 08:00 EST = 13:00 UTC (DST ended Nov 1)
        assert_eq!(pts[2], (ts("2026-11-02T13:00:00Z"), 3.93));
        let p1 = nyf_parse_rates(&body, "percentPercentile1", 8, true);
        assert_eq!(p1.len(), 2); // Oct 30 row lacks P1
        let daily = nyf_bucket(&pts, 86400, false);
        assert_eq!(daily[0].t, ts("2026-10-29T00:00:00Z"));
        assert_eq!(daily[0].c, 3.94);
    }

    #[tokio::test]
    #[ignore]
    async fn nyfed_live() {
        let end = Utc::now();
        let start = end - Duration::days(14);
        let tickers: Vec<String> = [
            "NYFED:SRF", "NYFED:SRF_SUBMITTED", "NYFED:RRP", "NYFED:RRP_CPTY", "NYFED:SOFR",
            "NYFED:SOFR_P99", "NYFED:EFFR", "NYFED:TGCR", "NYFED:BGCR", "NYFED:SOFR_30D",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let day = Span { n: 1, unit: SpanUnit::Day };
        let (series, errors) = fetch_nyfed_series(&tickers, start, end, day).await.unwrap();
        for s in &series {
            println!("{} -> {:?}", s.ticker, s.candles.iter().rev().take(4).map(|c| (c.t.date_naive(), c.c)).collect::<Vec<_>>());
        }
        println!("errors: {errors:?}");
        assert_eq!(series.len(), tickers.len());
    }
}
