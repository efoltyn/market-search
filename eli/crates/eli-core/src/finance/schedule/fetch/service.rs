pub async fn fetch_schedule(req: ScheduleRequest) -> Result<ScheduleResponse> {
    let start_date = parse_schedule_date(&req.start_date)?;
    let end_date = parse_schedule_date(&req.end_date)?;
    if end_date < start_date {
        return Err(Error::InvalidInput(
            "end_date must be >= start_date".to_string(),
        ));
    }

    let tickers: BTreeSet<String> = req
        .tickers
        .iter()
        .map(|t| t.trim().to_ascii_uppercase())
        .filter(|t| !t.is_empty())
        .collect();

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(SCHEDULE_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| Error::Provider(format!("schedule client init failed: {e}")))?;

    // For wider windows, split into <=61 day chunks so users don't have to manually shard.
    const MAX_WINDOW_DAYS: i64 = 60;
    let mut warnings = Vec::new();
    let mut earnings = Vec::new();
    let mut macro_events = Vec::new();
    let mut macro_days = Vec::new();
    let mut chunk_start = start_date;
    while chunk_start <= end_date {
        let chunk_end = std::cmp::min(
            end_date,
            chunk_start
                .checked_add_signed(chrono::Duration::days(MAX_WINDOW_DAYS))
                .ok_or_else(|| Error::Provider("date window overflow".to_string()))?,
        );
        let chunk =
            fetch_schedule_window(&client, req.kind.clone(), chunk_start, chunk_end, &tickers)
                .await?;
        earnings.extend(chunk.earnings);
        macro_events.extend(chunk.macro_events);
        macro_days.extend(chunk.macro_days);
        warnings.extend(chunk.warnings);

        if chunk_end == end_date {
            break;
        }
        chunk_start = chunk_end
            .succ_opt()
            .ok_or_else(|| Error::Provider("date iteration overflow".to_string()))?;
    }

    // De-dupe noisy repeated macro rows and optionally keep only major US releases.
    if req.major_only {
        macro_events.retain(|e| is_major_us_macro_title(&e.title));
    }
    {
        let mut seen = BTreeSet::new();
        macro_events.retain(|e| seen.insert((e.date.clone(), e.title.clone())));
    }
    macro_events.sort_by(|a, b| a.date.cmp(&b.date).then(a.title.cmp(&b.title)));
    if req.major_only {
        let mut last_fomc_date: Option<chrono::NaiveDate> = None;
        macro_events.retain(|e| {
            if e.title != "FOMC Press Release" {
                return true;
            }
            let d = chrono::NaiveDate::parse_from_str(&e.date, "%Y-%m-%d").ok();
            match (last_fomc_date, d) {
                (Some(prev), Some(cur)) if (cur - prev).num_days() < 14 => false,
                (_, Some(cur)) => {
                    last_fomc_date = Some(cur);
                    true
                }
                _ => true,
            }
        });
    }
    // If provider count endpoint failed, backfill macro_days from event rows.
    if macro_days.is_empty() && !macro_events.is_empty() {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for e in &macro_events {
            *counts.entry(e.date.clone()).or_insert(0) += 1;
        }
        macro_days = counts
            .into_iter()
            .map(|(date, release_count)| MacroScheduleDay { date, release_count })
            .collect();
    }

    let warnings = compact_schedule_warnings(warnings);

    Ok(ScheduleResponse {
        generated_at: Utc::now(),
        kind: req.kind,
        start_date: req.start_date,
        end_date: req.end_date,
        earnings,
        macro_events,
        macro_days,
        warnings,
    })
}

