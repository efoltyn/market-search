pub async fn fetch_rate_path(req: RatePathRequest) -> Result<RatePathResponse> {
    let cache_dir = odds_cache_dir(&req);
    let csv_path = cache_dir.join("all_markets.csv");
    if !csv_path.exists() {
        return Err(Error::InvalidInput(format!(
            "no local prediction market cache found at {}. Run `eli finance sync` first.",
            csv_path.display()
        )));
    }

    let mode = req.source_mode.clone().unwrap_or(RatePathSourceMode::Auto);
    let current_rate = fetch_current_fed_funds().await?;
    let as_of = cache_as_of(&csv_path);
    let now = Utc::now();
    let age_seconds = (now - as_of).num_seconds().max(0);

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&csv_path)
        .map_err(|e| Error::Provider(format!("failed reading {}: {e}", csv_path.display())))?;

    let mut meetings: BTreeMap<chrono::NaiveDate, (MeetingMeta, MeetingAgg)> = BTreeMap::new();
    let mut annual_cuts: BTreeMap<i32, HashMap<u32, f64>> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
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
            continue;
        }
        if mode == RatePathSourceMode::Fallback {
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
        {
            continue;
        }

        let meeting = parse_meeting_from_token(&row.event_ticker)
            .or_else(|| parse_meeting_from_token(&row.ticker))
            .or_else(|| parse_meeting_from_title(&row.title));
        let Some(meeting) = meeting else {
            warnings.push(format!("could not infer meeting for {}", row.ticker));
            continue;
        };

        let bucket = classify_bucket(&source_text, current_rate);
        let Some(bucket) = bucket else {
            continue;
        };

        let entry = meetings
            .entry(meeting.date)
            .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));

        match bucket {
            FedBucket::Hold => entry.1.hold_prob = entry.1.hold_prob.max(prob),
            FedBucket::Cut25 => entry.1.cut_25bp_prob = entry.1.cut_25bp_prob.max(prob),
            FedBucket::Cut50Plus => entry.1.cut_50bp_plus_prob = entry.1.cut_50bp_plus_prob.max(prob),
            FedBucket::Hike => entry.1.hike_prob = entry.1.hike_prob.max(prob),
        }
    }

    if meetings.is_empty() {
        if mode == RatePathSourceMode::Meeting {
            return Err(Error::Provider(
                "no meeting-level fed decision markets found in local CSV cache".to_string(),
            ));
        }
        if let Some(m) = build_fallback_meeting(&annual_cuts, current_rate, &mut warnings)? {
            return Ok(RatePathResponse {
                generated_at: now,
                as_of,
                age_seconds,
                current_rate,
                meetings: vec![m],
                source_mode: "fallback".to_string(),
                coverage_ratio: 1.0,
                confidence: 0.65,
                warnings,
            });
        }
        return Err(Error::Provider("no fallback fed-cuts markets found in local CSV cache".to_string()));
    }

    let mut implied_rate = current_rate;
    let mut out = Vec::new();
    for (_date_key, (meta, agg)) in meetings {
        let expected_delta = (-0.25 * agg.cut_25bp_prob)
            + (-0.50 * agg.cut_50bp_plus_prob)
            + (0.25 * agg.hike_prob);
        implied_rate += expected_delta;
        out.push(RatePathMeeting {
            date: meta.date.to_string(),
            label: meta.label,
            hold_prob: agg.hold_prob,
            cut_25bp_prob: agg.cut_25bp_prob,
            cut_50bp_plus_prob: agg.cut_50bp_plus_prob,
            hike_prob: agg.hike_prob,
            implied_rate,
            source: "kalshi_csv".to_string(),
        });
    }

    let coverage_ratio = (out.len() as f64 / 8.0).clamp(0.0, 1.0);
    let confidence = (0.55 + (0.35 * coverage_ratio)).clamp(0.0, 0.98);
    Ok(RatePathResponse {
        generated_at: now,
        as_of,
        age_seconds,
        current_rate,
        meetings: out,
        source_mode: "meeting".to_string(),
        coverage_ratio,
        confidence,
        warnings,
    })
}
