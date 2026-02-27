use super::super::*;

fn is_finance_like(title: &str, link: &str, keywords: &[String]) -> bool {
    let t = title.to_ascii_lowercase();
    let l = link.to_ascii_lowercase();
    keywords
        .iter()
        .any(|k| t.contains(k) || l.contains(k))
}

pub async fn fetch_news(req: NewsRequest) -> Result<NewsResponse> {
    let started = std::time::Instant::now();
    let generated_at = Utc::now();
    let policy_mode = req.policy_mode.unwrap_or_default();
    let policy_file = req
        .policy_file
        .as_deref()
        .map(std::path::Path::new);
    let resolved_policy = crate::finance::policy::load_policy(policy_file, policy_mode)?;
    let ticker = req.ticker.trim().to_ascii_uppercase();
    let date = req.date.trim();

    // Calculate window around the date
    let target_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| Error::InvalidInput(format!("invalid date '{date}' (use YYYY-MM-DD)")))?;

    // Tighten window: after (date - 1), before (date + 1)
    let after = target_date.pred_opt().unwrap_or(target_date);
    let before = target_date.succ_opt().unwrap_or(target_date);

    let after_str = after.format("%Y-%m-%d").to_string();
    let before_str = before.format("%Y-%m-%d").to_string();

    // Add "stock" to disambiguate short tickers that are common English words.
    // e.g. "SPY" alone returns espionage news, "TLT" returns tlt.ng crypto spam.
    let base_query = if ticker.starts_with('$') {
        format!("{ticker} stock")
    } else {
        format!("${ticker} stock")
    };
    let blocklist_suffix = resolved_policy
        .policy
        .filtering
        .news_noise_domain_blocklist
        .iter()
        .map(|d| format!("-site:{d}"))
        .collect::<Vec<_>>()
        .join(" ");
    let query = if blocklist_suffix.is_empty() {
        base_query
    } else {
        format!("{base_query} {blocklist_suffix}")
    };

    let url = format!(
        "https://news.google.com/rss/search?q={}+after:{}+before:{}&hl=en-US&gl=US&ceid=US:en",
        query, after_str, before_str
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| Error::Provider(format!("news client init failed: {e}")))?;
    let start_time = std::time::Instant::now();
    let resp = client
        .get(url.clone())
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("news fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "news fetch failed: http {}",
            resp.status()
        )));
    }

    let status = resp.status();
    let xml = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("news read failed: {e}")))?;
    let elapsed_ms = start_time.elapsed().as_millis();
    info!(
        target: "eli.finance.news",
        url = %url,
        status = %status,
        bytes = xml.len(),
        elapsed_ms = elapsed_ms,
        "news fetch"
    );

    // Simple manual XML parsing (extracting <item> tags)
    let mut news = Vec::new();
    let mut cursor = 0;
    while let Some(start) = xml[cursor..].find("<item>") {
        let abs_start = cursor + start;
        let end = match xml[abs_start..].find("</item>") {
            Some(e) => abs_start + e + 7,
            None => break,
        };
        let item_xml = &xml[abs_start..end];

        let title = extract_xml_tag(item_xml, "title").unwrap_or_default();
        let link = extract_xml_tag(item_xml, "link").unwrap_or_default();
        let pub_date = extract_xml_tag(item_xml, "pubDate").unwrap_or_default();

        news.push(NewsItem {
            title: html_escape::decode_html_entities(&title).to_string(),
            link,
            date: pub_date,
            published_at: None,
            freshness: Freshness::new(
                generated_at,
                generated_at,
                FreshnessState::Unknown,
                FreshnessOrigin::TransportReceived,
                FreshnessQuality::Estimated,
            ),
        });

        cursor = end;
        if news.len() >= 25 {
            break;
        }
    }

    // For short/ambiguous tickers, filter obvious non-finance noise while preserving coverage.
    let mut stale_count = 0usize;
    let mut max_age_seconds = 0i64;
    let mut data_as_of: Option<DateTime<Utc>> = None;
    let collected_at = generated_at;
    if ticker.len() <= 4 {
        let filtered: Vec<NewsItem> = news
            .iter()
            .filter(|n| {
                is_finance_like(
                    &n.title,
                    &n.link,
                    &resolved_policy.policy.filtering.news_finance_keywords,
                )
            })
            .cloned()
            .collect();
        if filtered.len() >= resolved_policy.policy.filtering.news_short_ticker_min_filtered_results {
            news = filtered;
        }
    }

    let policy_freshness = &resolved_policy.policy.freshness;
    for item in &mut news {
        let published_at = chrono::DateTime::parse_from_rfc2822(&item.date)
            .ok()
            .map(|d| d.with_timezone(&Utc))
            .or_else(|| chrono::DateTime::parse_from_rfc3339(&item.date).ok().map(|d| d.with_timezone(&Utc)));
        let observed_at = published_at.unwrap_or(collected_at);
        let freshness = crate::finance::policy::freshness_from_observed(
            observed_at,
            collected_at,
            policy_freshness,
            FreshnessOrigin::ProviderTimestamp,
            if published_at.is_some() {
                FreshnessQuality::Exact
            } else {
                FreshnessQuality::Estimated
            },
        );
        max_age_seconds = max_age_seconds.max(freshness.age_seconds);
        if matches!(freshness.state, FreshnessState::Stale) {
            stale_count = stale_count.saturating_add(1);
        }
        data_as_of = Some(data_as_of.map(|d| d.max(observed_at)).unwrap_or(observed_at));
        item.published_at = published_at;
        item.freshness = freshness;
    }

    Ok(NewsResponse {
        ticker,
        date: date.to_string(),
        generated_at,
        schema_version: "finance.news.v2".to_string(),
        freshness_summary: FreshnessSummary {
            data_as_of,
            max_age_seconds: Some(max_age_seconds),
            stale_count,
        },
        applied_policy: AppliedPolicy {
            mode: resolved_policy.mode,
            sources: resolved_policy.sources,
        },
        decision_trace: vec![
            "policy_driven_news_filtering=true".to_string(),
            format!("articles={}", news.len()),
        ],
        run_meta: RunMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            stdout_chars: 0,
            stored_bytes: 0,
            coverage_counts: std::collections::BTreeMap::from([(
                "articles".to_string(),
                news.len(),
            )]),
            token_efficiency: None,
        },
        news,
    })
}
