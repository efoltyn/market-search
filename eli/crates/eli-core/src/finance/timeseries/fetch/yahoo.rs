pub(crate) async fn fetch_yahoo_snapshots(
    tickers: &[String],
    collected_at: DateTime<Utc>,
    freshness_policy: &crate::finance::policy::FreshnessPolicy,
) -> Result<Vec<TickerSnapshot>> {
    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    let mut out = Vec::with_capacity(tickers.len());
    for ticker in tickers {
        let info = connector.get_ticker_info(ticker).await.map_err(|e| {
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

        out.push(TickerSnapshot {
            ticker: ticker.clone(),
            currency,
            exchange,
            short_name,
            long_name,
            current_price,
            previous_close,
            open,
            day_low,
            day_high,
            market_cap,
            enterprise_value,
            shares_outstanding,
            float_shares,
            last_split_factor,
            last_split_date,
            freshness,
            price_source_kind,
            session_state: "unknown".to_string(),
        });
    }

    Ok(out)
}

fn yahoo_alias_ticker(ticker: &str) -> Option<&'static str> {
    match ticker.trim().to_ascii_uppercase().as_str() {
        "DXY" => Some("DX-Y.NYB"),
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
    let (interval, base_span) = yahoo_base_interval(granularity);
    let include_prepost = matches!(base_span.unit, SpanUnit::Minute | SpanUnit::Hour);

    let base_step = base_span.approx_duration();
    let base_step_seconds = base_step.num_seconds();
    if base_step_seconds <= 0 {
        return Err(Error::InvalidInput("invalid base interval".to_string()));
    }

    let approx_points = ((end - start).num_seconds() / base_step_seconds).max(1) as usize + 1;
    if let Some(max_points_per_ticker) = max_points_per_ticker {
        if approx_points > max_points_per_ticker {
            return Err(Error::InvalidInput(format!(
                "requested ~{approx_points} raw points per ticker exceeds limit {max_points_per_ticker}; increase granularity or shrink range"
            )));
        }
    }

    let start_ts = time::OffsetDateTime::from_unix_timestamp(start.timestamp())
        .map_err(|e| Error::Provider(format!("invalid start timestamp: {e}")))?;
    let request_end = end + Duration::days(1);
    let end_ts = time::OffsetDateTime::from_unix_timestamp(request_end.timestamp())
        .map_err(|e| Error::Provider(format!("invalid end timestamp: {e}")))?;

    let requested_step = granularity.approx_duration();
    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();

    for ticker in tickers {
        let request_ticker = yahoo_alias_ticker(ticker).unwrap_or(ticker.as_str());
        let quotes = match yahoo_fetch_quotes_retry(
            request_ticker,
            start_ts,
            end_ts,
            interval,
            include_prepost,
        )
        .await
        {
            Ok(quotes) => quotes,
            Err(err) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: err.to_string(),
                });
                continue;
            }
        };

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
            });
        }

        if let Some(ts) = invalid_timestamp {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: format!("yahoo invalid timestamp: {ts}"),
            });
            continue;
        }

        candles.sort_by_key(|c| c.t);
        let candles = if requested_step == base_step {
            candles
        } else {
            resample_candles(&candles, start, requested_step)
        };

        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: "yahoo returned no data points in the requested range".to_string(),
            });
            continue;
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
        });
    }

    Ok((out, errors))
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

    for attempt in 0..MAX_ATTEMPTS {
        let connector = yahoo_finance_api::YahooConnector::new()
            .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

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
        }
    }

    Err(Error::Provider(format!(
        "yahoo returned no data for '{ticker}' ({})",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    )))
}

fn yahoo_base_interval(granularity: Span) -> (&'static str, Span) {
    // Largest supported interval <= requested granularity.
    // Supported intervals: 1m,2m,5m,15m,30m,90m,1h,1d,5d,1wk,1mo,3mo
    let requested = granularity.approx_duration().num_seconds().max(1);
    let mut best: (&'static str, Span, i64) = (
        "1m",
        Span {
            n: 1,
            unit: SpanUnit::Minute,
        },
        60,
    );

    let candidates: &[(&str, Span)] = &[
        (
            "1m",
            Span {
                n: 1,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "2m",
            Span {
                n: 2,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "5m",
            Span {
                n: 5,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "15m",
            Span {
                n: 15,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "30m",
            Span {
                n: 30,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "90m",
            Span {
                n: 90,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "1h",
            Span {
                n: 1,
                unit: SpanUnit::Hour,
            },
        ),
        (
            "1d",
            Span {
                n: 1,
                unit: SpanUnit::Day,
            },
        ),
        (
            "5d",
            Span {
                n: 5,
                unit: SpanUnit::Day,
            },
        ),
        (
            "1wk",
            Span {
                n: 1,
                unit: SpanUnit::Week,
            },
        ),
        (
            "1mo",
            Span {
                n: 1,
                unit: SpanUnit::Month,
            },
        ),
        (
            "3mo",
            Span {
                n: 3,
                unit: SpanUnit::Month,
            },
        ),
    ];

    for (interval, span) in candidates {
        let secs = span.approx_duration().num_seconds();
        if secs <= 0 || secs > requested {
            continue;
        }
        if secs >= best.2 {
            best = (*interval, *span, secs);
        }
    }

    (best.0, best.1)
}
