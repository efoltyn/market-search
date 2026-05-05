#[derive(Clone, Deserialize)]
struct YahooOptionsResp {
    #[serde(rename = "optionChain")]
    option_chain: YahooOptionChain,
}

#[derive(Clone, Deserialize)]
struct YahooOptionChain {
    result: Vec<YahooChainResult>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YahooChainResult {
    underlying_symbol: Option<String>,
    expiration_dates: Option<Vec<i64>>,
    quote: Option<YahooQuote>,
    options: Option<Vec<YahooOptions>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YahooQuote {
    regular_market_price: Option<f64>,
}

#[derive(Clone, Deserialize)]
struct YahooOptions {
    #[serde(rename = "expirationDate")]
    expiration_date: i64,
    calls: Option<Vec<YahooContract>>,
    puts: Option<Vec<YahooContract>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YahooContract {
    contract_symbol: String,
    strike: f64,
    expiration: i64,
    bid: Option<f64>,
    ask: Option<f64>,
    last_price: Option<f64>,
    change: Option<f64>,
    percent_change: Option<f64>,
    volume: Option<i64>,
    open_interest: Option<i64>,
    implied_volatility: Option<f64>,
    in_the_money: Option<bool>,
}

async fn load_options_for_ts(
    client: &reqwest::Client,
    ticker: &str,
    crumb: &str,
    first_expiration_ts: Option<i64>,
    first_chain_options: Option<&YahooOptions>,
    ts: i64,
) -> Result<Option<YahooOptions>> {
    if first_expiration_ts == Some(ts) {
        if let Some(opts) = first_chain_options.cloned() {
            return Ok(Some(opts));
        }
    }

    let url = format!(
        "{}/{}?crumb={}&date={}",
        YAHOO_OPTIONS_URL, ticker, crumb, ts
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo options expiry fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo options expiry fetch failed: http {}",
            resp.status()
        )));
    }

    let body: YahooOptionsResp = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("yahoo options expiry parse failed: {e}")))?;

    Ok(body
        .option_chain
        .result
        .into_iter()
        .next()
        .and_then(|r| r.options)
        .and_then(|o| o.into_iter().next()))
}

pub async fn fetch_options(req: OptionsRequest) -> Result<OptionsResponse> {
    if matches!(req.provider, ProviderKind::Ibkr) {
        return crate::finance::fetch_ibkr_options(&req).await;
    }
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    // Build client with cookie store for Yahoo auth
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .cookie_provider(jar.clone())
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)")
        .tcp_nodelay(true)
        .build()
        .map_err(|e| Error::Provider(format!("http client init failed: {e}")))?;

    if let Some(quote_type) = yahoo_lookup_quote_type(&client, &ticker).await {
        let quote_type_norm = quote_type.trim().to_ascii_uppercase();
        if matches!(quote_type_norm.as_str(), "INDEX" | "MUTUALFUND") {
            let error = ToolErrorInfo {
                error: "AssetTypeMismatch".to_string(),
                message: format!(
                    "Ticker '{ticker}' is type '{quote_type_norm}'. This provider does not support options chains for this asset class."
                ),
                hint: Some(
                    "Use a tradable instrument that lists options for this asset class."
                        .to_string(),
                ),
                debug: None,
            };
            return Ok(OptionsResponse {
                ticker,
                underlying_price: 0.0,
                generated_at: Utc::now(),
                status: Some("error".to_string()),
                error: Some(error),
                expirations: vec![],
                requested_expiry: req.expiry.clone(),
                selected_expiry: None,
                target_dte_days: req.target_dte_days,
                selected_days_to_expiry: None,
                auto_selected_expiry: false,
                selection_reason: None,
                calls: vec![],
                puts: vec![],
                atm_iv: None,
                metrics: None,
                note: None,
                multi_expiry_summary: None,
            });
        }
    }

    // First hit fc.yahoo.com to initialize cookies (required for crumb auth)
    let _ = client.get("https://fc.yahoo.com").send().await;

    // Get crumb for Yahoo API auth
    let crumb_resp = client
        .get(YAHOO_CRUMB_URL)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo crumb fetch failed: {e}")))?;

    if !crumb_resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo crumb fetch failed: http {}",
            crumb_resp.status()
        )));
    }

    let crumb = crumb_resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("yahoo crumb read failed: {e}")))?;

    // Fetch options data with crumb
    let base_url = format!("{}/{}?crumb={}", YAHOO_OPTIONS_URL, ticker, crumb);
    let resp = client
        .get(&base_url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo options fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo options fetch failed: http {} for {}",
            resp.status(),
            ticker
        )));
    }

    let body: YahooOptionsResp = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("yahoo options parse failed: {e}")))?;

    let chain = body
        .option_chain
        .result
        .into_iter()
        .next()
        .ok_or_else(|| Error::Provider(format!("no options data for {}", ticker)))?;

    let underlying_price = chain
        .quote
        .and_then(|q| q.regular_market_price)
        .unwrap_or(0.0);

    // Convert expiration timestamps to dates
    let expiration_timestamps_raw = chain.expiration_dates.clone().unwrap_or_default();
    let expirations: Vec<String> = expiration_timestamps_raw
        .iter()
        .filter_map(|&ts| {
            Utc.timestamp_opt(ts, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string())
        })
        .collect();

    let note = if expirations.is_empty() {
        Some(
            "No listed options expirations returned for this symbol. Some symbols (e.g., futures/indices) may not have options here; try an equity/ETF proxy or use `--expirations` on a different ticker."
                .to_string(),
        )
    } else {
        None
    };

    // If only listing expirations, return early
    if req.list_expirations {
        return Ok(OptionsResponse {
            ticker,
            underlying_price,
            generated_at: Utc::now(),
            status: None,
            error: None,
            expirations,
            requested_expiry: req.expiry.clone(),
            selected_expiry: None,
            target_dte_days: req.target_dte_days,
            selected_days_to_expiry: None,
            auto_selected_expiry: false,
            selection_reason: None,
            calls: vec![],
            puts: vec![],
            atm_iv: None,
            metrics: None,
            note,
            multi_expiry_summary: None,
        });
    }

    // Multi-expiry mode: fetch summary for multiple expirations
    if req.multi_expiry {
        let num_expiries = req.num_expiries.unwrap_or(3);
        let expiration_timestamps: Vec<i64> = chain
            .expiration_dates
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .take(num_expiries)
            .copied()
            .collect();

        let mut snapshots: Vec<ExpirySnapshot> = Vec::new();
        let mut aggregate_volume: u64 = 0;
        let mut weighted_pc_sum: f64 = 0.0;

        // Fetch all expirations in parallel (batches of 8 to avoid Yahoo rate limits).
        // Yahoo throttles around ~10 concurrent; 8 is the safe ceiling.
        // join_all preserves order so the snapshot Vec stays in expiration order.
        const PARALLEL_FETCH_BATCH: usize = 8;
        let mut fetched_chains: Vec<(i64, Option<YahooOptions>)> =
            Vec::with_capacity(expiration_timestamps.len());
        for batch in expiration_timestamps.chunks(PARALLEL_FETCH_BATCH) {
            let futs = batch.iter().map(|&exp_ts| {
                let url = format!(
                    "{}/{}?crumb={}&date={}",
                    YAHOO_OPTIONS_URL, ticker, crumb, exp_ts
                );
                let client = client.clone();
                async move {
                    let opts = match client.get(&url).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.json::<YahooOptionsResp>().await {
                                Ok(body) => body
                                    .option_chain
                                    .result
                                    .into_iter()
                                    .next()
                                    .and_then(|r| r.options)
                                    .and_then(|o| o.into_iter().next()),
                                Err(_) => None,
                            }
                        }
                        _ => None,
                    };
                    (exp_ts, opts)
                }
            });
            let batch_results = futures::future::join_all(futs).await;
            fetched_chains.extend(batch_results);
        }

        for (exp_ts, opts) in fetched_chains {
            if let Some(opts) = opts {
                {
                    {
                        {
                            {
                                let calls = opts.calls.unwrap_or_default();
                                let puts = opts.puts.unwrap_or_default();

                                let call_vol: u64 = calls
                                    .iter()
                                    .filter_map(|c| c.volume)
                                    .map(|v| v as u64)
                                    .sum();
                                let put_vol: u64 =
                                    puts.iter().filter_map(|p| p.volume).map(|v| v as u64).sum();
                                let call_oi: u64 = calls
                                    .iter()
                                    .filter_map(|c| c.open_interest)
                                    .map(|v| v as u64)
                                    .sum();
                                let put_oi: u64 = puts
                                    .iter()
                                    .filter_map(|p| p.open_interest)
                                    .map(|v| v as u64)
                                    .sum();

                                let pc_vol = if call_vol > 0 {
                                    put_vol as f64 / call_vol as f64
                                } else {
                                    0.0
                                };
                                let pc_oi = if call_oi > 0 {
                                    put_oi as f64 / call_oi as f64
                                } else {
                                    0.0
                                };

                                let total_vol = call_vol + put_vol;

                                // Max pain: the strike minimizing total dollar value paid to
                                // options holders (= where options writers profit most).
                                // Filter out extreme strikes (pre-split artifacts from Yahoo).
                                // Require OI on both sides for meaningful max pain.
                                let mp_lo = underlying_price * 0.50;
                                let mp_hi = underlying_price * 2.0;
                                let mp_call_oi: u64 = calls
                                    .iter()
                                    .filter(|c| c.strike >= mp_lo && c.strike <= mp_hi)
                                    .map(|c| c.open_interest.unwrap_or(0) as u64)
                                    .sum();
                                let mp_put_oi: u64 = puts
                                    .iter()
                                    .filter(|p| p.strike >= mp_lo && p.strike <= mp_hi)
                                    .map(|p| p.open_interest.unwrap_or(0) as u64)
                                    .sum();
                                let max_pain = if mp_call_oi > 0 && mp_put_oi > 0 {
                                    let mp_strikes: std::collections::BTreeSet<i64> = calls
                                        .iter()
                                        .filter(|c| c.strike >= mp_lo && c.strike <= mp_hi)
                                        .map(|c| (c.strike * 100.0).round() as i64)
                                        .chain(
                                            puts.iter()
                                                .filter(|p| p.strike >= mp_lo && p.strike <= mp_hi)
                                                .map(|p| (p.strike * 100.0).round() as i64),
                                        )
                                        .collect();
                                    mp_strikes
                                        .iter()
                                        .min_by_key(|&&k| {
                                            let k_price = k as f64 / 100.0;
                                            let call_itm: f64 = calls
                                                .iter()
                                                .filter(|c| c.strike >= mp_lo && c.strike <= mp_hi && c.strike < k_price)
                                                .map(|c| {
                                                    (k_price - c.strike)
                                                        * c.open_interest.unwrap_or(0) as f64
                                                })
                                                .sum();
                                            let put_itm: f64 = puts
                                                .iter()
                                                .filter(|p| p.strike >= mp_lo && p.strike <= mp_hi && p.strike > k_price)
                                                .map(|p| {
                                                    (p.strike - k_price)
                                                        * p.open_interest.unwrap_or(0) as f64
                                                })
                                                .sum();
                                            ((call_itm + put_itm) * 100.0) as i64
                                        })
                                        .map(|&k| k as f64 / 100.0)
                                } else {
                                    None
                                };

                                // ATM IV
                                let atm_iv = calls
                                    .iter()
                                    .min_by(|a, b| {
                                        (a.strike - underlying_price)
                                            .abs()
                                            .partial_cmp(&(b.strike - underlying_price).abs())
                                            .unwrap_or(std::cmp::Ordering::Equal)
                                    })
                                    .and_then(|c| c.implied_volatility);

                                let expiry_date = Utc
                                    .timestamp_opt(exp_ts, 0)
                                    .single()
                                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                                    .unwrap_or_default();

                                let days_to_expiry = Utc
                                    .timestamp_opt(exp_ts, 0)
                                    .single()
                                    .map(|dt| (dt - Utc::now()).num_days())
                                    .unwrap_or(0);

                                aggregate_volume += total_vol;
                                weighted_pc_sum += pc_vol * total_vol as f64;

                                // Detect monthly OpEx (3rd Friday)
                                let is_monthly = Utc
                                    .timestamp_opt(exp_ts, 0)
                                    .single()
                                    .map(|dt| {
                                        let d = dt.date_naive();
                                        d.weekday() == chrono::Weekday::Fri
                                            && d.day() >= 15
                                            && d.day() <= 21
                                    });

                                snapshots.push(ExpirySnapshot {
                                    expiry: expiry_date,
                                    days_to_expiry,
                                    total_volume: total_vol,
                                    total_oi: call_oi + put_oi,
                                    call_oi,
                                    put_oi,
                                    put_call_ratio_volume: pc_vol,
                                    put_call_ratio_oi: pc_oi,
                                    max_pain,
                                    atm_iv,
                                    is_monthly,
                                });
                            }
                        }
                    }
                }
            }
            // (parallel fetch already complete; no per-iteration sleep needed)
        }

        let weighted_put_call_ratio = if aggregate_volume > 0 {
            weighted_pc_sum / aggregate_volume as f64
        } else {
            0.0
        };

        // Compute cross-expiry analytics
        let aggregate_oi: u64 = snapshots.iter().map(|s| s.total_oi).sum();

        // OI concentration: top 3 by OI
        let oi_concentration = if !snapshots.is_empty() && aggregate_oi > 0 {
            let mut sorted: Vec<&ExpirySnapshot> = snapshots.iter().collect();
            sorted.sort_by(|a, b| b.total_oi.cmp(&a.total_oi));
            Some(
                sorted
                    .iter()
                    .take(3)
                    .map(|s| OiConcentration {
                        expiry: s.expiry.clone(),
                        days_to_expiry: s.days_to_expiry,
                        oi: s.total_oi,
                        pct_of_total: (s.total_oi as f64 / aggregate_oi as f64) * 100.0,
                        is_monthly: s.is_monthly.unwrap_or(false),
                    })
                    .collect(),
            )
        } else {
            None
        };

        // IV term structure
        let iv_term_structure: Option<Vec<IvTermPoint>> = {
            let points: Vec<IvTermPoint> = snapshots
                .iter()
                .filter_map(|s| {
                    s.atm_iv.map(|iv| IvTermPoint {
                        expiry: s.expiry.clone(),
                        days_to_expiry: s.days_to_expiry,
                        atm_iv: iv,
                    })
                })
                .collect();
            if points.is_empty() {
                None
            } else {
                Some(points)
            }
        };

        // Max pain range
        let max_pain_range = {
            let with_pain: Vec<&ExpirySnapshot> =
                snapshots.iter().filter(|s| s.max_pain.is_some()).collect();
            if with_pain.len() >= 2 {
                let min = with_pain
                    .iter()
                    .min_by(|a, b| {
                        a.max_pain
                            .unwrap()
                            .partial_cmp(&b.max_pain.unwrap())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .unwrap();
                let max = with_pain
                    .iter()
                    .max_by(|a, b| {
                        a.max_pain
                            .unwrap()
                            .partial_cmp(&b.max_pain.unwrap())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .unwrap();
                let nearest_monthly = snapshots
                    .iter()
                    .filter(|s| s.is_monthly == Some(true) && s.max_pain.is_some())
                    .min_by_key(|s| s.days_to_expiry.abs());
                Some(MaxPainRange {
                    min_expiry: min.expiry.clone(),
                    min_pain: min.max_pain.unwrap(),
                    max_expiry: max.expiry.clone(),
                    max_pain: max.max_pain.unwrap(),
                    nearest_monthly_pain: nearest_monthly.and_then(|s| s.max_pain),
                    nearest_monthly_expiry: nearest_monthly.map(|s| s.expiry.clone()),
                })
            } else {
                None
            }
        };

        // Top-level atm_iv for --all mode: pick the nearest non-expired snapshot that has IV.
        let top_atm_iv = snapshots
            .iter()
            .filter(|s| s.atm_iv.is_some() && s.days_to_expiry >= 0)
            .min_by_key(|s| s.days_to_expiry)
            .and_then(|s| s.atm_iv);

        let multi_summary = MultiExpirySummary {
            snapshots,
            aggregate_volume,
            aggregate_oi,
            weighted_put_call_ratio,
            oi_concentration,
            iv_term_structure,
            max_pain_range,
        };

        return Ok(OptionsResponse {
            ticker,
            underlying_price,
            generated_at: Utc::now(),
            status: None,
            error: None,
            expirations,
            requested_expiry: req.expiry.clone(),
            selected_expiry: None,
            target_dte_days: req.target_dte_days,
            selected_days_to_expiry: None,
            auto_selected_expiry: false,
            selection_reason: Some("multi_expiry_summary".to_string()),
            calls: vec![],
            puts: vec![],
            atm_iv: top_atm_iv,
            metrics: None,
            note,
            multi_expiry_summary: Some(multi_summary),
        });
    }

    let requested_expiry = req.expiry.clone();
    let today = Utc::now().date_naive();
    let expiry_dates: Vec<(i64, chrono::NaiveDate, String)> = expiration_timestamps_raw
        .iter()
        .filter_map(|&ts| {
            Utc.timestamp_opt(ts, 0).single().map(|dt| {
                let date = dt.date_naive();
                (ts, date, dt.format("%Y-%m-%d").to_string())
            })
        })
        .collect();

    let requested_expiry_ts: Option<i64> = if let Some(exp_str) = req.expiry.as_deref() {
        let date = chrono::NaiveDate::parse_from_str(exp_str.trim(), "%Y-%m-%d")
            .map_err(|_| Error::InvalidInput(format!("invalid expiry date: {exp_str}")))?;
        let dt =
            DateTime::<Utc>::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap(), Utc);
        Some(dt.timestamp())
    } else {
        None
    };

    let first_expiration_ts = expiration_timestamps_raw.first().copied();
    let first_chain_options = chain
        .options
        .as_ref()
        .and_then(|items| items.first())
        .cloned();

    let mut auto_selected_expiry = false;
    let mut selection_reason: Option<String> = None;

    let candidate_timestamps: Vec<i64> = if let Some(ts) = requested_expiry_ts {
        vec![ts]
    } else if let Some(target_dte_days) = req.target_dte_days {
        let mut candidates = expiry_dates.clone();
        candidates.sort_by_key(|(_, date, _)| ((*date - today).num_days() - target_dte_days).abs());
        if let Some((ts, _, label)) = candidates.first() {
            selection_reason = Some(format!("closest_to_target_dte:{target_dte_days}d->{label}"));
            vec![*ts]
        } else {
            vec![]
        }
    } else {
        // Auto-select: pin to nearest MONTHLY OpEx (3rd Friday) >= 14 days out.
        // Monthly OpEx is where open interest concentrates and squeeze mechanics
        // happen — always prefer it over weekly expiries.
        //
        // Cascade:
        //   1. Nearest monthly (3rd Friday) with >= 14 DTE
        //   2. Nearest monthly (3rd Friday) even if < 14 DTE
        //   3. Nearest weekly >= 14 DTE (only if no monthly exists at all)
        //   4. First available future expiry
        let min_dte = 14i64;

        let is_monthly = |date: &chrono::NaiveDate| -> bool {
            date.weekday() == chrono::Weekday::Fri && date.day() >= 15 && date.day() <= 21
        };

        let future_gte_14: Vec<&(i64, chrono::NaiveDate, String)> = expiry_dates
            .iter()
            .filter(|(_, date, _)| (*date - today).num_days() >= min_dte)
            .collect();

        // All monthly expiries (regardless of DTE)
        let all_monthly: Vec<&(i64, chrono::NaiveDate, String)> = expiry_dates
            .iter()
            .filter(|(_, date, _)| is_monthly(date) && *date > today)
            .collect();

        // Monthly expiries >= 14 DTE
        let monthly_gte_14: Vec<i64> = future_gte_14
            .iter()
            .filter(|(_, date, _)| is_monthly(date))
            .map(|(ts, _, _)| *ts)
            .collect();

        if !monthly_gte_14.is_empty() {
            // Best case: monthly OpEx with enough time
            auto_selected_expiry = true;
            selection_reason = Some("nearest_monthly_opex_gte_14dte".to_string());
            vec![monthly_gte_14[0]]
        } else if !all_monthly.is_empty() {
            // All monthlies are < 14 days out — still prefer monthly over weekly
            auto_selected_expiry = true;
            selection_reason = Some("nearest_monthly_opex_lt_14dte".to_string());
            vec![all_monthly[0].0]
        } else if !future_gte_14.is_empty() {
            // No monthly exists at all — fall back to nearest weekly >= 14 DTE
            auto_selected_expiry = true;
            selection_reason = Some("nearest_weekly_gte_14dte_no_monthly".to_string());
            vec![future_gte_14[0].0]
        } else {
            // Everything is < 14 days and no monthly — take first future expiry
            auto_selected_expiry = true;
            selection_reason = Some("first_available_expiry".to_string());
            expiry_dates.iter()
                .filter(|(_, date, _)| *date > today)
                .map(|(ts, _, _)| *ts)
                .take(1)
                .collect()
        }
    };

    // Check usability on the FULL chain (ignore near_money_pct / option_type filters)
    // so that summary mode doesn't reject an expiry just because a narrow filter
    // window happens to land on zero-volume strikes.
    let chain_is_usable = |opts: &YahooOptions| {
        let calls = opts.calls.as_deref().unwrap_or_default();
        let puts = opts.puts.as_deref().unwrap_or_default();

        let has_contracts = !calls.is_empty() || !puts.is_empty();
        let has_iv = calls
            .iter()
            .any(|c| c.implied_volatility.unwrap_or(0.0) > 0.0)
            || puts
                .iter()
                .any(|p| p.implied_volatility.unwrap_or(0.0) > 0.0);
        let total_oi: i64 = calls
            .iter()
            .map(|c| c.open_interest.unwrap_or(0))
            .sum::<i64>()
            + puts
                .iter()
                .map(|p| p.open_interest.unwrap_or(0))
                .sum::<i64>();
        let total_volume: i64 = calls.iter().map(|c| c.volume.unwrap_or(0)).sum::<i64>()
            + puts.iter().map(|p| p.volume.unwrap_or(0)).sum::<i64>();

        has_contracts && (has_iv || total_oi > 0 || total_volume > 0)
    };

    let mut options_data: Option<YahooOptions> = None;
    let mut selected_expiry: Option<String> = None;
    let mut selected_days_to_expiry: Option<i64> = None;

    for (idx, ts) in candidate_timestamps.iter().enumerate() {
        let Some(opts) = load_options_for_ts(
            &client,
            &ticker,
            &crumb,
            first_expiration_ts,
            first_chain_options.as_ref(),
            *ts,
        )
        .await?
        else {
            continue;
        };
        let exp_date = Utc
            .timestamp_opt(opts.expiration_date, 0)
            .single()
            .map(|dt| dt.date_naive());
        let usable = !req.summary_only || chain_is_usable(&opts);
        if usable {
            selected_days_to_expiry = exp_date.map(|date| (date - today).num_days());
            selected_expiry = exp_date.map(|date| date.format("%Y-%m-%d").to_string());
            if req.summary_only && requested_expiry.is_none() && req.target_dte_days.is_none() {
                if idx > 0 {
                    auto_selected_expiry = true;
                    let first_label = candidate_timestamps
                        .first()
                        .and_then(|first_ts| Utc.timestamp_opt(*first_ts, 0).single())
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .unwrap_or_default();
                    let chosen = selected_expiry.clone().unwrap_or_default();
                    selection_reason = Some(format!(
                        "auto_skipped_unusable_expiry:{first_label}->{chosen}"
                    ));
                } else if selection_reason.is_none() {
                    selection_reason = Some("first_usable_future_expiry".to_string());
                }
            }
            options_data = Some(opts);
            break;
        }
    }

    let (raw_calls, raw_puts, selected_expiry) = match options_data {
        Some(opts) => (
            opts.calls.unwrap_or_default(),
            opts.puts.unwrap_or_default(),
            selected_expiry,
        ),
        None => (vec![], vec![], None),
    };

    // Convert Yahoo contracts to our format
    let convert_contract = |c: YahooContract, opt_type: &str| -> OptionContract {
        let expiry = Utc
            .timestamp_opt(c.expiration, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        // Validate IV range. Yahoo returns garbage IV (≈0) when markets are closed
        // and bid/ask are 0.0 — the previous "v <= 0.0" gate let through values like
        // 0.0001 (= 0.01% annualized, impossibly low for any liquid option). Discard
        // anything below 0.005 (= 0.5% annualized) and anything above 5.0 (= 500%
        // annualized, hyperinflation-tier or junk). Real liquid-option IV ranges
        // 0.05 – 2.0 (5% to 200%) annualized; thinly-traded or far-OTM contracts
        // can stretch the upper bound but rarely cross 5.0.
        let iv = c
            .implied_volatility
            .and_then(|v| if v >= 0.005 && v <= 5.0 { Some(v) } else { None });

        OptionContract {
            contract_symbol: c.contract_symbol,
            strike: c.strike,
            expiry,
            option_type: opt_type.to_string(),
            bid: c.bid.unwrap_or(0.0),
            ask: c.ask.unwrap_or(0.0),
            last: c.last_price.unwrap_or(0.0),
            // Preserve null vs 0.0 distinction. Yahoo returns null for these
            // fields when markets are closed; collapsing to 0.0 fakes a real
            // "no movement" datapoint.
            change: c.change,
            pct_change: c.percent_change,
            volume: c.volume.unwrap_or(0) as u64,
            open_interest: c.open_interest.unwrap_or(0) as u64,
            implied_volatility: iv,
            in_the_money: c.in_the_money.unwrap_or(false),
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
        }
    };

    let mut calls: Vec<OptionContract> = raw_calls
        .into_iter()
        .map(|c| convert_contract(c, "call"))
        .collect();

    let mut puts: Vec<OptionContract> = raw_puts
        .into_iter()
        .map(|c| convert_contract(c, "put"))
        .collect();

    // Filter by option type if specified
    if let Some(ref opt_type) = req.option_type {
        let t = opt_type.trim().to_lowercase();
        if t == "calls" || t == "call" {
            puts.clear();
        } else if t == "puts" || t == "put" {
            calls.clear();
        }
    }

    // Filter by near-money percentage if specified
    if let Some(pct) = req.near_money_pct {
        if underlying_price > 0.0 && pct > 0.0 {
            let low = underlying_price * (1.0 - pct / 100.0);
            let high = underlying_price * (1.0 + pct / 100.0);
            calls.retain(|c| c.strike >= low && c.strike <= high);
            puts.retain(|p| p.strike >= low && p.strike <= high);
        }
    }

    // Calculate metrics
    let total_call_volume: u64 = calls.iter().map(|c| c.volume).sum();
    let total_put_volume: u64 = puts.iter().map(|p| p.volume).sum();
    let total_call_oi: u64 = calls.iter().map(|c| c.open_interest).sum();
    let total_put_oi: u64 = puts.iter().map(|p| p.open_interest).sum();

    // Use None when denominator is zero — 0.0 is misleading (looks like a computed
    // ratio of zero rather than "undefined").
    let put_call_ratio_volume = if total_call_volume > 0 {
        Some(total_put_volume as f64 / total_call_volume as f64)
    } else if total_put_volume > 0 {
        // All volume is puts, no call volume — extremely bearish; represent as high ratio.
        Some(f64::INFINITY.min(99.99))
    } else {
        // No volume at all (weekend/holiday) — ratio is undefined.
        None
    };

    let put_call_ratio_oi = if total_call_oi > 0 {
        Some(total_put_oi as f64 / total_call_oi as f64)
    } else if total_put_oi > 0 {
        Some(f64::INFINITY.min(99.99))
    } else {
        None
    };

    // Find ATM options — pick the closest strike that actually has IV data.
    // Previously we picked the absolute closest strike and then tried to read its IV,
    // which returned None when that specific contract had no IV (deep ITM, illiquid, etc.).
    let atm_iv_call = {
        let mut candidates: Vec<&OptionContract> =
            calls.iter().filter(|c| c.implied_volatility.is_some()).collect();
        candidates.sort_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.first().and_then(|c| c.implied_volatility)
    };

    let atm_iv_put = {
        let mut candidates: Vec<&OptionContract> =
            puts.iter().filter(|p| p.implied_volatility.is_some()).collect();
        candidates.sort_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.first().and_then(|p| p.implied_volatility)
    };

    let skew_near_put_call_iv_ratio = match (atm_iv_call, atm_iv_put) {
        (Some(call_iv), Some(put_iv)) if call_iv > 0.0 && put_iv > 0.0 => {
            Some(put_iv / call_iv)
        }
        _ => None,
    };
    let has_iv_data = atm_iv_call.is_some() || atm_iv_put.is_some();
    let has_liquid_near_money =
        (total_call_volume + total_put_volume + total_call_oi + total_put_oi) > 0;

    // Max pain: the strike minimizing total dollar value paid to options holders.
    // At max pain, call holders with strikes below K and put holders with strikes
    // above K are in-the-money.  Writers profit most at this strike.
    // Filter out extreme strikes (e.g. pre-split artifacts from Yahoo) that distort
    // the calculation.  Keep strikes between 10% and 500% of underlying.
    // Require OI on both sides (calls AND puts) for a meaningful max pain.
    let strike_lo = underlying_price * 0.50;
    let strike_hi = underlying_price * 2.0;
    let filtered_call_oi: u64 = calls
        .iter()
        .filter(|c| c.strike >= strike_lo && c.strike <= strike_hi)
        .map(|c| c.open_interest as u64)
        .sum();
    let filtered_put_oi: u64 = puts
        .iter()
        .filter(|p| p.strike >= strike_lo && p.strike <= strike_hi)
        .map(|p| p.open_interest as u64)
        .sum();
    let max_pain = if filtered_call_oi > 0 && filtered_put_oi > 0 {
        let all_strikes: std::collections::BTreeSet<i64> = calls
            .iter()
            .filter(|c| c.strike >= strike_lo && c.strike <= strike_hi)
            .map(|c| (c.strike * 100.0).round() as i64)
            .chain(
                puts.iter()
                    .filter(|p| p.strike >= strike_lo && p.strike <= strike_hi)
                    .map(|p| (p.strike * 100.0).round() as i64),
            )
            .collect();
        all_strikes
            .iter()
            .min_by_key(|&&k| {
                let k_price = k as f64 / 100.0;
                let call_itm: f64 = calls
                    .iter()
                    .filter(|c| c.strike >= strike_lo && c.strike <= strike_hi && c.strike < k_price)
                    .map(|c| (k_price - c.strike) * c.open_interest as f64)
                    .sum();
                let put_itm: f64 = puts
                    .iter()
                    .filter(|p| p.strike >= strike_lo && p.strike <= strike_hi && p.strike > k_price)
                    .map(|p| (p.strike - k_price) * p.open_interest as f64)
                    .sum();
                ((call_itm + put_itm) * 100.0) as i64
            })
            .map(|&k| k as f64 / 100.0)
    } else {
        None
    };

    let atm_iv = match (atm_iv_call, atm_iv_put) {
        (Some(call_iv), Some(put_iv)) => Some((call_iv + put_iv) / 2.0),
        (Some(call_iv), None) => Some(call_iv),
        (None, Some(put_iv)) => Some(put_iv),
        (None, None) => None,
    };

    let metrics = Some(OptionsMetrics {
        underlying_price,
        put_call_ratio_volume,
        put_call_ratio_oi,
        total_call_volume,
        total_put_volume,
        total_call_oi,
        total_put_oi,
        atm_iv_call,
        atm_iv_put,
        atm_iv,
        skew_near_put_call_iv_ratio,
        has_iv_data,
        has_liquid_near_money,
        max_pain,
        expirations_analyzed: Some(1),
    });

    // summary_only now keeps the chains (metrics + chains together is more useful)
    let (final_calls, final_puts) = (calls, puts);

    // Advisory `note` prose dropped — numeric fields (total_call_oi,
    // has_liquid_near_money, expirations) already convey the same info.
    let note: Option<String> = note;

    Ok(OptionsResponse {
        ticker,
        underlying_price,
        generated_at: Utc::now(),
        status: None,
        error: None,
        expirations,
        requested_expiry,
        selected_expiry,
        target_dte_days: req.target_dte_days,
        selected_days_to_expiry,
        auto_selected_expiry,
        selection_reason,
        calls: final_calls,
        puts: final_puts,
        atm_iv,
        metrics,
        note,
        multi_expiry_summary: None,
    })
}
