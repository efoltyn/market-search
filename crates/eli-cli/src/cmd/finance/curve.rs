/// Futures term structure (forward curve) via Yahoo Finance.
///
/// Maps commodity names to Yahoo futures ticker patterns, fetches the latest
/// close for each contract month, and outputs the term structure showing
/// contango/backwardation.

/// Yahoo futures month codes: F=Jan, G=Feb, H=Mar, J=Apr, K=May, M=Jun,
/// N=Jul, Q=Aug, U=Sep, V=Oct, X=Nov, Z=Dec
const MONTH_CODES: [(char, &str); 12] = [
    ('F', "Jan"),
    ('G', "Feb"),
    ('H', "Mar"),
    ('J', "Apr"),
    ('K', "May"),
    ('M', "Jun"),
    ('N', "Jul"),
    ('Q', "Aug"),
    ('U', "Sep"),
    ('V', "Oct"),
    ('X', "Nov"),
    ('Z', "Dec"),
];

#[derive(Debug, Clone)]
pub struct CommoditySpec {
    /// Yahoo root symbol (e.g. "CL" for WTI crude)
    pub root: &'static str,
    /// Yahoo exchange suffix (e.g. ".NYM" for NYMEX)
    pub exchange: &'static str,
    /// Human name
    pub name: &'static str,
    /// Unit of measure
    pub unit: &'static str,
}

pub fn lookup_commodity(query: &str) -> Option<CommoditySpec> {
    let q = query.to_ascii_lowercase();
    match q.as_str() {
        "oil" | "crude" | "wti" | "cl" => Some(CommoditySpec {
            root: "CL",
            exchange: ".NYM",
            name: "WTI Crude Oil",
            unit: "$/bbl",
        }),
        "brent" | "bz" => Some(CommoditySpec {
            root: "BZ",
            exchange: ".NYM",
            name: "Brent Crude Oil",
            unit: "$/bbl",
        }),
        "gold" | "gc" => Some(CommoditySpec {
            root: "GC",
            exchange: ".CMX",
            name: "Gold",
            unit: "$/oz",
        }),
        "silver" | "si" => Some(CommoditySpec {
            root: "SI",
            exchange: ".CMX",
            name: "Silver",
            unit: "$/oz",
        }),
        "natgas" | "gas" | "ng" | "natural gas" => Some(CommoditySpec {
            root: "NG",
            exchange: ".NYM",
            name: "Natural Gas",
            unit: "$/MMBtu",
        }),
        "copper" | "hg" => Some(CommoditySpec {
            root: "HG",
            exchange: ".CMX",
            name: "Copper",
            unit: "$/lb",
        }),
        "platinum" | "pl" => Some(CommoditySpec {
            root: "PL",
            exchange: ".NYM",
            name: "Platinum",
            unit: "$/oz",
        }),
        "palladium" | "pa" => Some(CommoditySpec {
            root: "PA",
            exchange: ".NYM",
            name: "Palladium",
            unit: "$/oz",
        }),
        "rbob" | "gasoline" | "rb" => Some(CommoditySpec {
            root: "RB",
            exchange: ".NYM",
            name: "RBOB Gasoline",
            unit: "$/gal",
        }),
        "heating" | "ho" | "heating oil" => Some(CommoditySpec {
            root: "HO",
            exchange: ".NYM",
            name: "Heating Oil",
            unit: "$/gal",
        }),
        _ => None,
    }
}

pub fn list_commodities() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("oil / crude / wti", "CL", "WTI Crude Oil"),
        ("brent", "BZ", "Brent Crude Oil"),
        ("gold", "GC", "Gold"),
        ("silver", "SI", "Silver"),
        ("natgas / gas", "NG", "Natural Gas"),
        ("copper", "HG", "Copper"),
        ("platinum", "PL", "Platinum"),
        ("palladium", "PA", "Palladium"),
        ("rbob / gasoline", "RB", "RBOB Gasoline"),
        ("heating / ho", "HO", "Heating Oil"),
    ]
}

/// Generate tickers for the next N contract months from today.
pub fn generate_futures_tickers(spec: &CommoditySpec, months: usize) -> Vec<(String, String)> {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    let mut year = now.year();
    // Start from next month — current month contract is often expired/rolling
    let mut month_idx = now.month() as usize + 1; // 1-based, skip current

    let mut tickers = Vec::with_capacity(months);
    for _ in 0..months {
        if month_idx > 12 {
            month_idx = 1;
            year += 1;
        }
        let (code, label) = MONTH_CODES[month_idx - 1];
        let yy = year % 100;
        let ticker = format!("{}{}{:02}{}", spec.root, code, yy, spec.exchange);
        let contract_label = format!("{} {}", label, year);
        tickers.push((ticker, contract_label));
        month_idx += 1;
    }
    tickers
}

#[derive(Serialize)]
struct CurveResponse {
    commodity: String,
    unit: String,
    generated_at: String,
    front_month_price: Option<f64>,
    back_month_price: Option<f64>,
    spread: Option<f64>,
    spread_pct: Option<f64>,
    contracts: Vec<ContractPoint>,
}

#[derive(Serialize)]
struct ContractPoint {
    ticker: String,
    contract: String,
    price: f64,
    change_from_front: Option<f64>,
    change_from_front_pct: Option<f64>,
}

async fn cmd_finance_curve(args: FinanceCurveArgs) -> Result<()> {
    if args.list {
        let commodities = list_commodities();
        let out = serde_json::json!({
            "commodities": commodities.iter().map(|(aliases, root, name)| {
                serde_json::json!({
                    "aliases": aliases,
                    "root_symbol": root,
                    "name": name,
                })
            }).collect::<Vec<_>>()
        });
        let pretty = serde_json::to_string_pretty(&out).unwrap();
        if let Some(out_path) = args.out {
            std::fs::write(&out_path, &pretty)
                .map_err(|e| anyhow::anyhow!("failed to write {}: {}", out_path.display(), e))?;
            println!(
                "{{\"ok\":true,\"path\":{}}}",
                serde_json::to_string(&out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string())
            );
        } else {
            println!("{}", pretty);
        }
        return Ok(());
    }

    let query = args.commodity.as_deref().unwrap_or("oil");

    // Handle "all" — fetch every commodity's curve
    let queries: Vec<String> = if query == "all" {
        list_commodities()
            .iter()
            .map(|(aliases, _, _)| aliases.split(" / ").next().unwrap_or(aliases).to_string())
            .collect()
    } else {
        vec![query.to_string()]
    };

    for (idx, q) in queries.iter().enumerate() {
    let spec = lookup_commodity(q).ok_or_else(|| {
        let commodities = list_commodities();
        let names: Vec<&str> = commodities.iter().map(|(a, _, _)| *a).collect();
        anyhow::anyhow!(
            "unknown commodity '{}'. Supported: {}, all. Use --list to see all.",
            q,
            names.join(", ")
        )
    })?;

    let months = args.months.min(24); // cap at 2 years
    let futures = generate_futures_tickers(&spec, months);

    let tickers_str: Vec<String> = futures.iter().map(|(t, _)| t.clone()).collect();
    let all_tickers = tickers_str.join(",");

    let range = eli_core::finance::Span::parse("5d")
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse range")?;
    let granularity = eli_core::finance::Span::parse("1d")
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse granularity")?;

    let paths = Paths::discover().context("discover paths")?;
    paths.ensure_dirs().context("ensure dirs")?;

    // Try IBKR first (better data), fall back to Yahoo.
    // IBKR tickers: FUT:CL:NYMEX:YYYYMM
    let ibkr_exchange = spec.exchange.trim_start_matches('.').replace("NYM", "NYMEX").replace("CMX", "COMEX");
    let ibkr_tickers: Vec<String> = futures.iter().map(|(yahoo_t, _)| {
        // Parse month/year from Yahoo ticker to build IBKR expiry
        // Yahoo: CLK26.NYM → month_code=K, yy=26
        let root = spec.root;
        let rest = yahoo_t.strip_prefix(root).unwrap_or(yahoo_t);
        let month_code = rest.chars().next().unwrap_or('F');
        let yy: u32 = rest[1..3].parse().unwrap_or(26);
        let month_num = MONTH_CODES.iter().position(|(c, _)| *c == month_code).map(|i| i + 1).unwrap_or(1);
        format!("FUT:{}:{}:{}{:02}", root, ibkr_exchange, 2000 + yy, month_num)
    }).collect();

    // Try IBKR
    let ibkr_req = eli_core::finance::TimeseriesRequest {
        tickers: ibkr_tickers.clone(),
        range: range.clone(),
        granularity: granularity.clone(),
        as_of: None,
        provider: eli_core::finance::ProviderKind::Ibkr,
        max_points_per_ticker: None,
        ibkr: None,
    };

    let resp = match eli_core::finance::fetch_timeseries(ibkr_req, &paths.cache_dir).await {
        Ok(r) if !r.series.is_empty() => {
            // Check that IBKR actually returned different prices per contract,
            // not just the front month repeated for every expiry
            let prices: Vec<f64> = r.series.iter()
                .filter_map(|s| s.candles.last().map(|c| c.c))
                .collect();
            // Use percentage difference to detect front-month-only duplication.
            // Real curves have >1% spread front-to-back. All-same means IBKR failed to resolve months.
            let spread_pct = if let (Some(&first), Some(&last)) = (prices.first(), prices.last()) {
                if first > 0.0 { ((last - first) / first).abs() * 100.0 } else { 0.0 }
            } else { 0.0 };
            let all_same = prices.len() > 1 && spread_pct < 0.1;
            eprintln!("[curve] IBKR prices: {:?} spread={:.2}% all_same={}", prices, spread_pct, all_same);
            if all_same {
                eprintln!("[curve] IBKR returned same price for all months (front-month only), falling back to Yahoo for {}", spec.name);
                let yahoo_req = eli_core::finance::TimeseriesRequest {
                    tickers: tickers_str.clone(),
                    range,
                    granularity,
                    as_of: None,
                    provider: eli_core::finance::ProviderKind::Yahoo,
                    max_points_per_ticker: None,
                    ibkr: None,
                };
                eli_core::finance::fetch_timeseries(yahoo_req, &paths.cache_dir)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("fetch futures timeseries")?
            } else {
                eprintln!("[curve] using IBKR for {} ({} series)", spec.name, r.series.len());
                r
            }
        }
        _ => {
            // Fall back to Yahoo
            eprintln!("[curve] IBKR unavailable, falling back to Yahoo for {}", spec.name);
            let yahoo_req = eli_core::finance::TimeseriesRequest {
                tickers: tickers_str.clone(),
                range,
                granularity,
                as_of: None,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: None,
                ibkr: None,
            };
            eli_core::finance::fetch_timeseries(yahoo_req, &paths.cache_dir)
                .await
                .map_err(|e| anyhow::anyhow!(e))
                .context("fetch futures timeseries")?
        }
    };

    // Extract latest close for each ticker.
    // Match by index position: futures[i] corresponds to resp.series in order,
    // or by ticker name if provider returns them (Yahoo uses yahoo tickers, IBKR uses IBKR tickers).
    let mut contracts: Vec<ContractPoint> = Vec::new();
    for (i, (ticker, label)) in futures.iter().enumerate() {
        // Try exact Yahoo match first, then exact IBKR match
        let series = resp.series.iter().find(|s| &s.ticker == ticker)
            .or_else(|| {
                if i < ibkr_tickers.len() {
                    resp.series.iter().find(|s| s.ticker == ibkr_tickers[i])
                } else {
                    None
                }
            });
        if let Some(series) = series {
            if let Some(last_candle) = series.candles.last() {
                contracts.push(ContractPoint {
                    ticker: ticker.clone(),
                    contract: label.clone(),
                    price: last_candle.c,
                    change_from_front: None,
                    change_from_front_pct: None,
                });
            }
        }
    }

    if contracts.is_empty() {
        anyhow::bail!("no futures data returned for {} ({})", spec.name, all_tickers);
    }

    // Compute changes from front month
    let front_price = contracts[0].price;
    for c in contracts.iter_mut() {
        let diff = c.price - front_price;
        c.change_from_front = Some(diff);
        c.change_from_front_pct = Some(diff / front_price * 100.0);
    }

    let back_price = contracts.last().map(|c| c.price);
    let spread = back_price.map(|b| b - front_price);
    let spread_pct = back_price.map(|b| (b - front_price) / front_price * 100.0);

    let response = CurveResponse {
        commodity: spec.name.to_string(),
        unit: spec.unit.to_string(),
        generated_at: chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        front_month_price: Some(front_price),
        back_month_price: back_price,
        spread,
        spread_pct,
        contracts,
    };

    if let Some(ref out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path.clone(),
            &response,
            "finance.curve",
            &[format!("commodity={}", q)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
    } else {
        let json =
            serde_json::to_string_pretty(&response).context("serialize curve response")?;
        println!("{json}");
    }

    } // end for loop over queries

    Ok(())
}
