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
    current_rate: f64,
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
            let hold_prob = *dist.get(&0).unwrap_or(&0.0);
            let cut_25bp_prob = *dist.get(&1).unwrap_or(&0.0);
            let cut_50bp_plus_prob = dist
                .iter()
                .filter(|(cuts, _)| **cuts >= 2)
                .map(|(_, p)| *p)
                .sum::<f64>();
            let expected_cuts = dist
                .iter()
                .map(|(cuts, p)| (*cuts as f64) * *p)
                .sum::<f64>();
            let implied_rate = current_rate - (0.25 * expected_cuts);
            let date = chrono::NaiveDate::from_ymd_opt(year, 12, 31)
                .ok_or_else(|| Error::Provider(format!("invalid fallback year: {year}")))?;
            warnings.push(
                "no meeting-level fed decision markets found; using annual Fed-cuts distribution fallback"
                    .to_string(),
            );
            return Ok(Some(RatePathMeeting {
                date: date.to_string(),
                label: format!("December {year} Meeting"),
                hold_prob,
                cut_25bp_prob,
                cut_50bp_plus_prob,
                hike_prob: 0.0,
                implied_rate,
                source: "polymarket_csv_annual_cuts".to_string(),
            }));
        }
    }
    Ok(None)
}

