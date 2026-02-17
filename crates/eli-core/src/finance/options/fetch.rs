use super::super::*;

async fn yahoo_lookup_quote_type(client: &reqwest::Client, ticker: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(YAHOO_SEARCH_URL).ok()?;
    url.query_pairs_mut()
        .append_pair("q", ticker)
        .append_pair("quotesCount", "8")
        .append_pair("newsCount", "0");

    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let quotes = json["quotes"].as_array()?;

    let mut fallback: Option<&serde_json::Value> = None;
    for q in quotes {
        if fallback.is_none() {
            fallback = Some(q);
        }
        if q["symbol"]
            .as_str()
            .map(|s| s.eq_ignore_ascii_case(ticker))
            .unwrap_or(false)
        {
            return q["quoteType"].as_str().map(|s| s.to_string());
        }
    }

    fallback.and_then(|q| q["quoteType"].as_str().map(|s| s.to_string()))
}

pub async fn fetch_options(req: OptionsRequest) -> Result<OptionsResponse> {
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
                selected_expiry: None,
                calls: vec![],
                puts: vec![],
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

    #[derive(Deserialize)]
    struct YahooOptionsResp {
        #[serde(rename = "optionChain")]
        option_chain: YahooOptionChain,
    }

    #[derive(Deserialize)]
    struct YahooOptionChain {
        result: Vec<YahooChainResult>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct YahooChainResult {
        underlying_symbol: Option<String>,
        expiration_dates: Option<Vec<i64>>,
        quote: Option<YahooQuote>,
        options: Option<Vec<YahooOptions>>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct YahooQuote {
        regular_market_price: Option<f64>,
    }

    #[derive(Deserialize)]
    struct YahooOptions {
        #[serde(rename = "expirationDate")]
        expiration_date: i64,
        calls: Option<Vec<YahooContract>>,
        puts: Option<Vec<YahooContract>>,
    }

    #[derive(Deserialize)]
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
            selected_expiry: None,
            calls: vec![],
            puts: vec![],
            metrics: None,
            note,
            multi_expiry_summary: None,
        });
    }

    // Multi-expiry mode: fetch summary for multiple expirations
    if req.multi_expiry {
        let num_expiries = req.num_expiries.unwrap_or(3).min(5);
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
        let mut first_pc_ratio: Option<f64> = None;

        for exp_ts in expiration_timestamps {
            let url = format!(
                "{}/{}?crumb={}&date={}",
                YAHOO_OPTIONS_URL, ticker, crumb, exp_ts
            );
            let resp = client.get(&url).send().await;

            if let Ok(resp) = resp {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<YahooOptionsResp>().await {
                        if let Some(chain_result) = body.option_chain.result.into_iter().next() {
                            if let Some(opts) =
                                chain_result.options.and_then(|o| o.into_iter().next())
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

                                // Max pain calculation
                                let mut strike_oi: std::collections::HashMap<i64, u64> =
                                    std::collections::HashMap::new();
                                for c in &calls {
                                    let strike_cents = (c.strike * 100.0).round() as i64;
                                    *strike_oi.entry(strike_cents).or_insert(0) +=
                                        c.open_interest.unwrap_or(0) as u64;
                                }
                                for p in &puts {
                                    let strike_cents = (p.strike * 100.0).round() as i64;
                                    *strike_oi.entry(strike_cents).or_insert(0) +=
                                        p.open_interest.unwrap_or(0) as u64;
                                }
                                let max_pain = strike_oi
                                    .into_iter()
                                    .max_by_key(|(_, oi)| *oi)
                                    .map(|(strike_cents, _)| strike_cents as f64 / 100.0);

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

                                if first_pc_ratio.is_none() && total_vol > 0 {
                                    first_pc_ratio = Some(pc_vol);
                                }

                                aggregate_volume += total_vol;
                                weighted_pc_sum += pc_vol * total_vol as f64;

                                snapshots.push(ExpirySnapshot {
                                    expiry: expiry_date,
                                    days_to_expiry,
                                    total_volume: total_vol,
                                    total_oi: call_oi + put_oi,
                                    put_call_ratio_volume: pc_vol,
                                    put_call_ratio_oi: pc_oi,
                                    max_pain,
                                    atm_iv,
                                });
                            }
                        }
                    }
                }
            }

            // Rate limit
            tokio::time::sleep(StdDuration::from_millis(100)).await;
        }

        let weighted_put_call_ratio = if aggregate_volume > 0 {
            weighted_pc_sum / aggregate_volume as f64
        } else {
            0.0
        };

        let near_term_bias = match first_pc_ratio {
            Some(pc) if pc < 0.7 => "bullish".to_string(),
            Some(pc) if pc > 1.3 => "bearish".to_string(),
            _ => "neutral".to_string(),
        };

        let multi_summary = MultiExpirySummary {
            snapshots,
            aggregate_volume,
            weighted_put_call_ratio,
            near_term_bias,
        };

        return Ok(OptionsResponse {
            ticker,
            underlying_price,
            generated_at: Utc::now(),
            status: None,
            error: None,
            expirations,
            selected_expiry: None,
            calls: vec![],
            puts: vec![],
            metrics: None,
            note,
            multi_expiry_summary: Some(multi_summary),
        });
    }

    // Determine which expiry to fetch
    let target_expiry_ts: Option<i64> = if let Some(exp_str) = req.expiry.as_deref() {
        // Parse user-provided expiry date
        let date = chrono::NaiveDate::parse_from_str(exp_str.trim(), "%Y-%m-%d")
            .map_err(|_| Error::InvalidInput(format!("invalid expiry date: {exp_str}")))?;
        let dt =
            DateTime::<Utc>::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap(), Utc);
        Some(dt.timestamp())
    } else {
        None
    };

    // Fetch specific expiry if requested (different from first fetch)
    let options_data = if let Some(ts) = target_expiry_ts {
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

        body.option_chain
            .result
            .into_iter()
            .next()
            .and_then(|r| r.options)
            .and_then(|o| o.into_iter().next())
    } else {
        chain.options.and_then(|o| o.into_iter().next())
    };

    let (raw_calls, raw_puts, selected_expiry) = match options_data {
        Some(opts) => {
            let exp_date = Utc
                .timestamp_opt(opts.expiration_date, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string());
            (
                opts.calls.unwrap_or_default(),
                opts.puts.unwrap_or_default(),
                exp_date,
            )
        }
        None => (vec![], vec![], None),
    };

    // Convert Yahoo contracts to our format
    let convert_contract = |c: YahooContract, opt_type: &str| -> OptionContract {
        let expiry = Utc
            .timestamp_opt(c.expiration, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let iv = c
            .implied_volatility
            .and_then(|v| if v.abs() < 1e-4 { None } else { Some(v) });

        OptionContract {
            contract_symbol: c.contract_symbol,
            strike: c.strike,
            expiry,
            option_type: opt_type.to_string(),
            bid: c.bid.unwrap_or(0.0),
            ask: c.ask.unwrap_or(0.0),
            last: c.last_price.unwrap_or(0.0),
            change: c.change.unwrap_or(0.0),
            pct_change: c.percent_change.unwrap_or(0.0),
            volume: c.volume.unwrap_or(0) as u64,
            open_interest: c.open_interest.unwrap_or(0) as u64,
            implied_volatility: iv,
            in_the_money: c.in_the_money.unwrap_or(false),
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

    let put_call_ratio_volume = if total_call_volume > 0 {
        total_put_volume as f64 / total_call_volume as f64
    } else {
        0.0
    };

    let put_call_ratio_oi = if total_call_oi > 0 {
        total_put_oi as f64 / total_call_oi as f64
    } else {
        0.0
    };

    // Find ATM options (closest to underlying price)
    let atm_iv_call = calls
        .iter()
        .min_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .and_then(|c| c.implied_volatility);

    let atm_iv_put = puts
        .iter()
        .min_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .and_then(|p| p.implied_volatility);

    // Calculate max pain (strike with highest total OI)
    let mut strike_oi: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
    for c in &calls {
        let strike_cents = (c.strike * 100.0).round() as i64;
        *strike_oi.entry(strike_cents).or_insert(0) += c.open_interest;
    }
    for p in &puts {
        let strike_cents = (p.strike * 100.0).round() as i64;
        *strike_oi.entry(strike_cents).or_insert(0) += p.open_interest;
    }
    let max_pain = strike_oi
        .into_iter()
        .max_by_key(|(_, oi)| *oi)
        .map(|(strike_cents, _)| strike_cents as f64 / 100.0);

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
        max_pain,
    });

    // summary_only now keeps the chains (metrics + chains together is more useful)
    let (final_calls, final_puts) = (calls, puts);

    let note = if note.is_some() {
        note
    } else if selected_expiry.is_none() {
        Some("No options chain returned for the requested expiry. Use `--expirations` to see valid dates.".to_string())
    } else {
        None
    };

    Ok(OptionsResponse {
        ticker,
        underlying_price,
        generated_at: Utc::now(),
        status: None,
        error: None,
        expirations,
        selected_expiry,
        calls: final_calls,
        puts: final_puts,
        metrics,
        note,
        multi_expiry_summary: None,
    })
}
