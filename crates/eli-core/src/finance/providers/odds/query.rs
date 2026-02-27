pub(crate) fn json_value_to_string(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn odds_freshness(observed_at: Option<DateTime<Utc>>) -> Freshness {
    let collected_at = Utc::now();
    match observed_at {
        Some(observed_at) => Freshness::new(
            observed_at,
            collected_at,
            FreshnessState::Live,
            FreshnessOrigin::ProviderTimestamp,
            FreshnessQuality::Exact,
        ),
        None => Freshness::new(
            collected_at,
            collected_at,
            FreshnessState::Unknown,
            FreshnessOrigin::TransportReceived,
            FreshnessQuality::Estimated,
        ),
    }
}

fn odds_response_freshness_summary(
    generated_at: DateTime<Utc>,
    markets: &[OddsMarket],
    listed_markets: Option<&[OddsListedMarket]>,
) -> FreshnessSummary {
    let mut data_as_of: Option<DateTime<Utc>> = None;
    let mut max_age_seconds: Option<i64> = None;
    let mut stale_count = 0usize;

    for freshness in markets
        .iter()
        .map(|m| &m.freshness)
        .chain(listed_markets.into_iter().flat_map(|rows| rows.iter().map(|m| &m.freshness)))
    {
        data_as_of = Some(match data_as_of {
            Some(existing) => existing.max(freshness.observed_at),
            None => freshness.observed_at,
        });
        max_age_seconds = Some(match max_age_seconds {
            Some(existing) => existing.max(freshness.age_seconds),
            None => freshness.age_seconds,
        });
        if matches!(freshness.state, FreshnessState::Stale) {
            stale_count = stale_count.saturating_add(1);
        }
    }

    FreshnessSummary {
        data_as_of: data_as_of.or(Some(generated_at)),
        max_age_seconds: max_age_seconds.or(Some(0)),
        stale_count,
    }
}

fn odds_run_meta(
    events_count: usize,
    markets_count: usize,
    listed_events_count: usize,
    listed_markets_count: usize,
) -> RunMeta {
    let mut coverage_counts = BTreeMap::new();
    coverage_counts.insert("events".to_string(), events_count);
    coverage_counts.insert("markets".to_string(), markets_count);
    coverage_counts.insert("listed_events".to_string(), listed_events_count);
    coverage_counts.insert("listed_markets".to_string(), listed_markets_count);

    RunMeta {
        latency_ms: 0,
        stdout_chars: 0,
        stored_bytes: 0,
        coverage_counts,
        token_efficiency: None,
    }
}

fn parse_json_array_strings(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(json_value_to_string)
        .collect()
}

pub(crate) fn parse_json_value_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(arr) => arr.iter().cloned().map(json_value_to_string).collect(),
        serde_json::Value::String(s) => parse_json_array_strings(s),
        serde_json::Value::Null => Vec::new(),
        other => vec![json_value_to_string(other.clone())],
    }
}

fn parse_probability(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok()
}

fn search_terms(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn contains_any_term(haystack: &str, terms: &[String]) -> bool {
    let lower = haystack.to_ascii_lowercase();
    terms.iter().any(|term| lower.contains(term))
}

fn matched_term_count(haystack: &str, terms: &[String]) -> usize {
    let lower = haystack.to_ascii_lowercase();
    terms
        .iter()
        .filter(|term| lower.contains(term.as_str()))
        .count()
}

fn matches_query_terms(phrase_match: bool, term_hits: usize, total_terms: usize) -> bool {
    if phrase_match {
        return true;
    }
    if total_terms >= 2 {
        term_hits >= 2
    } else {
        term_hits >= 1
    }
}
