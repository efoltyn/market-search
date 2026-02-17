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
            "{ticker} is an {qt} — financial statements are not available. Use `eli finance snapshot` for price data instead.",
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
        operating_income: fin.and_then(|f| f.operating_cashflow),
        net_income: stats
            .and_then(|s| s.net_income_to_common)
            .or_else(|| {
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

    Ok(FundamentalsResponse {
        ticker,
        company_name,
        currency,
        generated_at: Utc::now(),
        statements: if is_etf { vec![] } else { vec![statement] },
        note,
    })
}
