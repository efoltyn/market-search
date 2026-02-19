pub(crate) fn json_value_to_string(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
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

fn query_mentions_explicit_year(query: &str) -> bool {
    query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|token| token.parse::<i32>().ok())
        .any(|year| (1900..=2100).contains(&year))
}

fn extract_year_tokens(text: &str) -> Vec<i32> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|token| token.parse::<i32>().ok())
        .filter(|year| (1900..=2100).contains(year))
        .collect()
}

fn is_probably_stale_open_market(
    title: &str,
    status: Option<&str>,
    query: Option<&str>,
    current_year: i32,
) -> bool {
    let is_open = status
        .map(|s| s.trim().eq_ignore_ascii_case("open"))
        .unwrap_or(true);
    if !is_open {
        return false;
    }
    if let Some(q) = query {
        if query_mentions_explicit_year(q) {
            return false;
        }
    }
    extract_year_tokens(title)
        .into_iter()
        .any(|year| year <= current_year - 1)
}

