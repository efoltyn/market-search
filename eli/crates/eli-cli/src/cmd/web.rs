async fn cmd_web_crawl(args: WebCrawlArgs) -> Result<()> {
    let url_for_meta = args.url.clone();
    let req = eli_core::web::CrawlRequest {
        url: args.url,
        max_pages: Some(args.max_pages),
        respect_robots: args.respect_robots,
        include_subdomains: args.subdomains,
        include_sitemap: args.sitemap,
        smart_mode: args.smart,
    };

    let resp = eli_core::web::crawl_website(req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("crawl website")?;

    let mut write_result: Option<MetaWriteResult> = None;
    if let Some(out_path) = args.out.clone() {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "web.crawl",
            &[
                format!("url={url_for_meta}"),
                format!("sitemap={}", args.sitemap),
                format!("smart={}", args.smart),
                format!("view={}", format!("{:?}", args.view).to_ascii_lowercase()),
                format!("save={}", format!("{:?}", args.save).to_ascii_lowercase()),
            ],
        )?;
        write_result = Some(wr);
    } else if args.save == CrawlSaveMode::Auto {
        let wr = write_json_out_with_meta(
            PathBuf::from("eli_research/data/auto.json"),
            &resp,
            "web.crawl",
            &[
                format!("url={url_for_meta}"),
                format!("sitemap={}", args.sitemap),
                format!("smart={}", args.smart),
                format!("view={}", format!("{:?}", args.view).to_ascii_lowercase()),
                "save=auto".to_string(),
            ],
        )?;
        write_result = Some(wr);
    }

    match args.view {
        CrawlViewMode::Raw => {
            let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
            println!("{json}");
        }
        CrawlViewMode::Summary => {
            print_crawl_summary(&resp, write_result.as_ref());
        }
        CrawlViewMode::Path => {
            if let Some(wr) = write_result.as_ref() {
                println!(
                    "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"pages_crawled\":{},\"crawl_mode\":{}}}",
                    serde_json::to_string(&wr.out_path.display().to_string())
                        .unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(&wr.meta_path.display().to_string())
                        .unwrap_or_else(|_| "\"\"".to_string()),
                    resp.pages_crawled,
                    serde_json::to_string(&resp.crawl_mode)
                        .unwrap_or_else(|_| "\"\"".to_string()),
                );
            } else {
                println!(
                    "{{\"ok\":true,\"saved\":false,\"pages_crawled\":{},\"crawl_mode\":{}}}",
                    resp.pages_crawled,
                    serde_json::to_string(&resp.crawl_mode).unwrap_or_else(|_| "\"\"".to_string()),
                );
            }
        }
    }

    if args.out.is_some() && args.view != CrawlViewMode::Path {
        if let Some(wr) = write_result.as_ref() {
            println!(
                "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
        }
    } else if args.view != CrawlViewMode::Path {
        if let Some(wr) = write_result {
            println!(
                "{{\"saved\":true,\"path\":{},\"meta_path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
        }
    }

    Ok(())
}

fn print_crawl_summary(resp: &eli_core::web::CrawlResponse, wr: Option<&MetaWriteResult>) {
    println!(
        "crawl mode={} pages={} duration_ms={}",
        resp.crawl_mode, resp.pages_crawled, resp.duration_ms
    );
    if resp.pages.is_empty() {
        println!("pages: none");
    } else {
        println!("top pages:");
        for page in resp.pages.iter().take(5) {
            let title = page.title.as_deref().unwrap_or("(untitled)");
            let snippet = page
                .text_preview
                .split_whitespace()
                .take(24)
                .collect::<Vec<_>>()
                .join(" ");
            println!("- {} | {}", title, page.url);
            if !snippet.is_empty() {
                println!("  {}", snippet);
            }
        }
        if resp.pages.len() > 5 {
            println!("... {} more pages", resp.pages.len().saturating_sub(5));
        }
    }
    if let Some(wr) = wr {
        println!(
            "saved raw={} meta={}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
    }
}

async fn cmd_web_search(args: WebSearchArgs) -> Result<()> {
    let since = match args.since.as_deref() {
        Some(raw) => Some(
            chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("invalid --since '{raw}' (expected YYYY-MM-DD)"))?,
        ),
        None => None,
    };
    let until = match args.until.as_deref() {
        Some(raw) => Some(
            chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("invalid --until '{raw}' (expected YYYY-MM-DD)"))?,
        ),
        None => None,
    };
    if let (Some(since_date), Some(until_date)) = (since.as_ref(), until.as_ref()) {
        if since_date > until_date {
            anyhow::bail!("--since cannot be after --until");
        }
    }

    let req = eli_core::web::WebSearchRequest {
        query: args.query.clone(),
        mode: map_web_search_mode(args.mode),
        domains: args.domains.clone(),
        exclude_domains: args.exclude_domains.clone(),
        recency: map_web_search_recency(args.recency),
        since,
        until,
        top: args.top,
        probe_top: args.probe_top,
        max_parallel: args.max_parallel,
        track_key: args.track_key.clone(),
    };
    let resp = eli_core::web::providers::general::search_smart(req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("web search")?;
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "web.search",
            &[
                format!("query={}", args.query),
                format!(
                    "mode={}",
                    format!("{:?}", args.mode).to_ascii_lowercase()
                ),
                format!("top={}", args.top),
                format!("probe_top={}", args.probe_top),
            ],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let payload = if args.full {
        serde_json::to_value(&resp).context("serialize web search response")?
    } else {
        compact_web_search_json(&resp)
    };
    let json = serde_json::to_string_pretty(&payload).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_web_read(args: WebReadArgs) -> Result<()> {
    let urls = collect_web_read_urls(&args)?;
    if urls.is_empty() {
        anyhow::bail!("must provide at least one URL via --url or --urls-file");
    }

    let meta_args = vec![
        format!("urls={}", urls.len()),
        format!("max_parallel={}", args.max_parallel),
    ];

    if urls.len() == 1 {
        let article = eli_core::web::providers::read::read_url_with_diagnostics(&urls[0]).await;
        let payload = if args.full {
            serde_json::to_value(&article).context("serialize web read response")?
        } else {
            compact_web_read_json(&article, args.max_chars)
        };

        if let Some(out_path) = args.out {
            let wr = write_json_out_with_meta(out_path, &payload, "web.read", &meta_args)?;
            println!(
                "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
            return Ok(());
        }
        let json = serde_json::to_string_pretty(&payload).context("serialize response")?;
        println!("{json}");
        return Ok(());
    }

    let batch =
        eli_core::web::providers::read::read_urls_with_diagnostics(&urls, args.max_parallel).await;
    let payload = if args.full {
        serde_json::to_value(&batch).context("serialize web read batch response")?
    } else {
        compact_web_read_batch_json(&batch, args.max_chars)
    };

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &payload, "web.read", &meta_args)?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&payload).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn compact_web_search_json(resp: &eli_core::web::WebSearchResponse) -> serde_json::Value {
    let items = resp
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "rank": item.rank,
                "title": item.title,
                "url": item.url,
                "domain": item.domain,
                "published_at": item.published_at,
                "source": item.source,
                "score_final": item.scores.final_score,
                "read_probe": item.read_probe.as_ref().map(|probe| serde_json::json!({
                    "fetch_status": probe.fetch_status,
                    "blocked_reason": probe.blocked_reason,
                    "attempts_count": probe.attempts_count,
                    "text_chars": probe.text_chars,
                })),
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "query": resp.query,
        "mode": resp.mode,
        "generated_at": resp.generated_at,
        "providers": resp.providers,
        "stats": resp.stats,
        "run_delta": resp.run_delta,
        "items": items,
        "view": "compact",
    })
}

fn compact_web_read_json(
    resp: &eli_core::web::WebReadResponse,
    max_chars: usize,
) -> serde_json::Value {
    let (text, truncated, text_chars_total) = truncate_for_token_budget(&resp.text, max_chars);
    let failed_attempts = resp
        .attempts
        .iter()
        .filter(|attempt| !attempt.ok)
        .map(|attempt| {
            serde_json::json!({
                "attempt": attempt.attempt,
                "method": attempt.method,
                "http_status": attempt.http_status,
                "blocked_reason": attempt.blocked_reason,
                "error": attempt.error,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "url": resp.url,
        "final_url": resp.final_url,
        "title": resp.title,
        "text": text,
        "text_chars_total": text_chars_total,
        "text_truncated": truncated,
        "fetch_status": resp.fetch_status,
        "blocked_reason": resp.blocked_reason,
        "attempts_count": resp.attempts.len(),
        "failed_attempts": failed_attempts,
        "fetched_at": resp.fetched_at,
        "view": "compact",
    })
}

fn compact_web_read_batch_json(
    batch: &eli_core::web::WebReadBatchResponse,
    max_chars: usize,
) -> serde_json::Value {
    let results = batch
        .results
        .iter()
        .map(|resp| compact_web_read_json(resp, max_chars))
        .collect::<Vec<_>>();
    serde_json::json!({
        "mode": batch.mode,
        "requested": batch.requested,
        "deduped": batch.deduped,
        "completed": batch.completed,
        "success_count": batch.success_count,
        "partial_count": batch.partial_count,
        "blocked_count": batch.blocked_count,
        "error_count": batch.error_count,
        "results": results,
        "view": "compact",
    })
}

fn truncate_for_token_budget(text: &str, max_chars: usize) -> (String, bool, usize) {
    let total_chars = text.chars().count();
    if max_chars == 0 || total_chars <= max_chars {
        return (text.to_string(), false, total_chars);
    }

    let mut out = String::with_capacity(max_chars + 32);
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    let trimmed = out.trim_end().to_string();
    let mut final_text = trimmed;
    final_text.push_str("\n...[truncated]");
    (final_text, true, total_chars)
}

fn map_web_search_mode(mode: WebSearchModeArg) -> eli_core::web::WebSearchMode {
    match mode {
        WebSearchModeArg::Auto => eli_core::web::WebSearchMode::Auto,
        WebSearchModeArg::News => eli_core::web::WebSearchMode::News,
        WebSearchModeArg::Finance => eli_core::web::WebSearchMode::Finance,
        WebSearchModeArg::Research => eli_core::web::WebSearchMode::Research,
        WebSearchModeArg::Tech => eli_core::web::WebSearchMode::Tech,
        WebSearchModeArg::Encyclopedia => eli_core::web::WebSearchMode::Encyclopedia,
    }
}

fn map_web_search_recency(
    recency: Option<WebSearchRecencyArg>,
) -> Option<eli_core::web::WebSearchRecency> {
    recency.map(|r| match r {
        WebSearchRecencyArg::Day => eli_core::web::WebSearchRecency::Day,
        WebSearchRecencyArg::Week => eli_core::web::WebSearchRecency::Week,
        WebSearchRecencyArg::Month => eli_core::web::WebSearchRecency::Month,
        WebSearchRecencyArg::Year => eli_core::web::WebSearchRecency::Year,
    })
}

fn collect_web_read_urls(args: &WebReadArgs) -> Result<Vec<String>> {
    let mut out = Vec::<String>::new();
    for raw in &args.url {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    if let Some(path) = &args.urls_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read --urls-file {}", path.display()))?;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            out.push(trimmed.to_string());
        }
    }

    let mut deduped = Vec::<String>::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for url in out {
        if seen.insert(url.clone()) {
            deduped.push(url);
        }
    }
    Ok(deduped)
}

async fn cmd_web_extract(args: WebExtractArgs) -> Result<()> {
    let resp = if let Some(url) = args.url {
        eli_core::extraction::extract_from_url(&url, args.bullets, args.focus)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("extract from url")?
    } else if let Some(file) = args.file {
        eli_core::extraction::extract_from_file(&file, args.bullets, args.focus)
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("extract from file")?
    } else if let Some(text) = args.text {
        let req = eli_core::extraction::ExtractRequest {
            content: text,
            source: "inline".to_string(),
            bullets: args.bullets,
            focus: args.focus,
        };
        eli_core::extraction::extract_facts(req)
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("extract from text")?
    } else {
        anyhow::bail!("must provide --url, --file, or --text");
    };

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "web.extract",
            &[format!("bullets={}", args.bullets)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

/// Redirect JSON output files to eli_research/data/ if they're in the project root.
fn redirect_finance_output(path: std::path::PathBuf) -> std::path::PathBuf {
    // Only redirect if it's a bare filename (no directory component)
    if path
        .parent()
        .map(|p| p == std::path::Path::new("") || p == std::path::Path::new("."))
        .unwrap_or(true)
    {
        if let Some(filename) = path.file_name() {
            let target = std::path::Path::new("eli_research/data").join(filename);
            // Ensure directory exists
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            return target;
        }
    }
    path
}

fn is_auto_out_path(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    stem.eq_ignore_ascii_case("auto")
}

fn canonical_span_token(raw: &str) -> String {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return String::new();
    }
    let mut digits = String::new();
    let mut unit = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !ch.is_whitespace() {
            unit.push(ch);
        }
    }
    if digits.is_empty() {
        return normalize_name_token(&s, true, 16);
    }
    let suffix = match unit.as_str() {
        "y" | "yr" | "yrs" | "year" | "years" => "YR",
        "mo" | "mon" | "month" | "months" => "MO",
        "w" | "wk" | "week" | "weeks" => "W",
        "d" | "day" | "days" => "D",
        "h" | "hr" | "hour" | "hours" => "H",
        "m" | "min" | "mins" | "minute" | "minutes" => "MIN",
        other => return normalize_name_token(&format!("{digits}{other}"), true, 16),
    };
    format!("{digits}{suffix}")
}

fn normalize_name_token(raw: &str, uppercase: bool, max_len: usize) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        let c = if uppercase {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if c == '_' || c == '-' {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.chars().count() > max_len {
        out.chars().take(max_len).collect()
    } else {
        out
    }
}

fn parse_kv_args(args: &[String]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for arg in args {
        if let Some((k, v)) = arg.split_once('=') {
            let key = normalize_name_token(k, false, 48).to_ascii_lowercase();
            out.insert(key, v.to_string());
        }
    }
    out
}

fn tickers_from_payload(value: &serde_json::Value) -> Vec<String> {
    let mut tickers = Vec::new();
    if let Some(arr) = value.get("tickers").and_then(|v| v.as_array()) {
        for t in arr.iter().filter_map(|v| v.as_str()) {
            let tok = normalize_name_token(t, true, 12);
            if !tok.is_empty() {
                tickers.push(tok);
            }
        }
    }
    if tickers.is_empty() {
        if let Some(t) = value.get("ticker").and_then(|v| v.as_str()) {
            let tok = normalize_name_token(t, true, 12);
            if !tok.is_empty() {
                tickers.push(tok);
            }
        }
    }
    if tickers.is_empty() {
        if let Some(series) = value.get("series").and_then(|v| v.as_array()) {
            for row in series {
                if let Some(t) = row.get("ticker").and_then(|v| v.as_str()) {
                    let tok = normalize_name_token(t, true, 12);
                    if !tok.is_empty() {
                        tickers.push(tok);
                    }
                }
            }
        }
    }
    if tickers.is_empty() {
        if let Some(snaps) = value.get("snapshots").and_then(|v| v.as_array()) {
            for row in snaps {
                if let Some(t) = row.get("ticker").and_then(|v| v.as_str()) {
                    let tok = normalize_name_token(t, true, 12);
                    if !tok.is_empty() {
                        tickers.push(tok);
                    }
                }
            }
        }
    }
    tickers.sort();
    tickers.dedup();
    tickers
}

fn tool_prefix(tool_name: &str) -> String {
    match tool_name {
        "finance.timeseries" => "TIMESERIES".to_string(),
        "finance.snapshot" => "SNAPSHOT".to_string(),
        "finance.odds" => "ODDS".to_string(),
        "finance.sync" => "SYNC".to_string(),
        "finance.options" => "OPTIONS".to_string(),
        "finance.prices" => "PRICES".to_string(),
        "finance.news" => "NEWS".to_string(),
        "finance.fundamentals" => "FUNDAMENTALS".to_string(),
        "finance.filings" => "FILINGS".to_string(),
        "finance.search" => "SEARCH".to_string(),
        "finance.macro" => "MACRO".to_string(),
        "finance.schedule" => "SCHEDULE".to_string(),
        "web.search" => "WEBSEARCH".to_string(),
        "web.read" => "WEBREAD".to_string(),
        "web.crawl" => "WEBCRAWL".to_string(),
        "web.extract" => "WEBEXTRACT".to_string(),
        _ => normalize_name_token(tool_name, true, 20),
    }
}

fn build_programmatic_dataset_stem(
    tool_name: &str,
    value: &serde_json::Value,
    args: &[String],
    stamp: &str,
) -> String {
    let kv = parse_kv_args(args);
    let mut parts = vec![tool_prefix(tool_name)];

    let tickers = tickers_from_payload(value);
    if !tickers.is_empty() {
        parts.extend(tickers);
    }

    if let Some(range) = kv.get("range") {
        let tok = canonical_span_token(range);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }
    if let Some(granularity) = kv.get("granularity") {
        let tok = canonical_span_token(granularity);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }

    let provider = value
        .get("provider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| kv.get("provider").cloned());
    if let Some(provider) = provider {
        let tok = normalize_name_token(&provider, true, 12);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }

    if let Some(status) = kv.get("status") {
        let tok = normalize_name_token(status, true, 12);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }

    parts.push(stamp.to_string());
    let mut stem = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if stem.chars().count() > 220 {
        stem = stem.chars().take(220).collect();
    }
    stem
}

fn resolve_programmatic_out_path(
    requested: PathBuf,
    tool_name: &str,
    value: &serde_json::Value,
    args: &[String],
) -> PathBuf {
    let requested = redirect_finance_output(requested);
    if !is_auto_out_path(&requested) {
        return requested;
    }
    let parent = requested.parent().and_then(|p| {
        if p == Path::new("") || p == Path::new(".") {
            None
        } else {
            Some(p.to_path_buf())
        }
    });
    let dir = parent.unwrap_or_else(|| PathBuf::from("eli_research/data"));
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let stem = build_programmatic_dataset_stem(tool_name, value, args, &stamp);
    dir.join(format!("{stem}.json"))
}

#[derive(Clone, Debug)]
struct MetaWriteResult {
    out_path: PathBuf,
    meta_path: PathBuf,
}

fn resolve_abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

fn write_json_out_with_meta<T: Serialize>(
    out_path: PathBuf,
    payload: &T,
    tool_name: &str,
    args: &[String],
) -> Result<MetaWriteResult> {
    let value = serde_json::to_value(payload).context("serialize response to value")?;
    let out_path = resolve_programmatic_out_path(out_path, tool_name, &value, args);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let json = serde_json::to_string_pretty(payload).context("serialize response")?;
    std::fs::write(&out_path, &json).context("write --out")?;

    let abs = resolve_abs_path(&out_path);
    let ctx = eli_core::meta::MetaContext {
        source_path: Some(abs),
        source_kind: eli_core::meta::SourceKind::Json,
        source_size_bytes: Some(json.as_bytes().len() as u64),
        provenance: Some(eli_core::meta::MetaProvenance {
            tool: Some(tool_name.to_string()),
            command: Some(tool_name.to_string()),
            args: args.to_vec(),
            origin_query: None,
        }),
    };
    let meta = eli_core::meta::build_json_meta(&value, ctx);
    let meta_path =
        eli_core::meta::write_sidecar(&meta, &out_path).context("write sidecar meta")?;
    Ok(MetaWriteResult {
        out_path,
        meta_path,
    })
}

fn prediction_markets_path_for_output(out_path: &Path) -> PathBuf {
    let parent = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut existing: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flat_map(|rd| rd.filter_map(|e| e.ok()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.starts_with("prediction_markets_")
                        && n.ends_with(".json")
                        && !n.ends_with(".meta.json")
                })
                .unwrap_or(false)
        })
        .collect();
    existing.sort();
    if let Some(last) = existing.pop() {
        return last;
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    parent.join(format!("prediction_markets_{stamp}.json"))
}

fn push_unique_items(
    dst: &mut Vec<serde_json::Value>,
    src: &[serde_json::Value],
    key_fn: impl Fn(&serde_json::Value) -> Option<String>,
) {
    let mut seen: std::collections::HashSet<String> = dst.iter().filter_map(&key_fn).collect();
    for item in src {
        if let Some(key) = key_fn(item) {
            if seen.insert(key) {
                dst.push(item.clone());
            }
        } else {
            dst.push(item.clone());
        }
    }
}

fn parse_json_array_field(
    root: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<serde_json::Value> {
    root.get(key)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn update_prediction_markets(
    prediction_markets_path: &Path,
    req: &eli_core::finance::OddsRequest,
    resp: &eli_core::finance::OddsResponse,
    source_file: Option<&Path>,
) -> Result<()> {
    let mut bundle: serde_json::Map<String, serde_json::Value> = if prediction_markets_path.exists()
    {
        let raw = std::fs::read_to_string(prediction_markets_path).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    let resp_value = serde_json::to_value(resp).context("serialize odds response for bundle")?;
    let resp_obj = resp_value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("odds response was not an object"))?;

    let mut query_entry = serde_json::Map::new();
    query_entry.insert(
        "recorded_at".to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    query_entry.insert(
        "provider".to_string(),
        req.provider
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "search".to_string(),
        req.search
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "series_ticker".to_string(),
        req.series_ticker
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "event_ticker".to_string(),
        req.event_ticker
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "market_ticker".to_string(),
        req.market_ticker
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "status".to_string(),
        req.status
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "source_file".to_string(),
        source_file
            .map(|p| serde_json::Value::String(resolve_abs_path(p).display().to_string()))
            .unwrap_or(serde_json::Value::Null),
    );

    let mut queries = parse_json_array_field(&bundle, "queries");
    queries.push(serde_json::Value::Object(query_entry));
    if queries.len() > 500 {
        let drop_n = queries.len().saturating_sub(500);
        queries.drain(0..drop_n);
    }
    bundle.insert("queries".to_string(), serde_json::Value::Array(queries));

    let mut available_markets = parse_json_array_field(&bundle, "available_markets");
    let new_available_markets = parse_json_array_field(&resp_obj, "available_markets");
    push_unique_items(&mut available_markets, &new_available_markets, |v| {
        let obj = v.as_object()?;
        obj.get("market_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert(
        "available_markets".to_string(),
        serde_json::Value::Array(available_markets),
    );

    let mut markets = parse_json_array_field(&bundle, "markets");
    let new_markets = parse_json_array_field(&resp_obj, "markets");
    push_unique_items(&mut markets, &new_markets, |v| {
        let obj = v.as_object()?;
        obj.get("market_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert("markets".to_string(), serde_json::Value::Array(markets));

    let mut available_events = parse_json_array_field(&bundle, "available_events");
    let new_available_events = parse_json_array_field(&resp_obj, "available_events");
    push_unique_items(&mut available_events, &new_available_events, |v| {
        let obj = v.as_object()?;
        obj.get("event_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert(
        "available_events".to_string(),
        serde_json::Value::Array(available_events),
    );

    let mut events = parse_json_array_field(&bundle, "events");
    let new_events = parse_json_array_field(&resp_obj, "events");
    push_unique_items(&mut events, &new_events, |v| {
        let obj = v.as_object()?;
        obj.get("event_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert("events".to_string(), serde_json::Value::Array(events));

    let mut sources = parse_json_array_field(&bundle, "sources");
    let new_sources = parse_json_array_field(&resp_obj, "sources");
    push_unique_items(&mut sources, &new_sources, |v| {
        let obj = v.as_object()?;
        obj.get("source")
            .and_then(|x| x.as_str())
            .map(|s| format!("source:{s}"))
    });
    bundle.insert("sources".to_string(), serde_json::Value::Array(sources));

    if let Some(semantics) = resp_obj.get("field_semantics") {
        bundle.insert("field_semantics".to_string(), semantics.clone());
    }

    bundle.insert(
        "bundle_type".to_string(),
        serde_json::Value::String("eli_finance_prediction_markets".to_string()),
    );
    bundle.insert(
        "updated_at".to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    let query_count = bundle
        .get("queries")
        .and_then(|v| v.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    bundle.insert(
        "query_count".to_string(),
        serde_json::Value::Number(serde_json::Number::from(query_count as u64)),
    );

    if let Some(parent) = prediction_markets_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(bundle))
        .context("serialize odds bundle")?;
    std::fs::write(prediction_markets_path, json).context("write prediction markets")?;
    Ok(())
}

fn write_shadow_meta_for_value(
    target_data_path: &Path,
    value: &serde_json::Value,
    tool_name: &str,
    command: &str,
) -> Result<PathBuf> {
    let raw = serde_json::to_string(value).unwrap_or_default();
    let ctx = eli_core::meta::MetaContext {
        source_path: Some(resolve_abs_path(target_data_path)),
        source_kind: eli_core::meta::SourceKind::Json,
        source_size_bytes: Some(raw.as_bytes().len() as u64),
        provenance: Some(eli_core::meta::MetaProvenance {
            tool: Some(tool_name.to_string()),
            command: Some(command.to_string()),
            args: Vec::new(),
            origin_query: None,
        }),
    };
    let meta = eli_core::meta::build_json_meta(value, ctx);
    eli_core::meta::write_sidecar(&meta, target_data_path).context("write implicit sidecar meta")
}

fn schema_pattern_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let meta = eli_core::meta::build_json_meta(
        value,
        eli_core::meta::MetaContext {
            source_kind: eli_core::meta::SourceKind::Json,
            ..Default::default()
        },
    );
    let root = format!("schema_root={}", meta.schema_tree.kind);
    let path_count = format!("schema_paths={}", meta.path_index.len());
    let nullable = format!("nullable_fields={}", meta.vitals.nullable_paths);
    vec![root, path_count, nullable]
}
