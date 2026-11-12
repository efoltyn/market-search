/// Binance spot klines (candlesticks), read from two public venues.
///
/// - Binance (the global exchange), through its public market-data mirror
///   https://data-api.binance.vision/api/v3/klines. No key, not geo-blocked.
///   History back to 2017. This is where the USDT, USDC, FDUSD, BTC and ETH
///   quoted pairs actually trade.
/// - Binance.US, https://api.binance.us/api/v3/klines. No key. History back to
///   September 2019. The only one of the two with real plain-USD pairs, and a
///   thin venue: over two hours of one-minute bars on 2026-11-12 its BTCUSD
///   pair printed no trade in 55 of 121 bars, while BTCUSDT on the global
///   venue traded in every one.
///
/// Routing (see `binance_routes`):
///   BN:BTCUSDT, BN:ETHBTC, BN:BTC-FDUSD   global first, Binance.US as fallback
///   BN:BTCUSD, BN:BTC-USD                 Binance.US only
///   BN:BTC (a bare base asset)            BTCUSDT on the global venue, then
///                                         BTCUSD on Binance.US
///
/// A plain-USD pair is never sent to the mirror: it answers 200 for symbols
/// such as BTCUSD with flat, zero-volume bars, so an "invalid symbol" error
/// cannot be used to pick the venue.
///
/// `source` names the venue that served the series ("binance" or
/// "binance.us") and `upstream_id` the symbol that was served.
///
/// Both venues: 1000 candles per request, klines cost 2 request weight
/// (limit 6000 weight per minute on the global venue, 1200 on Binance.US).
/// This fetcher is sequential with 100 ms between pages and 200 ms between
/// tickers, so it stays under 10 requests per second per host.
///
/// Response: array of 12-element arrays (positional):
///   [0] open_time_ms, [1] open, [2] high, [3] low, [4] close,
///   [5] volume, [6] close_time_ms, [7] quote_volume, [8] trades, ...
/// A bar with no trades comes back with volume 0 and open = high = low =
/// close = the previous close. It is kept with `v: Some(0.0)` so a caller can
/// tell it from a traded bar.

const BINANCE_GLOBAL_KLINES: &str = "https://data-api.binance.vision/api/v3/klines";
const BINANCE_US_KLINES: &str = "https://api.binance.us/api/v3/klines";

/// Quote assets that cannot be mistaken for a plain-USD pair. `<BASE>TUSD`
/// and `<BASE>BUSD` are left out on purpose: without a separator DOTUSD,
/// BATUSD or ARBUSD read both ways, and those are live Binance.US USD pairs.
const BINANCE_STABLE_QUOTES: &[&str] = &["FDUSD", "USDT", "USDC", "USD1"];
/// Other quote assets, accepted only on a symbol of six characters or more so
/// that base assets such as WBTC, STETH or WBETH are not read as pairs.
const BINANCE_OTHER_QUOTES: &[&str] = &[
    "BTC", "ETH", "BNB", "DAI", "EUR", "TRY", "BRL", "JPY", "GBP", "AUD",
];
/// Assets whose own ticker ends in USD, so `BN:FDUSD` is a base asset and not
/// the pair FD/USD. Four-character ones (TUSD, BUSD, GUSD) need no entry.
const BINANCE_USD_NAMED_ASSETS: &[&str] = &["FDUSD", "PYUSD", "RLUSD", "BFUSD"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinanceVenue {
    Global,
    Us,
}

impl BinanceVenue {
    fn source(self) -> &'static str {
        match self {
            BinanceVenue::Global => "binance",
            BinanceVenue::Us => "binance.us",
        }
    }

    fn klines_url(self) -> &'static str {
        match self {
            BinanceVenue::Global => BINANCE_GLOBAL_KLINES,
            BinanceVenue::Us => BINANCE_US_KLINES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BinanceRoute {
    venue: BinanceVenue,
    symbol: String,
}

/// Best Binance interval <= requested, or exact match.
fn binance_best_interval(granularity: Span) -> (&'static str, i64) {
    let requested = granularity.approx_duration().num_seconds().max(1);
    let candidates: &[(&str, i64)] = &[
        ("1m", 60), ("3m", 180), ("5m", 300), ("15m", 900), ("30m", 1800),
        ("1h", 3600), ("2h", 7200), ("4h", 14400), ("6h", 21600),
        ("8h", 28800), ("12h", 43200), ("1d", 86400), ("3d", 259200),
        ("1w", 604800), ("1M", 2592000),
    ];
    // Exact match first.
    for &(interval, secs) in candidates {
        if secs == requested {
            return (interval, secs);
        }
    }
    // Largest that divides evenly.
    let mut best = ("1d", 86400_i64);
    for &(interval, secs) in candidates {
        if secs > requested || secs <= 0 { continue; }
        if requested % secs != 0 { continue; }
        if secs >= best.1 || best.1 > requested {
            best = (interval, secs);
        }
    }
    best
}

/// The part of a ticker after its `BN:` or `BINANCE:` prefix.
fn binance_strip_prefix(ticker: &str) -> &str {
    let t = ticker.trim();
    let upper = t.to_ascii_uppercase();
    if upper.starts_with("BINANCE:") {
        &t[8..]
    } else if upper.starts_with("BN:") {
        &t[3..]
    } else {
        t
    }
}

/// How a symbol written without a separator reads.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BinanceSymbolKind {
    /// A base asset on its own (BTC, WBTC, FDUSD).
    Base,
    /// A pair quoted in plain USD (BTCUSD). Binance.US only.
    UsdPair,
    /// A pair quoted in anything else (BTCUSDT, ETHBTC).
    OtherPair,
}

fn binance_symbol_kind(symbol: &str) -> BinanceSymbolKind {
    let len = symbol.len();
    if symbol.ends_with("USD") {
        if symbol.ends_with("FDUSD") && len > 5 {
            return BinanceSymbolKind::OtherPair;
        }
        if len >= 5 && !BINANCE_USD_NAMED_ASSETS.contains(&symbol) {
            return BinanceSymbolKind::UsdPair;
        }
        return BinanceSymbolKind::Base;
    }
    if BINANCE_STABLE_QUOTES
        .iter()
        .any(|q| symbol.ends_with(q) && len > q.len())
    {
        return BinanceSymbolKind::OtherPair;
    }
    if len >= 6 && BINANCE_OTHER_QUOTES.iter().any(|q| symbol.ends_with(q)) {
        return BinanceSymbolKind::OtherPair;
    }
    BinanceSymbolKind::Base
}

/// The venues to try for one ticker, in order, with the symbol to ask each for.
///
/// Accepts BN:BTCUSDT, BN:BTC, BINANCE:ETHUSDT, a bare BTCUSDT, and the
/// explicit forms BN:BTC-USDT, BN:BTC/USDT and BN:BTC_USDT, which name the
/// quote asset outright (the way to ask for a TUSD or BUSD quoted pair).
fn binance_routes(ticker: &str) -> std::result::Result<Vec<BinanceRoute>, String> {
    let raw = binance_strip_prefix(ticker).trim().to_ascii_uppercase();
    let route = |venue: BinanceVenue, symbol: String| BinanceRoute { venue, symbol };
    let valid = |s: &str| !s.is_empty() && s.len() <= 24 && s.chars().all(|c| c.is_ascii_alphanumeric());

    if let Some((base, quote)) = raw.split_once(|c| c == '-' || c == '/' || c == '_') {
        if !valid(base) || !valid(quote) {
            return Err(format!(
                "'{ticker}' is not a Binance pair; write BN:BTCUSDT or BN:BTC-USDT"
            ));
        }
        let symbol = format!("{base}{quote}");
        return Ok(if quote == "USD" {
            vec![route(BinanceVenue::Us, symbol)]
        } else {
            vec![
                route(BinanceVenue::Global, symbol.clone()),
                route(BinanceVenue::Us, symbol),
            ]
        });
    }

    if !valid(&raw) {
        return Err(format!(
            "'{ticker}' is not a Binance symbol; write BN:BTCUSDT, BN:BTC-USDT or a base asset such as BN:BTC"
        ));
    }
    Ok(match binance_symbol_kind(&raw) {
        BinanceSymbolKind::UsdPair => vec![route(BinanceVenue::Us, raw)],
        BinanceSymbolKind::OtherPair => vec![
            route(BinanceVenue::Global, raw.clone()),
            route(BinanceVenue::Us, raw),
        ],
        BinanceSymbolKind::Base => vec![
            route(BinanceVenue::Global, format!("{raw}USDT")),
            route(BinanceVenue::Us, format!("{raw}USD")),
        ],
    })
}

/// What one venue returned for one symbol.
struct BinanceRouteResult {
    candles: Vec<Candle>,
    /// Why the venue stopped before the end of the range, when it did.
    problem: Option<String>,
    /// The venue answered 429 or 418: leave it alone for the rest of the call.
    refused: bool,
}

/// Page through one venue's klines for one symbol.
async fn binance_fetch_route(
    client: &reqwest::Client,
    route: &BinanceRoute,
    interval: &str,
    start_ms: i64,
    end_ms: i64,
) -> BinanceRouteResult {
    let mut out = BinanceRouteResult {
        candles: Vec::new(),
        problem: None,
        refused: false,
    };
    let mut cursor_ms = start_ms;

    // Paginate: max 1000 candles per request.
    for _page in 0..50 {
        if cursor_ms >= end_ms {
            break;
        }
        let url = format!(
            "{}?symbol={}&interval={}&startTime={}&endTime={}&limit=1000",
            route.venue.klines_url(),
            route.symbol,
            interval,
            cursor_ms,
            end_ms
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                out.problem = Some(format!("request failed: {e}"));
                break;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = resp.text().await.unwrap_or_default();
            // Binance errors are {"code": -1121, "msg": "Invalid symbol."}.
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(str::to_string))
                .unwrap_or_else(|| body.chars().take(160).collect::<String>());
            let code = status.as_u16();
            if code == 429 || code == 418 {
                out.refused = true;
                out.problem = Some(match retry_after {
                    Some(secs) => format!("http {code}, rate limited, retry after {secs} s"),
                    None => format!("http {code}, rate limited, retry in a minute"),
                });
            } else {
                out.problem = Some(format!(
                    "http {code}: {}",
                    detail.trim().trim_end_matches('.')
                ));
            }
            break;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                out.problem = Some(format!("json parse failed: {e}"));
                break;
            }
        };
        let Some(rows) = body.as_array() else {
            out.problem = Some("response is not an array".to_string());
            break;
        };
        if rows.is_empty() {
            break;
        }
        let num = |v: &serde_json::Value| -> f64 {
            v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0)
        };
        for row in rows {
            let arr = match row.as_array() {
                Some(a) if a.len() >= 7 => a,
                _ => continue,
            };
            let open_time_ms = arr[0].as_i64().unwrap_or(0);
            let close_time_ms = arr[6].as_i64().unwrap_or(0);
            // Advance the cursor past this candle for pagination.
            if close_time_ms + 1 > cursor_ms {
                cursor_ms = close_time_ms + 1;
            }
            let (o, h, l, c, v) = (num(&arr[1]), num(&arr[2]), num(&arr[3]), num(&arr[4]), num(&arr[5]));
            if o == 0.0 && c == 0.0 {
                continue;
            }
            let Some(t) = DateTime::<Utc>::from_timestamp_millis(open_time_ms) else {
                continue;
            };
            // Volume is kept even at zero: a bar with no trades is a fact
            // about the venue, not a missing value.
            out.candles.push(Candle { t, o, h, l, c, v: Some(v.max(0.0)), kind: None });
        }
        if rows.len() < 1000 {
            break; // Last page.
        }
        // Rate limit: ~100ms between pages.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    out
}

pub(crate) async fn fetch_binance_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let (interval, interval_secs) = binance_best_interval(granularity);
    let needs_resample = interval_secs != granularity.approx_duration().num_seconds();
    let start_ms = start.timestamp_millis();
    let end_ms = end.timestamp_millis();

    let mut out = Vec::new();
    let mut errors = Vec::new();
    // A venue that answered 429 or 418 is not asked again in this call.
    let mut refused_global = false;
    let mut refused_us = false;

    for (idx, ticker) in tickers.iter().enumerate() {
        // Rate limit between tickers.
        if idx > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let routes = match binance_routes(ticker) {
            Ok(routes) => routes,
            Err(message) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message,
                });
                continue;
            }
        };

        // What each venue that did not serve the series said, in order.
        let mut misses: Vec<String> = Vec::new();
        let mut served: Option<(BinanceRoute, BinanceRouteResult)> = None;
        for route in &routes {
            let label = format!("{} ({})", route.venue.source(), route.symbol);
            let skip = match route.venue {
                BinanceVenue::Global => refused_global,
                BinanceVenue::Us => refused_us,
            };
            if skip {
                misses.push(format!("{label}: skipped, the venue rate limited an earlier request in this call"));
                continue;
            }
            let result = binance_fetch_route(client, route, interval, start_ms, end_ms).await;
            if result.refused {
                match route.venue {
                    BinanceVenue::Global => refused_global = true,
                    BinanceVenue::Us => refused_us = true,
                }
            }
            if result.candles.is_empty() {
                misses.push(match &result.problem {
                    Some(problem) => format!("{label}: {problem}"),
                    None => format!("{label}: no candles in this range"),
                });
                continue;
            }
            served = Some((route.clone(), result));
            break;
        }

        let Some((route, result)) = served else {
            let first = routes.first().map(|r| r.symbol.as_str()).unwrap_or_default();
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!(
                    "no Binance candles for '{ticker}': {}. Check the symbol (the first choice was {first}); to name the quote asset outright write BN:BASE-QUOTE (BN:BTC-USDT). Plain-USD pairs (BN:BTCUSD) exist only on Binance.US.",
                    misses.join("; ")
                ),
            });
            continue;
        };

        let mut candles = result.candles;
        candles.sort_by_key(|c| c.t);
        candles.dedup_by_key(|c| c.t);
        if needs_resample {
            candles = resample_candles(&candles, start, granularity.approx_duration());
        }
        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!(
                    "{} returned no candles for '{}' in this range",
                    route.venue.source(),
                    route.symbol
                ),
            });
            continue;
        }

        // The series stopped early on a venue that had already returned data:
        // report it as an error next to the partial series, as before.
        if let Some(problem) = &result.problem {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!(
                    "{} ({}) stopped before the end of the range: {problem}. The series is incomplete.",
                    route.venue.source(),
                    route.symbol
                ),
            });
        }
        // A fallback is a different venue (and for a bare base a different
        // quote asset) than the first choice. Say so; it is a notice, not a
        // failure.
        if !misses.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("data_quality".to_string()),
                message: format!(
                    "served {} from {} because the first choice failed: {}.{}",
                    route.symbol,
                    route.venue.source(),
                    misses.join("; "),
                    if route.venue == BinanceVenue::Us {
                        " Binance.US is a thin venue: expect bars with no trades (v = 0)."
                    } else {
                        ""
                    }
                ),
            });
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some(route.venue.source().to_string()),
            upstream_id: Some(route.symbol),
        });
    }

    Ok((out, errors))
}

/// Returns true if the ticker should route to Binance.
pub fn is_binance_ticker(ticker: &str) -> bool {
    let t = ticker.trim().to_ascii_uppercase();
    t.starts_with("BN:") || t.starts_with("BINANCE:")
}
