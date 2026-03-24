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
