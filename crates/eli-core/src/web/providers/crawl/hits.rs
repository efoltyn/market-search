pub fn crawl_to_hits(response: &CrawlResponse) -> Vec<WebHit> {
    response
        .pages
        .iter()
        .map(|page| WebHit {
            title: page.title.clone().unwrap_or_else(|| page.url.clone()),
            url: page.url.clone(),
            snippet: page.text_preview.clone(),
            source: "Spider Crawl".to_string(),
            score: 1.0,
            published: None,
            provenance: "crawl".to_string(),
        })
        .collect()
}
