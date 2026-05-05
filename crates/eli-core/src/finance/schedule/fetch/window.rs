fn merge_macro_events(
    rows_out: &mut Vec<MacroScheduleEvent>,
    incoming: Vec<MacroScheduleEvent>,
) {
    let existing: std::collections::BTreeSet<(String, String)> = rows_out
        .iter()
        .map(|e| (e.date.clone(), e.title.clone()))
        .collect();
    for row in incoming {
        if !existing.contains(&(row.date.clone(), row.title.clone())) {
            rows_out.push(row);
        }
    }
}

fn merge_macro_days(
    days_out: &mut Vec<MacroScheduleDay>,
    incoming: Vec<MacroScheduleDay>,
) {
    let mut by_date: std::collections::BTreeMap<String, usize> = days_out
        .iter()
        .map(|day| (day.date.clone(), day.release_count))
        .collect();
    for day in incoming {
        let entry = by_date.entry(day.date).or_insert(0);
        *entry = (*entry).max(day.release_count);
    }
    *days_out = by_date
        .into_iter()
        .map(|(date, release_count)| MacroScheduleDay { date, release_count })
        .collect();
}

async fn fetch_schedule_window(
    client: &reqwest::Client,
    kind: ScheduleKind,
    macro_profile: ScheduleMacroProfile,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
    tickers: &BTreeSet<String>,
) -> Result<ScheduleResponse> {
    let mut warnings = Vec::new();

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
        let mut m_days = Vec::new();
        let mut m_events = Vec::new();
        let mut warn = Vec::new();

        // All profiles (including Major) use the unified fetch path:
        // official_major (Census PDF) + BEA (exact times) + FRED API (Claims, JOLTS, IP, etc.).
        // The Major filter in service.rs::macro_profile_filter removes non-major events
        // after fetch. This ensures Claims (FRED release_id 180, in MAJOR specs) is included.
        // FRED API is supplementary only, gated on configured API key.
        let official_fut = timeout(
            TokioDuration::from_secs(SCHEDULE_HTTP_TIMEOUT_SECS),
            fetch_official_major_macro(start_date, end_date),
        );
        let bea_fut = timeout(
            TokioDuration::from_secs(SCHEDULE_HTTP_TIMEOUT_SECS),
            fetch_bea_macro_events(start_date, end_date),
        );
        let (official_res, bea_res, fred_api_res) =
            if crate::finance::credentials::has_fred_api_configuration_hint() {
                let fred_api_fut = timeout(
                    TokioDuration::from_secs(SCHEDULE_HTTP_TIMEOUT_SECS),
                    fetch_fred_macro_api_events(client, start_date, end_date, macro_profile.clone()),
                );
                let (official_res, bea_res, fred_api_res) =
                    tokio::join!(official_fut, bea_fut, fred_api_fut);
                (official_res, bea_res, Some(fred_api_res))
            } else {
                let (official_res, bea_res) = tokio::join!(official_fut, bea_fut);
                (official_res, bea_res, None)
            };

        match official_res {
            Ok(Ok((rows, days, official_warn))) => {
                m_events.extend(rows);
                m_days = days;
                warn.extend(official_warn);
            }
            Ok(Err(e)) => warn.push(format!("official macro supplemental failed: {e}")),
            Err(_) => warn.push("official macro supplemental: timed out".to_string()),
        }

        // BEA events first (reliable, exact times).
        match bea_res {
            Ok(Ok(rows)) => merge_macro_events(&mut m_events, rows),
            Ok(Err(e)) => warn.push(format!("bea calendar: {e}")),
            Err(_) => warn.push("bea calendar: timed out".to_string()),
        }

        if let Some(fred_api_res) = fred_api_res {
            match fred_api_res {
                Ok(Ok(rows)) => merge_macro_events(&mut m_events, rows),
                Ok(Err(e)) => warn.push(format!("fred api supplemental calendar: {e}")),
                Err(_) => warn.push("fred api supplemental calendar: timed out".to_string()),
            }
        }
        (m_events, m_days, warn)
    };

    let ((earnings, earn_warn), (macro_events, macro_days, macro_warn)) =
        tokio::join!(earnings_fut, macro_fut);
    warnings.extend(earn_warn);
    warnings.extend(macro_warn);

    Ok(ScheduleResponse {
        generated_at: Utc::now(),
        kind,
        macro_profile,
        start_date: start_date.to_string(),
        end_date: end_date.to_string(),
        earnings,
        macro_events,
        macro_days,
        warnings,
    })
}
