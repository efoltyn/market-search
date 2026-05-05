/// Pyth Benchmarks TradingView shim — 24/7 OHLC candles for crypto, commodities, FX, metals.
/// Endpoint: GET https://benchmarks.pyth.network/v1/shims/tradingview/history
/// Free, no auth, no rate limit (public Pyth Data Association instance).

/// Map a Pyth symbol query (user-friendly name) to the canonical Pyth feed symbol.
/// Returns None if the query doesn't map to a known Pyth feed.
pub(crate) fn resolve_pyth_symbol(query: &str) -> Option<&'static str> {
    let q = query.trim().to_ascii_lowercase();
    match q.as_str() {
        // Commodities
        "oil" | "wti" | "crude" | "crude oil" | "cl" | "usoilspot" => {
            Some("Commodities.USOILSPOT")
        }
        "brent" | "brent oil" | "bz" | "ukoilspot" => Some("Commodities.UKOILSPOT"),

        // Metals
        "gold" | "xau" | "gc" => Some("Metal.XAU/USD"),
        "silver" | "xag" | "si" => Some("Metal.XAG/USD"),

        // Crypto
        "btc" | "bitcoin" => Some("Crypto.BTC/USD"),
        "eth" | "ethereum" | "ether" => Some("Crypto.ETH/USD"),
        "sol" | "solana" => Some("Crypto.SOL/USD"),

        // FX
        "eurusd" | "eur/usd" => Some("FX.EUR/USD"),
        "usdjpy" | "usd/jpy" => Some("FX.USD/JPY"),
        "dxy" | "usdxy" | "dollar" => Some("FX.USDXY"),

        // If it already looks like a Pyth symbol (contains a dot), pass through
        _ if q.contains('.') || q.contains('/') => None, // caller should use as-is
        _ => None,
    }
}

/// Check if a ticker string should be routed to Pyth.
/// Tickers with "PYTH:" prefix are always Pyth. Also matches known commodity/crypto names
/// that Pyth covers 24/7 but Yahoo doesn't (or Yahoo has gaps on weekends).
pub fn is_pyth_ticker(ticker: &str) -> bool {
    ticker.trim().starts_with("PYTH:")
}

/// Strip the "PYTH:" prefix and resolve to a Pyth symbol.
pub(crate) fn parse_pyth_ticker(ticker: &str) -> String {
    let raw = ticker.trim().strip_prefix("PYTH:").unwrap_or(ticker).trim();
    // Try canonical mapping first, fall back to raw (user may pass "Commodities.USOILSPOT" directly)
    resolve_pyth_symbol(raw)
        .map(|s| s.to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// Map our Span granularity to a Pyth TradingView resolution string.
/// Supported: 1, 2, 5, 15, 30, 60, 120, 240, 360, 720, D, W, M
fn pyth_resolution(granularity: Span) -> &'static str {
    let secs = granularity.approx_duration().num_seconds();
    if secs <= 60 {
        "1" // 1 minute
    } else if secs <= 120 {
        "2"
    } else if secs <= 300 {
        "5"
    } else if secs <= 900 {
        "15"
    } else if secs <= 1800 {
        "30"
    } else if secs <= 3600 {
        "60" // 1 hour
    } else if secs <= 7200 {
        "120"
    } else if secs <= 14400 {
        "240"
    } else if secs <= 21600 {
        "360"
    } else if secs <= 43200 {
        "720"
    } else if secs <= 86400 {
        "D" // daily
    } else if secs <= 604800 {
        "W" // weekly
    } else {
        "M" // monthly
    }
}

pub(crate) async fn fetch_pyth_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let resolution = pyth_resolution(granularity);

    let client = &*crate::finance::shared_client::GENERAL;

    let from_ts = start.timestamp();
    let to_ts = end.timestamp();

    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();

    for ticker in tickers {
        let pyth_symbol = parse_pyth_ticker(ticker);

        // PYTH:OIL is a multi-source aggregate, not CME WTI. It typically
        // tracks ~$3 below the CME WTI front-month due to basket composition.
        // Attach a non-fatal warning so callers don't silently substitute it
        // for WTI in commentary or charts.
        if ticker.trim().eq_ignore_ascii_case("PYTH:OIL") {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("data_quality".to_string()),
                message: "PYTH:OIL is a multi-source aggregate, not CME WTI \
                    \u{2014} typically ~$3 below CL=F due to basket composition. \
                    Use CL=F for WTI front month."
                    .to_string(),
            });
        }

        let url = format!(
            "https://benchmarks.pyth.network/v1/shims/tradingview/history?symbol={}&resolution={}&from={}&to={}",
            urlencoding::encode(&pyth_symbol),
            resolution,
            from_ts,
            to_ts,
        );

        let start_time = std::time::Instant::now();
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!("pyth fetch failed: {e}"),
                });
                continue;
            }
        };

        let status = resp.status();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("read".to_string()),
                    message: format!("pyth body read failed: {e}"),
                });
                continue;
            }
        };

        info!(
            target: "eli.finance.timeseries.pyth",
            url = %url,
            status = %status,
            bytes = body.len(),
            elapsed_ms = start_time.elapsed().as_millis(),
            "pyth history fetch"
        );

        if !status.is_success() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!("pyth http {}: {}", status, body.chars().take(200).collect::<String>()),
            });
            continue;
        }

        // Parse the TradingView-style response: { s, t[], o[], h[], l[], c[], v[] }
        #[derive(Deserialize)]
        struct TvHistory {
            s: String,
            #[serde(default)]
            t: Vec<i64>,
            #[serde(default)]
            o: Vec<f64>,
            #[serde(default)]
            h: Vec<f64>,
            #[serde(default)]
            l: Vec<f64>,
            #[serde(default)]
            c: Vec<f64>,
            #[serde(default)]
            v: Vec<f64>,
            #[serde(default)]
            errmsg: Option<String>,
        }

        let tv: TvHistory = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(e) => {
                let debug_path = write_debug_payload("pyth_history", &url, &body);
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("parse".to_string()),
                    message: format!("pyth parse failed: {e}"),
                });
                continue;
            }
        };

        if tv.s == "error" {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("api".to_string()),
                message: format!(
                    "pyth API error: {}",
                    tv.errmsg.unwrap_or_else(|| "unknown".to_string())
                ),
            });
            continue;
        }

        if tv.t.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: "pyth returned 0 candles for this symbol/range".to_string(),
            });
            continue;
        }

        let n = tv.t.len();
        let mut candles = Vec::with_capacity(n);
        for i in 0..n {
            let Some(t) = Utc.timestamp_opt(tv.t[i], 0).single() else {
                continue;
            };
            candles.push(Candle {
                t,
                o: *tv.o.get(i).unwrap_or(&0.0),
                h: *tv.h.get(i).unwrap_or(&0.0),
                l: *tv.l.get(i).unwrap_or(&0.0),
                c: *tv.c.get(i).unwrap_or(&0.0),
                v: tv.v.get(i).copied().filter(|&v| v > 0.0),
                kind: None,
            });
        }

        candles.sort_by_key(|c| c.t);

        // Use the original ticker (with PYTH: prefix) as the series label
        // so downstream analytics can distinguish it
        let upstream = ticker
            .strip_prefix("PYTH:")
            .or_else(|| ticker.strip_prefix("pyth:"))
            .unwrap_or(ticker)
            .to_string();
        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("pyth".to_string()),
            upstream_id: Some(upstream),
        });
    }

    Ok((out, errors))
}
