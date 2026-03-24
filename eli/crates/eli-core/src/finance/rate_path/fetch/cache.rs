fn odds_cache_dir(req: &RatePathRequest) -> PathBuf {
    req.cache_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("", "", "eli")
                .map(|d| d.cache_dir().join("odds"))
                .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"))
        })
}

fn cache_as_of(csv_path: &Path) -> DateTime<Utc> {
    std::fs::metadata(csv_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .or(Some(SystemTime::now()))
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now)
}

fn build_fallback_meeting(
    annual_cuts: &BTreeMap<i32, HashMap<u32, f64>>,
    warnings: &mut Vec<String>,
) -> Result<Option<RatePathMeeting>> {
    let current_year = Utc::now().year();
    let fallback_year = annual_cuts
        .keys()
        .copied()
        .filter(|y| *y >= current_year)
        .min()
        .or_else(|| annual_cuts.keys().copied().max());

    if let Some(year) = fallback_year {
        if let Some(dist) = annual_cuts.get(&year) {
            let raw_hold = dist.get(&0).copied().unwrap_or(0.0).max(0.0);
            let raw_cut_25 = dist.get(&1).copied().unwrap_or(0.0).max(0.0);
            let raw_cut_50p: f64 = dist
                .iter()
                .filter(|(cuts, _)| **cuts >= 2)
                .map(|(_, p)| p.max(0.0))
                .sum();
            let raw_hike = 0.0_f64;
            // Normalize so probabilities sum to 1.0
            let sum = raw_hold + raw_cut_25 + raw_cut_50p + raw_hike;
            let (hold_prob, cut_25bp_prob, cut_50bp_plus_prob, hike_prob) = if sum > 0.0 {
                (raw_hold / sum, raw_cut_25 / sum, raw_cut_50p / sum, raw_hike / sum)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            let date = chrono::NaiveDate::from_ymd_opt(year, 12, 31)
                .ok_or_else(|| Error::Provider(format!("invalid fallback year: {year}")))?;
            warnings.push(
                "no meeting-level fed decision markets found; using annual Fed-cuts distribution fallback"
                    .to_string(),
            );
            return Ok(Some(RatePathMeeting {
                date: date.to_string(),
                label: format!("December {year} (annual distribution fallback)"),
                hold_prob,
                cut_prob: cut_25bp_prob + cut_50bp_plus_prob,
                cut_25bp_prob,
                cut_50bp_plus_prob,
                hike_prob,
                volume: 0,
            }));
        }
    }
    Ok(None)
}
