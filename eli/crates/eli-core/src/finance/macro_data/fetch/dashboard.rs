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

