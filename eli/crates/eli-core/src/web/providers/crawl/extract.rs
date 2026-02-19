fn extract_page_info(html: &str) -> (Option<String>, String, usize) {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);

    // Extract title
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.text().collect::<Vec<_>>().join("").trim().to_string());

    // Prefer semantic content nodes to avoid JavaScript/CSS boilerplate in modern docs sites.
    let semantic_selectors = [
        "main",
        "article",
        "[role='main']",
        "h1, h2, h3, p, li, td, th, pre, code, blockquote",
    ];
    let mut body_text = String::new();
    for css in semantic_selectors {
        let Ok(sel) = Selector::parse(css) else {
            continue;
        };
        let mut chunks: Vec<String> = Vec::new();
        for el in document.select(&sel) {
            let t = el
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !t.is_empty() {
                chunks.push(t);
            }
        }
        if !chunks.is_empty() {
            body_text = chunks.join(" ");
            break;
        }
    }
    if body_text.is_empty() {
        body_text = Selector::parse("body")
            .ok()
            .and_then(|sel| document.select(&sel).next())
            .map(|el| {
                el.text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
    }
    body_text = body_text
        .split_whitespace()
        .take(100)
        .collect::<Vec<_>>()
        .join(" ");

    // Count links
    let links_count = Selector::parse("a[href]")
        .ok()
        .map(|sel| document.select(&sel).count())
        .unwrap_or(0);

    (title, body_text, links_count)
}
