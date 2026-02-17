use super::super::macro_data::fetch::{
    fetch_fred_macro_counts, fetch_fred_macro_for_day, fetch_nasdaq_earnings_for_date,
    parse_schedule_date,
};
use super::super::*;

fn is_major_us_macro_title(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    let include = [
        "consumer price index",
        "cpi",
        "core pce",
        "personal consumption expenditures",
        "employment situation",
        "nonfarm payroll",
        "unemployment insurance weekly claims report",
        "state unemployment insurance weekly claims report",
        "fomc",
        "federal open market committee",
        "gdp",
        "gross domestic product",
        "retail sales",
        "producer price index",
        "ppi",
        "job openings",
        "jolts",
    ];
    let exclude = [
        "eurostat",
        "sonia",
        "coinbase",
        "ice bofa",
        "international financial statistics",
    ];
    include.iter().any(|k| t.contains(k)) && !exclude.iter().any(|k| t.contains(k))
}

fn compact_schedule_warnings(warnings: Vec<String>) -> Vec<String> {
    if warnings.is_empty() {
        return warnings;
    }

    let mut fred_macro_timeouts = 0usize;
    let mut nasdaq_earnings_timeouts = 0usize;
    let mut fred_macro_403 = 0usize;
    let mut fred_macro_502 = 0usize;
    let mut other = Vec::new();

    for w in warnings {
        if w.starts_with("fred macro ") && w.ends_with("timed out") {
            fred_macro_timeouts += 1;
            continue;
        }
        if w.starts_with("fred macro ")
            && (w.contains("http 403 Forbidden") || w.contains("http 403"))
        {
            fred_macro_403 += 1;
            continue;
        }
        if w.starts_with("fred macro ")
            && (w.contains("http 502 Bad Gateway") || w.contains("http 502"))
        {
            fred_macro_502 += 1;
            continue;
        }
        if w.starts_with("nasdaq earnings ") && w.ends_with("timed out") {
            nasdaq_earnings_timeouts += 1;
            continue;
        }
        other.push(w);
    }

    // Dedupe non-timeout warnings while preserving first-seen order.
    let mut seen = BTreeSet::new();
    other.retain(|w| seen.insert(w.clone()));

    let mut out = Vec::new();
    if fred_macro_timeouts > 0 {
        out.push(format!(
            "fred macro day fetches timed out: {fred_macro_timeouts}"
        ));
    }
    if fred_macro_403 > 0 {
        out.push(format!(
            "fred macro day fetches returned 403: {fred_macro_403}"
        ));
    }
    if fred_macro_502 > 0 {
        out.push(format!(
            "fred macro day fetches returned 502: {fred_macro_502}"
        ));
    }
    if nasdaq_earnings_timeouts > 0 {
        out.push(format!(
            "nasdaq earnings day fetches timed out: {nasdaq_earnings_timeouts}"
        ));
    }
    out.extend(other);

    const MAX_WARNINGS: usize = 10;
    if out.len() > MAX_WARNINGS {
        let omitted = out.len() - MAX_WARNINGS;
        out.truncate(MAX_WARNINGS);
        out.push(format!("additional warnings omitted: {omitted}"));
    }
    out
}

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
