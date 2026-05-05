use super::super::*;

pub async fn fetch_fundamentals(req: FundamentalsRequest) -> Result<FundamentalsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    let info = connector
        .get_ticker_info(&ticker)
        .await
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
    let note = if is_etf {
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
        date: Utc::now().date_naive().format("%Y-%m-%d").to_string(),
        period: "current".to_string(),
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

    let metrics = eli_finance_types::FundamentalsMetrics {
        current_price: fin.and_then(|f| f.current_price),
        market_cap: summary.and_then(|s| s.market_cap),
        enterprise_value: stats.and_then(|s| s.enterprise_value),
        trailing_pe: summary.and_then(|s| s.trailing_pe),
        forward_pe: summary
            .and_then(|s| s.forward_pe)
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
    };
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
