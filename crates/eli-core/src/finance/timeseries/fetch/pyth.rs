/// PYTH: tickers, served without Pyth.
///
/// Pyth ended keyless access on 2026-08-26: its Benchmarks history endpoint
/// answers 404, Hermes answers 401, and a key comes with a paid plan. So
/// nothing here calls a Pyth host and there is no key setting.
///
/// A PYTH: name that Kraken Futures has a market for is served from that
/// market's index (see kraken_futures.rs). The series keeps the ticker that
/// was asked for, says where it came from (`source` "kraken_futures",
/// `upstream_id` "spot/PF_XBTUSD") and always carries a data_quality notice
/// about the change. For gold, silver, WTI and Brent, Kraken lists that
/// index as a "24/7 Pyth Index", so those are still Pyth's numbers.
///
/// A name with no Kraken market (USD/JPY, the dollar index, any other Pyth
/// feed symbol) fails with an error that names what to use instead.

const PYTH_ENDED: &str = "Pyth ended keyless access on 2026-08-26";

enum PythRoute {
    /// Served from the Kraken Futures index of `symbol`. `kf` is the ticker
    /// that asks for the same series directly; `listed_as` is how Kraken
    /// describes the index when it is one of Pyth's.
    Kraken {
        symbol: &'static str,
        kf: &'static str,
        listed_as: Option<&'static str>,
    },
    /// Not served. The text names what to use instead.
    Unavailable(String),
}

/// Check if a ticker string should be routed here: the "PYTH:" prefix in
/// any case.
pub fn is_pyth_ticker(ticker: &str) -> bool {
    ticker
        .trim()
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("PYTH:"))
}

/// Where a PYTH: ticker goes now. The prefix is optional so that
/// `--provider pyth --tickers BTC` works. The names are the aliases this
/// tool has always accepted, plus the Pyth feed symbols they stood for.
fn pyth_route(ticker: &str) -> PythRoute {
    let t = ticker.trim();
    let raw = if is_pyth_ticker(t) { t[5..].trim() } else { t };
    let kraken = |symbol, kf, listed_as| PythRoute::Kraken { symbol, kf, listed_as };
    match raw.to_ascii_lowercase().as_str() {
        "oil" | "wti" | "crude" | "crude oil" | "cl" | "usoilspot" | "commodities.usoilspot" => {
            kraken("PF_WTIOILUSD", "KF:OIL", Some("WTI Crude Oil 24/7 Pyth Index"))
        }
        "brent" | "brent oil" | "bz" | "ukoilspot" | "commodities.ukoilspot" => {
            kraken("PF_BRENTOILUSD", "KF:BRENT", Some("Brent Crude Oil 24/7 Pyth Index"))
        }
        "gold" | "xau" | "gc" | "metal.xau/usd" => {
            kraken("PF_XAUUSD", "KF:GOLD", Some("Gold 24/7 Pyth Index"))
        }
        "silver" | "xag" | "si" | "metal.xag/usd" => {
            kraken("PF_XAGUSD", "KF:SILVER", Some("Silver 24/7 Pyth Index"))
        }
        "btc" | "bitcoin" | "crypto.btc/usd" => kraken("PF_XBTUSD", "KF:BTC", None),
        "eth" | "ethereum" | "ether" | "crypto.eth/usd" => kraken("PF_ETHUSD", "KF:ETH", None),
        "sol" | "solana" | "crypto.sol/usd" => kraken("PF_SOLUSD", "KF:SOL", None),
        "eurusd" | "eur/usd" | "fx.eur/usd" => kraken("PF_EURUSD", "KF:EUR", None),
        "usdjpy" | "usd/jpy" | "fx.usd/jpy" => PythRoute::Unavailable(format!(
            "{PYTH_ENDED}, and Kraken Futures lists no USD/JPY market. Use HL:xyz:JPY (a 24/7 perp) or USDJPY=X."
        )),
        "dxy" | "usdxy" | "dollar" | "fx.usdxy" => PythRoute::Unavailable(format!(
            "{PYTH_ENDED}, and no keyless 24/7 source carries the dollar index. Use DX-Y.NYB."
        )),
        "" => PythRoute::Unavailable(format!(
            "a PYTH: ticker needs a name, and {PYTH_ENDED}. Use a KF: ticker (Kraken Futures index, KF:BTC) or an HL: ticker (Hyperliquid perp, HL:xyz:GOLD)."
        )),
        _ => {
            // "Equity.US.AAPL/USD" -> AAPL, "Crypto.DOGE/USD" -> DOGE, "XRP" -> XRP.
            let name = raw.rsplit('.').next().unwrap_or(raw);
            let base = name.split('/').next().unwrap_or(name).trim().to_ascii_uppercase();
            let usable = !base.is_empty()
                && base.len() <= 20
                && base.chars().all(|c| c.is_ascii_alphanumeric());
            PythRoute::Unavailable(if usable {
                format!(
                    "{PYTH_ENDED}, and '{raw}' has no mapped replacement. Try KF:{base} (Kraken Futures index) or a Hyperliquid perp: HL:{base} for crypto, HL:xyz:{base} for US stocks, metals, oil and FX."
                )
            } else {
                format!(
                    "{PYTH_ENDED}, and '{raw}' has no mapped replacement. Use a KF: ticker (Kraken Futures index, KF:BTC) or an HL: ticker (Hyperliquid perp, HL:xyz:GOLD)."
                )
            })
        }
    }
}

pub(crate) async fn fetch_pyth_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    let mut refused = false;

    for ticker in tickers {
        let (symbol, kf, listed_as) = match pyth_route(ticker) {
            PythRoute::Kraken { symbol, kf, listed_as } => (symbol, kf, listed_as),
            PythRoute::Unavailable(message) => {
                errors.push(kf_issue(ticker, "input", message));
                continue;
            }
        };
        if refused {
            errors.push(kf_issue(ticker, "fetch", KF_SKIPPED.to_string()));
            continue;
        }
        let target = KfTarget {
            symbol: symbol.to_string(),
            tick: KfTick::Index,
        };
        match kf_fetch_target(&target, start, end, granularity).await {
            Ok(series) => {
                // Never a silent swap: the series says what it is.
                let what = match listed_as {
                    Some(name) => format!(", which Kraken lists as \"{name}\""),
                    None => String::new(),
                };
                errors.push(kf_issue(
                    ticker,
                    "data_quality",
                    format!("{PYTH_ENDED}; this is the Kraken Futures index ({kf}){what}."),
                ));
                for message in series.notices {
                    errors.push(kf_issue(ticker, "data_quality", message));
                }
                out.push(TickerSeries {
                    ticker: ticker.clone(),
                    candles: series.candles,
                    source: Some("kraken_futures".to_string()),
                    upstream_id: Some(target.upstream_id()),
                });
            }
            Err(e) => {
                refused = refused || e.refused;
                errors.push(kf_issue(ticker, "fetch", e.message));
            }
        }
    }

    Ok((out, errors))
}
