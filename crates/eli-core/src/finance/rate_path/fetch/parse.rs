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

/// Real FOMC decision-day calendar. Same source as
/// `crates/eli-core/src/finance/schedule/fetch/filters.rs:24` — duplicated here
/// so rate_path can snap month-only references (e.g. "September 2026 meeting")
/// to the actual decision day instead of using the 1st of the month placeholder.
const FOMC_DECISION_DAYS: &[(i32, u32, u32)] = &[
    (2025, 1, 29), (2025, 3, 19), (2025, 5, 7), (2025, 6, 18),
    (2025, 7, 30), (2025, 9, 17), (2025, 10, 29), (2025, 12, 10),
    (2026, 1, 28), (2026, 3, 18), (2026, 4, 29), (2026, 6, 17),
    (2026, 7, 29), (2026, 9, 16), (2026, 10, 28), (2026, 12, 9),
    (2027, 1, 27), (2027, 3, 17), (2027, 4, 28), (2027, 6, 9),
    (2027, 7, 28), (2027, 9, 15), (2027, 10, 27), (2027, 12, 8),
    (2028, 1, 26),
];

/// Resolve (year, month) to the actual FOMC decision day. Falls back to the
/// 1st of the month when the meeting is not in our hardcoded calendar.
fn fomc_decision_date(year: i32, month: u32) -> chrono::NaiveDate {
    for (y, m, d) in FOMC_DECISION_DAYS {
        if *y == year && *m == month {
            if let Some(date) = chrono::NaiveDate::from_ymd_opt(*y, *m, *d) {
                return date;
            }
        }
    }
    chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_else(|| {
        chrono::NaiveDate::from_ymd_opt(year, 1, 1).unwrap()
    })
}

fn parse_meeting_from_token(text: &str) -> Option<MeetingMeta> {
    let re = regex::Regex::new(r"(?i)(\d{2})(JAN|FEB|MAR|APR|MAY|JUN|JUL|AUG|SEP|OCT|NOV|DEC)").ok()?;
    let caps = re.captures(text)?;
    let yy = caps.get(1)?.as_str().parse::<i32>().ok()?;
    let month = month_from_token(caps.get(2)?.as_str())?;
    let year = 2000 + yy;
    let date = fomc_decision_date(year, month);
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
    let date = fomc_decision_date(year, month);
    let label = format!("{} {} Meeting", date.format("%B"), date.year());
    Some(MeetingMeta { date, label })
}

/// Recognises the Polymarket short-form "Fed Decision in <Month>?" event title.
/// These markets are anchored to the next FOMC meeting in the named month.
fn parse_meeting_from_short_title(title: &str, default_year: i32) -> Option<MeetingMeta> {
    let re = regex::Regex::new(
        r"(?i)Fed Decision in (january|february|march|april|may|june|july|august|september|october|november|december)\??$",
    ).ok()?;
    let caps = re.captures(title.trim())?;
    let month = month_from_token(&caps.get(1)?.as_str()[..3])?;
    let date = fomc_decision_date(default_year, month);
    let label = format!("{} {} Meeting", date.format("%B"), date.year());
    Some(MeetingMeta { date, label })
}

/// Detect joint multi-meeting Polymarket events: "Fed decisions (Mar-Jun)" /
/// "Fed decisions (Apr-Jul)" / "Fed decisions (Jun-Sep)". Per-meeting buckets
/// MUST exclude these — they are 3-meeting compound probabilities that decay
/// joint outcomes, not single-meeting marginals.
fn is_compound_meeting_market(title: &str) -> bool {
    let re_event = regex::Regex::new(r"(?i)Fed decisions \([A-Z][a-z]+-[A-Z][a-z]+\)").unwrap();
    if re_event.is_match(title) {
        return true;
    }
    // Per-market wording inside compound events:
    // "Will the Fed Pause–Pause–Pause in the next three decisions (Mar–Apr–Jun)?"
    let lower = title.to_ascii_lowercase();
    if lower.contains("next three decisions") {
        return true;
    }
    // Hyphen and en-dash variants of pause-pause / cut-pause
    if lower.contains("pause–pause") || lower.contains("pause-pause")
        || lower.contains("cut–pause") || lower.contains("cut-pause")
    {
        return true;
    }
    false
}

/// Extract the compound label "Mar-Jun" from "Fed decisions (Mar-Jun)" titles.
fn parse_compound_label(title: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)Fed decisions \(([A-Z][a-z]+-[A-Z][a-z]+)\)").ok()?;
    re.captures(title)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
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

