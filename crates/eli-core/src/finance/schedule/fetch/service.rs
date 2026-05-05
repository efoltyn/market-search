pub async fn fetch_schedule(req: ScheduleRequest) -> Result<ScheduleResponse> {
    let mut macro_profile = req.macro_profile.clone();
    if req.major_only {
        macro_profile = ScheduleMacroProfile::Major;
    }

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

    // Build a fresh native-TLS client per call. FRED's Akamai CDN fingerprints
    // reqwest even with native-tls — the shared LazyLock client gets ghosted.
    // A fresh client per call is marginally slower but more reliable.
    let client = reqwest::Client::builder()
        .use_native_tls()
        .http1_only()
        .timeout(StdDuration::from_secs(SCHEDULE_HTTP_TIMEOUT_SECS))
        .tcp_nodelay(true)
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
            fetch_schedule_window(
                &client,
                req.kind.clone(),
                macro_profile.clone(),
                chunk_start,
                chunk_end,
                &tickers,
            )
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

    // Row quality assertions: keep only usable rows.
    earnings.retain(|e| {
        !e.symbol.trim().is_empty()
            && !e.company_name.trim().is_empty()
            && !e.company_name.eq_ignore_ascii_case("n/a")
    });
    macro_events.retain(|e| !e.title.trim().is_empty());

    // Market cap filter
    if let Some(min_cap) = req.min_market_cap {
        earnings.retain(|e| {
            e.market_cap
                .map_or(false, |cap| (cap as f64) >= min_cap)
        });
    }

    // Time filter
    if let Some(ref time_filter) = req.time_filter {
        let tf = time_filter.trim().to_ascii_lowercase();
        earnings.retain(|e| {
            if let Some(ref t) = e.time {
                t.to_ascii_lowercase() == tf
            } else {
                tf == "not-supplied"
            }
        });
    }

    // De-dupe noisy repeated macro rows and apply profile filtering.
    {
        let mut seen = BTreeSet::new();
        macro_events.retain(|e| seen.insert((e.date.clone(), e.title.clone())));
    }
    macro_events.sort_by(|a, b| a.date.cmp(&b.date).then(a.title.cmp(&b.title)));
    match macro_profile {
        ScheduleMacroProfile::Broad => {}
        ScheduleMacroProfile::Major => {
            macro_events.retain(|e| {
                is_major_us_release_id(e.release_id) || is_major_us_macro_title(&e.title)
            });
        }
        ScheduleMacroProfile::Market => {
            let mut title_counts: BTreeMap<String, usize> = BTreeMap::new();
            for e in &macro_events {
                *title_counts.entry(e.title.clone()).or_insert(0) += 1;
            }
            macro_events.retain(|e| {
                if is_major_us_macro_title(&e.title) {
                    return true;
                }
                let repeated = title_counts.get(&e.title).copied().unwrap_or(0) >= 3;
                !(repeated && is_low_signal_macro_title(&e.title))
            });
        }
    }
    // FRED release 101 ("FOMC Press Release") fires on many dates — data refreshes,
    // minutes publications, not just rate decisions. Only keep FOMC entries on actual
    // FOMC meeting decision days (the last day of each 2-day meeting). The Fed
    // publishes these dates a year in advance; we hardcode 2025-2028.
    {
        macro_events.retain(|e| {
            if e.title != "FOMC Press Release" {
                return true;
            }
            let Some(d) = chrono::NaiveDate::parse_from_str(&e.date, "%Y-%m-%d").ok() else {
                return false;
            };
            is_fomc_decision_day(d)
        });
    }
    // Safety net: window.rs already calls fetch_official_major_macro for all profiles, so
    // the unified path normally produces CPI/PPI/PCE/GDP/Retail/Housing/NFP via the Census
    // PDF. Only re-fetch here if the filtered set somehow ended up empty or BEA-only — that
    // means the upstream call failed. Don't trigger this purely on `--major` (the unified
    // path covers Major correctly and re-running just emits a misleading warning).
    let needs_official_major_backfill = matches!(req.kind, ScheduleKind::Macro | ScheduleKind::All)
        && (macro_events.is_empty()
            || macro_events.iter().all(|e| e.source == "bea"));
    if needs_official_major_backfill {
        match fetch_official_major_macro(start_date, end_date).await {
            Ok((rows, days, official_warn)) if !rows.is_empty() => {
                let reason = if macro_events.is_empty() {
                    "using official major schedule fallback because filtered legacy macro result was empty"
                } else {
                    "using official major schedule fallback because filtered macro result only contained BEA events"
                };
                warnings.push(reason.to_string());
                merge_macro_events(&mut macro_events, rows);
                merge_macro_days(&mut macro_days, days);
                warnings.extend(official_warn);
            }
            Ok(_) => {}
            Err(e) => warnings.push(format!("official major fallback failed: {e}")),
        }
    }
    // Rebuild macro day counts from the filtered event set so `--major` really
    // reflects the major calendar instead of the provider's raw row count.
    if !macro_events.is_empty() {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for e in &macro_events {
            *counts.entry(e.date.clone()).or_insert(0) += 1;
        }
        macro_days = counts
            .into_iter()
            .map(|(date, release_count)| MacroScheduleDay { date, release_count })
            .collect();
    } else if macro_days.is_empty() {
        macro_days = Vec::new();
    }

    let warnings = compact_schedule_warnings(warnings);

    Ok(ScheduleResponse {
        generated_at: Utc::now(),
        kind: req.kind,
        macro_profile,
        start_date: req.start_date,
        end_date: req.end_date,
        earnings,
        macro_events,
        macro_days,
        warnings,
    })
}
