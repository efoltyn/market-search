fn generate_mock_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Vec<TickerSeries> {
    tickers
        .iter()
        .map(|ticker| TickerSeries {
            ticker: ticker.clone(),
            candles: generate_mock_candles(ticker, start, end, step),
            source: Some("mock".to_string()),
            upstream_id: Some(ticker.clone()),
        })
        .collect()
}

fn generate_mock_candles(
    ticker: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Vec<Candle> {
    let mut rng = XorShift64::new(seed_from_str(ticker));
    let mut t = start;
    let mut price = base_price_from_seed(rng.next_u64());
    let mut out = Vec::new();

    while t <= end {
        let open = price;
        let move_pct = (rng.next_f64() - 0.5) * 0.02; // +/-1%
        price = (price * (1.0 + move_pct)).max(0.01);
        let close = price;

        let wick = rng.next_f64() * 0.005; // up to 0.5%
        let high = open.max(close) * (1.0 + wick);
        let low = open.min(close) * (1.0 - wick).max(0.0);
        let vol = Some((rng.next_f64() * 1_000_000.0).round());

        out.push(Candle {
            t,
            o: round_4(open),
            h: round_4(high),
            l: round_4(low),
            c: round_4(close),
            v: vol,
            kind: None,
        });

        match t.checked_add_signed(step) {
            Some(next) => t = next,
            None => break,
        }
    }

    out
}

fn seed_from_str(s: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    let mut seed = 0u64;
    for b in bytes[..8].iter() {
        seed = (seed << 8) | (*b as u64);
    }
    seed
}

fn base_price_from_seed(seed: u64) -> f64 {
    let v = (seed % 20_000) as f64;
    10.0 + v / 10.0 // 10..2010
}

pub(crate) fn generate_mock_snapshots(tickers: &[String]) -> Vec<TickerSnapshot> {
    let now = Utc::now();
    tickers
        .iter()
        .map(|ticker| {
            let seed = seed_from_str(ticker);
            let price = round_4(base_price_from_seed(seed));
            // 0.1B .. 20.1B shares
            let shares = 100_000_000u64 + (seed % 20_000_000_000u64);
            let market_cap = (price * (shares as f64)).round() as u64;

            TickerSnapshot {
                ticker: ticker.clone(),
                currency: Some("USD".to_string()),
                exchange: Some("MOCK".to_string()),
                short_name: Some(format!("{ticker} Corp")),
                long_name: Some(format!("{ticker} Corporation")),
                current_price: Some(price),
                previous_close: Some(round_4(price * 0.995)),
                open: Some(round_4(price * 1.002)),
                day_low: Some(round_4(price * 0.99)),
                day_high: Some(round_4(price * 1.01)),
                price: Some(price),
                daily_return: Some((price / round_4(price * 0.995)) - 1.0),
                market_cap: Some(market_cap),
                enterprise_value: Some(market_cap as i64),
                shares_outstanding: Some(shares),
                float_shares: Some(shares.saturating_sub(shares / 10)),
                last_split_factor: None,
                last_split_date: None,
                freshness: Freshness::new(
                    now,
                    now,
                    FreshnessState::Unknown,
                    FreshnessOrigin::Derived,
                    FreshnessQuality::Estimated,
                ),
                price_source_kind: "mock".to_string(),
                session_state: "unknown".to_string(),
                market_closed_fallback: false,
                effective_at: Some(now),
                clock_status: None,
                integrity_note: None,
            }
        })
        .collect()
}
