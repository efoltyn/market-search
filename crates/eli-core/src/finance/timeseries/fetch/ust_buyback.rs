/// US Treasury buyback operations (Treasury Fiscal Data API).
///
/// Endpoint: https://api.fiscaldata.treasury.gov/services/api/fiscal_service/v1/accounting/od/buybacks_operations
///   No key. Paginated (`page[size]`, `meta.total-pages`); filtered server-side on operation_date.
///   ~231 rows total (2000-2002 legacy program + the 2024+ regular program).
///
/// Ticker grammar: `BUYBACK:<BUCKET>[:<FIELD>]` (case-insensitive, FIELD defaults to ACCEPTED)
///   BUCKET
///     `20-30Y`, `10-20Y`, `7-10Y`, `5-7Y`, `3-5Y`, `2-3Y`, `1M-2Y`  nominal-coupon maturity buckets
///         (also accepted: `20Y-30Y`, `1MO-2Y`, `20Y TO 30Y`)
///     `TIPS-1-10Y`, `TIPS-10-30Y`, `TIPS-1-7.5Y`, `TIPS-7.5-30Y`   TIPS maturity buckets
///     `ALL`      every operation (includes the 2000-2002 legacy program, which has no bucket)
///     `NOMINAL`  all nominal-coupon operations;  `TIPS` all TIPS operations
///     `LIQ`      all "Liquidity Support" operations;  `CASH` all "Cash Management" operations
///   FIELD
///     `OFFERED`  total_par_amt_offered, $bn
///     `ACCEPTED` total_par_amt_accepted, $bn
///     `MAX`      max_par_amt_redeemed (announced size), $bn
///     `COVER`    offered / max (dealer offers per $1 Treasury will buy back), ratio
///     `FILL`     accepted / max (1.0 = Treasury bought the full announced size), ratio
///     `ISSUES`   nbr_issues_accepted, count
///   Examples: BUYBACK:20-30Y:COVER, BUYBACK:10-20Y:OFFERED, BUYBACK:TIPS-1-10Y:ACCEPTED,
///             BUYBACK:ALL:ACCEPTED, BUYBACK:CASH:FILL
///
/// Timestamps: each operation is stamped at its operation close time
/// (`operation_close_time_est`, US/Eastern, DST-aware) converted to UTC. Results are
/// posted the same afternoon, shortly after the close, so nothing is look-ahead at
/// daily granularity. Operations are discrete events (roughly 2-3 per week), so there
/// is no candle on days without an operation.
///
/// Aggregation: when several operations fall in one granularity bucket (e.g. 1w, or
/// the ALL/LIQ aggregates), OFFERED/ACCEPTED/MAX/ISSUES are SUMMED and COVER/FILL are
/// recomputed from the sums. Candles are stamped at the bucket start (00:00 UTC for 1d).

const BUYBACK_URL: &str =
    "https://api.fiscaldata.treasury.gov/services/api/fiscal_service/v1/accounting/od/buybacks_operations";
/// Shared by the funding/plumbing providers (ust_buyback.rs, nyfed_ops.rs, ofr.rs).
const PLUMB_USER_AGENT: &str = concat!(
    "market-search/",
    env!("CARGO_PKG_VERSION"),
    " (timeseries; funding-plumbing providers)"
);

/// Returns true if the ticker routes to the Treasury buyback provider.
pub fn is_buyback_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    ticker_has_prefix(t, "BUYBACK:")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuybackField {
    Offered,
    Accepted,
    Max,
    Cover,
    Fill,
    Issues,
}

#[derive(Clone, Debug, PartialEq)]
enum BuybackBucket {
    All,
    Nominal,
    Tips,
    Liquidity,
    Cash,
    /// (is_tips, normalized range key like "20-30" / "1M-2" / "1-7.5")
    Range(bool, String),
}

#[derive(Clone, Debug, PartialEq)]
struct BuybackOp {
    t: DateTime<Utc>,
    operation_type: Option<String>,
    security_type: Option<String>,
    bucket: Option<String>,
    offered: Option<f64>,
    accepted: Option<f64>,
    max: Option<f64>,
    issues: Option<f64>,
}

/// "20Y to 30Y" / "20-30Y" / "20Y-30Y" / "1Mo to 2Y" / "1M-2Y" -> "20-30" / "1M-2".
fn bb_norm_range(s: &str) -> String {
    let up = s.trim().to_ascii_uppercase().replace(" TO ", "-");
    let compact: String = up.chars().filter(|c| !c.is_whitespace()).collect();
    compact.replace("MO", "M").replace('Y', "")
}

/// Parse `BUYBACK:<BUCKET>[:<FIELD>]`.
fn bb_parse_ticker(ticker: &str) -> std::result::Result<(BuybackBucket, BuybackField), String> {
    let t = ticker.trim();
    if !is_buyback_ticker(t) {
        return Err(format!("not a BUYBACK: ticker: '{ticker}'"));
    }
    let rest = &t[8..];
    let mut parts = rest.splitn(2, ':');
    let bucket_raw = parts.next().unwrap_or("").trim();
    let field_raw = parts.next().map(|s| s.trim()).unwrap_or("ACCEPTED");
    let field = match field_raw.to_ascii_uppercase().as_str() {
        "OFFERED" | "OFFER" | "OFFERS" => BuybackField::Offered,
        "ACCEPTED" | "ACCEPT" | "BOUGHT" => BuybackField::Accepted,
        "MAX" | "SIZE" => BuybackField::Max,
        "COVER" | "BTC" => BuybackField::Cover,
        "FILL" => BuybackField::Fill,
        "ISSUES" => BuybackField::Issues,
        other => {
            return Err(format!(
                "unknown BUYBACK field '{other}' (use OFFERED, ACCEPTED, MAX, COVER, FILL, ISSUES)"
            ))
        }
    };
    let up = bucket_raw.to_ascii_uppercase();
    let bucket = match up.as_str() {
        "" => return Err("BUYBACK ticker missing bucket (e.g. BUYBACK:20-30Y:OFFERED)".into()),
        "ALL" => BuybackBucket::All,
        "NOMINAL" | "NOM" | "COUPONS" => BuybackBucket::Nominal,
        "TIPS" => BuybackBucket::Tips,
        "LIQ" | "LIQUIDITY" => BuybackBucket::Liquidity,
        "CASH" | "CM" => BuybackBucket::Cash,
        _ => {
            let (tips, range) = match up.strip_prefix("TIPS-").or_else(|| up.strip_prefix("TIPS")) {
                Some(r) => (true, r),
                None => (false, up.as_str()),
            };
            let key = bb_norm_range(range);
            if !key.contains('-') {
                return Err(format!(
                    "unrecognized BUYBACK bucket '{bucket_raw}' (e.g. 20-30Y, 10-20Y, 1M-2Y, TIPS-1-10Y, ALL, NOMINAL, TIPS, LIQ, CASH)"
                ));
            }
            BuybackBucket::Range(tips, key)
        }
    };
    Ok((bucket, field))
}

fn bb_null(s: Option<&str>) -> Option<&str> {
    match s.map(str::trim) {
        None | Some("") | Some("null") => None,
        Some(v) => Some(v),
    }
}

/// US/Eastern -> UTC offset hours (also used by nyfed_ops.rs) (4 during DST, 5 otherwise) for a local date+time.
/// 2007+ rule: 2nd Sunday of March 02:00 to 1st Sunday of November 02:00.
/// Pre-2007 rule: 1st Sunday of April to last Sunday of October.
fn bb_eastern_offset_hours(local: chrono::NaiveDateTime) -> i64 {
    use chrono::Datelike;
    let y = local.date().year();
    let nth_sunday = |month: u32, n: u32| -> chrono::NaiveDate {
        let first = NaiveDate::from_ymd_opt(y, month, 1).unwrap_or_default();
        let to_sun = (7 - first.weekday().num_days_from_sunday()) % 7;
        first + Duration::days((to_sun + 7 * (n - 1)) as i64)
    };
    let (start, end) = if y >= 2007 {
        (nth_sunday(3, 2), nth_sunday(11, 1))
    } else {
        let last_oct = {
            let d = NaiveDate::from_ymd_opt(y, 10, 31).unwrap_or_default();
            d - Duration::days(d.weekday().num_days_from_sunday() as i64)
        };
        (nth_sunday(4, 1), last_oct)
    };
    let two = chrono::NaiveTime::from_hms_opt(2, 0, 0).unwrap_or_default();
    let dst = local >= start.and_time(two) && local < end.and_time(two);
    if dst { 4 } else { 5 }
}

/// "2026-10-27" + "02:00 PM" (US/Eastern) -> UTC. Falls back to 00:00 UTC of the date.
fn bb_stamp(date: &str, time_et: Option<&str>) -> Option<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let local_time = time_et
        .and_then(|s| chrono::NaiveTime::parse_from_str(s.trim(), "%I:%M %p").ok());
    match local_time {
        Some(lt) => {
            let local = d.and_time(lt);
            let off = bb_eastern_offset_hours(local);
            Some((local + Duration::hours(off)).and_utc())
        }
        None => d.and_hms_opt(0, 0, 0).map(|n| n.and_utc()),
    }
}

/// Parse the `data` array of a fiscaldata buybacks_operations response.
fn bb_parse_rows(body: &serde_json::Value) -> Vec<BuybackOp> {
    let rows = body.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in &rows {
        let s = |k: &str| bb_null(r.get(k).and_then(|v| v.as_str()));
        let num = |k: &str| s(k).and_then(|v| v.parse::<f64>().ok());
        let Some(date) = s("operation_date") else { continue };
        let Some(t) = bb_stamp(date, s("operation_close_time_est")) else { continue };
        out.push(BuybackOp {
            t,
            operation_type: s("operation_type").map(str::to_string),
            security_type: s("security_type").map(str::to_string),
            bucket: s("maturity_bucket").map(str::to_string),
            offered: num("total_par_amt_offered").map(|v| v / 1e9),
            accepted: num("total_par_amt_accepted").map(|v| v / 1e9),
            max: num("max_par_amt_redeemed").map(|v| v / 1e9),
            issues: num("nbr_issues_accepted"),
        });
    }
    out
}

fn bb_matches(op: &BuybackOp, bucket: &BuybackBucket) -> bool {
    let is_tips = op.security_type.as_deref().map(|s| s.eq_ignore_ascii_case("TIPS"));
    match bucket {
        BuybackBucket::All => true,
        BuybackBucket::Nominal => {
            op.security_type.as_deref().is_some_and(|s| s.to_ascii_lowercase().contains("nominal"))
        }
        BuybackBucket::Tips => is_tips == Some(true),
        BuybackBucket::Liquidity => op
            .operation_type
            .as_deref()
            .is_some_and(|s| s.to_ascii_lowercase().contains("liquidity")),
        BuybackBucket::Cash => op
            .operation_type
            .as_deref()
            .is_some_and(|s| s.to_ascii_lowercase().contains("cash")),
        BuybackBucket::Range(tips, key) => {
            is_tips == Some(*tips)
                && op.bucket.as_deref().is_some_and(|b| &bb_norm_range(b) == key)
        }
    }
}

/// Bucket matching ops by granularity step and derive the requested field.
fn bb_build_candles(
    ops: &[BuybackOp],
    bucket: &BuybackBucket,
    field: BuybackField,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step_secs: i64,
) -> Vec<Candle> {
    // key -> (offered, accepted, max, issues, saw_max)
    let mut agg: BTreeMap<i64, (f64, f64, f64, f64, bool, bool)> = BTreeMap::new();
    for op in ops.iter().filter(|o| o.t >= start && o.t <= end && bb_matches(o, bucket)) {
        let key = op.t.timestamp().div_euclid(step_secs) * step_secs;
        let e = agg.entry(key).or_insert((0.0, 0.0, 0.0, 0.0, false, false));
        e.0 += op.offered.unwrap_or(0.0);
        e.1 += op.accepted.unwrap_or(0.0);
        if let Some(m) = op.max {
            e.2 += m;
            e.4 = true;
        }
        e.3 += op.issues.unwrap_or(0.0);
        e.5 |= op.offered.is_some() || op.accepted.is_some();
    }
    let mut out = Vec::new();
    for (key, (offered, accepted, max, issues, saw_max, saw_amt)) in agg {
        let v = match field {
            BuybackField::Offered if saw_amt => Some(offered),
            BuybackField::Accepted if saw_amt => Some(accepted),
            BuybackField::Issues => Some(issues),
            BuybackField::Max if saw_max => Some(max),
            BuybackField::Cover if saw_max && max > 0.0 => Some(offered / max),
            BuybackField::Fill if saw_max && max > 0.0 => Some(accepted / max),
            _ => None,
        };
        let (Some(v), Some(t)) = (v, DateTime::<Utc>::from_timestamp(key, 0)) else { continue };
        let v = (v * 1e6).round() / 1e6;
        out.push(Candle { t, o: v, h: v, l: v, c: v, v: None, kind: None });
    }
    out
}

/// GET with retry/backoff on 429/5xx. Ok((status, body)) once a final response arrives.
/// Shared by the funding/plumbing providers (ust_buyback.rs, nyfed_ops.rs, ofr.rs).
async fn plumb_http_get(url: &str) -> std::result::Result<(u16, String), String> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut last_err = String::new();
    for attempt in 0..4u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1))).await;
        }
        match client
            .get(url)
            .header(reqwest::header::USER_AGENT, PLUMB_USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 429 || status >= 500 {
                    last_err = format!("http {status}");
                    continue;
                }
                let body = resp.text().await.map_err(|e| format!("body read failed: {e}"))?;
                return Ok((status, body));
            }
            Err(e) => last_err = format!("request failed: {e}"),
        }
    }
    Err(last_err)
}

/// Fetch all operations with operation_date in [start, end] (paginated).
async fn bb_fetch_ops(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> std::result::Result<Vec<BuybackOp>, String> {
    let mut ops = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!(
            "{BUYBACK_URL}?filter=operation_date:gte:{},operation_date:lte:{}&sort=operation_date&page%5Bsize%5D=500&page%5Bnumber%5D={page}",
            start.date_naive().format("%Y-%m-%d"),
            end.date_naive().format("%Y-%m-%d"),
        );
        let (status, body) = plumb_http_get(&url).await?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "treasury fiscaldata returned http {status}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("json parse failed: {e}"))?;
        ops.extend(bb_parse_rows(&json));
        let total_pages = json.pointer("/meta/total-pages").and_then(|v| v.as_u64()).unwrap_or(1);
        if u64::from(page) >= total_pages || page >= 50 {
            break;
        }
        page += 1;
    }
    ops.sort_by_key(|o| o.t);
    Ok(ops)
}

/// Fetch Treasury buyback series for `BUYBACK:<BUCKET>[:<FIELD>]` tickers.
/// One upstream call (paginated) is shared by every ticker in the request.
pub(crate) async fn fetch_buyback_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let step_secs = granularity.approx_duration().num_seconds().max(1);
    let mut out = Vec::new();
    let mut errors = Vec::new();

    let mut parsed = Vec::new();
    for ticker in tickers {
        match bb_parse_ticker(ticker) {
            Ok(p) => parsed.push((ticker.clone(), p)),
            Err(message) => errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message,
            }),
        }
    }
    if parsed.is_empty() {
        return Ok((out, errors));
    }

    // Operations close mid-afternoon ET; widen by a day so the UTC stamp filter decides.
    let ops = match bb_fetch_ops(start - Duration::days(1), end + Duration::days(1)).await {
        Ok(ops) => ops,
        Err(e) => {
            for (ticker, _) in parsed {
                errors.push(TimeseriesError {
                    ticker,
                    stage: Some("fetch".to_string()),
                    message: format!("treasury buybacks fetch failed: {e}"),
                });
            }
            return Ok((out, errors));
        }
    };

    for (ticker, (bucket, field)) in parsed {
        let candles = bb_build_candles(&ops, &bucket, field, start, end, step_secs);
        if candles.is_empty() {
            let mut seen: Vec<String> = ops
                .iter()
                .filter_map(|o| {
                    let b = o.bucket.as_deref()?;
                    let tips = o.security_type.as_deref() == Some("TIPS");
                    Some(format!("{}{}Y", if tips { "TIPS-" } else { "" }, bb_norm_range(b)))
                })
                .collect();
            seen.sort();
            seen.dedup();
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("insufficient_data".to_string()),
                message: format!(
                    "no Treasury buyback operations matched '{ticker}' in this window (buybacks run ~2-3x/week; try a longer range). Buckets in window: {}",
                    if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
                ),
            });
            continue;
        }
        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("treasury_fiscaldata".to_string()),
            upstream_id: Some(format!("buybacks_operations:{bucket:?}:{field:?}")),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod buyback_tests {
    use super::*;

    const FIXTURE: &str = r#"{"data":[
      {"operation_date":"2026-10-27","operation_start_time_est":"01:40 PM","operation_close_time_est":"02:00 PM","operation_type":"Liquidity Support","security_type":"Nominal Coupons","maturity_bucket":"20Y to 30Y","nbr_issues_accepted":"6","total_par_amt_offered":"10746000000.00","max_par_amt_redeemed":"6000000000","total_par_amt_accepted":"6000000000.00"},
      {"operation_date":"2026-10-21","operation_start_time_est":"01:40 PM","operation_close_time_est":"02:00 PM","operation_type":"Liquidity Support","security_type":"TIPS","maturity_bucket":"1Y to 10Y","nbr_issues_accepted":"8","total_par_amt_offered":"4751000000.00","max_par_amt_redeemed":"750000000","total_par_amt_accepted":"750000000.00"},
      {"operation_date":"2026-10-15","operation_start_time_est":"01:40 PM","operation_close_time_est":"02:00 PM","operation_type":"Liquidity Support","security_type":"Nominal Coupons","maturity_bucket":"10Y to 20Y","nbr_issues_accepted":"3","total_par_amt_offered":"40836000000.00","max_par_amt_redeemed":"6000000000","total_par_amt_accepted":"6000000000.00"},
      {"operation_date":"2026-10-14","operation_start_time_est":"10:00 AM","operation_close_time_est":"11:00 AM","operation_type":"Cash Management","security_type":"Nominal Coupons","maturity_bucket":"1Mo to 2Y","nbr_issues_accepted":"10","total_par_amt_offered":"20000000000.00","max_par_amt_redeemed":"10000000000","total_par_amt_accepted":"8000000000.00"},
      {"operation_date":"2000-03-09","operation_close_time_est":"11:00 AM","operation_type":"null","security_type":"null","maturity_bucket":"null","nbr_issues_accepted":"9","total_par_amt_offered":"8627000000.00","max_par_amt_redeemed":"null","total_par_amt_accepted":"1001000000.00"}
    ],"meta":{"total-pages":1}}"#;

    fn ops() -> Vec<BuybackOp> {
        bb_parse_rows(&serde_json::from_str(FIXTURE).unwrap())
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn buyback_ticker_grammar() {
        assert!(is_buyback_ticker("BUYBACK:20-30Y:COVER"));
        assert!(is_buyback_ticker("buyback:all"));
        assert!(!is_buyback_ticker("BUYBACK:"));
        assert!(!is_buyback_ticker("SPY"));
        assert_eq!(
            bb_parse_ticker("BUYBACK:20-30Y:OFFERED").unwrap(),
            (BuybackBucket::Range(false, "20-30".into()), BuybackField::Offered)
        );
        assert_eq!(
            bb_parse_ticker("buyback:20Y-30Y").unwrap(),
            (BuybackBucket::Range(false, "20-30".into()), BuybackField::Accepted)
        );
        assert_eq!(
            bb_parse_ticker("BUYBACK:TIPS-1-10Y:FILL").unwrap(),
            (BuybackBucket::Range(true, "1-10".into()), BuybackField::Fill)
        );
        assert_eq!(
            bb_parse_ticker("BUYBACK:1MO-2Y:MAX").unwrap().0,
            BuybackBucket::Range(false, "1M-2".into())
        );
        assert_eq!(bb_parse_ticker("BUYBACK:ALL:ACCEPTED").unwrap().0, BuybackBucket::All);
        assert!(bb_parse_ticker("BUYBACK:20-30Y:BOGUS").is_err());
        assert!(bb_parse_ticker("BUYBACK:LONG").is_err());
        assert_eq!(bb_norm_range("1Mo to 2Y"), "1M-2");
        assert_eq!(bb_norm_range("7.5Y to 30Y"), "7.5-30");
    }

    #[test]
    fn buyback_parse_and_stamp() {
        let ops = ops();
        assert_eq!(ops.len(), 5);
        // 02:00 PM EDT on 2026-10-27 -> 18:00 UTC
        assert_eq!(ops[0].t, ts("2026-10-27T18:00:00Z"));
        assert_eq!(ops[0].offered, Some(10.746));
        // March 2000, pre-2007 rule: EST -> 11:00 + 5 = 16:00 UTC
        assert_eq!(ops[4].t, ts("2000-03-09T16:00:00Z"));
        assert_eq!(ops[4].max, None);
        // Winter date under 2007+ rule
        let winter = bb_stamp("2026-12-01", Some("02:00 PM")).unwrap();
        assert_eq!(winter, ts("2026-12-01T19:00:00Z"));
    }

    #[test]
    fn buyback_fields_daily() {
        let ops = ops();
        let (s, e) = (ts("2026-10-01T00:00:00Z"), ts("2026-10-30T00:00:00Z"));
        let day = 86400;
        let c = bb_build_candles(&ops, &BuybackBucket::Range(false, "20-30".into()), BuybackField::Cover, s, e, day);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].t, ts("2026-10-27T00:00:00Z"));
        assert!((c[0].c - 1.791).abs() < 1e-9);
        let c = bb_build_candles(&ops, &BuybackBucket::Range(true, "1-10".into()), BuybackField::Accepted, s, e, day);
        assert_eq!(c[0].c, 0.75);
        let c = bb_build_candles(&ops, &BuybackBucket::Cash, BuybackField::Fill, s, e, day);
        assert_eq!(c.len(), 1);
        assert!((c[0].c - 0.8).abs() < 1e-9);
        let c = bb_build_candles(&ops, &BuybackBucket::Liquidity, BuybackField::Offered, s, e, day);
        assert_eq!(c.len(), 3);
        // Weekly (epoch-week) buckets sum amounts; total across buckets is preserved.
        let c = bb_build_candles(&ops, &BuybackBucket::All, BuybackField::Accepted, s, e, 7 * day);
        let total: f64 = c.iter().map(|c| c.c).sum();
        assert!((total - 20.75).abs() < 1e-9);
        // Legacy op with no MAX yields nothing for COVER
        let c = bb_build_candles(&ops, &BuybackBucket::All, BuybackField::Cover, ts("2000-01-01T00:00:00Z"), ts("2000-12-31T00:00:00Z"), day);
        assert!(c.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn buyback_live() {
        let end = Utc::now();
        let start = end - Duration::days(60);
        let tickers = vec![
            "BUYBACK:20-30Y:OFFERED".to_string(),
            "BUYBACK:10-20Y:COVER".to_string(),
            "BUYBACK:TIPS-1-10Y:ACCEPTED".to_string(),
            "BUYBACK:ALL:ACCEPTED".to_string(),
        ];
        let day = Span { n: 1, unit: SpanUnit::Day };
        let (series, errors) = fetch_buyback_series(&tickers, start, end, day).await.unwrap();
        for s in &series {
            println!("{} -> {:?}", s.ticker, s.candles.iter().map(|c| (c.t.date_naive(), c.c)).collect::<Vec<_>>());
        }
        println!("errors: {errors:?}");
        assert!(!series.is_empty());
    }
}
