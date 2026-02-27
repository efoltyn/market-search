use std::collections::{BTreeMap, HashMap};

const KALSHI_CANDLESTICKS_URL: &str =
    "https://api.elections.kalshi.com/trade-api/v2/markets/candlesticks";
const POLYMARKET_GAMMA_URL: &str = "https://gamma-api.polymarket.com";
const POLYMARKET_CLOB_HISTORY_URL: &str = "https://clob.polymarket.com/prices-history";

async fn cmd_finance_timeseries(args: FinanceTimeseriesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let odds_pair = parse_optional_odds_pair_request(
        args.odds_provider.as_deref(),
        args.odds_market.as_deref(),
        &args.odds_side,
    )?;

    let mut tickers = args.tickers;
    if let Some(path) = args.tickers_file {
        let raw = std::fs::read_to_string(&path).context("read tickers_file")?;
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            tickers.push(t.to_string());
        }
    }

    let mut range = eli_core::finance::Span::parse(&args.range)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --range")?;
    let granularity = eli_core::finance::Span::parse(&args.granularity)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --granularity")?;

    let mut as_of = match args.as_of.as_ref() {
        Some(raw) => Some(
            eli_core::finance::parse_as_of(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --as-of")?,
        ),
        None => None,
    };

    match (args.start.as_deref(), args.end.as_deref()) {
        (Some(start_raw), Some(end_raw)) => {
            if args.as_of.is_some() {
                anyhow::bail!("--as-of cannot be combined with --start/--end");
            }
            let start_dt = parse_window_start(start_raw).context("parse --start")?;
            let end_dt = eli_core::finance::parse_as_of(end_raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --end")?;
            if end_dt <= start_dt {
                anyhow::bail!("--end must be strictly after --start");
            }
            let delta = end_dt.signed_duration_since(start_dt);
            let minutes = ((delta.num_seconds() + 59) / 60).max(1);
            range = eli_core::finance::Span::parse(&format!("{minutes}min"))
                .map_err(|e| anyhow::anyhow!(e))
                .context("derive range from --start/--end")?;
            as_of = Some(end_dt);
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--start and --end must be used together");
        }
        (None, None) => {}
    }

    let provider_str = args.provider.trim().to_ascii_lowercase();
    let is_auto = provider_str == "auto";
    let provider = match provider_str.as_str() {
        "auto" | "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "mock" => eli_core::finance::ProviderKind::Mock,
        "fred" => eli_core::finance::ProviderKind::Fred,
        other => {
            anyhow::bail!("unsupported --provider '{other}' (supported: auto, mock, yahoo, fred)")
        }
    };

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    let req = eli_core::finance::TimeseriesRequest {
        tickers: tickers.clone(),
        range,
        granularity,
        as_of,
        provider,
        max_points_per_ticker: args.max_points_per_ticker,
    };

    let mut resp = eli_core::finance::fetch_timeseries(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch timeseries")?;

    // Auto-fallback: if in auto mode and Yahoo returned errors, retry failed tickers with FRED.
    // Also re-fetch valid Yahoo tickers individually so their data isn't lost (the core drops
    // all series when any ticker fails).
    let auto_fallback_needed = is_auto
        && (resp.series.is_empty()
            || resp.status.as_deref() == Some("error")
            || resp.errors.as_ref().map(|e| !e.is_empty()).unwrap_or(false));
    if auto_fallback_needed {
        let failed_tickers: Vec<String> = resp
            .errors
            .as_ref()
            .map(|errs| errs.iter().map(|e| e.ticker.clone()).collect())
            .unwrap_or_default();
        let valid_tickers: Vec<String> = resp.valid_tickers.clone().unwrap_or_default();

        let mut merged_series = Vec::new();
        let mut remaining_errors = Vec::new();

        // Re-fetch valid Yahoo tickers (core dropped their data on partial failure)
        for t in &valid_tickers {
            let re_req = eli_core::finance::TimeseriesRequest {
                tickers: vec![t.clone()],
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: args.max_points_per_ticker,
            };
            if let Ok(re_resp) = eli_core::finance::fetch_timeseries(re_req, &cache_dir).await {
                merged_series.extend(re_resp.series);
            }
        }

        // Retry failed tickers with FRED
        if !failed_tickers.is_empty() {
            let fred_req = eli_core::finance::TimeseriesRequest {
                tickers: failed_tickers.clone(),
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Fred,
                max_points_per_ticker: args.max_points_per_ticker,
            };
            match eli_core::finance::fetch_timeseries(fred_req, &cache_dir).await {
                Ok(fred_resp) => {
                    let fred_ok: std::collections::HashSet<String> =
                        fred_resp.series.iter().map(|s| s.ticker.clone()).collect();
                    merged_series.extend(fred_resp.series);
                    // Collect tickers that failed both Yahoo AND FRED
                    for t in &failed_tickers {
                        if !fred_ok.contains(t) {
                            remaining_errors.push(eli_core::finance::TimeseriesError {
                                ticker: t.clone(),
                                stage: Some("auto-fallback".to_string()),
                                message: "failed on both Yahoo and FRED".to_string(),
                            });
                        }
                    }
                }
                Err(_) => {
                    // FRED also failed entirely — keep original errors
                    if let Some(errs) = &resp.errors {
                        remaining_errors.extend(errs.iter().cloned());
                    }
                }
            }
        }

        if !merged_series.is_empty() {
            // If all data came from FRED (no Yahoo successes), label provider as fred;
            // if mixed, label as yahoo (primary) — the data speaks for itself.
            if valid_tickers.is_empty() {
                resp.provider = eli_core::finance::ProviderKind::Fred;
            }
            resp.series = merged_series;
            resp.status = if remaining_errors.is_empty() {
                None
            } else {
                Some("partial".to_string())
            };
            resp.error = None;
            resp.errors = if remaining_errors.is_empty() {
                None
            } else {
                Some(remaining_errors)
            };
            resp.valid_tickers = None;
        }
    }

    if let Some(pair_req) = odds_pair {
        let odds_series =
            fetch_prediction_market_series(&pair_req, resp.start, resp.end, granularity).await?;
        let paired = build_paired_timeseries_response(resp, odds_series, granularity);

        if let Some(out_path) = args.out {
            let wr = write_json_out_with_meta(
                out_path,
                &paired,
                "finance.timeseries",
                &[
                    format!("range={}", args.range),
                    format!("granularity={}", args.granularity),
                    format!("odds_provider={}", pair_req.provider.as_str()),
                    format!("odds_market={}", pair_req.market),
                    format!("odds_side={}", pair_req.side.as_str()),
                ],
            )?;
            println!(
                "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"cache\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&paired.base.cache).unwrap_or_else(|_| "null".to_string())
            );
            return Ok(());
        }

        let json = serde_json::to_string_pretty(&paired).context("serialize paired response")?;
        println!("{json}");
        return Ok(());
    }

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.timeseries",
            &[
                format!("range={}", args.range),
                format!("granularity={}", args.granularity),
            ],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"cache\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&resp.cache).unwrap_or_else(|_| "null".to_string())
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn parse_window_start(raw: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("empty start value");
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("invalid start '{raw}' (use YYYY-MM-DD or RFC3339)"))?;
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid start date '{raw}'"))?;
    Ok(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        naive,
        chrono::Utc,
    ))
}

#[derive(Clone, Copy, Debug)]
enum OddsPairProvider {
    Kalshi,
    Polymarket,
}

impl OddsPairProvider {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Kalshi => "kalshi",
            Self::Polymarket => "polymarket",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum OddsPairSide {
    Yes,
    No,
}

impl OddsPairSide {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
        }
    }

    fn apply(self, yes_probability: f64) -> f64 {
        let p = yes_probability.clamp(0.0, 1.0);
        match self {
            Self::Yes => p,
            Self::No => (1.0 - p).clamp(0.0, 1.0),
        }
    }
}

#[derive(Clone, Debug)]
struct OddsPairRequest {
    provider: OddsPairProvider,
    market: String,
    side: OddsPairSide,
}

fn parse_optional_odds_pair_request(
    provider: Option<&str>,
    market: Option<&str>,
    side: &str,
) -> Result<Option<OddsPairRequest>> {
    let provider = provider.map(str::trim).filter(|v| !v.is_empty());
    let market = market.map(str::trim).filter(|v| !v.is_empty());

    if provider.is_none() && market.is_none() {
        return Ok(None);
    }
    if provider.is_none() || market.is_none() {
        anyhow::bail!("--odds-provider and --odds-market must be provided together");
    }

    let provider = match provider.unwrap_or_default().to_ascii_lowercase().as_str() {
        "kalshi" => OddsPairProvider::Kalshi,
        "polymarket" => OddsPairProvider::Polymarket,
        other => {
            anyhow::bail!("unsupported --odds-provider '{other}' (supported: kalshi, polymarket)")
        }
    };

    let side = match side.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" => OddsPairSide::Yes,
        "no" | "n" => OddsPairSide::No,
        other => anyhow::bail!("unsupported --odds-side '{other}' (supported: yes, no)"),
    };

    Ok(Some(OddsPairRequest {
        provider,
        market: market.unwrap_or_default().to_string(),
        side,
    }))
}

#[derive(Clone, Debug)]
struct OddsMarketSeries {
    provider: String,
    market: String,
    side: String,
    side_label: Option<String>,
    token_id: Option<String>,
    series: eli_core::finance::TickerSeries,
}

async fn fetch_prediction_market_series(
    req: &OddsPairRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<OddsMarketSeries> {
    match req.provider {
        OddsPairProvider::Kalshi => fetch_kalshi_market_series(req, start, end, granularity).await,
        OddsPairProvider::Polymarket => {
            fetch_polymarket_market_series(req, start, end, granularity).await
        }
    }
}

fn granularity_seconds(span: eli_core::finance::Span) -> i64 {
    span.approx_duration().num_seconds().max(60)
}

fn granularity_minutes(span: eli_core::finance::Span) -> i64 {
    ((granularity_seconds(span) + 59) / 60).max(1)
}

async fn fetch_kalshi_market_series(
    req: &OddsPairRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<OddsMarketSeries> {
    #[derive(serde::Deserialize)]
    struct KalshiResp {
        #[serde(default)]
        markets: Vec<KalshiMarketCandles>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiMarketCandles {
        market_ticker: String,
        #[serde(default)]
        candlesticks: Vec<KalshiCandle>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiBidAsk {
        #[serde(default)]
        close_dollars: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiPrice {
        #[serde(default)]
        open_dollars: Option<String>,
        #[serde(default)]
        low_dollars: Option<String>,
        #[serde(default)]
        high_dollars: Option<String>,
        #[serde(default)]
        close_dollars: Option<String>,
        #[serde(default)]
        previous_dollars: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiCandle {
        end_period_ts: i64,
        #[serde(default)]
        price: Option<KalshiPrice>,
        #[serde(default)]
        yes_bid: Option<KalshiBidAsk>,
        #[serde(default)]
        yes_ask: Option<KalshiBidAsk>,
        #[serde(default)]
        volume: Option<i64>,
    }

    let interval_minutes = granularity_minutes(granularity);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init kalshi candlestick client")?;

    let start_ts = start.timestamp();
    let end_ts = end.timestamp();
    let start_ts_s = start_ts.to_string();
    let end_ts_s = end_ts.to_string();
    let interval_s = interval_minutes.to_string();

    let resp = client
        .get(KALSHI_CANDLESTICKS_URL)
        .query(&[
            ("market_tickers", req.market.as_str()),
            ("start_ts", start_ts_s.as_str()),
            ("end_ts", end_ts_s.as_str()),
            ("period_interval", interval_s.as_str()),
        ])
        .send()
        .await
        .context("request kalshi candlesticks")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "kalshi candlesticks failed for market {}: http {}: {}",
            req.market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let parsed: KalshiResp = resp
        .json()
        .await
        .context("parse kalshi candlesticks response")?;
    let market = parsed.markets.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!("kalshi returned no candlesticks for market {}", req.market)
    })?;

    let mut candles: Vec<eli_core::finance::Candle> = Vec::new();
    for kc in market.candlesticks {
        let yes_close = kc
            .price
            .as_ref()
            .and_then(|p| p.close_dollars.as_deref())
            .and_then(parse_decimal)
            .or_else(|| {
                match (
                    kc.yes_bid
                        .as_ref()
                        .and_then(|v| v.close_dollars.as_deref())
                        .and_then(parse_decimal),
                    kc.yes_ask
                        .as_ref()
                        .and_then(|v| v.close_dollars.as_deref())
                        .and_then(parse_decimal),
                ) {
                    (Some(bid), Some(ask)) => Some((bid + ask) * 0.5),
                    (Some(v), None) | (None, Some(v)) => Some(v),
                    (None, None) => None,
                }
            })
            .or_else(|| {
                kc.price
                    .as_ref()
                    .and_then(|p| p.previous_dollars.as_deref())
                    .and_then(parse_decimal)
            });

        let Some(yes_close) = yes_close else {
            continue;
        };

        let yes_open = kc
            .price
            .as_ref()
            .and_then(|p| p.open_dollars.as_deref())
            .and_then(parse_decimal)
            .unwrap_or(yes_close);
        let yes_high = kc
            .price
            .as_ref()
            .and_then(|p| p.high_dollars.as_deref())
            .and_then(parse_decimal)
            .unwrap_or(yes_close);
        let yes_low = kc
            .price
            .as_ref()
            .and_then(|p| p.low_dollars.as_deref())
            .and_then(parse_decimal)
            .unwrap_or(yes_close);

        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(kc.end_period_ts, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid kalshi candlestick timestamp"))?;

        candles.push(eli_core::finance::Candle {
            t: ts,
            o: req.side.apply(yes_open),
            h: req.side.apply(yes_high),
            l: req.side.apply(yes_low),
            c: req.side.apply(yes_close),
            v: kc.volume.map(|v| v as f64),
        });
    }

    if candles.is_empty() {
        anyhow::bail!(
            "no usable kalshi candlesticks found for market {} in requested window",
            req.market
        );
    }

    candles.sort_by_key(|c| c.t);

    Ok(OddsMarketSeries {
        provider: "kalshi".to_string(),
        market: market.market_ticker.clone(),
        side: req.side.as_str().to_string(),
        side_label: Some(req.side.as_str().to_ascii_uppercase()),
        token_id: None,
        series: eli_core::finance::TickerSeries {
            ticker: format!(
                "KALSHI:{}:{}",
                market.market_ticker,
                req.side.as_str().to_ascii_uppercase()
            ),
            candles,
        },
    })
}

async fn fetch_polymarket_market_series(
    req: &OddsPairRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<OddsMarketSeries> {
    #[derive(serde::Deserialize)]
    struct PolyHistoryPoint {
        t: serde_json::Value,
        p: serde_json::Value,
    }

    #[derive(serde::Deserialize)]
    struct PolyHistoryResp {
        #[serde(default)]
        history: Vec<PolyHistoryPoint>,
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init polymarket history client")?;

    let market_meta = fetch_polymarket_market_meta(&client, &req.market).await?;
    let market_id = market_meta
        .get("id")
        .map(json_value_to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| req.market.clone());

    let outcomes = market_meta
        .get("outcomes")
        .map(parse_jsonish_string_list)
        .unwrap_or_default();
    let token_ids = market_meta
        .get("clobTokenIds")
        .map(parse_jsonish_string_list)
        .unwrap_or_default();

    if token_ids.is_empty() {
        anyhow::bail!("polymarket market {} has no clob token IDs", req.market);
    }

    let outcome_index = pick_polymarket_outcome_index(&outcomes, req.side, token_ids.len());
    let token_id = token_ids
        .get(outcome_index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("invalid polymarket outcome index for {}", req.market))?;
    let side_label = outcomes.get(outcome_index).cloned();

    let fidelity = granularity_minutes(granularity).max(1);
    let fidelity_s = fidelity.to_string();

    let resp = client
        .get(POLYMARKET_CLOB_HISTORY_URL)
        .query(&[
            ("market", token_id.as_str()),
            ("interval", "max"),
            ("fidelity", fidelity_s.as_str()),
        ])
        .send()
        .await
        .context("request polymarket price history")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket price history failed for market {} (token {}): http {}: {}",
            req.market,
            token_id,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let parsed: PolyHistoryResp = resp
        .json()
        .await
        .context("parse polymarket price history response")?;

    let step_seconds = granularity_seconds(granularity).max(1);
    let start_ts = start.timestamp();
    let end_ts = end.timestamp();

    #[derive(Clone, Copy)]
    struct Ohlc {
        o: f64,
        h: f64,
        l: f64,
        c: f64,
        first_ts: i64,
        last_ts: i64,
        points: usize,
    }

    let mut buckets: BTreeMap<i64, Ohlc> = BTreeMap::new();

    for point in parsed.history {
        let ts = parse_json_number_i64(&point.t).unwrap_or_default();
        let price = parse_json_number_f64(&point.p).unwrap_or_default();
        if ts <= 0 || !price.is_finite() {
            continue;
        }
        if ts < start_ts || ts > end_ts {
            continue;
        }

        let bucket = ts.div_euclid(step_seconds) * step_seconds;
        let adj_price = req.side.apply(price);

        match buckets.get_mut(&bucket) {
            Some(entry) => {
                if ts < entry.first_ts {
                    entry.first_ts = ts;
                    entry.o = adj_price;
                }
                if ts >= entry.last_ts {
                    entry.last_ts = ts;
                    entry.c = adj_price;
                }
                if adj_price > entry.h {
                    entry.h = adj_price;
                }
                if adj_price < entry.l {
                    entry.l = adj_price;
                }
                entry.points += 1;
            }
            None => {
                buckets.insert(
                    bucket,
                    Ohlc {
                        o: adj_price,
                        h: adj_price,
                        l: adj_price,
                        c: adj_price,
                        first_ts: ts,
                        last_ts: ts,
                        points: 1,
                    },
                );
            }
        }
    }

    if buckets.is_empty() {
        anyhow::bail!(
            "no polymarket price history points found for market {} in requested window",
            req.market
        );
    }

    let mut candles = Vec::with_capacity(buckets.len());
    for (bucket, ohlc) in buckets {
        let end_bucket = bucket.saturating_add(step_seconds);
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(end_bucket, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid polymarket bucket timestamp"))?;
        candles.push(eli_core::finance::Candle {
            t: ts,
            o: ohlc.o,
            h: ohlc.h,
            l: ohlc.l,
            c: ohlc.c,
            v: None,
        });
    }

    Ok(OddsMarketSeries {
        provider: "polymarket".to_string(),
        market: market_id.clone(),
        side: req.side.as_str().to_string(),
        side_label,
        token_id: Some(token_id),
        series: eli_core::finance::TickerSeries {
            ticker: format!(
                "POLYMARKET:{market_id}:{}",
                req.side.as_str().to_ascii_uppercase()
            ),
            candles,
        },
    })
}

async fn fetch_polymarket_market_meta(
    client: &reqwest::Client,
    market: &str,
) -> Result<serde_json::Value> {
    let direct_url = format!("{POLYMARKET_GAMMA_URL}/markets/{market}");
    let direct = client
        .get(&direct_url)
        .send()
        .await
        .context("request polymarket market metadata")?;

    if direct.status().is_success() {
        let value: serde_json::Value = direct
            .json()
            .await
            .context("parse polymarket market metadata")?;
        if value.is_object() {
            return Ok(value);
        }
    } else if direct.status() != reqwest::StatusCode::NOT_FOUND {
        let status = direct.status();
        let body = direct.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket market metadata failed for {}: http {}: {}",
            market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let fallback = client
        .get(format!("{POLYMARKET_GAMMA_URL}/markets"))
        .query(&[("slug", market), ("limit", "1")])
        .send()
        .await
        .context("request polymarket market metadata by slug")?;

    if !fallback.status().is_success() {
        let status = fallback.status();
        let body = fallback.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket market metadata (slug) failed for {}: http {}: {}",
            market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let value: serde_json::Value = fallback
        .json()
        .await
        .context("parse polymarket market metadata by slug")?;

    let Some(first) = value.as_array().and_then(|items| items.first()).cloned() else {
        anyhow::bail!("polymarket market '{}' not found by id or slug", market);
    };

    Ok(first)
}

fn parse_decimal(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn parse_jsonish_string_list(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(json_value_to_string)
            .filter(|s| !s.trim().is_empty())
            .collect(),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Vec::new();
            }
            if trimmed.starts_with('[') {
                if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
                    return parsed
                        .iter()
                        .map(json_value_to_string)
                        .filter(|v| !v.trim().is_empty())
                        .collect();
                }
            }
            vec![trimmed.to_string()]
        }
        other => vec![json_value_to_string(other)],
    }
}

fn json_value_to_string(value: &serde_json::Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(i) = value.as_i64() {
        return i.to_string();
    }
    if let Some(u) = value.as_u64() {
        return u.to_string();
    }
    if let Some(f) = value.as_f64() {
        return f.to_string();
    }
    value.to_string()
}

fn parse_json_number_i64(value: &serde_json::Value) -> Option<i64> {
    if let Some(v) = value.as_i64() {
        return Some(v);
    }
    if let Some(v) = value.as_u64() {
        return i64::try_from(v).ok();
    }
    value.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

fn parse_json_number_f64(value: &serde_json::Value) -> Option<f64> {
    if let Some(v) = value.as_f64() {
        return Some(v);
    }
    if let Some(v) = value.as_i64() {
        return Some(v as f64);
    }
    if let Some(v) = value.as_u64() {
        return Some(v as f64);
    }
    value.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

fn pick_polymarket_outcome_index(
    outcomes: &[String],
    side: OddsPairSide,
    token_count: usize,
) -> usize {
    if token_count <= 1 {
        return 0;
    }

    let target = match side {
        OddsPairSide::Yes => "yes",
        OddsPairSide::No => "no",
    };

    if let Some(idx) = outcomes
        .iter()
        .position(|o| o.trim().eq_ignore_ascii_case(target))
    {
        return idx.min(token_count - 1);
    }

    match side {
        OddsPairSide::Yes => 0,
        OddsPairSide::No => 1.min(token_count - 1),
    }
}

#[derive(serde::Serialize)]
struct PairedTimeseriesResponse {
    mode: &'static str,
    generated_at: chrono::DateTime<chrono::Utc>,
    base: eli_core::finance::TimeseriesResponse,
    odds: PairedOddsLeg,
    analytics: PairedTimeseriesAnalytics,
}

#[derive(serde::Serialize)]
struct PairedOddsLeg {
    provider: String,
    market: String,
    side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    side_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_id: Option<String>,
    points: usize,
    series: eli_core::finance::TickerSeries,
}

#[derive(serde::Serialize)]
struct PairedTimeseriesAnalytics {
    granularity: String,
    ticker_pairs: Vec<PairedTickerAnalytics>,
}

#[derive(serde::Serialize)]
struct PairedTickerAnalytics {
    ticker: String,
    overlap_points: usize,
    overlap_returns: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    return_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    odds_leads_1_bar_return_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_leads_1_bar_return_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_total_return: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    odds_total_return: Option<f64>,
}

fn build_paired_timeseries_response(
    base: eli_core::finance::TimeseriesResponse,
    odds: OddsMarketSeries,
    granularity: eli_core::finance::Span,
) -> PairedTimeseriesResponse {
    let analytics = build_paired_analytics(&base.series, &odds.series, granularity);
    let points = odds.series.candles.len();

    PairedTimeseriesResponse {
        mode: "paired_prediction_market",
        generated_at: chrono::Utc::now(),
        base,
        odds: PairedOddsLeg {
            provider: odds.provider,
            market: odds.market,
            side: odds.side,
            side_label: odds.side_label,
            token_id: odds.token_id,
            points,
            series: odds.series,
        },
        analytics,
    }
}

fn build_paired_analytics(
    base_series: &[eli_core::finance::TickerSeries],
    odds_series: &eli_core::finance::TickerSeries,
    granularity: eli_core::finance::Span,
) -> PairedTimeseriesAnalytics {
    let step_seconds = granularity_seconds(granularity).max(1);

    let odds_bucketed = bucketed_closes(&odds_series.candles, step_seconds);
    let mut pairs = Vec::new();

    for series in base_series {
        let base_bucketed = bucketed_closes(&series.candles, step_seconds);
        let mut keys: Vec<i64> = base_bucketed
            .keys()
            .filter(|k| odds_bucketed.contains_key(k))
            .copied()
            .collect();
        keys.sort_unstable();

        let mut base_closes = Vec::with_capacity(keys.len());
        let mut odds_closes = Vec::with_capacity(keys.len());

        for key in &keys {
            if let (Some(b), Some(o)) = (base_bucketed.get(key), odds_bucketed.get(key)) {
                base_closes.push(*b);
                odds_closes.push(*o);
            }
        }

        let base_returns = simple_returns(&base_closes);
        let odds_returns = simple_returns(&odds_closes);

        let return_corr = pearson(&base_returns, &odds_returns);
        let close_corr = pearson(&base_closes, &odds_closes);

        let (odds_leads, base_leads) = lagged_return_corrs(&base_returns, &odds_returns);

        pairs.push(PairedTickerAnalytics {
            ticker: series.ticker.clone(),
            overlap_points: base_closes.len(),
            overlap_returns: base_returns.len().min(odds_returns.len()),
            close_correlation: close_corr,
            return_correlation: return_corr,
            odds_leads_1_bar_return_correlation: odds_leads,
            base_leads_1_bar_return_correlation: base_leads,
            base_total_return: total_return(&base_closes),
            odds_total_return: total_return(&odds_closes),
        });
    }

    PairedTimeseriesAnalytics {
        granularity: granularity.to_string_compact(),
        ticker_pairs: pairs,
    }
}

fn bucketed_closes(candles: &[eli_core::finance::Candle], step_seconds: i64) -> HashMap<i64, f64> {
    let mut sorted = candles.to_vec();
    sorted.sort_by_key(|c| c.t);

    let mut out = HashMap::new();
    for c in sorted {
        let ts = c.t.timestamp();
        let bucket = ts.div_euclid(step_seconds) * step_seconds;
        out.insert(bucket, c.c);
    }
    out
}

fn simple_returns(closes: &[f64]) -> Vec<f64> {
    if closes.len() < 2 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(closes.len() - 1);
    for w in closes.windows(2) {
        let prev = w[0];
        let curr = w[1];
        if prev == 0.0 {
            continue;
        }
        out.push((curr / prev) - 1.0);
    }
    out
}

fn total_return(closes: &[f64]) -> Option<f64> {
    if closes.len() < 2 {
        return None;
    }
    let first = closes.first().copied()?;
    let last = closes.last().copied()?;
    if first == 0.0 {
        return None;
    }
    Some((last / first) - 1.0)
}

fn lagged_return_corrs(base_returns: &[f64], odds_returns: &[f64]) -> (Option<f64>, Option<f64>) {
    if base_returns.len() < 2 || odds_returns.len() < 2 {
        return (None, None);
    }

    // Odds leads by 1 bar: corr(base[t], odds[t-1]).
    let lag_n = base_returns.len().min(odds_returns.len());
    if lag_n < 2 {
        return (None, None);
    }

    let mut base_now = Vec::new();
    let mut odds_prev = Vec::new();
    for idx in 1..lag_n {
        base_now.push(base_returns[idx]);
        odds_prev.push(odds_returns[idx - 1]);
    }

    let mut base_prev = Vec::new();
    let mut odds_now = Vec::new();
    for idx in 1..lag_n {
        base_prev.push(base_returns[idx - 1]);
        odds_now.push(odds_returns[idx]);
    }

    (
        pearson(&base_now, &odds_prev),
        pearson(&base_prev, &odds_now),
    )
}

fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 2 || ys.len() < 2 || xs.len() != ys.len() {
        return None;
    }

    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    let mean_y = ys.iter().sum::<f64>() / ys.len() as f64;

    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;

    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = *x - mean_x;
        let dy = *y - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }

    if var_x <= f64::EPSILON || var_y <= f64::EPSILON {
        return None;
    }

    Some(cov / (var_x.sqrt() * var_y.sqrt()))
}

#[cfg(test)]
mod timeseries_tests {
    use super::*;

    #[test]
    fn odds_pair_requires_provider_and_market_together() {
        let err = parse_optional_odds_pair_request(Some("kalshi"), None, "yes")
            .expect_err("provider without market should fail");
        assert!(err.to_string().contains("must be provided together"));
    }

    #[test]
    fn picks_yes_no_outcome_index_when_available() {
        let outcomes = vec!["No".to_string(), "Yes".to_string()];
        let yes_idx = pick_polymarket_outcome_index(&outcomes, OddsPairSide::Yes, 2);
        let no_idx = pick_polymarket_outcome_index(&outcomes, OddsPairSide::No, 2);
        assert_eq!(yes_idx, 1);
        assert_eq!(no_idx, 0);
    }

    #[test]
    fn return_corr_is_positive_for_parallel_series() {
        let xs = vec![0.01, 0.02, 0.03, 0.04];
        let ys = vec![0.02, 0.03, 0.04, 0.05];
        let corr = pearson(&xs, &ys).expect("corr should exist");
        assert!(corr > 0.9);
    }
}
