fn parse_terms(query: &str) -> Vec<String> {
    query
        .to_ascii_lowercase()
        .split_whitespace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn compile_term_patterns(terms: &[String]) -> Vec<(String, regex::Regex)> {
    terms
        .iter()
        .filter_map(|t| {
            regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                .ok()
                .map(|re| (t.clone(), re))
        })
        .collect()
}

fn compute_match_terms(text: &str, term_patterns: &[(String, regex::Regex)]) -> Vec<String> {
    term_patterns
        .iter()
        .filter_map(|(term, re)| re.is_match(text).then_some(term.clone()))
        .collect()
}

fn contains_keyword(haystack: &str, keyword: &str) -> bool {
    if keyword.contains(' ') || keyword.contains('.') {
        return haystack.contains(keyword);
    }
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == keyword)
}

fn us_hints(text: &str) -> Vec<String> {
    let lowered = text.to_ascii_lowercase();
    let keywords = [
        "us",
        "u.s.",
        "united states",
        "american",
        "nfp",
        "nonfarm payrolls",
        "fomc",
        "federal reserve",
        "cpi",
        "pce",
        "gdpnow",
    ];
    keywords
        .iter()
        .filter(|k| contains_keyword(&lowered, k))
        .map(|k| k.to_string())
        .collect()
}

fn match_score(row: &OddsCsvRow, query: &str, match_terms: &[String], volume_usd: f64) -> i64 {
    let q = query.to_ascii_lowercase();
    let title = row.title.to_ascii_lowercase();
    let ticker = row.ticker.to_ascii_lowercase();
    let event = row.event_ticker.to_ascii_lowercase();
    let category = row.category.to_ascii_lowercase();
    let topic = row.topic.to_ascii_lowercase();

    let mut score = 0.0;
    if !q.is_empty() && title.contains(&q) {
        score += 30.0;
    }
    for t in match_terms {
        if title.contains(t) {
            score += 10.0;
        }
        if ticker.contains(t) || event.contains(t) {
            score += 6.0;
        }
        if category.contains(t) || topic.contains(t) {
            score += 4.0;
        }
    }
    score += (match_terms.len() as f64) * 8.0;
    score += (volume_usd.max(0.0) + 1.0).log10() * 3.0;
    score.round() as i64
}

fn odds_cache_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds").join("all_markets.csv"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache").join("all_markets.csv"))
}

fn search_odds_csv(query: &str, limit: usize) -> Result<DashboardOddsSearch> {
    let csv_path = odds_cache_path();
    if !csv_path.exists() {
        return Err(Error::InvalidInput(format!(
            "no local prediction market cache found at {}. Run `eli finance sync` first.",
            csv_path.display()
        )));
    }

    let terms = parse_terms(query);
    let term_patterns = compile_term_patterns(&terms);
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&csv_path)
        .map_err(|e| Error::Provider(format!("open {} failed: {e}", csv_path.display())))?;

    let mut rows: Vec<DashboardOddsMarket> = Vec::new();
    for rec in rdr.deserialize::<OddsCsvRow>() {
        let rec = match rec {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            rec.source, rec.ticker, rec.title, rec.event_ticker, rec.category, rec.topic
        );
        let match_terms = compute_match_terms(&searchable, &term_patterns);
        if match_terms.is_empty() {
            continue;
        }

        let volume: f64 = rec.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = volume / 100.0;
        let yes_price: f64 = rec.yes_price.trim().parse().unwrap_or(0.0);
        let probability: f64 = rec.probability.trim().parse().unwrap_or(0.0);
        let hints = us_hints(&searchable);
        let score = match_score(&rec, query, &match_terms, volume_usd);

        rows.push(DashboardOddsMarket {
            source: rec.source,
            ticker: rec.ticker,
            title: rec.title,
            event_ticker: rec.event_ticker,
            yes_price,
            volume,
            volume_usd,
            status: rec.status,
            probability,
            category: rec.category,
            topic: rec.topic,
            match_score: score,
            match_terms,
            country_hints: hints,
        });
    }

    rows.sort_by(|a, b| {
        b.match_score.cmp(&a.match_score).then_with(|| {
            b.volume_usd
                .partial_cmp(&a.volume_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    let total_matches = rows.len();
    rows.truncate(limit);

    Ok(DashboardOddsSearch {
        query: query.to_string(),
        total_matches,
        markets: rows,
    })
}

