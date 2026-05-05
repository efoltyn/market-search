/// Direct Kalshi + Polymarket API fetch for per-meeting Fed rate probabilities,
/// year-aggregate views ("How many cuts in 2026?"), and joint multi-meeting
/// compound paths. Used as the primary source — the CSV cache is no longer the
/// hot path because Polymarket gamma's tag filter for `fed-funds-rate` is
/// effectively broken (returns the entire active catalog ranked by volume).

#[derive(Default)]
struct LiveExtras {
    year_view: Option<YearView>,
    compound_paths: Vec<CompoundPath>,
}

/// Returns (meetings, cumulative_signals, warnings, extras).
async fn fetch_rate_path_live(
    current_rate: f64,
) -> Result<(
    BTreeMap<chrono::NaiveDate, (MeetingMeta, MeetingAgg)>,
    Vec<CumulativeFedSignal>,
    Vec<String>,
    LiveExtras,
)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut meetings: BTreeMap<chrono::NaiveDate, (MeetingMeta, MeetingAgg)> = BTreeMap::new();
    let mut cumulative_signals: Vec<CumulativeFedSignal> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut extras = LiveExtras::default();
    let now_year = chrono::Utc::now().date_naive().year();

    // ── Kalshi: per-meeting Hold/Cut/Hike binaries ──
    match fetch_kalshi_fed_markets(client).await {
        Ok(markets) => {
            for m in &markets {
                let source_text = format!("{} {} {}", m.ticker, m.event_ticker, m.title);
                let ev_upper = m.event_ticker.to_ascii_uppercase();
                if ev_upper.starts_with("KXFED") && !ev_upper.starts_with("KXFEDDECISION") {
                    continue;
                }
                let meeting = parse_meeting_from_token(&m.event_ticker)
                    .or_else(|| parse_meeting_from_token(&m.ticker))
                    .or_else(|| parse_meeting_from_title(&m.title));
                let Some(meeting) = meeting else { continue };
                let Some(bucket) = classify_bucket(&source_text, current_rate) else { continue };
                // KXFEDDECISION binaries are split into 5 contracts per meeting
                // (H0/H25/H26/C25/C26). Each individual outcome can be naturally
                // small (~$1-7K) even when the event total is healthy ($20K+).
                // Use a lower per-binary threshold for Kalshi Fed binaries so
                // Sep/Oct/Dec meetings are not silently dropped, while still
                // filtering thin junk one-off markets.
                if m.volume < KALSHI_FED_MIN_MARKET_VOLUME {
                    continue;
                }
                let entry = meetings
                    .entry(meeting.date)
                    .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));
                entry.1.add(bucket, m.probability, m.volume);
            }

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

    // ── Polymarket: per-meeting via known slug list. The gamma `tag=` filter
    //    silently drops to "all active markets ranked by volume" — useless
    //    for our needs. Direct slug fetch is reliable. ──
    let per_meeting_slugs: &[(&str, i32)] = &[
        // Currently open 2026 per-meeting events. The "-825" / "-181" suffixes
        // are Polymarket internal slug discriminators; the human title is
        // "Fed Decision in <Month>?".
        ("fed-decision-in-june-825", 2026),
        ("fed-decision-in-july-181", 2026),
        ("fed-decision-in-september", 2026), // search index returns 2026 binaries here
        ("fed-decision-in-october", 2026),
        ("fed-decision-in-december", 2026),
    ];
    for (slug, default_year) in per_meeting_slugs {
        match fetch_polymarket_event_markets(client, slug).await {
            Ok(event_markets) => {
                for em in event_markets {
                    if is_compound_meeting_market(&em.title) {
                        continue;
                    }
                    if em.volume < MIN_MARKET_VOLUME {
                        continue;
                    }
                    let meeting = parse_meeting_from_title(&em.title)
                        .or_else(|| parse_meeting_from_short_title(&em.title, *default_year));
                    let Some(meeting) = meeting else { continue };
                    let Some(bucket) = classify_bucket(&em.title, current_rate) else { continue };
                    let entry = meetings
                        .entry(meeting.date)
                        .or_insert_with(|| (meeting.clone(), MeetingAgg::default()));
                    entry.1.add(bucket, em.probability, em.volume);
                }
            }
            Err(e) => warnings.push(format!("polymarket {slug} fetch failed: {e}")),
        }
    }

    // ── Polymarket: year-aggregate markets ──
    let mut year_cuts: Option<std::collections::BTreeMap<i32, f64>> = None;
    let mut year_eoy: Option<std::collections::BTreeMap<String, f64>> = None;
    let mut year_cut_by: Option<std::collections::BTreeMap<String, f64>> = None;
    let mut year_vol_total: i64 = 0;
    let mut year_n_markets: usize = 0;

    if let Ok(ms) = fetch_polymarket_event_markets(client, "how-many-fed-rate-cuts-in-2026").await {
        let re_n = regex::Regex::new(r"(?i)Will (no|\d+) Fed rate cuts? happen in 20\d{2}").unwrap();
        let mut dist: std::collections::BTreeMap<i32, f64> = Default::default();
        for em in ms {
            if let Some(caps) = re_n.captures(&em.title) {
                let raw = caps.get(1).unwrap().as_str();
                let n: i32 = if raw.eq_ignore_ascii_case("no") { 0 } else { raw.parse().unwrap_or(0) };
                dist.insert(n, em.probability);
                year_vol_total += em.volume;
                year_n_markets += 1;
            }
        }
        if !dist.is_empty() { year_cuts = Some(dist); }
    }
    if let Ok(ms) = fetch_polymarket_event_markets(client, "what-will-the-fed-rate-be-at-the-end-of-2026").await {
        let re_eoy = regex::Regex::new(r"(?i)(\d+(?:\.\d+)?\s*%)").unwrap();
        let mut dist: std::collections::BTreeMap<String, f64> = Default::default();
        for em in ms {
            if let Some(caps) = re_eoy.captures(&em.title) {
                let bucket = caps.get(1).unwrap().as_str().replace(' ', "");
                let prev = dist.get(&bucket).copied().unwrap_or(0.0);
                if em.probability > prev {
                    dist.insert(bucket, em.probability);
                }
                year_vol_total += em.volume;
                year_n_markets += 1;
            }
        }
        if !dist.is_empty() { year_eoy = Some(dist); }
    }
    if let Ok(ms) = fetch_polymarket_event_markets(client, "fed-rate-cut-by-629").await {
        let re_by = regex::Regex::new(
            r"(?i)by (january|february|march|april|may|june|july|august|september|october|november|december)\s+20\d{2}"
        ).unwrap();
        let mut dist: std::collections::BTreeMap<String, f64> = Default::default();
        for em in ms {
            if let Some(caps) = re_by.captures(&em.title) {
                let m = caps.get(1).unwrap().as_str().to_ascii_lowercase();
                let prev = dist.get(&m).copied().unwrap_or(0.0);
                if em.probability > prev {
                    dist.insert(m, em.probability);
                }
                year_vol_total += em.volume;
                year_n_markets += 1;
            }
        }
        if !dist.is_empty() { year_cut_by = Some(dist); }
    }

    if year_cuts.is_some() || year_eoy.is_some() || year_cut_by.is_some() {
        extras.year_view = Some(YearView {
            year: now_year,
            cuts_distribution: year_cuts,
            eoy_rate_distribution: year_eoy,
            cut_by_meeting_distribution: year_cut_by,
            volume_total: year_vol_total,
            n_markets: year_n_markets,
        });
    }

    // ── Polymarket: joint multi-meeting compound paths ──
    for slug in ["fed-decisions-mar-jun", "fed-decisions-apr-jul", "fed-decisions-jun-sep"] {
        if let Ok(ms) = fetch_polymarket_event_markets(client, slug).await {
            // The event title "Fed decisions (Mar-Jun)" only appears on the event
            // object, not the per-market titles. Derive label from slug.
            let label = match slug {
                "fed-decisions-mar-jun" => "Mar-Jun",
                "fed-decisions-apr-jul" => "Apr-Jul",
                "fed-decisions-jun-sep" => "Jun-Sep",
                _ => "?",
            };
            for em in ms {
                // Outcome = market title minus boilerplate prefix.
                let lower = em.title.to_ascii_lowercase();
                let outcome = if let Some(rest) = lower.strip_prefix("will the fed ") {
                    rest.split(" in the next three decisions")
                        .next()
                        .unwrap_or(rest)
                        .replace('–', "-")
                        .trim()
                        .to_string()
                } else {
                    em.title.clone()
                };
                if em.probability < 0.005 && em.volume < 1000 {
                    continue;
                }
                extras.compound_paths.push(CompoundPath {
                    label: label.to_string(),
                    outcome,
                    probability: em.probability,
                    volume: em.volume,
                    source: "polymarket".to_string(),
                });
            }
        }
    }

    sort_cumulative_signals_by_date(&mut cumulative_signals);
    cumulative_signals.dedup_by(|a, b| a.title == b.title);

    Ok((meetings, cumulative_signals, warnings, extras))
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

/// Direct slug → event lookup. Returns a flat list of binary markets inside
/// the event with title + probability + volume in USD (Polymarket reports
/// volume in tokens; multiply by ~1.0 since outcome tokens redeem at $1).
async fn fetch_polymarket_event_markets(
    client: &reqwest::Client,
    slug: &str,
) -> Result<Vec<LiveMarket>> {
    let url = format!("{}/events?slug={}", crate::finance::POLYMARKET_GAMMA_URL, slug);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("polymarket event {slug}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "polymarket event {slug} returned http {}",
            resp.status()
        )));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("polymarket event {slug} parse: {e}")))?;

    let event = body.as_array().and_then(|a| a.first()).cloned().unwrap_or(serde_json::Value::Null);
    let event_title = event
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let markets = event
        .get("markets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(markets.len());
    for m in &markets {
        let title = m
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // outcomePrices is a JSON-encoded string like "[\"0.955\",\"0.045\"]";
        // index 0 is the YES price.
        let probability = m
            .get("outcomePrices")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .and_then(|prices| prices.first().and_then(|p| p.parse::<f64>().ok()))
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        // volumeNum is float USD. `volume` is a string also in USD — prefer the
        // float to avoid parse errors on null.
        let volume = m
            .get("volumeNum")
            .and_then(|v| v.as_f64())
            .or_else(|| {
                m.get("volume")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
            })
            .unwrap_or(0.0) as i64;
        out.push(LiveMarket {
            ticker: m.get("conditionId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            event_ticker: event_title.clone(),
            title,
            probability,
            volume,
        });
    }
    Ok(out)
}
