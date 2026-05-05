/// Direct Kalshi + Polymarket API fetch for per-meeting Fed rate probabilities.
/// Used as fallback when the CSV cache from `eli finance sync` is missing or stale.

/// Fetches KXFEDDECISION markets directly from the Kalshi API.
/// Returns (meetings, cumulative_signals, warnings).
async fn fetch_rate_path_live(
    current_rate: f64,
) -> Result<(
    BTreeMap<chrono::NaiveDate, (MeetingMeta, MeetingAgg)>,
    Vec<CumulativeFedSignal>,
    Vec<String>,
)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut meetings: BTreeMap<chrono::NaiveDate, (MeetingMeta, MeetingAgg)> = BTreeMap::new();
    let mut cumulative_signals: Vec<CumulativeFedSignal> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ── Kalshi: fetch KXFEDDECISION markets ──
    let kalshi_res = fetch_kalshi_fed_markets(client).await;
    match kalshi_res {
        Ok(markets) => {
            for m in &markets {
                let source_text = format!(
                    "{} {} {}",
                    m.ticker, m.event_ticker, m.title
                );
                let ev_upper = m.event_ticker.to_ascii_uppercase();
                if ev_upper.starts_with("KXFED") && !ev_upper.starts_with("KXFEDDECISION") {
                    continue;
                }

                let meeting = parse_meeting_from_token(&m.event_ticker)
                    .or_else(|| parse_meeting_from_token(&m.ticker))
                    .or_else(|| parse_meeting_from_title(&m.title));
                let Some(meeting) = meeting else { continue };

                let bucket = classify_bucket(&source_text, current_rate);
                let Some(bucket) = bucket else { continue };

                if m.volume < MIN_MARKET_VOLUME {
                    continue;
                }
                let entry = meetings
                    .entry(meeting.date)
                    .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));
                entry.1.add(bucket, m.probability, m.volume);
            }

            // Extract cumulative signals from Kalshi.
            let cumulative_re = regex::Regex::new(
                r"(?i)Federal Reserve (hike|cut)\s+rates?\s+by\s+((?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2},?\s+\d{4})"
            ).ok();
            if let Some(ref re) = cumulative_re {
                for m in &markets {
                    if let Some(caps) = re.captures(&m.title) {
                        let direction = caps.get(1).map(|c| c.as_str().to_lowercase()).unwrap_or_default();
                        let date_str = caps.get(2).map(|c| c.as_str()).unwrap_or_default();
                        if m.probability > 0.01 {
                            cumulative_signals.push(CumulativeFedSignal {
                                direction,
                                by_date: date_str.to_string(),
                                probability: m.probability,
                                title: m.title.clone(),
                            });
                        }
                    }
                }
            }
        }
        Err(e) => warnings.push(format!("kalshi live fetch failed: {e}")),
    }

    // ── Polymarket: search for Fed rate decision markets ──
    let poly_res = fetch_polymarket_fed_markets(client).await;
    match poly_res {
        Ok(markets) => {
            let annual_cuts_re = regex::Regex::new(
                r"(?i)\bwill\s+(no|\d+)\s+fed rate cuts?\s+happen in\s+(20\d{2})\b"
            ).ok();
            for m in &markets {
                // Per-meeting parsing
                if let Some(meeting) = parse_meeting_from_title(&m.title) {
                    if let Some(bucket) = classify_bucket(&m.title, current_rate) {
                        if m.volume >= MIN_MARKET_VOLUME {
                            let entry = meetings
                                .entry(meeting.date)
                                .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));
                            entry.1.add(bucket, m.probability, m.volume);
                        }
                    }
                }
                // Annual cuts parsing
                if let Some(ref re) = annual_cuts_re {
                    if let Some(_caps) = re.captures(&m.title) {
                        // Annual cuts are only used for fallback; per-meeting data is preferred.
                    }
                }
            }
        }
        Err(e) => warnings.push(format!("polymarket live fetch failed: {e}")),
    }

    cumulative_signals.sort_by(|a, b| a.by_date.cmp(&b.by_date));
    cumulative_signals.dedup_by(|a, b| a.title == b.title);

    Ok((meetings, cumulative_signals, warnings))
}

#[derive(Debug)]
struct LiveMarket {
    ticker: String,
    event_ticker: String,
    title: String,
    probability: f64,
    volume: i64,
}

async fn fetch_kalshi_fed_markets(client: &reqwest::Client) -> Result<Vec<LiveMarket>> {
    let base = crate::finance::KALSHI_BASE_URL;
    let mut all_markets = Vec::new();

    // Fetch markets with series_ticker=KXFEDDECISION
    let mut cursor: Option<String> = None;
    for _page in 0..5 {
        let mut url = reqwest::Url::parse(&format!("{}/markets", base))
            .map_err(|e| Error::Provider(format!("kalshi url parse: {e}")))?;
        url.query_pairs_mut()
            .append_pair("series_ticker", "KXFEDDECISION")
            .append_pair("status", "open")
            .append_pair("limit", "200");
        if let Some(ref c) = cursor {
            url.query_pairs_mut().append_pair("cursor", c);
        }

        let resp = client
            .get(url.as_str())
            .send()
            .await
            .map_err(|e| Error::Provider(format!("kalshi fed markets fetch: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "kalshi fed markets returned {status}: {body}"
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("kalshi fed markets parse: {e}")))?;

        let markets = body
            .get("markets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if markets.is_empty() {
            break;
        }

        for m in &markets {
            let ticker = m.get("ticker").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let event_ticker = m
                .get("event_ticker")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = m.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // Kalshi v2 returns dollar-denominated string fields:
            //   yes_bid_dollars: "0.06", last_price_dollars: "0.06"
            // Probability = mid-price or last_price (already in 0.0–1.0 range).
            let yes_bid: f64 = m
                .get("yes_bid_dollars")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let yes_ask: f64 = m
                .get("yes_ask_dollars")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let last_price: f64 = m
                .get("last_price_dollars")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            // Use mid-price if both sides available, otherwise last_price.
            let probability = if yes_bid > 0.0 && yes_ask > 0.0 {
                ((yes_bid + yes_ask) / 2.0).clamp(0.0, 1.0)
            } else if last_price > 0.0 {
                last_price.clamp(0.0, 1.0)
            } else {
                yes_bid.max(yes_ask).clamp(0.0, 1.0)
            };

            let volume = m
                .get("volume_fp")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0) as i64;

            all_markets.push(LiveMarket {
                ticker,
                event_ticker,
                title,
                probability,
                volume,
            });
        }

        cursor = body
            .get("cursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty());
        if cursor.is_none() {
            break;
        }
    }

    Ok(all_markets)
}

async fn fetch_polymarket_fed_markets(client: &reqwest::Client) -> Result<Vec<LiveMarket>> {
    // Polymarket CLOB API: search for Fed rate decision markets.
    let url = "https://clob.polymarket.com/markets?tag=fed-funds-rate&active=true&limit=100";
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("polymarket fed markets fetch: {e}")))?;

    if !resp.status().is_success() {
        // Polymarket search may not support tag filter — try gamma API.
        return fetch_polymarket_fed_markets_gamma(client).await;
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("polymarket fed parse: {e}")))?;

    let mut markets = Vec::new();
    let items = body.as_array().cloned().unwrap_or_default();

    for item in &items {
        let question = item
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title_lower = question.to_ascii_lowercase();
        if !title_lower.contains("fed")
            && !title_lower.contains("fomc")
            && !title_lower.contains("interest rate")
        {
            continue;
        }

        let probability = item
            .get("outcomePrices")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .and_then(|prices| prices.first().and_then(|p| p.parse::<f64>().ok()))
            .unwrap_or(0.0);

        let volume = item
            .get("volume")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as i64;

        let cond_id = item
            .get("condition_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        markets.push(LiveMarket {
            ticker: cond_id,
            event_ticker: String::new(),
            title: question.to_string(),
            probability,
            volume,
        });
    }

    Ok(markets)
}

async fn fetch_polymarket_fed_markets_gamma(client: &reqwest::Client) -> Result<Vec<LiveMarket>> {
    // Gamma API fallback: search for Fed rate markets.
    let url = "https://gamma-api.polymarket.com/markets?tag=fed-funds-rate&active=true&limit=100&order=volume&ascending=false";
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("polymarket gamma fed fetch: {e}")))?;

    if !resp.status().is_success() {
        return Ok(Vec::new()); // Silently return empty — Kalshi is the primary source.
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("polymarket gamma fed parse: {e}")))?;

    let items = body.as_array().cloned().unwrap_or_default();
    let mut markets = Vec::new();

    for item in &items {
        let question = item
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title_lower = question.to_ascii_lowercase();
        if !title_lower.contains("fed")
            && !title_lower.contains("fomc")
            && !title_lower.contains("interest rate")
        {
            continue;
        }

        let probability = item
            .get("outcomePrices")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .and_then(|prices| prices.first().and_then(|p| p.parse::<f64>().ok()))
            .unwrap_or(0.0);

        let volume = item
            .get("volume")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as i64;

        markets.push(LiveMarket {
            ticker: String::new(),
            event_ticker: String::new(),
            title: question.to_string(),
            probability,
            volume,
        });
    }

    Ok(markets)
}
