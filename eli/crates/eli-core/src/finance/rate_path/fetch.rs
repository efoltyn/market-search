use super::super::timeseries::fetch::fetch_fred_series;
use super::super::*;
use chrono::Datelike;
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Debug, Deserialize)]
struct OddsCsvRow {
    source: String,
    ticker: String,
    title: String,
    event_ticker: String,
    yes_price: String,
    probability: String,
}

#[derive(Debug, Clone)]
struct MeetingMeta {
    date: chrono::NaiveDate,
    label: String,
}

#[derive(Debug, Clone, Default)]
struct MeetingAgg {
    hold_prob: f64,
    cut_25bp_prob: f64,
    cut_50bp_plus_prob: f64,
    hike_prob: f64,
}

#[derive(Debug, Clone, Copy)]
enum FedBucket {
    Hold,
    Cut25,
    Cut50Plus,
    Hike,
}

fn month_from_token(token: &str) -> Option<u32> {
    match token.to_ascii_uppercase().as_str() {
        "JAN" => Some(1),
        "FEB" => Some(2),
        "MAR" => Some(3),
        "APR" => Some(4),
        "MAY" => Some(5),
        "JUN" => Some(6),
        "JUL" => Some(7),
        "AUG" => Some(8),
        "SEP" => Some(9),
        "OCT" => Some(10),
        "NOV" => Some(11),
        "DEC" => Some(12),
        _ => None,
    }
}

fn parse_meeting_from_token(text: &str) -> Option<MeetingMeta> {
    let re = regex::Regex::new(r"(?i)(\d{2})(JAN|FEB|MAR|APR|MAY|JUN|JUL|AUG|SEP|OCT|NOV|DEC)").ok()?;
    let caps = re.captures(text)?;
    let yy = caps.get(1)?.as_str().parse::<i32>().ok()?;
    let month = month_from_token(caps.get(2)?.as_str())?;
    let year = 2000 + yy;
    let date = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    let label = format!("{} {} Meeting", date.format("%B"), date.year());
    Some(MeetingMeta { date, label })
}

fn parse_meeting_from_title(title: &str) -> Option<MeetingMeta> {
    let re = regex::Regex::new(
        r"(?i)(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:tember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\s+(20\d{2})",
    )
    .ok()?;
    let caps = re.captures(title)?;
    let month_token = caps.get(1)?.as_str();
    let month = month_from_token(&month_token[..3])?;
    let year = caps.get(2)?.as_str().parse::<i32>().ok()?;
    let date = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    let label = format!("{} {} Meeting", date.format("%B"), date.year());
    Some(MeetingMeta { date, label })
}

fn classify_bucket(text: &str, current_rate: f64) -> Option<FedBucket> {
    let t = text.to_ascii_lowercase();
    if t.contains("hold") || t.contains("no change") {
        return Some(FedBucket::Hold);
    }
    if t.contains("hike") || t.contains("raise") || t.contains("increase") {
        return Some(FedBucket::Hike);
    }
    if t.contains("cut") || t.contains("lower") || t.contains("decrease") {
        if t.contains("50") || t.contains("0.50") || t.contains("0.5") || t.contains("half") || t.contains("50bp") {
            return Some(FedBucket::Cut50Plus);
        }
        return Some(FedBucket::Cut25);
    }
    // Kalshi Fed contracts often encode a terminal/target rate in ticker suffixes like "-T3.50".
    if let Ok(re) = regex::Regex::new(r"(?i)[-_]T(\d+(?:\.\d+)?)") {
        if let Some(caps) = re.captures(&t) {
            if let Some(target) = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok()) {
                let diff = target - current_rate;
                if diff.abs() < 0.125 {
                    return Some(FedBucket::Hold);
                }
                if diff < -0.375 {
                    return Some(FedBucket::Cut50Plus);
                }
                if diff < 0.0 {
                    return Some(FedBucket::Cut25);
                }
                if diff > 0.0 {
                    return Some(FedBucket::Hike);
                }
            }
        }
    }
    None
}

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
