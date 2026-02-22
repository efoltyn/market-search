use crate::web::providers::{community, finance_ext, papers, read, wiki};
use crate::web::{
    WebHit, WebProviderAttempt, WebRankShift, WebReadProbeSummary, WebSearchItem, WebSearchMode,
    WebSearchRecency, WebSearchRequest, WebSearchResponse, WebSearchRunDelta,
    WebSearchRunDeltaMeta, WebSearchScores, WebSearchStats,
};
use crate::{Error, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use futures::StreamExt;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;
use urlencoding;

#[derive(Clone, Copy, Debug)]
enum ProviderTarget {
    DuckDuckGo,
    Papers,
    Community,
    Wiki,
    FinanceExt,
}

impl ProviderTarget {
    fn name(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Papers => "papers",
            Self::Community => "community",
            Self::Wiki => "wikipedia",
            Self::FinanceExt => "finance_ext",
        }
    }
}

#[derive(Clone, Debug)]
struct SearchCandidate {
    title: String,
    url: String,
    domain: String,
    snippet: String,
    published_at: Option<DateTime<Utc>>,
    source: String,
    provenance: String,
    scores: WebSearchScores,
    read_probe: Option<WebReadProbeSummary>,
}

impl SearchCandidate {
    fn to_item(&self, rank: usize) -> WebSearchItem {
        WebSearchItem {
            rank,
            title: self.title.clone(),
            url: self.url.clone(),
            domain: self.domain.clone(),
            snippet: self.snippet.clone(),
            published_at: self.published_at,
            source: self.source.clone(),
            provenance: self.provenance.clone(),
            scores: self.scores.clone(),
            read_probe: self.read_probe.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct SearchRunState {
    #[serde(default)]
    tracks: BTreeMap<String, SearchRunEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SearchRunEntry {
    updated_at: DateTime<Utc>,
    urls: Vec<String>,
    #[serde(default)]
    request_fingerprint: String,
}

pub async fn search_general(query: &str) -> Result<Vec<WebHit>> {
    fetch_duckduckgo(query, None).await
}

pub async fn search_smart(mut req: WebSearchRequest) -> Result<WebSearchResponse> {
    req.top = req.top.max(1);
    req.probe_top = req.probe_top.min(req.top);
    req.max_parallel = req.max_parallel.max(1);

    let targets = mode_targets(req.mode);
    let effective_query = build_effective_query(&req);
    let request_recency = req.recency;
    let provider_fetches = futures::stream::iter(targets.into_iter())
        .map(|target| {
            let query = effective_query.clone();
            async move {
                let start = Instant::now();
                let fetched = fetch_provider(target, &query, request_recency).await;
                let duration_ms = start.elapsed().as_millis() as u64;
                (target, fetched, duration_ms)
            }
        })
        .buffer_unordered(req.max_parallel)
        .collect::<Vec<_>>()
        .await;

    let mut providers = Vec::<WebProviderAttempt>::new();
    let mut all_hits = Vec::<WebHit>::new();
    for (target, fetched, duration_ms) in provider_fetches {
        match fetched {
            Ok(mut hits) => {
                providers.push(WebProviderAttempt {
                    provider: target.name().to_string(),
                    ok: true,
                    duration_ms,
                    raw_hits: hits.len(),
                    error: None,
                });
                all_hits.append(&mut hits);
            }
            Err(err) => {
                providers.push(WebProviderAttempt {
                    provider: target.name().to_string(),
                    ok: false,
                    duration_ms,
                    raw_hits: 0,
                    error: Some(err.to_string()),
                });
            }
        }
    }
    providers.sort_by(|a, b| a.provider.cmp(&b.provider));

    let total_raw_hits = all_hits.len();
    all_hits.sort_by(|a, b| {
        a.url
            .cmp(&b.url)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.title.cmp(&b.title))
    });

    let mut seen = HashSet::<String>::new();
    let mut deduped_hits = Vec::<WebHit>::new();
    for hit in all_hits {
        let canonical = canonicalize_url(&hit.url);
        if canonical.is_empty() {
            continue;
        }
        if seen.insert(canonical) {
            deduped_hits.push(hit);
        }
    }
    let deduped_count = deduped_hits.len();

    let include_domains = normalize_domains(&req.domains);
    let exclude_domains = normalize_domains(&req.exclude_domains);
    let mut after_domain = Vec::<WebHit>::new();
    for hit in deduped_hits {
        let domain = extract_domain(&hit.url);
        if !domain_passes_filters(&domain, &include_domains, &exclude_domains) {
            continue;
        }
        after_domain.push(hit);
    }
    let after_domain_filter = after_domain.len();

    let mut warnings = Vec::<String>::new();
    let recency_days = recency_days(req.recency);
    let mut missing_published_dropped = 0usize;
    let mut missing_published_kept = 0usize;
    let mut missing_published_fallback_kept = 0usize;
    let mut after_time = Vec::<SearchCandidate>::new();
    let mut missing_date_candidates = Vec::<SearchCandidate>::new();
    for hit in after_domain {
        let published_at = hit.published;
        let published_date = published_at.map(|d| d.date_naive());
        let lexical = lexical_score(&req.query, &hit.title, &hit.snippet);
        let freshness = freshness_score(published_at);
        let source_trust = source_trust_score(&hit.url, &hit.source, &hit.provenance);
        let readability = 0.5;
        let final_score = weighted_score(lexical, freshness, source_trust, readability);
        let domain = extract_domain(&hit.url);
        let url = hit.url;

        let candidate = SearchCandidate {
            title: hit.title,
            url,
            domain,
            snippet: hit.snippet,
            published_at,
            source: hit.source,
            provenance: hit.provenance,
            scores: WebSearchScores {
                lexical,
                freshness,
                source_trust,
                readability,
                final_score,
            },
            read_probe: None,
        };

        if published_date.is_none() {
            if !allow_missing_published(req.since, req.until) {
                missing_published_dropped += 1;
                missing_date_candidates.push(candidate);
                continue;
            }
            if recency_days.is_some() {
                missing_published_kept += 1;
            }
            after_time.push(candidate);
            continue;
        }

        if !passes_time_filters(published_date, req.since, req.until, recency_days) {
            continue;
        }
        after_time.push(candidate);
    }

    if should_apply_missing_date_fallback(
        req.since,
        req.until,
        after_time.is_empty(),
        missing_date_candidates.is_empty(),
    ) {
        missing_published_fallback_kept = missing_date_candidates.len();
        after_time.extend(missing_date_candidates);
    }
    if missing_published_dropped > 0 {
        warnings.push(format!(
            "dropped {missing_published_dropped} hits because published_at was missing while time filters were active"
        ));
    }
    if missing_published_kept > 0 {
        warnings.push(format!(
            "kept {missing_published_kept} hits with missing published_at because only recency filtering was active"
        ));
    }
    if missing_published_fallback_kept > 0 {
        warnings.push(format!(
            "kept {missing_published_fallback_kept} hits with missing published_at because no dated hits remained after strict date filtering"
        ));
    }
    let after_time_filter = after_time.len();

    after_time.sort_by(compare_candidates);
    after_time.truncate(req.top);

    let probe_count = req.probe_top.min(after_time.len());
    if probe_count > 0 {
        let indexed_probes = futures::stream::iter((0..probe_count).collect::<Vec<_>>())
            .map(|idx| {
                let url = after_time[idx].url.clone();
                async move {
                    let response = read::read_url_with_diagnostics(&url).await;
                    let summary = read::to_probe_summary(&response);
                    (idx, summary)
                }
            })
            .buffer_unordered(req.max_parallel)
            .collect::<Vec<_>>()
            .await;
        for (idx, summary) in indexed_probes {
            let readability = summary.fetch_status.readability_score();
            if let Some(candidate) = after_time.get_mut(idx) {
                candidate.scores.readability = readability;
                candidate.scores.final_score = weighted_score(
                    candidate.scores.lexical,
                    candidate.scores.freshness,
                    candidate.scores.source_trust,
                    readability,
                );
                candidate.read_probe = Some(summary);
            }
        }
    }

    after_time.sort_by(compare_candidates);
    let mut items = Vec::<WebSearchItem>::with_capacity(after_time.len());
    for (idx, candidate) in after_time.iter().enumerate() {
        items.push(candidate.to_item(idx + 1));
    }

    let (run_delta, run_delta_meta) = if let Some(track_key) = req.track_key.as_deref() {
        let fingerprint = request_fingerprint(&req);
        match load_update_run_delta(track_key, &fingerprint, &items) {
            Ok(outcome) => {
                let delta = if outcome.reset_for_request_change {
                    warnings.push(
                        "run delta baseline reset because query/filters changed for this track key"
                            .to_string(),
                    );
                    finalize_run_delta(outcome.delta, true)
                } else {
                    finalize_run_delta(outcome.delta, false)
                };
                (
                    Some(delta),
                    Some(WebSearchRunDeltaMeta {
                        track_key: Some(track_key.to_string()),
                        baseline_reset_applied: outcome.reset_for_request_change,
                        previous_fingerprint: outcome.previous_fingerprint,
                        current_fingerprint: outcome.current_fingerprint,
                        reason: outcome.reset_reason,
                    }),
                )
            }
            Err(err) => {
                warnings.push(format!("run delta tracking failed: {err}"));
                (
                    None,
                    Some(WebSearchRunDeltaMeta {
                        track_key: Some(track_key.to_string()),
                        baseline_reset_applied: false,
                        previous_fingerprint: None,
                        current_fingerprint: fingerprint,
                        reason: Some("tracking_failed".to_string()),
                    }),
                )
            }
        }
    } else {
        (None, None)
    };

    Ok(WebSearchResponse {
        query: req.query,
        mode: req.mode,
        generated_at: Utc::now(),
        providers,
        items,
        stats: WebSearchStats {
            total_raw_hits,
            deduped_hits: deduped_count,
            after_domain_filter,
            after_time_filter,
            returned_items: after_time_filter.min(req.top),
            probed_items: probe_count,
            warnings,
        },
        run_delta,
        run_delta_meta,
    })
}

async fn fetch_provider(
    target: ProviderTarget,
    query: &str,
    recency: Option<WebSearchRecency>,
) -> Result<Vec<WebHit>> {
    match target {
        ProviderTarget::DuckDuckGo => fetch_duckduckgo(query, recency).await,
        ProviderTarget::Papers => papers::search_papers(query).await,
        ProviderTarget::Community => community::search_community(query).await,
        ProviderTarget::Wiki => wiki::search_wiki(query).await,
        ProviderTarget::FinanceExt => finance_ext::search_finance_ext(query).await,
    }
}

fn mode_targets(mode: WebSearchMode) -> Vec<ProviderTarget> {
    match mode {
        WebSearchMode::Auto | WebSearchMode::News => vec![ProviderTarget::DuckDuckGo],
        WebSearchMode::Finance => vec![ProviderTarget::DuckDuckGo, ProviderTarget::FinanceExt],
        WebSearchMode::Research => vec![ProviderTarget::Papers, ProviderTarget::DuckDuckGo],
        WebSearchMode::Tech => vec![ProviderTarget::Community, ProviderTarget::DuckDuckGo],
        WebSearchMode::Encyclopedia => vec![ProviderTarget::Wiki, ProviderTarget::DuckDuckGo],
    }
}

fn build_effective_query(req: &WebSearchRequest) -> String {
    let mut query = req.query.trim().to_string();
    if let Some(since) = req.since {
        query.push_str(&format!(" after:{}", since.format("%Y-%m-%d")));
    }
    if let Some(until) = req.until {
        query.push_str(&format!(" before:{}", until.format("%Y-%m-%d")));
    }
    query
}

async fn fetch_duckduckgo(query: &str, recency: Option<WebSearchRecency>) -> Result<Vec<WebHit>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| Error::Provider(format!("general client init failed: {e}")))?;

    let url = "https://html.duckduckgo.com/html/";
    let mut params: Vec<(&str, String)> = vec![("q", query.to_string())];
    if let Some(df) = map_recency_to_ddg(recency) {
        params.push(("df", df.to_string()));
    }

    let resp = client
        .post(url)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("duckduckgo fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "duckduckgo fetch failed: http {}",
            resp.status()
        )));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("duckduckgo read failed: {e}")))?;
    if looks_like_ddg_challenge(&html) {
        return Err(Error::Provider(
            "duckduckgo fetch blocked: captcha_or_bot_challenge".to_string(),
        ));
    }
    let document = Html::parse_document(&html);
    let result_selector = Selector::parse(".result")
        .map_err(|e| Error::Provider(format!("duckduckgo selector parse failed: {e}")))?;
    let title_selector = Selector::parse(".result__a")
        .map_err(|e| Error::Provider(format!("duckduckgo selector parse failed: {e}")))?;
    let snippet_selector = Selector::parse(".result__snippet")
        .map_err(|e| Error::Provider(format!("duckduckgo selector parse failed: {e}")))?;

    let mut hits = Vec::new();
    for element in document.select(&result_selector) {
        let Some(title_el) = element.select(&title_selector).next() else {
            continue;
        };
        let title = title_el
            .text()
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let Some(raw_url) = title_el.value().attr("href") else {
            continue;
        };
        let clean_url = decode_ddg_url(raw_url);
        if clean_url.trim().is_empty() {
            continue;
        }
        let snippet = element
            .select(&snippet_selector)
            .next()
            .map(|el| el.text().collect::<Vec<_>>().join(""))
            .unwrap_or_default()
            .trim()
            .to_string();

        hits.push(WebHit {
            title,
            url: clean_url,
            snippet,
            source: "DuckDuckGo".to_string(),
            score: 1.0,
            published: None,
            provenance: "web_search".to_string(),
        });
    }
    Ok(hits)
}

fn map_recency_to_ddg(recency: Option<WebSearchRecency>) -> Option<&'static str> {
    match recency {
        Some(WebSearchRecency::Day) => Some("d"),
        Some(WebSearchRecency::Week) => Some("w"),
        Some(WebSearchRecency::Month) => Some("m"),
        Some(WebSearchRecency::Year) => Some("y"),
        None => None,
    }
}

fn decode_ddg_url(url: &str) -> String {
    if let Some(start) = url.find("uddg=") {
        let rest = &url[start + 5..];
        if let Some(end) = rest.find('&') {
            return urlencoding::decode(&rest[..end])
                .unwrap_or_default()
                .to_string();
        }
        return urlencoding::decode(rest).unwrap_or_default().to_string();
    }
    url.to_string()
}

fn looks_like_ddg_challenge(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("anomaly.js")
        || lower.contains("anomaly-modal")
        || lower.contains("challenge-form")
        || lower.contains("captcha")
        || lower.contains("are you a robot")
}

fn normalize_domains(domains: &[String]) -> Vec<String> {
    domains
        .iter()
        .flat_map(|entry| entry.split(','))
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_start_matches("www.")
                .split('/')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn extract_domain(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_else(|| {
            url.trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
        })
}

fn domain_passes_filters(domain: &str, include: &[String], exclude: &[String]) -> bool {
    if domain.is_empty() {
        return false;
    }
    if !include.is_empty() && !include.iter().any(|needle| domain_matches(domain, needle)) {
        return false;
    }
    if exclude.iter().any(|needle| domain_matches(domain, needle)) {
        return false;
    }
    true
}

fn domain_matches(domain: &str, needle: &str) -> bool {
    if domain == needle {
        return true;
    }
    domain.ends_with(&format!(".{needle}"))
}

fn recency_days(recency: Option<WebSearchRecency>) -> Option<i64> {
    match recency {
        Some(WebSearchRecency::Day) => Some(1),
        Some(WebSearchRecency::Week) => Some(7),
        Some(WebSearchRecency::Month) => Some(31),
        Some(WebSearchRecency::Year) => Some(366),
        None => None,
    }
}

fn allow_missing_published(since: Option<NaiveDate>, until: Option<NaiveDate>) -> bool {
    since.is_none() && until.is_none()
}

fn should_apply_missing_date_fallback(
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    no_dated_hits: bool,
    no_missing_hits: bool,
) -> bool {
    (since.is_some() || until.is_some()) && no_dated_hits && !no_missing_hits
}

fn passes_time_filters(
    published: Option<NaiveDate>,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    recency_days: Option<i64>,
) -> bool {
    let Some(published) = published else {
        return since.is_none() && until.is_none() && recency_days.is_none();
    };
    if let Some(since) = since {
        if published < since {
            return false;
        }
    }
    if let Some(until) = until {
        if published > until {
            return false;
        }
    }
    if let Some(days) = recency_days {
        let threshold = Utc::now().date_naive() - Duration::days(days);
        if published < threshold {
            return false;
        }
    }
    true
}

fn lexical_score(query: &str, title: &str, snippet: &str) -> f64 {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return 0.0;
    }
    let title_l = title.to_ascii_lowercase();
    let snippet_l = snippet.to_ascii_lowercase();

    let mut score = 0.0f64;
    if title_l.contains(&q) {
        score += 0.6;
    }
    if snippet_l.contains(&q) {
        score += 0.3;
    }

    for term in q.split_whitespace().filter(|t| !t.is_empty()) {
        if title_l.contains(term) {
            score += 0.12;
        }
        if snippet_l.contains(term) {
            score += 0.06;
        }
    }
    score.clamp(0.0, 1.0)
}

fn freshness_score(published: Option<DateTime<Utc>>) -> f64 {
    let Some(published) = published else {
        return 0.25;
    };
    let age_days = (Utc::now() - published).num_days().max(0);
    match age_days {
        0..=1 => 1.0,
        2..=7 => 0.85,
        8..=30 => 0.6,
        31..=180 => 0.35,
        181..=365 => 0.2,
        _ => 0.1,
    }
}

fn source_trust_score(url: &str, source: &str, provenance: &str) -> f64 {
    let domain = extract_domain(url);
    let high_trust_domains = [
        "reuters.com",
        "bloomberg.com",
        "ft.com",
        "wsj.com",
        "apnews.com",
        "federalreserve.gov",
        "sec.gov",
    ];
    if high_trust_domains
        .iter()
        .any(|d| domain_matches(&domain, d))
    {
        return 0.96;
    }
    if domain_matches(&domain, "wikipedia.org") || provenance == "encyclopedic" {
        return 0.9;
    }
    if source.eq_ignore_ascii_case("stackoverflow") || provenance == "technical_qa" {
        return 0.85;
    }
    if provenance == "scholarly" || provenance == "preprint" {
        return 0.9;
    }
    0.6
}

fn weighted_score(lexical: f64, freshness: f64, source_trust: f64, readability: f64) -> f64 {
    (0.45 * lexical + 0.20 * freshness + 0.20 * source_trust + 0.15 * readability).clamp(0.0, 1.0)
}

fn compare_candidates(a: &SearchCandidate, b: &SearchCandidate) -> std::cmp::Ordering {
    b.scores
        .final_score
        .partial_cmp(&a.scores.final_score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| {
            b.scores
                .lexical
                .partial_cmp(&a.scores.lexical)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| a.url.cmp(&b.url))
}

fn canonicalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(mut parsed) = reqwest::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    parsed.set_fragment(None);
    let query_pairs = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let key = k.to_ascii_lowercase();
            !key.starts_with("utm_") && key != "fbclid" && key != "gclid"
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect::<Vec<_>>();
    if query_pairs.is_empty() {
        parsed.set_query(None);
    } else {
        let mut qp = parsed.query_pairs_mut();
        qp.clear();
        for (k, v) in query_pairs {
            qp.append_pair(&k, &v);
        }
        drop(qp);
    }
    parsed.to_string()
}

fn search_runs_path() -> std::path::PathBuf {
    directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("web").join("search_runs.json"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-web-search-runs.json"))
}

#[derive(Clone, Debug)]
struct RunDeltaOutcome {
    delta: WebSearchRunDelta,
    reset_for_request_change: bool,
    previous_fingerprint: Option<String>,
    current_fingerprint: String,
    reset_reason: Option<String>,
}

fn request_fingerprint(req: &WebSearchRequest) -> String {
    let mut include = normalize_domains(&req.domains);
    include.sort();
    include.dedup();
    let mut exclude = normalize_domains(&req.exclude_domains);
    exclude.sort();
    exclude.dedup();
    let payload = serde_json::json!({
        "query": req.query.trim().to_ascii_lowercase(),
        "mode": req.mode,
        "domains": include,
        "exclude_domains": exclude,
        "recency": req.recency,
        "since": req.since,
        "until": req.until,
        "top": req.top.max(1),
        "probe_top": req.probe_top.min(req.top.max(1)),
    })
    .to_string();
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn load_update_run_delta(
    track_key: &str,
    fingerprint: &str,
    items: &[WebSearchItem],
) -> Result<RunDeltaOutcome> {
    let path = search_runs_path();
    let raw = std::fs::read_to_string(&path).ok();
    let mut state = raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<SearchRunState>(s).ok())
        .unwrap_or_default();

    let current_urls = items.iter().map(|i| i.url.clone()).collect::<Vec<_>>();
    let (previous_urls, reset_for_request_change, previous_fingerprint) =
        match state.tracks.get(track_key) {
            Some(entry)
                if !entry.request_fingerprint.is_empty()
                    && entry.request_fingerprint != fingerprint =>
            {
                (
                    Vec::<String>::new(),
                    true,
                    Some(entry.request_fingerprint.clone()),
                )
            }
            Some(entry) => (
                entry.urls.clone(),
                false,
                Some(entry.request_fingerprint.clone()),
            ),
            None => (Vec::<String>::new(), false, None),
        };
    let delta =
        compute_effective_run_delta(&previous_urls, &current_urls, reset_for_request_change);

    state.tracks.insert(
        track_key.to_string(),
        SearchRunEntry {
            updated_at: Utc::now(),
            urls: current_urls,
            request_fingerprint: fingerprint.to_string(),
        },
    );
    if state.tracks.len() > 512 {
        let mut entries = state
            .tracks
            .iter()
            .map(|(k, v)| (k.clone(), v.updated_at))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.1.cmp(&b.1));
        let remove_count = state.tracks.len() - 512;
        for (key, _) in entries.into_iter().take(remove_count) {
            state.tracks.remove(&key);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Other(format!(
                "create web search cache dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let serialized = serde_json::to_string_pretty(&state)
        .map_err(|e| Error::Other(format!("serialize web search run state: {e}")))?;
    std::fs::write(&path, serialized).map_err(|e| {
        Error::Other(format!(
            "write web search run state {}: {e}",
            path.display()
        ))
    })?;

    Ok(RunDeltaOutcome {
        delta,
        reset_for_request_change,
        previous_fingerprint,
        current_fingerprint: fingerprint.to_string(),
        reset_reason: reset_for_request_change.then_some("request_fingerprint_changed".to_string()),
    })
}

fn empty_run_delta() -> WebSearchRunDelta {
    WebSearchRunDelta {
        new_urls: Vec::new(),
        dropped_urls: Vec::new(),
        rank_up: Vec::new(),
        rank_down: Vec::new(),
        unchanged: 0,
    }
}

fn finalize_run_delta(
    delta: WebSearchRunDelta,
    reset_for_request_change: bool,
) -> WebSearchRunDelta {
    if reset_for_request_change {
        return empty_run_delta();
    }
    delta
}

fn compute_effective_run_delta(
    previous_urls: &[String],
    current_urls: &[String],
    reset_for_request_change: bool,
) -> WebSearchRunDelta {
    if reset_for_request_change {
        return empty_run_delta();
    }
    compute_run_delta(previous_urls, current_urls)
}

fn compute_run_delta(previous_urls: &[String], current_urls: &[String]) -> WebSearchRunDelta {
    let prev_map = previous_urls
        .iter()
        .enumerate()
        .map(|(idx, url)| (url.clone(), idx + 1))
        .collect::<HashMap<_, _>>();
    let curr_map = current_urls
        .iter()
        .enumerate()
        .map(|(idx, url)| (url.clone(), idx + 1))
        .collect::<HashMap<_, _>>();

    let mut new_urls = Vec::<String>::new();
    let mut rank_up = Vec::<WebRankShift>::new();
    let mut rank_down = Vec::<WebRankShift>::new();
    let mut unchanged = 0usize;

    for (url, to_rank) in &curr_map {
        match prev_map.get(url) {
            None => new_urls.push(url.clone()),
            Some(from_rank) if to_rank < from_rank => rank_up.push(WebRankShift {
                url: url.clone(),
                from_rank: *from_rank,
                to_rank: *to_rank,
            }),
            Some(from_rank) if to_rank > from_rank => rank_down.push(WebRankShift {
                url: url.clone(),
                from_rank: *from_rank,
                to_rank: *to_rank,
            }),
            Some(_) => unchanged += 1,
        }
    }

    let mut dropped_urls = previous_urls
        .iter()
        .filter(|url| !curr_map.contains_key(*url))
        .cloned()
        .collect::<Vec<_>>();

    new_urls.sort();
    dropped_urls.sort();
    rank_up.sort_by(|a, b| a.to_rank.cmp(&b.to_rank).then_with(|| a.url.cmp(&b.url)));
    rank_down.sort_by(|a, b| a.to_rank.cmp(&b.to_rank).then_with(|| a.url.cmp(&b.url)));

    WebSearchRunDelta {
        new_urls,
        dropped_urls,
        rank_up,
        rank_down,
        unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_filtering_is_strict() {
        let include = normalize_domains(&["reuters.com".to_string()]);
        let exclude = normalize_domains(&["sports.reuters.com".to_string()]);
        assert!(domain_passes_filters("www.reuters.com", &include, &[]));
        assert!(domain_passes_filters("reuters.com", &include, &[]));
        assert!(!domain_passes_filters("example.com", &include, &[]));
        assert!(!domain_passes_filters(
            "sports.reuters.com",
            &include,
            &exclude
        ));
    }

    #[test]
    fn time_filters_drop_outside_window() {
        let since = NaiveDate::from_ymd_opt(2026, 1, 1).expect("date");
        let until = NaiveDate::from_ymd_opt(2026, 1, 31).expect("date");
        assert!(passes_time_filters(
            Some(NaiveDate::from_ymd_opt(2026, 1, 15).expect("date")),
            Some(since),
            Some(until),
            None
        ));
        assert!(!passes_time_filters(
            Some(NaiveDate::from_ymd_opt(2025, 12, 31).expect("date")),
            Some(since),
            Some(until),
            None
        ));
        assert!(!passes_time_filters(None, Some(since), Some(until), None));
    }

    #[test]
    fn recency_only_allows_missing_published_dates() {
        let since = NaiveDate::from_ymd_opt(2026, 1, 1).expect("date");
        assert!(allow_missing_published(None, None));
        assert!(!allow_missing_published(Some(since), None));
        assert!(!allow_missing_published(None, Some(since)));
        assert!(!allow_missing_published(Some(since), Some(since)));
        assert!(!passes_time_filters(None, None, None, Some(7)));
    }

    #[test]
    fn date_window_can_fallback_to_missing_dates_when_all_filtered() {
        let since = NaiveDate::from_ymd_opt(2026, 1, 1).expect("date");
        assert!(should_apply_missing_date_fallback(
            Some(since),
            None,
            true,
            false
        ));
        assert!(should_apply_missing_date_fallback(
            None,
            Some(since),
            true,
            false
        ));
        assert!(!should_apply_missing_date_fallback(
            Some(since),
            None,
            false,
            false
        ));
        assert!(!should_apply_missing_date_fallback(
            Some(since),
            None,
            true,
            true
        ));
    }

    #[test]
    fn run_delta_detects_new_dropped_and_rank_shift() {
        let previous = vec![
            "https://a.com".to_string(),
            "https://b.com".to_string(),
            "https://c.com".to_string(),
        ];
        let current = vec![
            "https://b.com".to_string(),
            "https://d.com".to_string(),
            "https://a.com".to_string(),
        ];
        let delta = compute_run_delta(&previous, &current);
        assert_eq!(delta.new_urls, vec!["https://d.com".to_string()]);
        assert_eq!(delta.dropped_urls, vec!["https://c.com".to_string()]);
        assert_eq!(delta.rank_up.len(), 1);
        assert_eq!(delta.rank_down.len(), 1);
    }

    #[test]
    fn request_fingerprint_is_order_insensitive_for_domains() {
        let mut req_a = WebSearchRequest::default();
        req_a.query = "Tariffs".to_string();
        req_a.mode = WebSearchMode::News;
        req_a.domains = vec!["Reuters.com".to_string(), "NPR.org".to_string()];
        req_a.exclude_domains = vec!["Example.com".to_string()];
        req_a.top = 10;
        req_a.probe_top = 4;

        let mut req_b = req_a.clone();
        req_b.domains = vec!["npr.org".to_string(), "reuters.com".to_string()];
        req_b.exclude_domains = vec!["example.com".to_string()];

        assert_eq!(request_fingerprint(&req_a), request_fingerprint(&req_b));
    }

    #[test]
    fn request_fingerprint_changes_when_filters_change() {
        let mut req_a = WebSearchRequest::default();
        req_a.query = "tariffs".to_string();
        req_a.mode = WebSearchMode::News;
        req_a.top = 10;
        req_a.probe_top = 4;

        let mut req_b = req_a.clone();
        req_b.since = NaiveDate::from_ymd_opt(2026, 2, 1);

        assert_ne!(request_fingerprint(&req_a), request_fingerprint(&req_b));
    }

    #[test]
    fn detects_duckduckgo_challenge_pages() {
        let html = r#"<html><body><form id="challenge-form"></form><div class="anomaly-modal"></div></body></html>"#;
        assert!(looks_like_ddg_challenge(html));
        assert!(!looks_like_ddg_challenge(
            "<html><body><div class='result'>ok</div></body></html>"
        ));
    }

    #[test]
    fn effective_run_delta_is_empty_when_reset_is_true() {
        let previous = vec!["https://a.com".to_string()];
        let current = vec!["https://b.com".to_string()];
        let delta = compute_effective_run_delta(&previous, &current, true);
        assert!(delta.new_urls.is_empty());
        assert!(delta.dropped_urls.is_empty());
        assert!(delta.rank_up.is_empty());
        assert!(delta.rank_down.is_empty());
        assert_eq!(delta.unchanged, 0);
    }

    #[test]
    fn finalize_run_delta_enforces_reset_invariant() {
        let delta = WebSearchRunDelta {
            new_urls: vec!["https://x.com".to_string()],
            dropped_urls: vec!["https://y.com".to_string()],
            rank_up: vec![WebRankShift {
                url: "https://x.com".to_string(),
                from_rank: 2,
                to_rank: 1,
            }],
            rank_down: vec![WebRankShift {
                url: "https://z.com".to_string(),
                from_rank: 1,
                to_rank: 2,
            }],
            unchanged: 3,
        };

        let reset = finalize_run_delta(delta.clone(), true);
        assert!(reset.new_urls.is_empty());
        assert!(reset.dropped_urls.is_empty());
        assert!(reset.rank_up.is_empty());
        assert!(reset.rank_down.is_empty());
        assert_eq!(reset.unchanged, 0);

        let kept = finalize_run_delta(delta.clone(), false);
        assert_eq!(kept.new_urls, delta.new_urls);
        assert_eq!(kept.dropped_urls, delta.dropped_urls);
        assert_eq!(kept.rank_up.len(), delta.rank_up.len());
        assert_eq!(kept.rank_down.len(), delta.rank_down.len());
        assert_eq!(kept.unchanged, delta.unchanged);
    }
}
