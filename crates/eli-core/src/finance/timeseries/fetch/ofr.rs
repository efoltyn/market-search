/// Office of Financial Research (OFR) data as timeseries: generic mnemonic passthrough.
///
/// Endpoints (no key):
///   Short-term Funding Monitor: https://data.financialresearch.gov/v1/series/timeseries?mnemonic=<M>&start_date=YYYY-MM-DD
///     datasets/prefixes: REPO- (U.S. repo markets, daily), MMF- (money funds, monthly),
///     NYPD- (primary dealer stats, weekly), FNYR- (NY Fed reference rates), TYLD- (Treasury yields)
///     catalog: https://data.financialresearch.gov/v1/metadata/mnemonics[?dataset=repo]
///   Hedge Fund Monitor:         https://data.financialresearch.gov/hf/v1/series/timeseries?mnemonic=<M>&start_date=YYYY-MM-DD
///     prefixes: FICC- (sponsored repo, daily), FPF- (Form PF, quarterly), TFF- (CFTC TFF),
///     SCOOS- (Fed SCOOS survey)
///     catalog: https://data.financialresearch.gov/hf/v1/metadata/mnemonics
///   Financial Stress Index:     https://www.financialresearch.gov/financial-stress-index/data/fsi.csv
///     (same CSV the `finance_stress` tool reads)
///   Both JSON APIs return a bare `[[date, value], ...]` array (value may be null). Responses
///   are plain JSON unless the client asks for gzip, so no decompression is needed. The hf API
///   returns rows UNSORTED when called without start_date, so we always pass start_date and
///   sort. `end_date` beyond the server's "today" is a 400, so it is never sent; the window
///   end is applied client-side.
///
/// Ticker grammar: `OFR:<MNEMONIC>` (case-insensitive; mnemonic is uppercased)
///   `OFR:REPO-DVP_OV_TOT-P`          DVP repo outstanding volume, USD (raw dollars)
///   `OFR:REPO-TRI_AR_OO-P`           tri-party overnight average rate, percent
///   `OFR:FICC-SPONSORED_REPO_VOL`    FICC sponsored repo volume, USD (raw dollars)
///   `OFR:FPF-ALLQHF_CURRENTLYGATES_PERCENT`  Form PF qualifying HFs currently gating, percent
///   `OFR:FSI`, `OFR:FSI_CREDIT`, `OFR:FSI_EQUITY`, `OFR:FSI_SAFE`, `OFR:FSI_FUNDING`,
///   `OFR:FSI_VOL`, `OFR:FSI_US`, `OFR:FSI_OAE`, `OFR:FSI_EM`   OFR Financial Stress Index + components
///   Routing: FICC-/FPF-/TFF-/SCOOS- go to hf/v1 first, everything else to v1; on an
///   "Invalid mnemonic" 400 the other API is tried.
///
/// Units: values are passed through RAW (no scaling): dollar volumes are in dollars
/// (3.69e12 = $3.69tn), rates/percent series in percent, the FSI in index points.
/// The unit for any mnemonic is in /v1/metadata/query?mnemonic=<M> (`unit` block).
///
/// Timestamps: 00:00 UTC of the observation date. LOOK-AHEAD CAVEAT: OFR publishes no
/// per-observation release time, so these are observation-date stamps, not publication
/// stamps. Known lags: REPO- daily data ~T+1 business day (preliminary "-P" series are
/// later revised); FSI ~T+1/T+2; FICC sponsored repo is released monthly (daily rows
/// appear weeks after the fact); FPF- quarterly rows arrive months after quarter end.
/// Do not read same-day causality from these stamps.

const OFR_API_BASE: &str = "https://data.financialresearch.gov";
const OFR_FSI_CSV_URL: &str = "https://www.financialresearch.gov/financial-stress-index/data/fsi.csv";

/// Returns true if the ticker routes to the OFR provider.
pub fn is_ofr_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    ticker_has_prefix(t, "OFR:")
}

fn ofr_mnemonic(ticker: &str) -> String {
    let t = ticker.trim();
    let raw = if is_ofr_ticker(t) { &t[4..] } else { t };
    raw.trim().to_ascii_uppercase()
}

/// FSI CSV column index for an `OFR:FSI[_X]` mnemonic.
fn ofr_fsi_column(m: &str) -> Option<usize> {
    // CSV: Date,OFR FSI,Credit,Equity valuation,Safe assets,Funding,Volatility,United States,Other advanced economies,Emerging markets
    Some(match m {
        "FSI" => 1,
        "FSI_CREDIT" => 2,
        "FSI_EQUITY" | "FSI_EQUITY_VALUATION" => 3,
        "FSI_SAFE" | "FSI_SAFE_ASSETS" => 4,
        "FSI_FUNDING" => 5,
        "FSI_VOL" | "FSI_VOLATILITY" => 6,
        "FSI_US" => 7,
        "FSI_OAE" | "FSI_OTHER_ADVANCED" => 8,
        "FSI_EM" => 9,
        _ => return None,
    })
}

/// True when the mnemonic belongs to the Hedge Fund Monitor (hf/v1) API.
fn ofr_is_hf(m: &str) -> bool {
    ["FICC-", "FPF-", "TFF-", "SCOOS-"].iter().any(|p| m.starts_with(p))
}

fn ofr_day(date: &str) -> Option<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(date.trim().get(..10)?, "%Y-%m-%d").ok()?;
    d.and_hms_opt(0, 0, 0).map(|n| n.and_utc())
}

/// Parse `[[date, value], ...]` (value may be null / string).
fn ofr_parse_pairs(body: &serde_json::Value) -> Vec<(DateTime<Utc>, f64)> {
    let mut out: Vec<(DateTime<Utc>, f64)> = body
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let r = r.as_array()?;
                    let t = ofr_day(r.first()?.as_str()?)?;
                    let v = match r.get(1)? {
                        serde_json::Value::Number(n) => n.as_f64()?,
                        serde_json::Value::String(s) => s.trim().parse().ok()?,
                        _ => return None,
                    };
                    v.is_finite().then_some((t, v))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|p| p.0);
    out.dedup_by_key(|p| p.0);
    out
}

/// Parse the FSI CSV for one column.
fn ofr_parse_fsi_csv(body: &str, col: usize) -> Vec<(DateTime<Utc>, f64)> {
    let mut out = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let line = line.trim_start_matches('\u{FEFF}');
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        let Some(t) = cols.first().and_then(|d| ofr_day(d)) else { continue };
        if let Some(v) = cols.get(col).and_then(|s| s.trim().parse::<f64>().ok()) {
            out.push((t, v));
        }
    }
    out.sort_by_key(|p| p.0);
    out
}

/// Fetch one mnemonic from v1 or hf/v1 (with fallback to the other API).
async fn ofr_fetch_mnemonic(
    m: &str,
    start: DateTime<Utc>,
) -> std::result::Result<(Vec<(DateTime<Utc>, f64)>, &'static str), String> {
    let order: [&'static str; 2] = if ofr_is_hf(m) { ["hf/v1", "v1"] } else { ["v1", "hf/v1"] };
    let start_date = start.date_naive().max(NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default());
    // start_date after the server's today is rejected; clamp a little behind now.
    let latest_ok = (Utc::now() - Duration::days(1)).date_naive();
    let start_date = start_date.min(latest_ok).format("%Y-%m-%d");
    let mut last_err = String::new();
    for api in order {
        let url = format!(
            "{OFR_API_BASE}/{api}/series/timeseries?mnemonic={}&start_date={start_date}",
            urlencoding::encode(m)
        );
        match plumb_http_get(&url).await {
            Ok((200, body)) => {
                let json: serde_json::Value = serde_json::from_str(&body)
                    .map_err(|e| format!("ofr json parse failed ({api}): {e}"))?;
                return Ok((ofr_parse_pairs(&json), api));
            }
            Ok((status, body)) => {
                let snippet: String = body.chars().take(160).collect();
                last_err = format!("ofr {api} returned http {status}: {snippet}");
                if !snippet.to_ascii_lowercase().contains("invalid mnemonic") {
                    return Err(last_err);
                }
            }
            Err(e) => return Err(format!("ofr {api} request failed: {e}")),
        }
    }
    Err(format!(
        "{last_err} (unknown OFR mnemonic '{m}'; browse {OFR_API_BASE}/v1/metadata/mnemonics or {OFR_API_BASE}/hf/v1/metadata/mnemonics)"
    ))
}

/// Fetch OFR series for `OFR:<MNEMONIC>` tickers (and `OFR:FSI[_X]`).
pub(crate) async fn fetch_ofr_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let step = granularity.approx_duration();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mut fsi_csv: Option<std::result::Result<String, String>> = None;

    for ticker in tickers {
        let m = ofr_mnemonic(ticker);
        if m.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: "OFR ticker missing mnemonic (e.g. OFR:REPO-DVP_OV_TOT-P, OFR:FSI)".to_string(),
            });
            continue;
        }

        let (points, upstream) = if let Some(col) = ofr_fsi_column(&m) {
            if fsi_csv.is_none() {
                fsi_csv = Some(match plumb_http_get(OFR_FSI_CSV_URL).await {
                    Ok((200, body)) => Ok(body),
                    Ok((status, _)) => Err(format!("OFR FSI csv returned http {status}")),
                    Err(e) => Err(format!("OFR FSI csv request failed: {e}")),
                });
            }
            match fsi_csv.as_ref() {
                Some(Ok(body)) => (ofr_parse_fsi_csv(body, col), format!("fsi.csv:{m}")),
                Some(Err(e)) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("fetch".to_string()),
                        message: e.clone(),
                    });
                    continue;
                }
                None => continue,
            }
        } else if m.starts_with("FSI") {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: format!(
                    "unknown FSI component '{m}' (FSI, FSI_CREDIT, FSI_EQUITY, FSI_SAFE, FSI_FUNDING, FSI_VOL, FSI_US, FSI_OAE, FSI_EM)"
                ),
            });
            continue;
        } else {
            match ofr_fetch_mnemonic(&m, start).await {
                Ok((pts, api)) => (pts, format!("{api}:{m}")),
                Err(message) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("fetch".to_string()),
                        message,
                    });
                    continue;
                }
            }
        };

        let candles: Vec<Candle> = points
            .into_iter()
            .filter(|(t, _)| *t >= start && *t <= end)
            .map(|(t, v)| Candle { t, o: v, h: v, l: v, c: v, v: None, kind: None })
            .collect();
        let candles = if step.num_seconds() > 86400 {
            resample_candles(&candles, start, step)
        } else {
            candles
        };
        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("insufficient_data".to_string()),
                message: format!(
                    "OFR returned no observations for '{m}' in this window (many OFR series are weekly/monthly/quarterly and published with a lag; try a longer range)"
                ),
            });
            continue;
        }
        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("ofr".to_string()),
            upstream_id: Some(upstream),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod ofr_tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn ofr_ticker_grammar() {
        assert!(is_ofr_ticker("OFR:REPO-DVP_OV_TOT-P"));
        assert!(is_ofr_ticker("ofr:fsi"));
        assert!(!is_ofr_ticker("OFR:"));
        assert!(!is_ofr_ticker("SPY"));
        assert_eq!(ofr_mnemonic("ofr:ficc-sponsored_repo_vol"), "FICC-SPONSORED_REPO_VOL");
        assert!(ofr_is_hf("FICC-SPONSORED_REPO_VOL"));
        assert!(ofr_is_hf("FPF-ALLQHF_CURRENTLYGATES_PERCENT"));
        assert!(!ofr_is_hf("REPO-TRI_AR_OO-P"));
        assert_eq!(ofr_fsi_column("FSI"), Some(1));
        assert_eq!(ofr_fsi_column("FSI_FUNDING"), Some(5));
        assert_eq!(ofr_fsi_column("FSI_EM"), Some(9));
        assert_eq!(ofr_fsi_column("FSIX"), None);
    }

    #[test]
    fn ofr_parse_v1_pairs() {
        // Real v1 response (REPO-DVP_OV_TOT-P, 2026-10-20..28) with a null and shuffled order added.
        let body: serde_json::Value = serde_json::from_str(
            r#"[["2026-10-27", 3745832453236.84], ["2026-10-20", 3370635436631.22], ["2026-10-21", null], ["2026-10-28", 3690452175955.61]]"#,
        )
        .unwrap();
        let pts = ofr_parse_pairs(&body);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].0, ts("2026-10-20T00:00:00Z"));
        assert_eq!(pts[2], (ts("2026-10-28T00:00:00Z"), 3690452175955.61));
    }

    #[test]
    fn ofr_parse_hf_pairs() {
        // Real hf/v1 response (FPF-ALLQHF_CURRENTLYGATES_PERCENT, quarterly).
        let body: serde_json::Value = serde_json::from_str(
            r#"[["2025-03-31", 2.6], ["2025-06-30", 2.6], ["2025-09-30", 2.6], ["2025-12-31", 2.6], ["2026-03-31", 2.4], ["2026-06-30", 2.4]]"#,
        )
        .unwrap();
        let pts = ofr_parse_pairs(&body);
        assert_eq!(pts.len(), 6);
        assert_eq!(pts[5], (ts("2026-06-30T00:00:00Z"), 2.4));
        assert!(ofr_parse_pairs(&serde_json::json!("Invalid mnemonic")).is_empty());
    }

    #[test]
    fn ofr_parse_fsi() {
        let csv = "\u{FEFF}Date,OFR FSI,Credit,Equity valuation,Safe assets,Funding,Volatility,United States,Other advanced economies,Emerging markets\n\
2026-10-27,-1.913,-0.891,-0.487,-0.292,-0.134,-0.109,-1.092,-0.326,-0.495\n\
2026-10-28,-1.829,-0.915,-0.498,-0.289,-0.143,0.016,-1.139,-0.186,-0.505\n";
        let fsi = ofr_parse_fsi_csv(csv, 1);
        assert_eq!(fsi, vec![(ts("2026-10-27T00:00:00Z"), -1.913), (ts("2026-10-28T00:00:00Z"), -1.829)]);
        let vol = ofr_parse_fsi_csv(csv, 6);
        assert_eq!(vol[1].1, 0.016);
    }

    #[tokio::test]
    #[ignore]
    async fn ofr_live() {
        let end = Utc::now();
        let start = end - Duration::days(30);
        let tickers: Vec<String> = [
            "OFR:REPO-DVP_OV_TOT-P",
            "OFR:REPO-TRI_AR_OO-P",
            "OFR:REPO-GCF_AR_OO-P",
            "OFR:FICC-SPONSORED_REPO_VOL",
            "OFR:FSI",
            "OFR:FSI_FUNDING",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let day = Span { n: 1, unit: SpanUnit::Day };
        let (series, errors) = fetch_ofr_series(&tickers, start, end, day).await.unwrap();
        for s in &series {
            println!("{} [{:?}] -> {:?}", s.ticker, s.upstream_id, s.candles.iter().rev().take(3).map(|c| (c.t.date_naive(), c.c)).collect::<Vec<_>>());
        }
        println!("errors: {errors:?}");
        let (q, qerr) = fetch_ofr_series(
            &["OFR:FPF-ALLQHF_CURRENTLYGATES_PERCENT".to_string()],
            end - Duration::days(800),
            end,
            day,
        )
        .await
        .unwrap();
        println!("FPF -> {:?} {qerr:?}", q.first().map(|s| s.candles.iter().map(|c| (c.t.date_naive(), c.c)).collect::<Vec<_>>()));
        assert_eq!(series.len(), tickers.len());
    }
}
