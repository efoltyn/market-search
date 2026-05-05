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
        #[serde(default)]
        rows: Option<Vec<NasdaqEarningsRow>>,
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
            .unwrap_or_default()
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
                    market_cap: row.market_cap.as_deref().and_then(parse_money_u64),
                    fiscal_quarter_ending: row
                        .fiscal_quarter_ending
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    eps_forecast: row.eps_forecast.as_deref().and_then(parse_money_f64),
                    no_of_estimates: row.no_of_estimates.as_deref().and_then(parse_u32_loose),
                    last_year_report_date: row
                        .last_year_report_date
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    last_year_eps: row.last_year_eps.as_deref().and_then(parse_money_f64),
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

