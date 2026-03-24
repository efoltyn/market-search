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
    // "-H0" suffix = Kalshi encoding for "hike by 0bps" = Hold
    // Also catch "hike rates by 0bps" in the title text
    if t.contains("hold") || t.contains("no change") || t.contains("maintain")
        || t.contains("-h0")
        || (t.contains("hike") && t.contains("0bps"))
    {
        return Some(FedBucket::Hold);
    }
    if t.contains("hike") || t.contains("raise") || t.contains("increase") {
        return Some(FedBucket::Hike);
    }
    if t.contains("cut") || t.contains("lower") || t.contains("decrease") {
        // -C26 = Kalshi suffix for "cut by >25bps" (i.e. 50bp, 75bp, 100bp).
        // Also catch explicit "50bp/bps" wording, but NOT bare "50" which
        // falsely matches target-rate strings like "3.50%".
        if t.contains("-c26")
            || t.contains(">25bps") || t.contains(">25 bps") || t.contains(">25bp")
            || t.contains("50bps") || t.contains("50bp")
            || t.contains("0.50%") || t.contains("half")
        {
            return Some(FedBucket::Cut50Plus);
        }
        return Some(FedBucket::Cut25);
    }
    // Kalshi Fed contracts often encode a terminal/target rate in ticker suffixes like "-T3.50".
    // Round current_rate to nearest 0.25 (Fed target grid) to avoid effective-vs-target mismatch
    // e.g. FRED FEDFUNDS effective rate 3.64 vs Kalshi -T375 target 3.75 → diff 0.11 → Hold ✓
    let current_rate_rounded = (current_rate / 0.25).round() * 0.25;
    if let Ok(re) = regex::Regex::new(r"(?i)[-_]T(\d+(?:\.\d+)?)") {
        if let Some(caps) = re.captures(&t) {
            if let Some(target) = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok()) {
                let diff = target - current_rate_rounded;
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

