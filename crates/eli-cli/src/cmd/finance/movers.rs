const YAHOO_SCREENER_SAVED_URL: &str =
    "https://query1.finance.yahoo.com/v1/finance/screener/predefined/saved";
const YAHOO_QUOTE_SUMMARY_URL: &str =
    "https://query2.finance.yahoo.com/v10/finance/quoteSummary";

#[derive(Debug, Clone)]
struct MoversCandidate {
    ticker: String,
    name: Option<String>,
    exchange: Option<String>,
    price: Option<f64>,
    previous_close: Option<f64>,
    change_pct: Option<f64>,
    change_abs: Option<f64>,
    market_cap: Option<u64>,
    volume: Option<u64>,
    source: String,
    quote_source: Option<String>,
    market_state: Option<String>,
    sector: Option<String>,
    industry: Option<String>,
    quote_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct FinanceMoversResponse {
    schema_version: &'static str,
    generated_at: chrono::DateTime<chrono::Utc>,
    provider: String,
    universe: String,
    direction: String,
    sort_by: String,
    candidate_count: usize,
    returned: usize,
    filters: FinanceMoversFilters,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    movers: Vec<FinanceMover>,
}

#[derive(Debug, Serialize)]
struct FinanceMoversFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    min_market_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_market_cap: Option<u64>,
    min_change_pct: f64,
    min_price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_volume: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    industry: Option<String>,
    limit: usize,
    scan_limit: usize,
}

#[derive(Debug, Serialize)]
struct FinanceMover {
    ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exchange: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_close: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_abs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    market_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dollar_volume: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_value_change: Option<f64>,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    market_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    industry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_previous_close: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_change_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_change_abs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_timestamp_utc: Option<chrono::DateTime<chrono::Utc>>,
}

async fn cmd_finance_movers(args: FinanceMoversArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let started = std::time::Instant::now();
    let min_market_cap = args
        .min_market_cap
        .as_deref()
        .map(movers_parse_market_cap)
        .transpose()
        .context("parse --min-market-cap")?;
    let max_market_cap = args
        .max_market_cap
        .as_deref()
        .map(movers_parse_market_cap)
        .transpose()
        .context("parse --max-market-cap")?;
    if let (Some(min), Some(max)) = (min_market_cap, max_market_cap) {
        if min > max {
            anyhow::bail!("--min-market-cap cannot exceed --max-market-cap");
        }
    }
    if args.limit == 0 {
        anyhow::bail!("--limit must be > 0");
    }
    if args.scan_limit == 0 {
        anyhow::bail!("--scan-limit must be > 0");
    }

    let direction = movers_parse_direction(&args.direction)?;
    let sort_by = movers_parse_sort_by(&args.sort_by)?;
    let mut universe = args.universe.trim().to_ascii_lowercase();
    let provider_arg = args.provider.trim().to_ascii_lowercase();
    let use_ibkr = match provider_arg.as_str() {
        "auto" => movers_has_ibkr_hint(&args),
        "ibkr" => true,
        "yahoo" => false,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: auto, yahoo, ibkr)"),
    };

    let mut warnings = Vec::new();

    // Auto-promote default day_movers → small_cap_gainers + day_losers when the user
    // is filtering by max_market_cap ≤ 5B. The day_gainers scrId only includes large
    // caps, so a small-cap filter on top of it returns 0 candidates. The dedicated
    // small_cap_gainers scrId actually surfaces sub-$5B names.
    let mut auto_promoted = false;
    if universe == "day_movers" && args.tickers.is_empty() {
        if let Some(max_cap) = max_market_cap {
            if max_cap <= 5_000_000_000 {
                let promoted = match direction {
                    MoversDirection::Gainers => "small_cap_gainers",
                    MoversDirection::Losers => "day_losers",
                    MoversDirection::Both => "small_cap_combo",
                };
                warnings.push(format!(
                    "auto-promoted --universe from 'day_movers' to '{promoted}' because --max-market-cap is ≤ 5B (Yahoo's day_gainers screener only includes large caps)"
                ));
                universe = promoted.to_string();
                auto_promoted = true;
            }
        }
    }
    let _ = auto_promoted; // reserved for telemetry

    let mut candidates = if !args.tickers.is_empty() || universe == "tickers" {
        let tickers = movers_normalize_tickers(&args.tickers);
        if tickers.is_empty() {
            anyhow::bail!("--tickers is required when --universe tickers");
        }
        let mut from_snapshot = movers_fetch_snapshot_candidates(
            &tickers,
            eli_core::finance::ProviderKind::Yahoo,
            &None,
            &mut warnings,
        )
        .await?;
        // eli-core's snapshot path sometimes returns price == previous_close for
        // ETFs (and other instruments) when the regular session is closed —
        // change_pct comes out as 0 even when there's a real intraday move
        // visible on Yahoo's chart endpoint. Re-enrich any candidate whose
        // change_pct is 0 (or null) by reading regularMarketPrice and
        // chartPreviousClose from /v8/finance/chart, which doesn't require
        // a Yahoo crumb cookie. NOTE: the upstream root cause is in
        // eli-core's snapshot mapping, but per task scope we patch downstream
        // here without modifying eli-core.
        movers_enrich_change_pct_from_chart(&mut from_snapshot, &mut warnings).await;
        from_snapshot
    } else {
        movers_fetch_yahoo_screener_candidates(&universe, direction, args.scan_limit, &mut warnings)
            .await?
    };

    // ETF AUM enrichment — Yahoo's screener leaves marketCap null on ETFs because
    // they have AUM (totalAssets), not a market cap. Pull totalAssets via
    // quoteSummary?modules=fundProfile,defaultKeyStatistics for any ETF still
    // missing market_cap, and surface the AUM in the market_cap field so the
    // existing min/max-market-cap filters and ranking work on ETFs as expected.
    movers_enrich_etf_aum(&mut candidates, &mut warnings).await;

    let yahoo_meta = if use_ibkr {
        let tickers: Vec<String> = candidates.iter().map(|c| c.ticker.clone()).collect();
        let meta = movers_fetch_snapshot_candidates(
            &tickers,
            eli_core::finance::ProviderKind::Yahoo,
            &None,
            &mut warnings,
        )
        .await
        .unwrap_or_default();
        Some(meta)
    } else {
        None
    };

    if use_ibkr {
        let tickers: Vec<String> = candidates.iter().map(|c| c.ticker.clone()).collect();
        match movers_fetch_snapshot_candidates(
            &tickers,
            eli_core::finance::ProviderKind::Ibkr,
            &movers_ibkr_config(&args),
            &mut warnings,
        )
        .await
        {
            Ok(ibkr_candidates) if !ibkr_candidates.is_empty() => {
                candidates = movers_merge_price_source(ibkr_candidates, candidates, yahoo_meta.unwrap_or_default());
            }
            Ok(_) => {
                warnings.push("IBKR returned no mover snapshots; using Yahoo candidates".to_string());
            }
            Err(err) => {
                if provider_arg == "ibkr" {
                    return Err(err);
                }
                eprintln!("movers: IBKR unavailable ({err}); using Yahoo candidates");
                warnings.push("IBKR unavailable; using Yahoo candidates".to_string());
            }
        }
    }

    let candidate_count = candidates.len();
    let filters = FinanceMoversFilters {
        min_market_cap,
        max_market_cap,
        min_change_pct: args.min_change_pct,
        min_price: args.min_price,
        min_volume: args.min_volume,
        sector: args
            .sector
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        industry: args
            .industry
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        limit: args.limit,
        scan_limit: args.scan_limit,
    };

    let mut movers: Vec<FinanceMover> = candidates
        .into_iter()
        .filter(|candidate| movers_passes_filters(candidate, direction, &filters))
        .map(movers_candidate_to_output)
        .collect();

    if args.include_extended_hours && !movers.is_empty() {
        let tickers: Vec<String> = movers.iter().map(|m| m.ticker.clone()).collect();
        let quotes = fetch_extended_hours_quotes_batch(&tickers).await;
        let by_ticker: std::collections::BTreeMap<String, ExtendedHoursQuote> =
            quotes.into_iter().map(|q| (q.ticker.clone(), q)).collect();
        for mover in movers.iter_mut() {
            if let Some(eh) = by_ticker.get(&mover.ticker) {
                mover.extended_price = eh.extended_price;
                mover.extended_previous_close = eh.regular_price;
                mover.extended_change_pct = eh.extended_change_pct;
                mover.extended_change_abs = eh.extended_change_abs;
                mover.extended_session = eh.session.clone();
                mover.extended_timestamp_utc = eh.timestamp_utc;
            }
        }
    }

    movers_sort(&mut movers, sort_by, direction);
    movers.truncate(args.limit);

    let provider = if use_ibkr && movers.iter().any(|m| m.source == "ibkr") {
        "ibkr"
    } else {
        "yahoo"
    };
    let response = FinanceMoversResponse {
        schema_version: "finance.movers.v1",
        generated_at: chrono::Utc::now(),
        provider: provider.to_string(),
        universe: args.universe,
        direction: args.direction,
        sort_by: args.sort_by,
        candidate_count,
        returned: movers.len(),
        filters,
        warnings,
        movers,
    };

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &response,
            "finance.movers",
            &[
                format!("provider={provider}"),
                format!("latency_ms={}", started.elapsed().as_millis()),
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

    let json = serde_json::to_string_pretty(&response).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn movers_fetch_yahoo_screener_candidates(
    universe: &str,
    direction: MoversDirection,
    scan_limit: usize,
    warnings: &mut Vec<String>,
) -> Result<Vec<MoversCandidate>> {
    let scr_ids: Vec<&str> = match universe {
        "day_movers" | "us_equities" => match direction {
            MoversDirection::Gainers => vec!["day_gainers"],
            MoversDirection::Losers => vec!["day_losers"],
            MoversDirection::Both => vec!["day_gainers", "day_losers"],
        },
        "day_gainers" | "gainers" => vec!["day_gainers"],
        "day_losers" | "losers" => vec!["day_losers"],
        "most_actives" | "actives" | "most_active" => vec!["most_actives"],
        "small_cap_gainers" | "small_caps" | "small_cap" => vec!["small_cap_gainers"],
        "aggressive_small_caps" | "aggressive" => vec!["aggressive_small_caps"],
        // Yahoo's ETF screener canonical IDs are `top_etfs_us` (gainers) and
        // `most_actives_etfs` (volume). The legacy `top_etfs` alias 404s.
        "top_etfs" | "top_etfs_us" | "etfs" | "etf" => vec!["top_etfs_us"],
        "most_active_etfs" | "most_actives_etfs" => vec!["most_actives_etfs"],
        "most_shorted" | "most_shorted_stocks" | "shorted" => vec!["most_shorted_stocks"],
        // Synthetic combo used by auto-promotion: small-cap gainers + day losers.
        // Yahoo doesn't expose a small_cap_losers scrId, so day_losers is the
        // closest available losers feed.
        "small_cap_combo" => vec!["small_cap_gainers", "day_losers"],
        other => anyhow::bail!(
            "unsupported --universe '{other}' (supported: day_movers, day_gainers, day_losers, most_actives, small_cap_gainers, aggressive_small_caps, top_etfs, most_shorted, tickers)"
        ),
    };

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()
        .context("build yahoo screener client")?;
    // Yahoo's screener omits sector/industry/totalAssets unless we ask for them
    // explicitly via the `fields` query parameter.
    const SCREENER_FIELDS: &str = "symbol,shortName,longName,displayName,quoteType,exchange,fullExchangeName,marketCap,totalAssets,regularMarketPrice,regularMarketPreviousClose,regularMarketChange,regularMarketChangePercent,regularMarketVolume,quoteSourceName,marketState,sector,industry";
    let mut out = Vec::new();
    for scr_id in scr_ids {
        let resp = client
            .get(YAHOO_SCREENER_SAVED_URL)
            .query(&[
                ("scrIds", scr_id),
                ("count", scan_limit.min(250).to_string().as_str()),
                ("fields", SCREENER_FIELDS),
            ])
            .send()
            .await
            .with_context(|| format!("fetch yahoo screener {scr_id}"))?;
        if !resp.status().is_success() {
            warnings.push(format!("Yahoo screener {scr_id} returned HTTP {}", resp.status()));
            continue;
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("parse yahoo screener {scr_id}"))?;
        let quotes = body
            .get("finance")
            .and_then(|v| v.get("result"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.get("quotes"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for quote in quotes {
            if let Some(candidate) = movers_candidate_from_yahoo_quote(&quote, scr_id) {
                out.push(candidate);
            }
        }
    }
    movers_dedupe(out)
}

async fn movers_fetch_snapshot_candidates(
    tickers: &[String],
    provider: eli_core::finance::ProviderKind,
    ibkr: &Option<eli_core::finance::IbkrConnectionConfig>,
    warnings: &mut Vec<String>,
) -> Result<Vec<MoversCandidate>> {
    if tickers.is_empty() {
        return Ok(Vec::new());
    }
    let req = eli_core::finance::SnapshotRequest {
        tickers: tickers.to_vec(),
        as_of: None,
        provider: provider.clone(),
        ibkr: ibkr.clone(),
    };
    let resp = eli_core::finance::fetch_snapshot(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(errors) = resp.errors.as_ref() {
        for err in errors.iter().take(5) {
            warnings.push(format!("{} snapshot error for {}: {}", movers_provider_name(&provider), err.ticker, err.message));
        }
        if errors.len() > 5 {
            warnings.push(format!("{} more {} snapshot errors", errors.len() - 5, movers_provider_name(&provider)));
        }
    }
    Ok(resp
        .snapshots
        .iter()
        .map(|snap| movers_candidate_from_snapshot(snap, movers_provider_name(&provider)))
        .collect())
}

/// Enrich ETF candidates that have a null `market_cap` with Yahoo's `totalAssets`
/// (AUM) via `quoteSummary?modules=fundProfile,defaultKeyStatistics,summaryDetail`.
/// ETFs don't have a market cap in the traditional sense — Yahoo's screener leaves
/// it null, which causes any min/max-market-cap filter to drop every ETF. AUM is
/// a sane proxy for fund size, so we surface it in the existing `market_cap` field
/// and keep the output schema stable.
async fn movers_enrich_etf_aum(candidates: &mut [MoversCandidate], warnings: &mut Vec<String>) {
    let tickers: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.market_cap.is_none())
        .filter(|candidate| {
            candidate
                .quote_type
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("ETF"))
                || candidate.source.to_ascii_lowercase().contains("top_etfs")
        })
        .map(|candidate| candidate.ticker.clone())
        .collect();
    if tickers.is_empty() {
        return;
    }

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warnings.push(format!("ETF AUM enrichment unavailable: {err}"));
            return;
        }
    };

    let mut aum_by_ticker = std::collections::BTreeMap::new();
    let mut error_count: usize = 0;
    let mut first_error: Option<String> = None;
    let mut auth_blocked = false;
    for ticker in movers_dedupe_tickers(tickers).into_iter().take(50) {
        if auth_blocked {
            break;
        }
        let url = format!("{YAHOO_QUOTE_SUMMARY_URL}/{ticker}");
        let resp = match client
            .get(&url)
            .query(&[(
                "modules",
                "fundProfile,defaultKeyStatistics,summaryDetail,price",
            )])
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                error_count += 1;
                if first_error.is_none() {
                    first_error = Some(format!("fetch failed for {ticker}: {err}"));
                }
                continue;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            error_count += 1;
            if first_error.is_none() {
                first_error = Some(format!("HTTP {status} for {ticker}"));
            }
            // Yahoo's quoteSummary endpoint requires a `crumb` cookie on many
            // accounts; without it the server returns 401 for every ticker.
            // Bail out after the first 401 so we don't spam dozens of warnings.
            if status == reqwest::StatusCode::UNAUTHORIZED {
                auth_blocked = true;
            }
            continue;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(body) => body,
            Err(err) => {
                error_count += 1;
                if first_error.is_none() {
                    first_error = Some(format!("parse failed for {ticker}: {err}"));
                }
                continue;
            }
        };
        if let Some(aum) = movers_extract_yahoo_total_assets(&body) {
            aum_by_ticker.insert(ticker.to_ascii_uppercase(), aum);
        }
    }

    if error_count > 0 {
        let detail = first_error.unwrap_or_else(|| "unknown".to_string());
        if auth_blocked {
            warnings.push(format!(
                "ETF AUM enrichment blocked by Yahoo (401 Unauthorized — quoteSummary endpoint now requires a crumb cookie). {error_count} lookup(s) skipped. First: {detail}"
            ));
        } else {
            warnings.push(format!(
                "ETF AUM enrichment had {error_count} error(s). First: {detail}"
            ));
        }
    }

    for candidate in candidates.iter_mut() {
        if candidate.market_cap.is_none() {
            candidate.market_cap = aum_by_ticker
                .get(&candidate.ticker.to_ascii_uppercase())
                .copied();
        }
    }
}

/// Recompute change_pct / price / previous_close for explicit-ticker candidates
/// when the eli-core snapshot returned price == previous_close (a known
/// upstream issue: regular-session-closed ETFs sometimes carry the same value
/// in both fields, masking real intraday moves). Pulls regularMarketPrice and
/// chartPreviousClose from the open `/v8/finance/chart` endpoint, which doesn't
/// require a Yahoo crumb cookie. Only mutates a candidate when both numbers
/// come back finite and disagree.
async fn movers_enrich_change_pct_from_chart(
    candidates: &mut [MoversCandidate],
    warnings: &mut Vec<String>,
) {
    // Find candidates worth enriching: zero/missing change_pct OR price ==
    // previous_close. Skip ones where the snapshot already produced a non-zero
    // delta (those are trustworthy).
    let needs: Vec<String> = candidates
        .iter()
        .filter(|c| {
            let zero_or_none = c
                .change_pct
                .map(|v| !v.is_finite() || v.abs() < f64::EPSILON)
                .unwrap_or(true);
            let equal_pp = matches!((c.price, c.previous_close), (Some(p), Some(pc)) if (p - pc).abs() < f64::EPSILON);
            zero_or_none || equal_pp
        })
        .map(|c| c.ticker.clone())
        .collect();
    if needs.is_empty() {
        return;
    }

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warnings.push(format!("change_pct enrichment unavailable: {err}"));
            return;
        }
    };

    let mut updates: std::collections::BTreeMap<String, (f64, f64)> =
        std::collections::BTreeMap::new();
    let mut error_count: usize = 0;
    for ticker in movers_dedupe_tickers(needs).into_iter().take(50) {
        let url = format!(
            "https://query1.finance.yahoo.com/v8/finance/chart/{ticker}"
        );
        let resp = match client
            .get(&url)
            .query(&[("interval", "1d"), ("range", "2d")])
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => {
                error_count += 1;
                continue;
            }
        };
        if !resp.status().is_success() {
            error_count += 1;
            continue;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => {
                error_count += 1;
                continue;
            }
        };
        let meta = body
            .get("chart")
            .and_then(|v| v.get("result"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.get("meta"));
        let Some(meta) = meta else {
            continue;
        };
        let regular = meta.get("regularMarketPrice").and_then(|v| v.as_f64());
        let prev = meta
            .get("chartPreviousClose")
            .and_then(|v| v.as_f64())
            .or_else(|| meta.get("previousClose").and_then(|v| v.as_f64()));
        if let (Some(px), Some(prv)) = (regular, prev) {
            if px.is_finite() && prv.is_finite() && prv != 0.0 {
                updates.insert(ticker.to_ascii_uppercase(), (px, prv));
            }
        }
    }

    if error_count > 0 {
        warnings.push(format!(
            "change_pct enrichment via /v8/finance/chart had {error_count} error(s)"
        ));
    }

    for c in candidates.iter_mut() {
        let key = c.ticker.to_ascii_uppercase();
        if let Some((px, prv)) = updates.get(&key) {
            // Only overwrite when the chart-meta price actually disagrees with
            // the snapshot — preserves "legit flat day" zeros.
            if (*px - *prv).abs() < f64::EPSILON {
                continue;
            }
            c.price = Some(*px);
            c.previous_close = Some(*prv);
            c.change_abs = Some(*px - *prv);
            c.change_pct = Some((*px / *prv - 1.0) * 100.0);
            c.quote_source = c
                .quote_source
                .clone()
                .or_else(|| Some("yahoo chart-meta fallback".to_string()));
        }
    }
}

fn movers_dedupe_tickers(tickers: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for ticker in tickers {
        let normalized = ticker.trim().to_ascii_uppercase();
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

fn movers_extract_yahoo_total_assets(body: &serde_json::Value) -> Option<u64> {
    let result = body
        .get("quoteSummary")?
        .get("result")?
        .as_array()?
        .first()?;
    for path in [
        ["summaryDetail", "totalAssets"],
        ["defaultKeyStatistics", "totalAssets"],
        ["fundProfile", "totalAssets"],
        ["price", "totalAssets"],
    ] {
        if let Some(parsed) = result
            .get(path[0])
            .and_then(|section| section.get(path[1]))
            .and_then(movers_json_u64)
        {
            return Some(parsed);
        }
    }
    None
}

fn movers_json_u64(value: &serde_json::Value) -> Option<u64> {
    let value = value.get("raw").unwrap_or(value);
    value
        .as_u64()
        .or_else(|| value.as_f64().and_then(movers_f64_to_u64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
}

fn movers_f64_to_u64(value: f64) -> Option<u64> {
    if value.is_finite() && value >= 0.0 && value < u64::MAX as f64 {
        Some(value.round() as u64)
    } else {
        None
    }
}

fn movers_candidate_from_yahoo_quote(v: &serde_json::Value, source: &str) -> Option<MoversCandidate> {
    let ticker = v.get("symbol").and_then(|v| v.as_str())?.trim().to_string();
    if ticker.is_empty() {
        return None;
    }
    let quote_type = v
        .get("quoteType")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // ETFs sometimes carry totalAssets (AUM) in the screener payload. When
    // marketCap is null but totalAssets is populated, treat AUM as a usable
    // size proxy so size-bucket filters and market-cap sort still work.
    let market_cap = v
        .get("marketCap")
        .and_then(|v| v.as_u64())
        .or_else(|| v.get("totalAssets").and_then(|v| v.as_u64()))
        .or_else(|| {
            // Yahoo occasionally encodes totalAssets as a float — round to u64.
            v.get("totalAssets").and_then(|v| v.as_f64()).and_then(|f| {
                if f.is_finite() && f >= 0.0 && f < u64::MAX as f64 {
                    Some(f.round() as u64)
                } else {
                    None
                }
            })
        });
    Some(MoversCandidate {
        ticker,
        name: v
            .get("shortName")
            .or_else(|| v.get("displayName"))
            .or_else(|| v.get("longName"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        exchange: v
            .get("exchange")
            .or_else(|| v.get("fullExchangeName"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        price: v.get("regularMarketPrice").and_then(|v| v.as_f64()),
        previous_close: v.get("regularMarketPreviousClose").and_then(|v| v.as_f64()),
        change_pct: v.get("regularMarketChangePercent").and_then(|v| v.as_f64()),
        change_abs: v.get("regularMarketChange").and_then(|v| v.as_f64()),
        market_cap,
        volume: v.get("regularMarketVolume").and_then(|v| v.as_u64()),
        source: source.to_string(),
        quote_source: v.get("quoteSourceName").and_then(|v| v.as_str()).map(str::to_string),
        market_state: v.get("marketState").and_then(|v| v.as_str()).map(str::to_string),
        sector: v.get("sector").and_then(|v| v.as_str()).map(str::to_string),
        industry: v.get("industry").and_then(|v| v.as_str()).map(str::to_string),
        quote_type,
    })
}

fn movers_candidate_from_snapshot(
    snap: &eli_core::finance::TickerSnapshot,
    provider: &str,
) -> MoversCandidate {
    let price = snap.current_price.or(snap.price);
    let previous_close = snap.previous_close;
    // Always derive change_pct from price / previous_close when both are
    // present and previous_close is non-zero. The eli-core SnapshotResponse
    // doesn't always populate daily_return for ETFs (regularMarketChangePercent
    // is sometimes 0 or null even when there's a real intraday move), so the
    // computed delta is the reliable source. Fall back to daily_return only
    // when the math can't be done.
    let change_pct = match (price, previous_close) {
        (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 && px.is_finite() => {
            Some((px / prev - 1.0) * 100.0)
        }
        _ => snap.daily_return.map(|ret| ret * 100.0),
    };
    let change_abs = match (price, previous_close) {
        (Some(px), Some(prev)) if px.is_finite() && prev.is_finite() => Some(px - prev),
        _ => None,
    };
    MoversCandidate {
        ticker: snap.ticker.clone(),
        name: snap
            .short_name
            .clone()
            .or_else(|| snap.long_name.clone()),
        exchange: snap.exchange.clone(),
        price,
        previous_close,
        change_pct,
        change_abs,
        market_cap: snap.market_cap,
        volume: None,
        source: provider.to_string(),
        quote_source: None,
        market_state: Some(snap.session_state.clone()),
        sector: None,
        industry: None,
        quote_type: None,
    }
}

fn movers_merge_price_source(
    price_source: Vec<MoversCandidate>,
    yahoo_candidates: Vec<MoversCandidate>,
    yahoo_meta: Vec<MoversCandidate>,
) -> Vec<MoversCandidate> {
    let mut yahoo_by_ticker = std::collections::BTreeMap::new();
    for candidate in yahoo_candidates.into_iter().chain(yahoo_meta.into_iter()) {
        yahoo_by_ticker.insert(candidate.ticker.to_ascii_uppercase(), candidate);
    }

    price_source
        .into_iter()
        .map(|mut primary| {
            if let Some(meta) = yahoo_by_ticker.get(&primary.ticker.to_ascii_uppercase()) {
                primary.name = primary.name.or_else(|| meta.name.clone());
                primary.exchange = primary.exchange.or_else(|| meta.exchange.clone());
                primary.market_cap = primary.market_cap.or(meta.market_cap);
                primary.volume = primary.volume.or(meta.volume);
                primary.quote_source = primary.quote_source.or_else(|| meta.quote_source.clone());
                primary.sector = primary.sector.or_else(|| meta.sector.clone());
                primary.industry = primary.industry.or_else(|| meta.industry.clone());
                primary.quote_type = primary.quote_type.or_else(|| meta.quote_type.clone());
            }
            primary
        })
        .collect()
}

fn movers_passes_filters(
    candidate: &MoversCandidate,
    direction: MoversDirection,
    filters: &FinanceMoversFilters,
) -> bool {
    let Some(change_pct) = candidate.change_pct else {
        return false;
    };
    if !change_pct.is_finite() {
        return false;
    }
    match direction {
        MoversDirection::Gainers if change_pct <= 0.0 => return false,
        MoversDirection::Losers if change_pct >= 0.0 => return false,
        _ => {}
    }
    if change_pct.abs() < filters.min_change_pct {
        return false;
    }
    if candidate.price.map(|px| px < filters.min_price).unwrap_or(true) {
        return false;
    }
    if let Some(min_cap) = filters.min_market_cap {
        if candidate.market_cap.map(|cap| cap < min_cap).unwrap_or(true) {
            return false;
        }
    }
    if let Some(max_cap) = filters.max_market_cap {
        if candidate.market_cap.map(|cap| cap > max_cap).unwrap_or(true) {
            return false;
        }
    }
    if let Some(min_volume) = filters.min_volume {
        if candidate.volume.map(|vol| vol < min_volume).unwrap_or(true) {
            return false;
        }
    }
    if let Some(needle) = filters.sector.as_ref() {
        let needle_lc = needle.to_ascii_lowercase();
        match candidate.sector.as_ref() {
            Some(s) if s.to_ascii_lowercase().contains(&needle_lc) => {}
            _ => return false,
        }
    }
    if let Some(needle) = filters.industry.as_ref() {
        let needle_lc = needle.to_ascii_lowercase();
        match candidate.industry.as_ref() {
            Some(s) if s.to_ascii_lowercase().contains(&needle_lc) => {}
            _ => return false,
        }
    }
    true
}

fn movers_candidate_to_output(candidate: MoversCandidate) -> FinanceMover {
    let dollar_volume = match (candidate.price, candidate.volume) {
        (Some(px), Some(vol)) if px.is_finite() => Some(px * vol as f64),
        _ => None,
    };
    let estimated_value_change = match (candidate.market_cap, candidate.change_pct) {
        (Some(cap), Some(pct)) if pct.is_finite() => Some(cap as f64 * pct / 100.0),
        _ => None,
    };
    FinanceMover {
        ticker: candidate.ticker,
        name: candidate.name,
        exchange: candidate.exchange,
        price: candidate.price.map(movers_round_price),
        previous_close: candidate.previous_close.map(movers_round_price),
        change_pct: candidate.change_pct.map(|v| (v * 1000.0).round() / 1000.0),
        change_abs: candidate.change_abs.map(movers_round_price),
        market_cap: candidate.market_cap,
        volume: candidate.volume,
        dollar_volume,
        estimated_value_change,
        source: candidate.source,
        quote_source: candidate.quote_source,
        market_state: candidate.market_state,
        sector: candidate.sector,
        industry: candidate.industry,
        quote_type: candidate.quote_type,
        extended_price: None,
        extended_previous_close: None,
        extended_change_pct: None,
        extended_change_abs: None,
        extended_session: None,
        extended_timestamp_utc: None,
    }
}

fn movers_sort(movers: &mut [FinanceMover], sort_by: MoversSortBy, direction: MoversDirection) {
    movers.sort_by(|a, b| {
        let av = movers_sort_value(a, sort_by, direction);
        let bv = movers_sort_value(b, sort_by, direction);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Combined regular + extended-hours percent change. When extended-hours data
/// is absent or zero, this collapses to plain `change_pct` so non-extended runs
/// are unchanged.
fn combined_change_pct(mover: &FinanceMover) -> f64 {
    let regular = mover.change_pct.unwrap_or(0.0);
    let extended = mover.extended_change_pct.unwrap_or(0.0);
    regular + extended
}

fn movers_sort_value(mover: &FinanceMover, sort_by: MoversSortBy, direction: MoversDirection) -> f64 {
    match sort_by {
        MoversSortBy::Percent => {
            let combined = combined_change_pct(mover);
            match direction {
                MoversDirection::Losers => -combined,
                MoversDirection::Both => combined.abs(),
                MoversDirection::Gainers => combined,
            }
        }
        MoversSortBy::AbsPercent => combined_change_pct(mover).abs(),
        MoversSortBy::MarketCap => mover.market_cap.unwrap_or(0) as f64,
        MoversSortBy::ValueChange => {
            let combined = combined_change_pct(mover);
            if combined != 0.0 {
                if let Some(cap) = mover.market_cap {
                    return ((combined / 100.0) * cap as f64).abs();
                }
            }
            mover.estimated_value_change.unwrap_or(0.0).abs()
        }
        MoversSortBy::DollarVolume => mover.dollar_volume.unwrap_or(0.0),
        MoversSortBy::Volume => mover.volume.unwrap_or(0) as f64,
    }
}

fn movers_parse_market_cap(raw: &str) -> anyhow::Result<u64> {
    let cleaned = raw
        .trim()
        .trim_start_matches('$')
        .replace([',', '_'], "")
        .to_ascii_uppercase();
    if cleaned.is_empty() {
        anyhow::bail!("empty market cap");
    }
    let (num, mult) = if let Some(n) = cleaned.strip_suffix('T') {
        (n, 1e12)
    } else if let Some(n) = cleaned.strip_suffix('B') {
        (n, 1e9)
    } else if let Some(n) = cleaned.strip_suffix('M') {
        (n, 1e6)
    } else if let Some(n) = cleaned.strip_suffix('K') {
        (n, 1e3)
    } else {
        (cleaned.as_str(), 1.0)
    };
    let value = num
        .parse::<f64>()
        .with_context(|| format!("invalid market cap value '{raw}'"))?;
    if !value.is_finite() || value < 0.0 {
        anyhow::bail!("market cap must be a non-negative finite value");
    }
    Ok((value * mult).round() as u64)
}

fn movers_normalize_tickers(tickers: &[String]) -> Vec<String> {
    tickers
        .iter()
        .map(|ticker| ticker.trim().to_ascii_uppercase())
        .filter(|ticker| !ticker.is_empty())
        .collect()
}

fn movers_ibkr_config(args: &FinanceMoversArgs) -> Option<eli_core::finance::IbkrConnectionConfig> {
    Some(eli_core::finance::IbkrConnectionConfig {
        account: args.ibkr_account.clone(),
        host: args.ibkr_host.clone(),
        port: args.ibkr_port,
        client_id: args.ibkr_client_id,
        market_data_type: args.ibkr_market_data_type,
        timeout_secs: args.ibkr_timeout_secs,
    })
}

fn movers_has_ibkr_hint(args: &FinanceMoversArgs) -> bool {
    args.ibkr_account.is_some()
        || args.ibkr_host.is_some()
        || args.ibkr_port.is_some()
        || args.ibkr_client_id.is_some()
        || args.ibkr_market_data_type.is_some()
        || args.ibkr_timeout_secs.is_some()
        || [
            "IBKR_ACCOUNT",
            "IBKR_HOST",
            "IBKR_PORT",
            "IBKR_CLIENT_ID",
            "IBKR_MARKET_DATA_TYPE",
            "IBKR_TIMEOUT_SECS",
        ]
        .iter()
        .any(|key| {
            std::env::var(key)
                .ok()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
        || movers_inventory_has_ibkr_hint()
}

fn movers_inventory_has_ibkr_hint() -> bool {
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return false,
    };
    let path = std::env::var("ELI_INV_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(home)
                .join(".config")
                .join("eli")
                .join("inv.toml")
        });
    std::fs::read_to_string(path)
        .map(|raw| raw.contains("[ibkr") || raw.contains("IBKR_"))
        .unwrap_or(false)
}

fn movers_dedupe(candidates: Vec<MoversCandidate>) -> Result<Vec<MoversCandidate>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for candidate in candidates {
        let key = candidate.ticker.to_ascii_uppercase();
        if seen.insert(key) {
            out.push(candidate);
        }
    }
    Ok(out)
}

fn movers_provider_name(provider: &eli_core::finance::ProviderKind) -> &'static str {
    match provider {
        eli_core::finance::ProviderKind::Ibkr => "ibkr",
        eli_core::finance::ProviderKind::Yahoo => "yahoo",
        _ => "provider",
    }
}

fn movers_round_price(value: f64) -> f64 {
    if value.abs() >= 100.0 {
        (value * 100.0).round() / 100.0
    } else {
        (value * 10_000.0).round() / 10_000.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoversDirection {
    Gainers,
    Losers,
    Both,
}

fn movers_parse_direction(raw: &str) -> anyhow::Result<MoversDirection> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "gainer" | "gainers" | "up" => Ok(MoversDirection::Gainers),
        "loser" | "losers" | "down" => Ok(MoversDirection::Losers),
        "both" | "all" => Ok(MoversDirection::Both),
        other => anyhow::bail!("unsupported --direction '{other}' (supported: gainers, losers, both)"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoversSortBy {
    Percent,
    AbsPercent,
    MarketCap,
    ValueChange,
    DollarVolume,
    Volume,
}

fn movers_parse_sort_by(raw: &str) -> anyhow::Result<MoversSortBy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "percent" | "pct" | "change_pct" => Ok(MoversSortBy::Percent),
        "abs_percent" | "abs-percent" | "absolute_percent" | "absolute-percent" | "abs_pct" | "abs-pct" => Ok(MoversSortBy::AbsPercent),
        "market_cap" | "market-cap" | "marketcap" | "cap" => Ok(MoversSortBy::MarketCap),
        "value_change" | "value-change" | "market_cap_change" | "market-cap-change" | "mcap_change" | "mcap-change" => Ok(MoversSortBy::ValueChange),
        "dollar_volume" | "dollar-volume" | "dollar_vol" | "dollar-vol" | "liquidity" => Ok(MoversSortBy::DollarVolume),
        "volume" | "vol" => Ok(MoversSortBy::Volume),
        other => anyhow::bail!("unsupported --sort-by '{other}' (supported: percent, abs_percent, market_cap, value_change, dollar_volume, volume)"),
    }
}
