async fn fetch_current_fed_funds() -> Result<f64> {
    let end = Utc::now();
    let start = end - chrono::Duration::days(800);
    let (series, _errors) = fetch_fred_series(
        &["FEDFUNDS".to_string()],
        start,
        end,
        Span {
            n: 1,
            unit: SpanUnit::Month,
        },
    )
    .await?;

    if let Some(latest) = series
        .first()
        .and_then(|s| s.candles.last())
        .map(|c| c.c)
    {
        return Ok(latest);
    }

    // Fallback to macro response if direct FRED path is sparse/unavailable.
    if let Ok(macro_resp) = fetch_macro(MacroRequest {
        range: None,
        compare_to: None,
    })
    .await
    {
        if let Some(v) = macro_resp
            .indicators
            .into_iter()
            .find(|i| i.symbol == "FEDFUNDS")
            .map(|i| i.current_value)
        {
            return Ok(v);
        }
    }

    Err(Error::Provider(
        "FEDFUNDS series has no observations".to_string(),
    ))
}

