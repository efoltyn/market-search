use super::super::*;

fn is_finance_like(title: &str, link: &str, keywords: &[String]) -> bool {
    let t = title.to_ascii_lowercase();
    let l = link.to_ascii_lowercase();
    keywords.iter().any(|k| t.contains(k) || l.contains(k))
}

fn build_news_queries(ticker: &str) -> Vec<String> {
    let normalized = ticker.trim().trim_start_matches('$').to_ascii_uppercase();
    let mut queries = vec![
        format!("${normalized} stock"),
        format!("{normalized} stock"),
        normalized.clone(),
    ];
    if normalized == "SPY" || normalized == "TLT" || normalized == "QQQ" || normalized == "GLD" {
        queries.push(format!("{normalized} etf"));
    }
    queries
}

pub async fn fetch_news(req: NewsRequest) -> Result<NewsResponse> {
    let started = std::time::Instant::now();
    let generated_at = Utc::now();
    let anchor_at = req.as_of;
    let policy_mode = req.policy_mode.unwrap_or_default();
    let policy_file = req.policy_file.as_deref().map(std::path::Path::new);
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

    let blocklist_suffix = resolved_policy
        .policy
        .filtering
        .news_noise_domain_blocklist
        .iter()
        .map(|d| format!("-site:{d}"))
        .collect::<Vec<_>>()
        .join(" ");
    let client = &*crate::finance::shared_client::GENERAL;
    let mut news = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let queries = build_news_queries(&ticker);
    for base_query in queries {
        let query = if blocklist_suffix.is_empty() {
            base_query
        } else {
            format!("{base_query} {blocklist_suffix}")
        };
        let url = format!(
            "https://news.google.com/rss/search?q={}+after:{}+before:{}&hl=en-US&gl=US&ceid=US:en",
            query, after_str, before_str
        );

        let start_time = std::time::Instant::now();
        let resp = client
            .get(url.clone())
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await
            .map_err(|e| Error::Provider(format!("news fetch failed: {e}")))?;

        if !resp.status().is_success() {
            continue;
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
            let key = format!("{}|{}", title.trim(), link.trim());
            if seen.insert(key) {
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
                    effective_at: None,
                    clock_status: None,
                    integrity_note: None,
                });
            }
            cursor = end;
            if news.len() >= 50 {
                break;
            }
        }
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
        if filtered.len()
            >= resolved_policy
                .policy
                .filtering
                .news_short_ticker_min_filtered_results
        {
            news = filtered;
        }
    }

    let policy_freshness = &resolved_policy.policy.freshness;
    let mut dropped_after_anchor = 0usize;
    let mut filtered_news = Vec::with_capacity(news.len());
    for item in &mut news {
        let published_at = chrono::DateTime::parse_from_rfc2822(&item.date)
            .ok()
            .map(|d| d.with_timezone(&Utc))
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(&item.date)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            });
        if let Some(anchor_at) = anchor_at {
            match published_at {
                Some(published_at) if published_at <= anchor_at => {}
                _ => {
                    dropped_after_anchor = dropped_after_anchor.saturating_add(1);
                    continue;
                }
            }
        }
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
        data_as_of = Some(
            data_as_of
                .map(|d| d.max(observed_at))
                .unwrap_or(observed_at),
        );
        item.published_at = published_at;
        item.freshness = freshness;
        item.effective_at = Some(observed_at);
        item.clock_status = anchor_at.map(|_| "synced".to_string());
        item.integrity_note = anchor_at.map(|anchor| {
            format!(
                "article retained because published_at <= report anchor {}",
                anchor.to_rfc3339()
            )
        });
        filtered_news.push(item.clone());
    }
    let decision_trace = if let Some(anchor_at) = anchor_at {
        vec![
            "policy_driven_news_filtering=true".to_string(),
            format!("research_anchor_at={}", anchor_at.to_rfc3339()),
            format!("articles={}", filtered_news.len()),
            format!("dropped_after_anchor={dropped_after_anchor}"),
        ]
    } else {
        vec![
            "policy_driven_news_filtering=true".to_string(),
            format!("articles={}", filtered_news.len()),
        ]
    };

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
        decision_trace,
        run_meta: RunMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            stdout_chars: 0,
            stored_bytes: 0,
            coverage_counts: std::collections::BTreeMap::from([(
                "articles".to_string(),
                filtered_news.len(),
            )]),
            token_efficiency: None,
        },
        articles: filtered_news.clone(),
        news: filtered_news,
    })
}
