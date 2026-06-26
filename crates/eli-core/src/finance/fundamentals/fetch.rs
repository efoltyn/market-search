use super::super::*;

/// Yahoo's v7 quote endpoint exposes `priceEpsCurrentYear` (current fiscal year
/// forward P/E) which Yahoo's web UI displays as "Forward P/E". The v10
/// quoteSummary endpoint's `forwardPE` instead uses next-fiscal-year EPS, which
/// can read meaningfully lower (e.g. NVDA: v10 17.66 vs v7 23.80 on 2026-05-05).
/// We prefer the v7 number since it matches what readers see on Yahoo Finance.
#[derive(Default)]
struct V7Quote {
    forward_pe_current_year: Option<f64>,
}

async fn fetch_v7_quote(ticker: &str) -> V7Quote {
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = match reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .cookie_provider(jar)
        .user_agent("Mozilla/5.0")
        .build()
    {
        Ok(c) => c,
        Err(_) => return V7Quote::default(),
    };

    // Initialize cookies, then get crumb.
    let _ = client.get("https://fc.yahoo.com").send().await;
    let crumb = match client.get(YAHOO_CRUMB_URL).send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => return V7Quote::default(),
    };
    if crumb.is_empty() {
        return V7Quote::default();
    }

    let url = format!(
        "https://query2.finance.yahoo.com/v7/finance/quote?symbols={}&crumb={}",
        urlencoding::encode(ticker),
        urlencoding::encode(&crumb),
    );
    let body: serde_json::Value = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(_) => return V7Quote::default(),
        },
        _ => return V7Quote::default(),
    };

    let row = body
        .get("quoteResponse")
        .and_then(|q| q.get("result"))
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.first());

    V7Quote {
        forward_pe_current_year: row
            .and_then(|r| r.get("priceEpsCurrentYear"))
            .and_then(|v| v.as_f64()),
    }
}

/// Fetch the freshest last-trade price and its timestamp from Yahoo's v8 chart
/// `meta` block. This needs no crumb/cookie and is the same endpoint the timeseries
/// path uses, so it is the authoritative "last price + as-of" for a ticker. The v10
/// quoteSummary price (`financialData.currentPrice`) can lag the live tape intraday;
/// v8 `regularMarketPrice`/`regularMarketTime` does not. Returns (price, epoch_secs).
async fn fetch_live_price(ticker: &str) -> Option<(f64, i64)> {
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .user_agent("Mozilla/5.0")
        .build()
        .ok()?;
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?range=1d&interval=1d",
        urlencoding::encode(ticker),
    );
    let body: serde_json::Value = client.get(&url).send().await.ok()?.json().await.ok()?;
    let meta = body
        .get("chart")?
        .get("result")?
        .get(0)?
        .get("meta")?;
    let price = meta.get("regularMarketPrice").and_then(|p| p.as_f64())?;
    let ts = meta
        .get("regularMarketTime")
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    if price.is_finite() && price > 0.0 {
        Some((price, ts))
    } else {
        None
    }
}

pub async fn fetch_fundamentals(req: FundamentalsRequest) -> Result<FundamentalsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    // Fan out the v10 quoteSummary (via the upstream connector) and the v7 quote
    // call in parallel — v7 is the only path that exposes `priceEpsCurrentYear`,
    // which is what Yahoo's UI shows as "Forward P/E".
    let info_fut = connector.get_ticker_info(&ticker);
    let v7_fut = fetch_v7_quote(&ticker);
    let live_fut = fetch_live_price(&ticker);
    let (info_res, v7, live_price) = tokio::join!(info_fut, v7_fut, live_fut);
    let info = info_res
        .map_err(|e| Error::Provider(format!("yahoo fundamentals failed for '{ticker}': {e}")))?;

    let qs = info
        .quote_summary
        .ok_or_else(|| Error::Provider(format!("yahoo quote summary missing for '{ticker}'")))?;
    let result = qs.result.ok_or_else(|| {
        Error::Provider(format!("yahoo quote summary result missing for '{ticker}'"))
    })?;
    let first = result.get(0).ok_or_else(|| {
        Error::Provider(format!("yahoo quote summary result empty for '{ticker}'"))
    })?;

    let quote_type = first.quote_type.as_ref();
    let profile = first.asset_profile.as_ref();
    let summary = first.summary_detail.as_ref();
    let stats = first.default_key_statistics.as_ref();
    let fin = first.financial_data.as_ref();

    let company_name =
        quote_type.and_then(|q| q.long_name.clone().or_else(|| q.short_name.clone()));
    let currency = fin.and_then(|f| f.financial_currency.clone());

    // Detect ETFs/funds — they don't have financial statements
    let qt_str = quote_type
        .and_then(|q| q.quote_type.as_deref())
        .unwrap_or("");
    let is_etf = qt_str.eq_ignore_ascii_case("ETF")
        || qt_str.eq_ignore_ascii_case("MUTUALFUND")
        || qt_str.eq_ignore_ascii_case("INDEX");
    let mut note = if is_etf {
        Some(format!(
            "{ticker} is an {qt} — financial statements are not available. Use `eli finance timeseries` for price data instead.",
            qt = qt_str
        ))
    } else if fin.is_none() {
        Some(format!(
            "no financial data available for {ticker} — this ticker may be an ETF, index, or non-reporting security"
        ))
    } else {
        None
    };

    let statement = FinancialStatement {
        // Yahoo's financialData figures are trailing-twelve-month, not a single fiscal
        // period. Stamping today's date misled consumers into reading this as a fresh
        // period report; label it TTM instead.
        date: "ttm".to_string(),
        period: "ttm".to_string(),
        total_revenue: fin.and_then(|f| f.total_revenue).map(|v| v as i64),
        cost_of_revenue: None,
        gross_profit: fin.and_then(|f| f.gross_profits).map(|v| v as i64),
        operating_income: None, // Yahoo financialData doesn't expose operatingIncome directly
        net_income: stats.and_then(|s| s.net_income_to_common).or_else(|| {
            // Derive from revenue * profit margin if direct net income unavailable
            let rev = fin.and_then(|f| f.total_revenue)? as f64;
            let margin = fin.and_then(|f| f.profit_margins)?;
            Some((rev * margin) as i64)
        }),
        ebitda: fin.and_then(|f| f.ebitda).map(|v| v as i64),
        total_assets: None,
        total_liabilities: None,
        total_equity: None,
        cash_and_equivalents: fin.and_then(|f| f.total_cash).map(|v| v as i64),
        total_debt: fin.and_then(|f| f.total_debt).map(|v| v as i64),
        operating_cash_flow: fin.and_then(|f| f.operating_cashflow),
        investing_cash_flow: None,
        financing_cash_flow: None,
        capital_expenditure: None,
        free_cash_flow: fin.and_then(|f| f.free_cashflow),
    };

    let mut metrics = eli_finance_types::FundamentalsMetrics {
        current_price: fin.and_then(|f| f.current_price),
        market_cap: summary.and_then(|s| s.market_cap),
        enterprise_value: stats.and_then(|s| s.enterprise_value),
        trailing_pe: summary.and_then(|s| s.trailing_pe),
        // Prefer v7 `priceEpsCurrentYear` (current-fiscal-year forward P/E,
        // matches Yahoo Finance's UI). Fall back to v10's `forwardPE` (computed
        // from next-fiscal-year EPS estimate, often reads materially lower).
        forward_pe: v7
            .forward_pe_current_year
            .or_else(|| summary.and_then(|s| s.forward_pe))
            .or_else(|| stats.and_then(|s| s.forward_pe)),
        trailing_eps: stats.and_then(|s| s.trailing_eps),
        forward_eps: stats.and_then(|s| s.forward_eps),
        price_to_book: stats.and_then(|s| s.price_to_book),
        book_value_per_share: stats.and_then(|s| s.book_value),
        enterprise_to_revenue: stats.and_then(|s| s.enterprise_to_revenue),
        enterprise_to_ebitda: stats.and_then(|s| s.enterprise_to_ebitda),
        profit_margin: fin
            .and_then(|f| f.profit_margins)
            .or_else(|| stats.and_then(|s| s.profit_margins)),
        gross_margin: fin.and_then(|f| f.gross_margins),
        operating_margin: fin.and_then(|f| f.operating_margins),
        ebitda_margin: fin.and_then(|f| f.ebitda_margins),
        return_on_assets: fin.and_then(|f| f.return_on_assets),
        return_on_equity: fin.and_then(|f| f.return_on_equity),
        debt_to_equity: fin.and_then(|f| f.debt_to_equity),
        current_ratio: fin.and_then(|f| f.current_ratio),
        quick_ratio: fin.and_then(|f| f.quick_ratio),
        revenue_growth: fin.and_then(|f| f.revenue_growth),
        earnings_growth: fin
            .and_then(|f| f.earnings_growth)
            .or_else(|| stats.and_then(|s| s.earnings_quarterly_growth)),
        revenue_per_share: fin.and_then(|f| f.revenue_per_share),
        total_cash_per_share: fin.and_then(|f| f.total_cash_per_share),
        shares_outstanding: stats.and_then(|s| s.shares_outstanding),
        float_shares: stats.and_then(|s| s.float_shares),
        short_ratio: stats.and_then(|s| s.short_ratio),
        short_percent_of_float: stats.and_then(|s| s.short_percent_of_float),
        analyst_target_mean_price: fin.and_then(|f| f.target_mean_price),
        recommendation_mean: fin.and_then(|f| f.recommendation_mean),
        recommendation_key: fin.and_then(|f| f.recommendation_key.clone()),
        analyst_count: fin.and_then(|f| f.number_of_analyst_opinions),
        dividend_yield: summary.and_then(|s| s.dividend_yield),
        price_as_of: None,
        price_provider: None,
    };

    // ── Freshness + price-derived recompute ──────────────────────────────────
    // The v10 quoteSummary delivers most metrics, but its price-derived multiples
    // can be stale: `enterprise_value` (and EV/rev, EV/ebitda) come from a semi-daily
    // batch field that lags the live market cap by up to ~10% on fast movers, and
    // nothing in the response stamps how fresh the price is. We pull the freshest
    // last-trade from Yahoo v8, recompute the price-derived multiples from it for
    // USD reporters (so they are internally consistent with that one price), and
    // always expose `price_as_of` so a consumer can see the data's age.
    let round2 = |x: f64| (x * 100.0).round() / 100.0;
    let is_usd = currency
        .as_deref()
        .map(|c| c.eq_ignore_ascii_case("USD"))
        .unwrap_or(true);
    let (live_px, price_as_of, price_provider) = match live_price {
        Some((px, ts)) if ts > 0 => (
            Some(px),
            chrono::DateTime::from_timestamp(ts, 0).map(|d: DateTime<Utc>| d.to_rfc3339()),
            Some("yahoo".to_string()),
        ),
        Some((px, _)) => (Some(px), None, Some("yahoo".to_string())),
        None => (metrics.current_price, None, Some("yahoo".to_string())),
    };
    if let Some(px) = live_px {
        metrics.current_price = Some(round2(px));
        if is_usd && !is_etf {
            // Recompute every price-derived multiple from this one fresh price so the
            // snapshot is internally consistent (market_cap = price×shares, etc.).
            if let Some(sh) = metrics.shares_outstanding {
                metrics.market_cap = Some((px * sh as f64).round() as u64);
            }
            metrics.trailing_pe = metrics
                .trailing_eps
                .filter(|&eps| eps > 0.0)
                .map(|eps| round2(px / eps));
            metrics.price_to_book = metrics
                .book_value_per_share
                .filter(|&bv| bv != 0.0)
                .map(|bv| round2(px / bv))
                // A P/B that rounds below 0.01 is never a real "0x book" — it signals a
                // share-class / units mismatch (e.g. Yahoo reports BRK-B's book on the
                // A-share basis, ~1500x the B price). Null rather than print "0.0".
                .filter(|&pb| pb >= 0.01);
            // EV = market_cap + total_debt − cash, recomputed from the fresh market cap
            // (Yahoo's batch EV is the stale field we are replacing).
            if let (Some(mc), Some(debt), Some(cash)) = (
                metrics.market_cap,
                statement.total_debt,
                statement.cash_and_equivalents,
            ) {
                let ev = mc as i64 + debt - cash;
                metrics.enterprise_value = Some(ev);
                metrics.enterprise_to_revenue = statement
                    .total_revenue
                    .filter(|&r| r != 0)
                    .map(|r| round2(ev as f64 / r as f64));
                // Only a positive EBITDA yields a meaningful EV/EBITDA. A negative one
                // produces a negative multiple (RIVN, SNAP) that reads as a confident
                // number but is meaningless — null it, same as the negative-forward_pe guard.
                metrics.enterprise_to_ebitda = statement
                    .ebitda
                    .filter(|&e| e > 0)
                    .map(|e| round2(ev as f64 / e as f64));
            }
        }
    }
    metrics.price_as_of = price_as_of;
    metrics.price_provider = price_provider;

    // Currency guard: for non-USD reporters (e.g. TWD-denominated ADRs like TSM),
    // Yahoo divides a USD price by a local-currency book value, producing a
    // dimensionally meaningless price_to_book and EV-ratio set. Null those rather
    // than emit a confidently wrong number, and say why.
    if !is_usd {
        metrics.price_to_book = None;
        metrics.enterprise_value = None;
        metrics.enterprise_to_revenue = None;
        metrics.enterprise_to_ebitda = None;
        let ccy = currency.as_deref().unwrap_or("a non-USD currency");
        let msg = format!(
            "financials reported in {ccy}; price_to_book and enterprise-value ratios are nulled because mixing a USD price with {ccy} book/statement values is not meaningful."
        );
        note = Some(match note {
            Some(n) => format!("{n} | {msg}"),
            None => msg,
        });
    }

    // Drop a negative forward P/E for loss-makers: when both trailing and forward EPS
    // are negative the ratio is algebraically real but semantically useless and reads
    // as a confident number to an agent. Null it.
    if matches!(metrics.trailing_eps, Some(e) if e < 0.0)
        && matches!(metrics.forward_eps, Some(e) if e < 0.0)
    {
        metrics.forward_pe = None;
    }

    let profile = eli_finance_types::FundamentalsProfile {
        sector: profile.and_then(|p| p.sector.clone()),
        industry: profile.and_then(|p| p.industry.clone()),
        website: profile.and_then(|p| p.website.clone()),
        full_time_employees: profile.and_then(|p| p.full_time_employees),
    };
    let metrics = if serde_json::to_value(&metrics)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .is_some_and(|fields| fields.values().any(|value| !value.is_null()))
    {
        Some(metrics)
    } else {
        None
    };
    let profile = if serde_json::to_value(&profile)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .is_some_and(|fields| fields.values().any(|value| !value.is_null()))
    {
        Some(profile)
    } else {
        None
    };

    Ok(FundamentalsResponse {
        ticker,
        company_name,
        currency,
        generated_at: Utc::now(),
        statements: if is_etf { vec![] } else { vec![statement] },
        metrics,
        profile,
        note,
    })
}
