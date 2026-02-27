pub async fn crawl_website(req: CrawlRequest) -> Result<CrawlResponse> {
    let start = std::time::Instant::now();
    let generated_at = chrono::Utc::now();

    let mut website = Website::new(&req.url);

    // Configure crawl behavior
    website.with_respect_robots_txt(req.respect_robots);

    if !req.include_subdomains {
        website.with_subdomains(false);
    }

    // Set up page collection
    let max_pages = req.max_pages.unwrap_or(50);
    website.with_limit(max_pages as u32);
    let pages: Arc<Mutex<Vec<CrawledPage>>> = Arc::new(Mutex::new(Vec::new()));
    let pages_clone = Arc::clone(&pages);
    let done = Arc::new(Notify::new());
    let done_clone = Arc::clone(&done);

    // If the user passed a deep URL path, restrict crawling to that path prefix. This aligns the
    // tool contract ("crawl this URL") with spider's default behavior (crawl the whole host).
    if let Ok(url) = reqwest::Url::parse(&req.url) {
        let host = match url.host_str() {
            Some(h) if !h.is_empty() => h,
            _ => "",
        };
        if !host.is_empty() {
            let mut path_prefix = url.path().to_string();
            if !path_prefix.ends_with('/') {
                if let Some((dir, _file)) = path_prefix.rsplit_once('/') {
                    path_prefix = if dir.is_empty() {
                        "/".to_string()
                    } else {
                        format!("{dir}/")
                    };
                } else {
                    path_prefix = "/".to_string();
                }
            }

            let mut host_port = host.to_string();
            if let Some(port) = url.port() {
                host_port.push(':');
                host_port.push_str(&port.to_string());
            }

            let escape_re = |s: &str| {
                let mut out = String::with_capacity(s.len());
                for ch in s.chars() {
                    match ch {
                        '.' | '+' | '*' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}'
                        | '|' | '\\' => {
                            out.push('\\');
                            out.push(ch);
                        }
                        _ => out.push(ch),
                    }
                }
                out
            };

            let pattern = format!(
                "^https?://{}{}.*",
                escape_re(&host_port),
                escape_re(&path_prefix)
            );
            website.with_whitelist_url(Some(vec![pattern.into()]));
        }
    }

    // Subscribe to crawl events
    let mut rx = website
        .subscribe(16)
        .ok_or_else(|| Error::Provider("Failed to subscribe to crawl events".to_string()))?;

    // Spawn task to collect pages
    let collector = tokio::spawn(async move {
        while let Ok(page) = rx.recv().await {
            let mut pages_guard = pages_clone.lock().await;
            if pages_guard.len() >= max_pages {
                break;
            }

            let url = page.get_url().to_string();
            let html = page.get_html();

            // Extract title and text preview
            let (title, text_preview, links_count) = extract_page_info(&html);

            pages_guard.push(CrawledPage {
                url,
                title,
                text_preview,
                links_found: links_count,
                fetched_at: chrono::Utc::now(),
            });

            if pages_guard.len() >= max_pages {
                done_clone.notify_one();
                break;
            }
        }
    });

    // Run the crawl (bounded by max_pages and a hard timeout)
    let timeout = StdDuration::from_secs(10 + max_pages as u64);
    info!(
        target: "eli.web.crawl",
        url = %req.url,
        max_pages = max_pages,
        timeout_secs = timeout.as_secs(),
        "crawl start"
    );
    let include_sitemap = req.include_sitemap;
    let smart_mode = req.smart_mode;
    let mut crawl_task = tokio::spawn(async move {
        if include_sitemap {
            website.crawl_sitemap().await;
        } else if smart_mode {
            website.crawl_smart().await;
        } else {
            website.crawl().await;
        }
    });

    tokio::select! {
        _ = done.notified() => {
            info!(
                target: "eli.web.crawl",
                url = %req.url,
                pages = max_pages,
                elapsed_ms = start.elapsed().as_millis(),
                "crawl reached max_pages"
            );
            crawl_task.abort();
        }
        _ = tokio::time::sleep(timeout) => {
            warn!(
                target: "eli.web.crawl",
                url = %req.url,
                elapsed_ms = start.elapsed().as_millis(),
                "crawl timed out"
            );
            crawl_task.abort();
            collector.abort();
            return Err(Error::Provider("crawl timed out".to_string()));
        }
        _ = &mut crawl_task => {
            info!(
                target: "eli.web.crawl",
                url = %req.url,
                elapsed_ms = start.elapsed().as_millis(),
                "crawl completed"
            );
        }
    }

    // Wait for collector to finish
    let _ = collector.await;

    let pages_result = pages.lock().await.clone();
    let duration_ms = start.elapsed().as_millis();

    Ok(CrawlResponse {
        base_url: req.url,
        generated_at,
        crawl_mode: if req.include_sitemap {
            "sitemap".to_string()
        } else if req.smart_mode {
            "smart".to_string()
        } else {
            "crawl".to_string()
        },
        pages_crawled: pages_result.len(),
        pages: pages_result,
        duration_ms,
    })
}
