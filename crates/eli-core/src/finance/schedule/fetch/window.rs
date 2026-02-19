async fn fetch_schedule_window(
    client: &reqwest::Client,
    kind: ScheduleKind,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
    tickers: &BTreeSet<String>,
) -> Result<ScheduleResponse> {
    let mut warnings = Vec::new();
    let mut earnings = Vec::new();
    let mut macro_events = Vec::new();
    let mut macro_days = Vec::new();

    // Collect all dates in the window.
    let mut dates = Vec::new();
    {
        let mut d = start_date;
        while d <= end_date {
            dates.push(d);
            d = d
                .succ_opt()
                .ok_or_else(|| Error::Provider("date iteration overflow".to_string()))?;
        }
    }

    // Fetch earnings and macro in parallel across all dates (not sequential per-day).
    let do_earnings = matches!(kind, ScheduleKind::Earnings | ScheduleKind::All);
    let do_macro = matches!(kind, ScheduleKind::Macro | ScheduleKind::All);

    let earnings_fut = async {
        if !do_earnings {
            return (Vec::new(), Vec::new());
        }
        let futs: Vec<_> = dates
            .iter()
            .map(|&d| {
                let client = client.clone();
                async move {
                    match timeout(
                        TokioDuration::from_secs(SCHEDULE_PER_DAY_TIMEOUT_SECS),
                        fetch_nasdaq_earnings_for_date(&client, d),
                    )
                    .await
                    {
                        Ok(Ok(rows)) => (Some(rows), None),
                        Ok(Err(e)) => (None, Some(format!("nasdaq earnings {d}: {e}"))),
                        Err(_) => (None, Some(format!("nasdaq earnings {d}: timed out"))),
                    }
                }
            })
            .collect();
        let results = futures::future::join_all(futs).await;
        let mut earn = Vec::new();
        let mut warn = Vec::new();
        for (rows, w) in results {
            if let Some(mut r) = rows {
                if !tickers.is_empty() {
                    r.retain(|row| tickers.contains(&row.symbol));
                }
                earn.extend(r);
            }
            if let Some(w) = w {
                warn.push(w);
            }
        }
        (earn, warn)
    };

    let macro_fut = async {
        if !do_macro {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        // Fetch day counts + per-day details in parallel
        let counts_fut = timeout(
            TokioDuration::from_secs(SCHEDULE_PER_DAY_TIMEOUT_SECS),
            fetch_fred_macro_counts(client, start_date, end_date),
        );
        let day_futs: Vec<_> = dates
            .iter()
            .map(|&d| {
                let client = client.clone();
                async move {
                    match timeout(
                        TokioDuration::from_secs(SCHEDULE_PER_DAY_TIMEOUT_SECS),
                        fetch_fred_macro_for_day(&client, d),
                    )
                    .await
                    {
                        Ok(Ok(rows)) => (Some(rows), None),
                        Ok(Err(e)) => (None, Some(format!("fred macro {d}: {e}"))),
                        Err(_) => (None, Some(format!("fred macro {d}: timed out"))),
                    }
                }
            })
            .collect();
        let (counts_result, day_results) =
            tokio::join!(counts_fut, futures::future::join_all(day_futs));
        let mut m_days = Vec::new();
        let mut m_events = Vec::new();
        let mut warn = Vec::new();
        match counts_result {
            Ok(Ok(days)) => m_days = days,
            Ok(Err(e)) => warn.push(format!("fred macro counts: {e}")),
            Err(_) => warn.push("fred macro counts: timed out".to_string()),
        }
        for (rows, w) in day_results {
            if let Some(r) = rows {
                m_events.extend(r);
            }
            if let Some(w) = w {
                warn.push(w);
            }
        }
        (m_events, m_days, warn)
    };

    let ((earn, earn_warn), (m_events, m_days, macro_warn)) =
        tokio::join!(earnings_fut, macro_fut);
    earnings = earn;
    macro_events = m_events;
    macro_days = m_days;
    warnings.extend(earn_warn);
    warnings.extend(macro_warn);

    Ok(ScheduleResponse {
        generated_at: Utc::now(),
        kind,
        start_date: start_date.to_string(),
        end_date: end_date.to_string(),
        earnings,
        macro_events,
        macro_days,
        warnings,
    })
}
