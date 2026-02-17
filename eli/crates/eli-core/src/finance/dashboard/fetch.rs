use super::super::*;
use tokio::time::{timeout, Duration as TokioDuration};

#[derive(Debug, Deserialize)]
struct OddsCsvRow {
    source: String,
    ticker: String,
    title: String,
    event_ticker: String,
    yes_price: String,
    volume: String,
    status: String,
    probability: String,
    category: String,
    topic: String,
}

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

pub async fn fetch_dashboard(req: DashboardRequest) -> Result<DashboardResponse> {
    let preset = req.preset.trim().to_ascii_lowercase();
    if preset != "recession" {
        return Err(Error::InvalidInput(
            "unsupported --preset (v1 supports: recession)".to_string(),
        ));
    }

    let macro_fut = async {
        fetch_macro(MacroRequest {
            range: None,
            compare_to: None,
        })
        .await
    };
    let snapshot_fut = async {
        fetch_snapshot(SnapshotRequest {
            tickers: vec![
                "SPY".to_string(),
                "TLT".to_string(),
                "HYG".to_string(),
                "GLD".to_string(),
                "UUP".to_string(),
            ],
            provider: ProviderKind::Yahoo,
        })
        .await
    };
    let odds_fut = async {
        if req.max_ms.unwrap_or(0) > 0 && req.max_ms.unwrap_or(0) <= 5_000 {
            return Err(Error::Provider(
                "skipped due tight max-ms budget".to_string(),
            ));
        }
        let queries = ["recession", "unemployment", "federal reserve"];
        let mut out = Vec::new();
        for q in queries {
            out.push(search_odds_csv(q, 25)?);
        }
        Ok::<Vec<DashboardOddsSearch>, Error>(out)
    };
    let options_fut = async {
        if req.max_ms.unwrap_or(0) > 0 && req.max_ms.unwrap_or(0) <= 5_000 {
            return Err(Error::Provider(
                "skipped due tight max-ms budget".to_string(),
            ));
        }
        fetch_options(OptionsRequest {
            ticker: "SPY".to_string(),
            expiry: None,
            option_type: None,
            near_money_pct: None,
            summary_only: true,
            list_expirations: false,
            multi_expiry: false,
            num_expiries: None,
        })
        .await
    };
    let rate_path_fut = async {
        fetch_rate_path(RatePathRequest {
            cache_dir: None,
            source_mode: Some(RatePathSourceMode::Auto),
        })
        .await
    };

    fn section_budget_ms(global_ms: Option<u64>, default_cap_ms: u64) -> Option<u64> {
        match global_ms {
            Some(ms) if ms > 0 => Some(ms.min(default_cap_ms)),
            Some(_) => None,
            None => Some(default_cap_ms),
        }
    }

    let macro_ms = section_budget_ms(req.max_ms, 15_000);
    let snapshot_ms = section_budget_ms(req.max_ms, 12_000);
    let odds_ms = section_budget_ms(req.max_ms, 10_000);
    let options_ms = section_budget_ms(req.max_ms, 20_000);
    let rate_ms = section_budget_ms(req.max_ms, 20_000);

    let macro_fut = async {
        if let Some(ms) = macro_ms {
            timeout(TokioDuration::from_millis(ms), macro_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            macro_fut.await
        }
    };
    let snapshot_fut = async {
        if let Some(ms) = snapshot_ms {
            timeout(TokioDuration::from_millis(ms), snapshot_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            snapshot_fut.await
        }
    };
    let odds_fut = async {
        if let Some(ms) = odds_ms {
            timeout(TokioDuration::from_millis(ms), odds_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            odds_fut.await
        }
    };
    let options_fut = async {
        if let Some(ms) = options_ms {
            timeout(TokioDuration::from_millis(ms), options_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            options_fut.await
        }
    };
    let rate_path_fut = async {
        if let Some(ms) = rate_ms {
            timeout(TokioDuration::from_millis(ms), rate_path_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            rate_path_fut.await
        }
    };

    let (macro_r, snap_r, odds_r, options_r, rate_r) = tokio::join!(
        macro_fut,
        snapshot_fut,
        odds_fut,
        options_fut,
        rate_path_fut
    );

    let mut warnings = Vec::new();
    let mut health: BTreeMap<String, SectionHealth> = BTreeMap::new();
    let now = Utc::now();
    let macro_data = match macro_r {
        Ok(v) => {
            let coverage = (v.indicators.len() as f64 / 32.0).clamp(0.0, 1.0);
            health.insert(
                "macro".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: coverage,
                    confidence: (0.55 + (coverage * 0.35)).clamp(0.0, 0.99),
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("macro: {e}"));
            health.insert(
                "macro".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let snapshots = match snap_r {
        Ok(v) => {
            let coverage = (v.snapshots.len() as f64 / 5.0).clamp(0.0, 1.0);
            health.insert(
                "snapshot".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: coverage,
                    confidence: (0.55 + (coverage * 0.35)).clamp(0.0, 0.99),
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("snapshot: {e}"));
            health.insert(
                "snapshot".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let odds = match odds_r {
        Ok(v) => {
            let covered = v.iter().filter(|s| !s.markets.is_empty()).count();
            let coverage = (covered as f64 / 3.0).clamp(0.0, 1.0);
            health.insert(
                "odds".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: coverage,
                    confidence: (0.5 + (coverage * 0.35)).clamp(0.0, 0.95),
                    as_of: Some(now),
                    age_seconds: Some(0),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("odds: {e}"));
            health.insert(
                "odds".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let options = match options_r {
        Ok(v) => {
            health.insert(
                "options".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: 1.0,
                    confidence: 0.75,
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("options: {e}"));
            health.insert(
                "options".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let rate_path = match rate_r {
        Ok(v) => {
            health.insert(
                "rate_path".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: v.coverage_ratio,
                    confidence: v.confidence,
                    as_of: Some(v.as_of),
                    age_seconds: Some(v.age_seconds),
                    notes: v.warnings.clone(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("rate_path: {e}"));
            health.insert(
                "rate_path".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };

    if macro_data.is_none()
        && snapshots.is_none()
        && odds.is_none()
        && options.is_none()
        && rate_path.is_none()
    {
        return Err(Error::Provider(
            "all dashboard sections failed for preset recession".to_string(),
        ));
    }

    let as_of = health.values().filter_map(|h| h.as_of).max().unwrap_or(now);
    let age_seconds = (now - as_of).num_seconds().max(0);
    Ok(DashboardResponse {
        preset,
        generated_at: now,
        as_of,
        age_seconds,
        macro_data,
        snapshots,
        odds,
        options,
        rate_path,
        section_health: Some(health),
        warnings,
    })
}
