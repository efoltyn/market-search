use super::super::timeseries::fetch::fetch_fred_series;
use super::super::*;

pub async fn fetch_macro(req: MacroRequest) -> Result<MacroResponse> {
    let indicators = vec![
        // === Inflation ===
        ("CPIAUCSL", "CPI (Headline Inflation)", "inflation"),
        ("CPILFESL", "Core CPI (Ex Food & Energy)", "inflation"),
        ("PCEPILFE", "Core PCE (Fed Preferred Inflation)", "inflation"),
        ("PPIACO", "PPI (Producer Prices)", "inflation"),
        ("T10YIE", "10Y Breakeven Inflation", "inflation"),
        // === Employment ===
        ("UNRATE", "Unemployment Rate", "employment"),
        ("PAYEMS", "Non-farm Payrolls", "employment"),
        ("ICSA", "Initial Jobless Claims", "employment"),
        ("JTSJOL", "Job Openings (JOLTS)", "employment"),
        // === GDP & Output ===
        ("GDPC1", "Real GDP", "gdp"),
        ("INDPRO", "Industrial Production", "gdp"),
        // === Interest Rates & Yields ===
        ("FEDFUNDS", "Fed Funds Rate", "rates"),
        ("DGS2", "2-Year Treasury Yield", "rates"),
        ("DGS10", "10-Year Treasury Yield", "rates"),
        ("DGS30", "30-Year Treasury Yield", "rates"),
        ("T10Y2Y", "10Y-2Y Yield Spread", "rates"),
        ("DFII10", "10Y TIPS Real Yield", "rates"),
        ("MORTGAGE30US", "30-Year Mortgage Rate", "rates"),
        // === Debt & Fiscal ===
        ("GFDEGDQ188S", "Federal Debt to GDP (Total)", "debt"),
        ("FYGFGDQ188S", "Federal Debt to GDP (Public)", "debt"),
        ("GFDEBTN", "Federal Debt Total", "debt"),
        // === Money Supply & Fed ===
        ("M2SL", "M2 Money Supply", "money"),
        ("WALCL", "Fed Balance Sheet Total Assets", "money"),
        // === Consumer & Housing ===
        ("UMCSENT", "Consumer Sentiment (UMich)", "consumer"),
        ("RSAFS", "Retail Sales", "consumer"),
        ("PSAVERT", "Personal Savings Rate", "consumer"),
        ("CSUSHPISA", "Case-Shiller Home Price Index", "consumer"),
        ("HOUST", "Housing Starts", "consumer"),
        ("TOTALSA", "Total Vehicle Sales", "consumer"),
        // === Credit & Risk ===
        ("BAMLH0A0HYM2", "High Yield Credit Spread", "credit"),
        // === Commodities & FX ===
        ("DCOILWTICO", "WTI Oil Price", "commodities"),
        ("DTWEXBGS", "Trade-Weighted Dollar Index", "commodities"),
    ];

    let range = req.range.unwrap_or(Span {
        n: 1,
        unit: SpanUnit::Year,
    });
    let end = Utc::now();
    let mut start = end - range.approx_duration() - Duration::days(400); // extra for 1y change
    let compare_to_dt = req.compare_to.and_then(|d| {
        d.and_hms_opt(23, 59, 59)
            .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
    });
    if let Some(cmp) = compare_to_dt {
        let compare_start = cmp - Duration::days(400);
        if compare_start < start {
            start = compare_start;
        }
    }

    // Fetch indicators with bounded concurrency (8 at a time) to avoid FRED rate limits.
    use futures::stream::{self, StreamExt};
    let granularity = Span { n: 1, unit: SpanUnit::Month };
    let quarterly = Span { n: 3, unit: SpanUnit::Month };
    let out: Vec<MacroIndicator> = stream::iter(indicators.iter().map(|(id, name, category)| {
        let id = id.to_string();
        let name = name.to_string();
        let category = category.to_string();
        let compare_to_dt = compare_to_dt.clone();
        async move {
            // Try monthly first; fall back to quarterly for GDP-type series.
            let series = fetch_fred_series(&[id.clone()], start, end, granularity).await;
            let candles = match series {
                Ok((mut svec, _)) => svec.pop().and_then(|s| {
                    if s.candles.is_empty() { None } else { Some(s.candles) }
                }),
                Err(_) => None,
            };
            let candles = match candles {
                Some(c) => c,
                None => {
                    // Retry with quarterly granularity (e.g. GDPC1)
                    match fetch_fred_series(&[id.clone()], start, end, quarterly).await {
                        Ok((mut svec, _)) => svec.pop().map(|s| s.candles).unwrap_or_default(),
                        Err(_) => return None,
                    }
                }
            };
            if let Some(latest) = candles.last() {
                let mut change_1y = None;
                let lookback = if candles.len() > 12 { 13 } else if candles.len() > 4 { 5 } else { 0 };
                if lookback > 0 {
                    let ago = &candles[candles.len().saturating_sub(lookback)];
                    if ago.c != 0.0 {
                        change_1y = Some((latest.c - ago.c) / ago.c * 100.0);
                    }
                }
                let mut compare_value = None;
                let mut delta_abs = None;
                let mut delta_pct = None;
                if let Some(compare_ts) = compare_to_dt {
                    if let Some(anchor) = candles.iter().rev().find(|c| c.t <= compare_ts) {
                        compare_value = Some(anchor.c);
                        let dabs = latest.c - anchor.c;
                        delta_abs = Some(dabs);
                        if anchor.c != 0.0 {
                            delta_pct = Some((dabs / anchor.c) * 100.0);
                        }
                    }
                }
                return Some(MacroIndicator {
                    symbol: id,
                    name,
                    category,
                    current_value: latest.c,
                    change_1y,
                    compare_value,
                    delta_abs,
                    delta_pct,
                });
            }
            None
        }
    }))
    .buffer_unordered(8)
    .filter_map(|x| async { x })
    .collect()
    .await;

    Ok(MacroResponse {
        generated_at: Utc::now(),
        indicators: out,
    })
}

pub(crate) fn parse_schedule_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| Error::InvalidInput(format!("invalid date '{s}' (expected YYYY-MM-DD)")))
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_nasdaq_time(raw: Option<&str>) -> Option<String> {
    let v = raw?.trim();
    if v.is_empty() {
        return None;
    }
    Some(match v {
        "time-pre-market" => "pre-market".to_string(),
        "time-after-hours" => "after-hours".to_string(),
        "time-not-supplied" => "not-supplied".to_string(),
        other => other.to_string(),
    })
}

pub(crate) async fn fetch_nasdaq_earnings_for_date(
    client: &reqwest::Client,
    date: NaiveDate,
) -> Result<Vec<EarningsScheduleEvent>> {
    #[derive(Debug, Deserialize)]
    struct NasdaqCalendarResp {
        data: NasdaqCalendarData,
    }
    #[derive(Debug, Deserialize)]
    struct NasdaqCalendarData {
        #[serde(rename = "asOf")]
        #[allow(dead_code)]
        as_of: Option<String>,
        rows: Vec<NasdaqEarningsRow>,
    }
    #[derive(Debug, Deserialize)]
    struct NasdaqEarningsRow {
        time: Option<String>,
        symbol: Option<String>,
        name: Option<String>,
        #[serde(rename = "marketCap")]
        market_cap: Option<String>,
        #[serde(rename = "fiscalQuarterEnding")]
        fiscal_quarter_ending: Option<String>,
        #[serde(rename = "epsForecast")]
        eps_forecast: Option<String>,
        #[serde(rename = "noOfEsts")]
        no_of_estimates: Option<String>,
        #[serde(rename = "lastYearRptDt")]
        last_year_report_date: Option<String>,
        #[serde(rename = "lastYearEPS")]
        last_year_eps: Option<String>,
    }

    let url = "https://api.nasdaq.com/api/calendar/earnings";
    let date_s = date.format("%Y-%m-%d").to_string();
    let mut last_err: Option<String> = None;

    for attempt in 0..4 {
        let start_time = std::time::Instant::now();
        let resp = client
            .get(url)
            .query(&[("date", &date_s)])
            .header("Accept", "application/json, text/plain, */*")
            .header("Origin", "https://www.nasdaq.com")
            .header("Referer", "https://www.nasdaq.com/")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(format!("request error: {e}"));
                if attempt < 3 {
                    let backoff_ms = 350u64.saturating_mul(1u64 << attempt);
                    sleep(TokioDuration::from_millis(backoff_ms)).await;
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("nasdaq earnings read failed: {e}")))?;
        info!(
            target: "eli.finance.schedule",
            provider = "nasdaq",
            endpoint = "calendar/earnings",
            date = %date_s,
            status = %status,
            bytes = body.len(),
            elapsed_ms = start_time.elapsed().as_millis(),
            "schedule fetch"
        );

        let looks_rate_limited = body.to_ascii_lowercase().contains("too many requests");
        let retryable_status = status.as_u16() == 429 || status.as_u16() >= 500;
        if looks_rate_limited || retryable_status {
            last_err = Some(format!("http {} (rate-limited/retryable)", status));
            if attempt < 3 {
                let backoff_ms = 450u64.saturating_mul(1u64 << attempt);
                sleep(TokioDuration::from_millis(backoff_ms)).await;
                continue;
            }
            break;
        }

        if !status.is_success() {
            return Err(Error::Provider(format!(
                "nasdaq earnings fetch failed: http {}",
                status
            )));
        }

        let parsed: NasdaqCalendarResp = serde_json::from_str(&body)
            .map_err(|e| Error::Provider(format!("nasdaq earnings parse failed: {e}")))?;

        let out = parsed
            .data
            .rows
            .into_iter()
            .filter_map(|row| {
                let symbol = row.symbol?.trim().to_string();
                if symbol.is_empty() {
                    return None;
                }
                let company_name = row.name.unwrap_or_default().trim().to_string();
                Some(EarningsScheduleEvent {
                    date: date_s.clone(),
                    symbol,
                    company_name,
                    time: parse_nasdaq_time(row.time.as_deref()),
                    market_cap: row
                        .market_cap
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    fiscal_quarter_ending: row
                        .fiscal_quarter_ending
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    eps_forecast: row
                        .eps_forecast
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    no_of_estimates: row
                        .no_of_estimates
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    last_year_report_date: row
                        .last_year_report_date
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    last_year_eps: row
                        .last_year_eps
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    source: "nasdaq".to_string(),
                })
            })
            .collect();
        return Ok(out);
    }

    Err(Error::Provider(format!(
        "nasdaq earnings fetch failed after retries: {}",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    )))
}

pub(crate) async fn fetch_fred_macro_counts(
    client: &reqwest::Client,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> Result<Vec<MacroScheduleDay>> {
    #[derive(Debug, Deserialize)]
    struct FredCountResp {
        events: Vec<FredCountEvent>,
    }
    #[derive(Debug, Deserialize)]
    struct FredCountEvent {
        title: String,
        start: String,
    }

    let mut url = reqwest::Url::parse("https://fred.stlouisfed.org/releases/calendar")
        .map_err(|e| Error::Provider(format!("fred calendar url build failed: {e}")))?;
    url.query_pairs_mut()
        .append_pair("rdc", "1")
        .append_pair("vs", &start_date.format("%Y-%m-%d").to_string())
        .append_pair("ve", &end_date.format("%Y-%m-%d").to_string())
        .append_pair("rid", "0");
    let url_s = url.to_string();

    let start_time = std::time::Instant::now();
    let resp = client
        .get(url)
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "eli/finance-schedule")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar counts fetch failed: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar counts read failed: {e}")))?;
    info!(
        target: "eli.finance.schedule",
        provider = "fred",
        endpoint = "releases/calendar rdc=1",
        url = %url_s,
        status = %status,
        bytes = body.len(),
        elapsed_ms = start_time.elapsed().as_millis(),
        "schedule fetch"
    );
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "fred calendar counts fetch failed: http {}",
            status
        )));
    }

    let parsed: FredCountResp = serde_json::from_str(&body)
        .map_err(|e| Error::Provider(format!("fred calendar counts parse failed: {e}")))?;
    let days = parsed
        .events
        .into_iter()
        .map(|e| {
            let release_count = e
                .title
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            MacroScheduleDay {
                date: e.start,
                release_count,
            }
        })
        .collect();
    Ok(days)
}

pub(crate) async fn fetch_fred_macro_for_day(
    client: &reqwest::Client,
    date: NaiveDate,
) -> Result<Vec<MacroScheduleEvent>> {
    #[derive(Debug, Deserialize)]
    struct FredPagerResp {
        pager: String,
    }

    let date_s = date.format("%Y-%m-%d").to_string();
    let mut url = reqwest::Url::parse("https://fred.stlouisfed.org/releases/calendar")
        .map_err(|e| Error::Provider(format!("fred calendar url build failed: {e}")))?;
    url.query_pairs_mut()
        .append_pair("po", "1")
        .append_pair("ptic", "0")
        .append_pair("vs", &date_s)
        .append_pair("ve", &date_s)
        .append_pair("rid", "0");
    let url_s = url.to_string();

    let start_time = std::time::Instant::now();
    let resp = client
        .get(url)
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "eli/finance-schedule")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar day fetch failed: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar day read failed: {e}")))?;
    info!(
        target: "eli.finance.schedule",
        provider = "fred",
        endpoint = "releases/calendar po=1",
        url = %url_s,
        status = %status,
        bytes = body.len(),
        elapsed_ms = start_time.elapsed().as_millis(),
        "schedule fetch"
    );
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "fred calendar day fetch failed: http {}",
            status
        )));
    }
    let parsed: FredPagerResp = serde_json::from_str(&body)
        .map_err(|e| Error::Provider(format!("fred calendar day parse failed: {e}")))?;

    let fragment = scraper::Html::parse_fragment(&parsed.pager);
    let row_sel = scraper::Selector::parse("tr")
        .map_err(|e| Error::Provider(format!("fred html parse failed: {e}")))?;
    let td_sel = scraper::Selector::parse("td")
        .map_err(|e| Error::Provider(format!("fred html parse failed: {e}")))?;
    let a_sel = scraper::Selector::parse("a[href]")
        .map_err(|e| Error::Provider(format!("fred html parse failed: {e}")))?;

    let mut out = Vec::new();
    for row in fragment.select(&row_sel) {
        let cells: Vec<_> = row.select(&td_sel).collect();
        if cells.len() < 2 {
            continue;
        }
        let time = collapse_ws(&cells[0].text().collect::<Vec<_>>().join(" "));
        let detail_cell = cells[1];
        let Some(anchor) = detail_cell.select(&a_sel).next() else {
            continue;
        };
        let title = collapse_ws(&anchor.text().collect::<Vec<_>>().join(" "));
        if title.is_empty() {
            continue;
        }
        let href = anchor
            .value()
            .attr("href")
            .unwrap_or_default()
            .trim()
            .to_string();
        let release_url = if href.starts_with("/release?rid=") {
            Some(format!("https://fred.stlouisfed.org{href}"))
        } else {
            None
        };
        let release_id = href
            .split("rid=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse::<u32>().ok());

        out.push(MacroScheduleEvent {
            date: date_s.clone(),
            time: if time.is_empty() { None } else { Some(time) },
            title,
            release_id,
            release_url,
            source: "fred".to_string(),
        });
    }

    Ok(out)
}
