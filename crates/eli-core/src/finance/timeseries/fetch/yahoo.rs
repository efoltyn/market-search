pub(crate) async fn fetch_yahoo_snapshots(
    tickers: &[String],
    collected_at: DateTime<Utc>,
    freshness_policy: &crate::finance::policy::FreshnessPolicy,
) -> Result<(Vec<TickerSnapshot>, Vec<SnapshotError>)> {
    // Fast path: 1-2 tickers — sequential avoids per-task connector overhead
    if tickers.len() <= 2 {
        let mut connector = yahoo_finance_api::YahooConnector::new()
            .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;
        let mut out = Vec::with_capacity(tickers.len());
        let mut errors = Vec::new();
        for ticker in tickers {
            match fetch_single_yahoo_snapshot(&mut connector, ticker, collected_at, freshness_policy)
                .await
            {
                Ok(snapshot) => out.push(snapshot),
                Err(error) => errors.push(SnapshotError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: error.to_string(),
                }),
            }
        }
        return Ok((out, errors));
    }

    // Parallel path: 3+ tickers — fire up to 8 concurrent requests
    use futures::stream::{self, StreamExt};
    let mut results: Vec<(usize, String, Result<TickerSnapshot>)> =
        stream::iter(tickers.iter().enumerate().map(|(idx, ticker)| {
        let freshness_policy = freshness_policy;
        let ticker = ticker.clone();
        async move {
            match yahoo_finance_api::YahooConnector::new()
                .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))
            {
                Ok(mut connector) => (
                    idx,
                    ticker.clone(),
                    fetch_single_yahoo_snapshot(
                        &mut connector,
                        &ticker,
                        collected_at,
                        freshness_policy,
                    )
                    .await,
                ),
                Err(error) => (idx, ticker, Err(error)),
            }
        }
    }))
        .buffer_unordered(8)
        .collect()
        .await;

    results.sort_by_key(|(idx, _, _)| *idx);
    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    for (_idx, ticker, result) in results {
        match result {
            Ok(snapshot) => out.push(snapshot),
            Err(error) => errors.push(SnapshotError {
                ticker,
                stage: Some("fetch".to_string()),
                message: error.to_string(),
            }),
        }
    }
    Ok((out, errors))
}

async fn fetch_single_yahoo_snapshot(
    connector: &mut yahoo_finance_api::YahooConnector,
    ticker: &str,
    collected_at: DateTime<Utc>,
    freshness_policy: &crate::finance::policy::FreshnessPolicy,
) -> Result<TickerSnapshot> {
        let request_ticker = yahoo_alias_ticker(ticker).unwrap_or(ticker);
        let info = connector.get_ticker_info(request_ticker).await.map_err(|e| {
            Error::Provider(format!("yahoo quote summary failed for '{ticker}': {e}"))
        })?;

        let qs = info.quote_summary.ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary missing for '{ticker}'"))
        })?;
        let result = qs.result.ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary result missing for '{ticker}'"))
        })?;
        let first = result.get(0).ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary result empty for '{ticker}'"))
        })?;

        let quote_type = first.quote_type.as_ref();
        let summary = first.summary_detail.as_ref();
        let stats = first.default_key_statistics.as_ref();
        let fin = first.financial_data.as_ref();

        let currency = summary.and_then(|s| s.currency.clone());
        let exchange = quote_type.and_then(|q| q.exchange.clone());
        let short_name = quote_type.and_then(|q| q.short_name.clone());
        let long_name = quote_type.and_then(|q| q.long_name.clone());

        let mut current_price = fin.and_then(|f| f.current_price);
        let previous_close =
            summary.and_then(|s| s.regular_market_previous_close.or(s.previous_close));
        let open = summary.and_then(|s| s.regular_market_open.or(s.open));
        let day_low = summary.and_then(|s| s.regular_market_day_low.or(s.day_low));
        let day_high = summary.and_then(|s| s.regular_market_day_high.or(s.day_high));

        let mut price_source_kind = "current_price".to_string();
        if current_price.is_none() {
            current_price = previous_close
                .or(open)
                .or_else(|| match (day_low, day_high) {
                    (Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() => {
                        Some((lo + hi) / 2.0)
                    }
                    _ => None,
                });
            price_source_kind = if previous_close.is_some() {
                "previous_close_fallback".to_string()
            } else if open.is_some() {
                "open_fallback".to_string()
            } else if day_low.is_some() && day_high.is_some() {
                "midpoint_fallback".to_string()
            } else {
                "unknown".to_string()
            };
        }
        let session_state = if is_crypto_ticker(ticker) {
            "24/7".to_string()
        } else if is_futures_ticker(ticker) {
            futures_session_state(collected_at)
        } else if is_fx_ticker(ticker) {
            fx_session_state(collected_at)
        } else {
            us_equity_session_state(collected_at)
        };
        // Mark as fallback whenever the market is not in regular session,
        // OR when price==previous_close during regular hours (data looks stale).
        let price_matches_prev = match (current_price, previous_close) {
            (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 => (px - prev).abs() < 1e-4,
            _ => false,
        };
        let data_looks_stale = session_state == "regular" && price_matches_prev;
        let market_closed_fallback = session_state != "regular" || data_looks_stale;

        let enterprise_value = stats.and_then(|s| s.enterprise_value);
        let shares_outstanding = stats.and_then(|s| s.shares_outstanding);
        let float_shares = stats.and_then(|s| s.float_shares);

        let mut market_cap = summary.and_then(|s| s.market_cap);
        if market_cap.is_none() {
            if let (Some(px), Some(sh)) = (current_price, shares_outstanding) {
                if px.is_finite() && px > 0.0 {
                    market_cap = Some((px * (sh as f64)).round() as u64);
                }
            }
        }

        let last_split_factor = stats.and_then(|s| s.last_split_factor.clone());
        let last_split_date = stats
            .and_then(|s| s.last_split_date)
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        let freshness = crate::finance::policy::freshness_from_observed(
            collected_at,
            collected_at,
            freshness_policy,
            FreshnessOrigin::TransportReceived,
            FreshnessQuality::Estimated,
        );

        let observed_at = freshness.observed_at;
        Ok(TickerSnapshot {
            ticker: ticker.to_string(),
            currency,
            exchange,
            short_name,
            long_name,
            current_price,
            previous_close,
            open,
            day_low,
            day_high,
            price: current_price,
            daily_return: if market_closed_fallback {
                // Fallback price ≈ previous_close → daily_return would be ~0.0,
                // which is misleading.  Emit None so consumers know the return
                // is unavailable rather than actually zero.
                None
            } else {
                match (current_price, previous_close) {
                    (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 => {
                        Some((px / prev) - 1.0)
                    }
                    _ => None,
                }
            },
            market_cap,
            enterprise_value,
            shares_outstanding,
            float_shares,
            last_split_factor,
            last_split_date,
            freshness,
            price_source_kind,
            session_state,
            market_closed_fallback,
            effective_at: Some(observed_at),
            clock_status: None,
            integrity_note: if data_looks_stale {
                Some("price==previous_close during regular session; likely stale".to_string())
            } else {
                None
            },
        })
}

/// Infer US equity session state from wall-clock time (Eastern Time).
/// Returns "pre_market", "regular", "after_hours", or "closed".
/// Not holiday-aware, but dramatically better than the hardcoded "unknown".
fn us_equity_session_state(now: DateTime<Utc>) -> String {
    use chrono::{Datelike, Timelike};
    // ET offset: -5h (EST) or -4h (EDT).  Use simple heuristic: DST runs
    // second Sunday of March through first Sunday of November.
    let month = now.month();
    let et_offset_hours: i64 = if month >= 3 && month <= 11 { -4 } else { -5 };
    let et_hour = ((now.hour() as i64 + et_offset_hours).rem_euclid(24)) as u32;
    let et_minute = now.minute();
    let weekday = now.weekday().number_from_monday(); // 1=Mon … 7=Sun

    if weekday >= 6 {
        return "closed".to_string(); // weekend
    }
    // Times in ET: pre-market 4:00–9:30, regular 9:30–16:00, after-hours 16:00–20:00
    let et_mins = et_hour * 60 + et_minute;
    if et_mins < 4 * 60 {
        "closed".to_string()
    } else if et_mins < 9 * 60 + 30 {
        "pre_market".to_string()
    } else if et_mins < 16 * 60 {
        "regular".to_string()
    } else if et_mins < 20 * 60 {
        "after_hours".to_string()
    } else {
        "closed".to_string()
    }
}

/// Returns true for crypto tickers that trade 24/7 (e.g. BTC-USD, ETH-USD).
fn is_crypto_ticker(ticker: &str) -> bool {
    let t = ticker.trim().to_ascii_uppercase();
    t.ends_with("-USD")
        && matches!(
            t.strip_suffix("-USD").unwrap_or(""),
            "BTC" | "ETH" | "SOL" | "XRP" | "ADA" | "DOGE" | "AVAX" | "DOT"
                | "MATIC" | "LINK" | "UNI" | "ATOM" | "LTC" | "BCH" | "NEAR"
                | "APT" | "ARB" | "OP" | "FIL" | "ICP" | "SHIB" | "BNB"
        )
}

/// Returns true for futures tickers that trade nearly 24h Sun-Fri (e.g. CL=F, GC=F).
fn is_futures_ticker(ticker: &str) -> bool {
    ticker.trim().to_ascii_uppercase().ends_with("=F")
}

fn is_fx_ticker(ticker: &str) -> bool {
    ticker.trim().to_ascii_uppercase().ends_with("=X")
}

/// CME/NYMEX/COMEX futures: Sun 5pm CT – Fri 4pm CT, daily break 4-5pm CT.
fn futures_session_state(now: DateTime<Utc>) -> String {
    use chrono::{Datelike, Timelike, Weekday};
    let ct = now - chrono::Duration::hours(5); // UTC-5 = CDT (close enough)
    let wd = ct.weekday();
    let hm = ct.hour() * 60 + ct.minute();
    match wd {
        Weekday::Sat => "closed".to_string(),
        Weekday::Sun => {
            if hm >= 17 * 60 { "regular".to_string() } else { "closed".to_string() }
        }
        Weekday::Fri => {
            if hm < 16 * 60 { "regular".to_string() } else { "closed".to_string() }
        }
        _ => {
            // Daily maintenance break 4:00-5:00pm CT
            if hm >= 16 * 60 && hm < 17 * 60 {
                "break".to_string()
            } else {
                "regular".to_string()
            }
        }
    }
}

/// Retail FX pairs on Yahoo trade nearly 24h from Sunday 5pm ET through Friday 5pm ET.
fn fx_session_state(now: DateTime<Utc>) -> String {
    use chrono::{Datelike, Timelike, Weekday};
    let et = now - chrono::Duration::hours(4); // close enough for DST-heavy months
    let wd = et.weekday();
    let hm = et.hour() * 60 + et.minute();
    match wd {
        Weekday::Sat => "closed".to_string(),
        Weekday::Sun => {
            if hm >= 17 * 60 {
                "regular".to_string()
            } else {
                "closed".to_string()
            }
        }
        Weekday::Fri => {
            if hm < 17 * 60 {
                "regular".to_string()
            } else {
                "closed".to_string()
            }
        }
        _ => "regular".to_string(),
    }
}

fn yahoo_alias_ticker(ticker: &str) -> Option<&'static str> {
    match ticker.trim().to_ascii_uppercase().as_str() {
        "DXY" => Some("DX-Y.NYB"),
        "TTF" | "TTF GAS" | "DUTCH TTF" | "EUROPEAN GAS" => Some("TTF=F"),
        "MOVE" => Some("^MOVE"),
        "OVX" => Some("^OVX"),
        "GVZ" => Some("^GVZ"),
        "VVIX" => Some("^VVIX"),
        "VIX3M" => Some("^VIX3M"),
        "TYVIX" => Some("^TYVIX"),
        _ => None,
    }
}

async fn fetch_yahoo_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
    max_points_per_ticker: Option<usize>,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let start_ts = time::OffsetDateTime::from_unix_timestamp(start.timestamp())
        .map_err(|e| Error::Provider(format!("invalid start timestamp: {e}")))?;
    let request_end = end + Duration::days(1);
    let end_ts = time::OffsetDateTime::from_unix_timestamp(request_end.timestamp())
        .map_err(|e| Error::Provider(format!("invalid end timestamp: {e}")))?;

    let requested_step = granularity.approx_duration();

    // Fetch all tickers concurrently (up to 8 in flight).
    use futures::stream::{self, StreamExt};
    let results: Vec<std::result::Result<TickerSeries, TimeseriesError>> =
        stream::iter(tickers.iter().map(|ticker| {
            let ticker = ticker.clone();
            async move {
                let aligned_intraday = is_crypto_ticker(&ticker)
                    || is_futures_ticker(&ticker)
                    || is_fx_ticker(&ticker);
                let (interval, base_span) =
                    yahoo_base_interval(granularity, aligned_intraday, end - start).map_err(
                        |message| TimeseriesError {
                            ticker: ticker.clone(),
                            stage: Some("input".to_string()),
                            message,
                        },
                    )?;
                let intraday = matches!(base_span.unit, SpanUnit::Minute | SpanUnit::Hour);
                let include_prepost = intraday && aligned_intraday;
                let base_step = base_span.approx_duration();
                let base_step_seconds = base_step.num_seconds();
                if base_step_seconds <= 0 {
                    return Err(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("input".to_string()),
                        message: "invalid yahoo base interval".to_string(),
                    });
                }
                let approx_points =
                    ((end - start).num_seconds() / base_step_seconds).max(1) as usize + 1;
                if let Some(max_points_per_ticker) = max_points_per_ticker {
                    if approx_points > max_points_per_ticker {
                        return Err(TimeseriesError {
                            ticker: ticker.clone(),
                            stage: Some("input".to_string()),
                            message: format!(
                                "requested ~{approx_points} raw points exceeds limit {max_points_per_ticker}; increase granularity or shrink range"
                            ),
                        });
                    }
                }
                let request_ticker = yahoo_alias_ticker(&ticker)
                    .unwrap_or(ticker.as_str())
                    .to_string();
                let quotes = yahoo_fetch_quotes_retry(
                    &request_ticker,
                    start_ts,
                    end_ts,
                    interval,
                    include_prepost,
                )
                .await
                .map_err(|err| TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: err.to_string(),
                })?;

                let mut candles = Vec::with_capacity(quotes.len());
                let mut invalid_timestamp = None;
                for q in quotes {
                    let Some(t) = Utc.timestamp_opt(q.timestamp as i64, 0).single() else {
                        invalid_timestamp = Some(q.timestamp);
                        break;
                    };
                    if t < start || t > end {
                        continue;
                    }
                    candles.push(Candle {
                        t,
                        o: q.open,
                        h: q.high,
                        l: q.low,
                        c: q.close,
                        v: Some(q.volume as f64),
                        kind: None,
                    });
                }

                if let Some(ts) = invalid_timestamp {
                    return Err(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("parse".to_string()),
                        message: format!("yahoo invalid timestamp: {ts}"),
                    });
                }

                candles.sort_by_key(|c| c.t);
                let candles = if requested_step == base_step {
                    candles
                } else {
                    resample_candles(&candles, start, requested_step)
                };

                if candles.is_empty() {
                    return Err(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("fetch".to_string()),
                        message: "yahoo returned no data points in the requested range".to_string(),
                    });
                }

                Ok(TickerSeries {
                    ticker: ticker.clone(),
                    candles,
                    source: Some("yahoo".to_string()),
                    upstream_id: Some(ticker.clone()),
                })
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(series) => out.push(series),
            Err(err) => errors.push(err),
        }
    }

    Ok((out, errors))
}

#[cfg(test)]
mod tests {
    use super::{fx_session_state, yahoo_alias_ticker, yahoo_base_interval};
    use crate::finance::{Span, SpanUnit};
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn yahoo_aliases_cover_discovered_energy_and_vol_tickers() {
        assert_eq!(yahoo_alias_ticker("TTF"), Some("TTF=F"));
        assert_eq!(yahoo_alias_ticker("european gas"), Some("TTF=F"));
        assert_eq!(yahoo_alias_ticker("MOVE"), Some("^MOVE"));
        assert_eq!(yahoo_alias_ticker("DXY"), Some("DX-Y.NYB"));
    }

    #[test]
    fn fx_session_state_is_open_midweek() {
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 15, 0, 0).unwrap();
        assert_eq!(fx_session_state(now), "regular");
    }

    #[test]
    fn fx_session_state_is_closed_before_sunday_open() {
        let now = Utc.with_ymd_and_hms(2026, 3, 15, 18, 0, 0).unwrap();
        assert_eq!(fx_session_state(now), "closed");
    }

    #[test]
    fn intraday_include_prepost_is_disabled_for_equities() {
        let intraday = true;
        let spy = intraday
            && (super::is_crypto_ticker("SPY")
                || super::is_futures_ticker("SPY")
                || super::is_fx_ticker("SPY"));
        let btc = intraday
            && (super::is_crypto_ticker("BTC-USD")
                || super::is_futures_ticker("BTC-USD")
                || super::is_fx_ticker("BTC-USD"));
        let cl = intraday
            && (super::is_crypto_ticker("CL=F")
                || super::is_futures_ticker("CL=F")
                || super::is_fx_ticker("CL=F"));

        assert!(!spy);
        assert!(btc);
        assert!(cl);
    }

    #[test]
    fn yahoo_base_interval_uses_30m_for_offset_risk_2h() {
        let granularity = Span { n: 2, unit: SpanUnit::Hour };
        let (interval, _) =
            yahoo_base_interval(granularity, false, Duration::days(10)).expect("2h base");
        assert_eq!(interval, "30m");
    }

    #[test]
    fn yahoo_base_interval_uses_30m_for_offset_risk_4h() {
        let granularity = Span { n: 4, unit: SpanUnit::Hour };
        let (interval, _) =
            yahoo_base_interval(granularity, false, Duration::days(10)).expect("4h base");
        assert_eq!(interval, "30m");
    }

    #[test]
    fn yahoo_base_interval_uses_1h_for_aligned_3h() {
        let granularity = Span { n: 3, unit: SpanUnit::Hour };
        let (interval, _) =
            yahoo_base_interval(granularity, true, Duration::days(90)).expect("3h base");
        assert_eq!(interval, "1h");
    }

    #[test]
    fn yahoo_base_interval_rejects_long_range_offset_risk_resample() {
        let granularity = Span { n: 3, unit: SpanUnit::Hour };
        let err = yahoo_base_interval(granularity, false, Duration::days(90))
            .expect_err("expected long-range resample rejection");
        assert!(err.contains("last 60 days"));
    }
}

async fn yahoo_fetch_quotes_retry(
    ticker: &str,
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
    interval: &str,
    include_prepost: bool,
) -> Result<Vec<yahoo_finance_api::Quote>> {
    const MAX_ATTEMPTS: usize = 3;
    let mut last_err: Option<String> = None;
    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    for attempt in 0..MAX_ATTEMPTS {

        let resp = if include_prepost {
            connector
                .get_quote_history_interval_prepost(ticker, start, end, interval, true)
                .await
        } else {
            connector
                .get_quote_history_interval(ticker, start, end, interval)
                .await
        };

        match resp {
            Ok(resp) => match resp.quotes() {
                Ok(quotes) if !quotes.is_empty() => return Ok(quotes),
                Ok(_) => last_err = Some("yahoo returned 0 quotes".to_string()),
                Err(e) => last_err = Some(e.to_string()),
            },
            Err(e) => {
                last_err = Some(e.to_string());

                let retryable = matches!(
                    e,
                    yahoo_finance_api::YahooError::ConnectionFailed(_)
                        | yahoo_finance_api::YahooError::FetchFailed(_)
                        | yahoo_finance_api::YahooError::DeserializeFailed(_)
                        | yahoo_finance_api::YahooError::DeserializeFailedDebug(_)
                        | yahoo_finance_api::YahooError::TooManyRequests(_)
                        | yahoo_finance_api::YahooError::Unauthorized
                        | yahoo_finance_api::YahooError::InvalidCrumb
                        | yahoo_finance_api::YahooError::NoCookies
                        | yahoo_finance_api::YahooError::InvalidCookie
                );

                if !retryable {
                    break;
                }
            }
        }

        if attempt + 1 < MAX_ATTEMPTS {
            let backoff_ms = 250u64.saturating_mul((attempt as u64) + 1);
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            // Fresh connector on retry to reset TLS/cookie state.
            if let Ok(fresh) = yahoo_finance_api::YahooConnector::new() {
                connector = fresh;
            }
        }
    }

    Err(Error::Provider(format!(
        "yahoo returned no data for '{ticker}' ({})",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    )))
}

fn yahoo_base_interval(
    granularity: Span,
    aligned_intraday: bool,
    range: Duration,
) -> std::result::Result<(&'static str, Span), String> {
    // Yahoo intraday retention is provider-specific. The key truth constraints are:
    // - Offset-risk tickers (equities/ETFs/indices) need 30m bars for truthful
    //   non-native intraday resamples because 1h and 90m bars can straddle UTC
    //   bucket boundaries, especially across DST.
    // - Aligned tickers (crypto/futures/fx) can safely use 1h for hour-multiple
    //   resamples because their native bars sit on clock boundaries.
    // - Yahoo only serves 30m and 90m history for roughly the last 60 days.
    let requested = granularity.approx_duration().num_seconds().max(1);
    let intraday = requested < 86400;
    let range_seconds = range.num_seconds().max(0);
    let fine_intraday_limit = 60 * 24 * 60 * 60;
    let requested_label = format!("{}s", requested);

    if !intraday {
        let candidates: &[(&str, Span, i64)] = &[
            ("1d", Span { n: 1, unit: SpanUnit::Day }, 86400),
            ("5d", Span { n: 5, unit: SpanUnit::Day }, 432000),
            ("1wk", Span { n: 1, unit: SpanUnit::Week }, 604800),
            ("1mo", Span { n: 1, unit: SpanUnit::Month }, 2592000),
            ("3mo", Span { n: 3, unit: SpanUnit::Month }, 7776000),
        ];
        for &(interval, span, secs) in candidates {
            if secs == requested {
                return Ok((interval, span));
            }
        }
        for &(interval, span, secs) in candidates.iter().rev() {
            if secs <= requested && requested % secs == 0 {
                return Ok((interval, span));
            }
        }
        return Ok(("1d", Span { n: 1, unit: SpanUnit::Day }));
    }

    if requested == 3600 {
        return Ok(("1h", Span { n: 1, unit: SpanUnit::Hour }));
    }

    if !aligned_intraday && requested > 3600 && requested % 1800 == 0 {
        if range_seconds > fine_intraday_limit {
            return Err(format!(
                "truthful yahoo intraday resample for market-hours tickers requires 30m source bars, but Yahoo only serves 30m for the last 60 days; use --range <=60d or choose 1h/1d granularity"
            ));
        }
        return Ok(("30m", Span { n: 30, unit: SpanUnit::Minute }));
    }

    if aligned_intraday && requested > 3600 && requested % 3600 == 0 {
        return Ok(("1h", Span { n: 1, unit: SpanUnit::Hour }));
    }

    if requested % 1800 == 0 {
        if range_seconds > fine_intraday_limit {
            return Err(format!(
                "yahoo {} intraday data is only available for the last 60 days; use --range <=60d or choose a coarser granularity",
                requested_label
            ));
        }
        return Ok(("30m", Span { n: 30, unit: SpanUnit::Minute }));
    }

    let exact_candidates: &[(&str, Span, i64)] = &[
        ("1m", Span { n: 1, unit: SpanUnit::Minute }, 60),
        ("2m", Span { n: 2, unit: SpanUnit::Minute }, 120),
        ("5m", Span { n: 5, unit: SpanUnit::Minute }, 300),
        ("15m", Span { n: 15, unit: SpanUnit::Minute }, 900),
        ("30m", Span { n: 30, unit: SpanUnit::Minute }, 1800),
        ("1h", Span { n: 1, unit: SpanUnit::Hour }, 3600),
    ];
    for &(interval, span, secs) in exact_candidates {
        if secs != requested {
            continue;
        }
        if secs < 3600 && range_seconds > fine_intraday_limit {
            return Err(format!(
                "yahoo {} intraday data is only available for the last 60 days; use --range <=60d or choose a coarser granularity",
                requested_label
            ));
        }
        return Ok((interval, span));
    }

    Err(format!(
        "no truthful yahoo source interval available for {} over this range",
        requested_label
    ))
}
