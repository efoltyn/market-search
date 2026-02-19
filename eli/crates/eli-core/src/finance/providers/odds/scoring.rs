fn text_relevance_score(text: &str, terms: &[String]) -> i32 {
    if terms.is_empty() {
        return 0;
    }
    let lower = text.to_ascii_lowercase();
    let mut score = 0i32;
    for term in terms {
        if lower == *term {
            score += 6;
        } else if lower.contains(term) {
            score += 3;
        }
        if lower.starts_with(term) {
            score += 2;
        }
    }
    score
}

fn score_listed_event(e: &OddsListedEvent, terms: &[String]) -> i32 {
    let mut score = 0i32;
    score += text_relevance_score(&e.title, terms) * 4;
    score += text_relevance_score(&e.ticker, terms) * 3;
    if let Some(slug) = e.slug.as_deref() {
        score += text_relevance_score(slug, terms) * 2;
    }
    if let Some(category) = e.category.as_deref() {
        score += text_relevance_score(category, terms);
    }
    score
}

fn score_listed_market(m: &OddsListedMarket, terms: &[String]) -> i32 {
    let mut score = 0i32;
    score += text_relevance_score(&m.title, terms) * 4;
    score += text_relevance_score(&m.ticker, terms) * 3;
    score += text_relevance_score(&m.event_ticker, terms) * 2;
    if let Some(slug) = m.slug.as_deref() {
        score += text_relevance_score(slug, terms) * 2;
    }
    if let Some(category) = m.category.as_deref() {
        score += text_relevance_score(category, terms);
    }
    if let Some(status) = m.status.as_deref() {
        if status.eq_ignore_ascii_case("open") {
            score += 4;
        }
    }
    if let Some(volume) = m.volume {
        if volume > 0 {
            score += ((volume as f64).log10().floor() as i32).max(1);
        }
    }
    score
}

fn score_market(m: &OddsMarket, terms: &[String]) -> i32 {
    let mut score = 0i32;
    score += text_relevance_score(&m.title, terms) * 4;
    score += text_relevance_score(&m.ticker, terms) * 3;
    score += text_relevance_score(&m.event_ticker, terms) * 2;
    if let Some(slug) = m.slug.as_deref() {
        score += text_relevance_score(slug, terms) * 2;
    }
    if let Some(status) = m.status.as_deref() {
        if status.eq_ignore_ascii_case("open") {
            score += 4;
        }
    }
    if let Some(volume) = m.volume {
        if volume > 0 {
            score += ((volume as f64).log10().floor() as i32).max(1);
        }
    }
    score
}

