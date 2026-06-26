pub async fn fetch_rate_path(req: RatePathRequest) -> Result<RatePathResponse> {
    let cache_dir = odds_cache_dir(&req);
    let csv_path = cache_dir.join("all_markets.csv");

    // NOTE: req.source_mode is accepted for backwards compatibility but the
    // live path now ALWAYS dominates. The CSV cache fallback is preserved for
    // disaster-recovery (no network), but the historical auto/meeting/fallback
    // distinction was a no-op — kept the request field, dropped the branching.
    let _mode_compat = req.source_mode.clone();
    let current_rates = fetch_current_fed_rates().await?;
    let current_rate = current_rates.classification_anchor_rate;
    let now = Utc::now();

    let csv_exists = csv_path.exists();
    let csv_stale = csv_exists
        && cache_as_of(&csv_path)
            .signed_duration_since(now)
            .num_seconds()
            .abs()
            > 86400;

    if !csv_exists || csv_stale {
        match fetch_rate_path_live(current_rate).await {
            Ok((meetings, cumulative_signals, mut live_warnings, extras)) => {
                if !meetings.is_empty() {
                    // Filter out meetings whose date has passed. Also drop
                    // pure-noise far-future meetings (single-binary <$3K vol
                    // pins on 2027/2028 dates) — they have no information
                    // value and just clutter the output. Keep anything in
                    // 2026 or with reasonable depth.
                    let today = now.date_naive();
                    let near_horizon = chrono::NaiveDate::from_ymd_opt(today.year(), 12, 31)
                        .unwrap_or(today);
                    let out: Vec<RatePathMeeting> = meetings
                        .into_iter()
                        .filter(|(date_key, (_meta, agg))| {
                            if *date_key < today { return false; }
                            // Always keep meetings in the current calendar year.
                            if *date_key <= near_horizon { return true; }
                            // For future-year meetings, require either ≥3
                            // markets aggregated OR ≥$5K volume.
                            agg.n_markets >= 3 || agg.volume >= 5_000
                        })
                        .map(|(_date_key, (meta, agg))| {
                            let (h, c25, c50, hk) = agg.weighted();
                            let (hold_prob, cut_25bp_prob, cut_50bp_plus_prob, hike_prob) =
                                normalize_bucket_probs(h, c25, c50, hk);
                            RatePathMeeting {
                                date: meta.date.to_string(),
                                label: meta.label,
                                hold_prob,
                                cut_prob: cut_25bp_prob + cut_50bp_plus_prob,
                                cut_25bp_prob,
                                cut_50bp_plus_prob,
                                hike_prob,
                                volume: agg.volume,
                                volume_total: agg.volume,
                                n_markets: agg.n_markets,
                            }
                        })
                        .collect();
                    let coverage_ratio = (out.len() as f64 / 8.0).clamp(0.0, 1.0);
                    if coverage_ratio < 0.5 {
                        live_warnings.push(format!(
                            "incomplete meeting coverage: only {} of ~8 expected FOMC meetings",
                            out.len()
                        ));
                    }
                    let mut rate_warnings = Vec::new();
                    if current_rates.classification_anchor_basis != "target_midpoint" {
                        rate_warnings.push(format!(
                            "rate-path classification anchor fell back to {}",
                            current_rates.classification_anchor_basis
                        ));
                    }
                    rate_warnings.extend(live_warnings);
                    return Ok(RatePathResponse {
                        generated_at: now,
                        as_of: now,
                        age_seconds: 0,
                        current_rate,
                        current_rate_basis: current_rates.classification_anchor_basis.clone(),
                        current_rates,
                        meetings: out,
                        source_mode: "live".to_string(),
                        coverage_ratio,
                        warnings: rate_warnings,
                        cumulative_signals,
                        year_view: extras.year_view,
                        compound_paths: extras.compound_paths,
                    });
                }
                // If live returned no meetings, fall through to CSV (if it exists).
            }
            Err(e) => {
                if !csv_exists {
                    return Err(Error::Provider(format!(
                        "no CSV cache and live API fetch failed: {e}. Run `eli finance sync` or check network."
                    )));
                }
                // Fall through to CSV path.
            }
        }
    }

    // CSV-based path (original logic).
    if !csv_exists {
        return Err(Error::InvalidInput(format!(
            "no local prediction market cache found at {}. Run `eli finance sync` first.",
            csv_path.display()
        )));
    }

    let as_of = cache_as_of(&csv_path);
    let age_seconds = (now - as_of).num_seconds().max(0);

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&csv_path)
        .map_err(|e| Error::Provider(format!("failed reading {}: {e}", csv_path.display())))?;

    let mut meetings: BTreeMap<chrono::NaiveDate, (MeetingMeta, MeetingAgg)> = BTreeMap::new();
    let mut annual_cuts: BTreeMap<i32, HashMap<u32, f64>> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    if current_rates.classification_anchor_basis != "target_midpoint" {
        warnings.push(format!(
            "rate-path classification anchor fell back to {} because the target range series was unavailable",
            current_rates.classification_anchor_basis
        ));
    }
    let annual_cuts_re =
        regex::Regex::new(r"(?i)\bwill\s+(no|\d+)\s+fed rate cuts?\s+happen in\s+(20\d{2})\b")
            .map_err(|e| Error::Provider(format!("rate-path regex compile failed: {e}")))?;

    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut prob = row.probability.trim().parse::<f64>().unwrap_or(0.0);
        if prob <= 0.0 {
            prob = row.yes_price.trim().parse::<f64>().unwrap_or(0.0) / 100.0;
        }
        prob = prob.clamp(0.0, 1.0);
        let vol: i64 = row.volume.trim().parse::<f64>().unwrap_or(0.0) as i64;

        if row.source.trim().eq_ignore_ascii_case("polymarket") {
            if let Some(caps) = annual_cuts_re.captures(&row.title) {
                let cuts_raw = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
                let cuts = if cuts_raw.eq_ignore_ascii_case("no") {
                    0
                } else {
                    cuts_raw.parse::<u32>().unwrap_or(0)
                };
                if let Some(year) = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()) {
                    annual_cuts
                        .entry(year)
                        .or_default()
                        .entry(cuts)
                        .and_modify(|v| *v = v.max(prob))
                        .or_insert(prob);
                }
            }
            // Also extract per-meeting data from Polymarket titles like
            // "Will the Fed decrease interest rates by 25 bps after the March 2026 meeting?"
            // Skip joint multi-meeting compound markets — they encode joint
            // outcomes (e.g. "Pause-Pause-Pause") and would drag per-meeting
            // marginals if added to the bucket aggregator.
            if !is_compound_meeting_market(&row.title) {
                if let Some(meeting) = parse_meeting_from_title(&row.title) {
                    if let Some(bucket) = classify_bucket(&row.title, current_rate) {
                        if vol < MIN_MARKET_VOLUME {
                            continue;
                        }
                        let entry = meetings
                            .entry(meeting.date)
                            .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));
                        entry.1.add(bucket, prob, vol);
                    }
                }
            }
            continue;
        }
        if row.source.trim().to_ascii_lowercase() != "kalshi" {
            continue;
        }

        let source_text = format!("{} {} {}", row.ticker, row.event_ticker, row.title);
        let upper = source_text.to_ascii_uppercase();
        if !upper.contains("KXFED")
            && !upper.contains("FED DECISION")
            && !upper.contains("FOMC")
            && !upper.contains("FED DECREASE INTEREST")
            && !upper.contains("FED INCREASE INTEREST")
            && !upper.contains("FED RATE CUT")
            && !upper.contains("FED RATE HIKE")
            && !upper.contains("FEDERAL RESERVE")
            && !(upper.contains("FED")
                && (upper.contains("INTEREST RATE") || upper.contains("BPS")))
        {
            continue;
        }

        // Only KXFEDDECISION per-meeting bucket contracts have the exclusive
        // probability structure (H0/C25/C26/H25/H26) we need.
        // All other KXFED series have incompatible semantics and are skipped.
        let ev_upper = row.event_ticker.to_ascii_uppercase();
        if ev_upper.starts_with("KXFED") && !ev_upper.starts_with("KXFEDDECISION") {
            continue;
        }

        let meeting = parse_meeting_from_token(&row.event_ticker)
            .or_else(|| parse_meeting_from_token(&row.ticker))
            .or_else(|| parse_meeting_from_title(&row.title));
        let Some(meeting) = meeting else {
            continue; // non-meeting KXFEDDECISION variants — skip silently
        };

        let bucket = classify_bucket(&source_text, current_rate);
        let Some(bucket) = bucket else {
            continue;
        };

        if vol < MIN_MARKET_VOLUME {
            continue;
        }
        let entry = meetings
            .entry(meeting.date)
            .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));
        entry.1.add(bucket, prob, vol);
    }

    // Extract cumulative "hike/cut by date X" signals from Kalshi titles.
    let cumulative_re = regex::Regex::new(
        r"(?i)Federal Reserve (hike|cut)\s+rates?\s+by\s+((?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2},?\s+\d{4})"
    ).ok();
    let mut cumulative_signals: Vec<CumulativeFedSignal> = Vec::new();
    if let Some(ref re) = cumulative_re {
        let mut rdr2 = csv::ReaderBuilder::new()
            .flexible(true)
            .from_path(&csv_path)
            .map_err(|e| Error::Provider(format!("failed reading {}: {e}", csv_path.display())))?;
        for row in rdr2.deserialize::<OddsCsvRow>() {
            let row = match row {
                Ok(r) => r,
                Err(_) => continue,
            };
            if !row.source.trim().eq_ignore_ascii_case("kalshi") {
                continue;
            }
            if let Some(caps) = re.captures(&row.title) {
                let direction = caps
                    .get(1)
                    .map(|m| m.as_str().to_lowercase())
                    .unwrap_or_default();
                let date_str = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
                let mut prob = row.probability.trim().parse::<f64>().unwrap_or(0.0);
                if prob <= 0.0 {
                    prob = row.yes_price.trim().parse::<f64>().unwrap_or(0.0) / 100.0;
                }
                prob = prob.clamp(0.0, 1.0);
                if prob > 0.01 {
                    cumulative_signals.push(CumulativeFedSignal {
                        direction,
                        by_date: date_str.to_string(),
                        probability: prob,
                        title: row.title.clone(),
                    });
                }
            }
        }
        sort_cumulative_signals_by_date(&mut cumulative_signals);
        cumulative_signals.dedup_by(|a, b| a.title == b.title);
    }

    if meetings.is_empty() {
        if let Some(m) = build_fallback_meeting(&annual_cuts, &mut warnings)? {
            return Ok(RatePathResponse {
                generated_at: now,
                as_of,
                age_seconds,
                current_rate,
                current_rate_basis: current_rates.classification_anchor_basis.clone(),
                current_rates: current_rates.clone(),
                meetings: vec![m],
                source_mode: "fallback".to_string(),
                coverage_ratio: 1.0,
                warnings,
                cumulative_signals,
                year_view: None,
                compound_paths: Vec::new(),
            });
        }
        return Err(Error::Provider(
            "no fallback fed-cuts markets found in local CSV cache".to_string(),
        ));
    }

    // Filter out past meetings — their probabilities are stale from settled contracts.
    // Dates are now snapped to actual FOMC decision days (see fomc_decision_date)
    // so we can exclude any meeting whose decision day has already passed.
    let today = now.date_naive();
    let out: Vec<RatePathMeeting> = meetings
        .into_iter()
        .filter(|(date_key, _)| *date_key >= today)
        .map(|(_date_key, (meta, agg))| {
            let (h, c25, c50, hk) = agg.weighted();
            let (hold_prob, cut_25bp_prob, cut_50bp_plus_prob, hike_prob) =
                normalize_bucket_probs(h, c25, c50, hk);
            RatePathMeeting {
                date: meta.date.to_string(),
                label: meta.label,
                hold_prob,
                cut_prob: cut_25bp_prob + cut_50bp_plus_prob,
                cut_25bp_prob,
                cut_50bp_plus_prob,
                hike_prob,
                volume: agg.volume,
                volume_total: agg.volume,
                n_markets: agg.n_markets,
            }
        })
        .collect();

    let coverage_ratio = (out.len() as f64 / 8.0).clamp(0.0, 1.0);
    if coverage_ratio < 0.5 {
        warnings.push(format!(
            "incomplete meeting coverage: only {} of ~8 expected FOMC meetings found in cache. \
             Probabilities may be unreliable. Re-run plain 'eli finance sync' for complete data.",
            out.len()
        ));
    }
    Ok(RatePathResponse {
        generated_at: now,
        as_of,
        age_seconds,
        current_rate,
        current_rate_basis: current_rates.classification_anchor_basis.clone(),
        current_rates,
        meetings: out,
        source_mode: "meeting".to_string(),
        coverage_ratio,
        warnings,
        cumulative_signals,
        year_view: None,
        compound_paths: Vec::new(),
    })
}

/// Sort cumulative_signals chronologically by parsed `by_date` ("June 30, 2026" → date).
/// Falls back to alphabetical for entries that don't parse. Dedupes ties alphabetically by title.
fn sort_cumulative_signals_by_date(signals: &mut Vec<CumulativeFedSignal>) {
    signals.sort_by(|a, b| {
        let pa = chrono::NaiveDate::parse_from_str(&a.by_date, "%B %d, %Y").ok();
        let pb = chrono::NaiveDate::parse_from_str(&b.by_date, "%B %d, %Y").ok();
        match (pa, pb) {
            (Some(da), Some(db)) => da.cmp(&db).then_with(|| a.title.cmp(&b.title)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.by_date.cmp(&b.by_date).then_with(|| a.title.cmp(&b.title)),
        }
    });
}

fn normalize_bucket_probs(hold: f64, cut25: f64, cut50p: f64, hike: f64) -> (f64, f64, f64, f64) {
    let values = [
        hold.max(0.0),
        cut25.max(0.0),
        cut50p.max(0.0),
        hike.max(0.0),
    ];
    let sum: f64 = values.iter().sum();
    if sum <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    // Always normalize so probabilities sum to 1.0.
    // Raw market prices rarely sum to exactly 1.0 due to bid-ask spreads,
    // vig, and incomplete outcome coverage — dividing by the sum is standard.
    (
        values[0] / sum,
        values[1] / sum,
        values[2] / sum,
        values[3] / sum,
    )
}
