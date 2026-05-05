pub(crate) fn parse_schedule_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| Error::InvalidInput(format!("invalid date '{s}' (expected YYYY-MM-DD)")))
}

fn parse_nasdaq_time(raw: Option<&str>) -> Option<String> {
    let v = raw?.trim();
    if v.is_empty() {
        return None;
    }
    Some(match v {
        "time-pre-market" => "pre-market".to_string(),
        "time-after-hours" => "after-hours".to_string(),
        "time-not-supplied" => "not-supplied".to_string(),
        other => other.to_string(),
    })
}

/// Strip Nasdaq's "$1,234.56"-style decoration. Returns the numeric portion only,
/// or None when the cleaned string is empty / clearly not numeric (e.g. "n/a").
fn clean_money_str(s: &str) -> Option<String> {
    let cleaned: String = s
        .trim()
        .trim_start_matches('$')
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("n/a") {
        return None;
    }
    Some(cleaned)
}

/// Parse "$1.06" / "1.06" / "$1,234.56" → 1.06 / 1.06 / 1234.56. Negative values
/// like "($0.12)" are returned as -0.12.
fn parse_money_f64(s: &str) -> Option<f64> {
    // Handle "(0.12)" parenthetical-negative convention.
    let s_trimmed = s.trim();
    let (negate, body) = if s_trimmed.starts_with('(') && s_trimmed.ends_with(')') {
        (true, &s_trimmed[1..s_trimmed.len() - 1])
    } else {
        (false, s_trimmed)
    };
    let cleaned = clean_money_str(body)?;
    let v: f64 = cleaned.parse().ok()?;
    Some(if negate { -v } else { v })
}

/// Parse "$587,802,343,381" → 587_802_343_381. Decimals are truncated; negatives
/// return None (market caps are non-negative).
fn parse_money_u64(s: &str) -> Option<u64> {
    let cleaned = clean_money_str(s)?;
    if let Ok(n) = cleaned.parse::<u64>() {
        return Some(n);
    }
    // Tolerate decimal-form ints like "1.0" by truncating.
    let v: f64 = cleaned.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some(v as u64)
}

/// Parse loose integer strings like "11" / " 11 " → 11.
fn parse_u32_loose(s: &str) -> Option<u32> {
    let cleaned = clean_money_str(s)?;
    cleaned.parse::<u32>().ok()
}
