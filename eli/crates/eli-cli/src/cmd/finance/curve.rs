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
struct CommoditySpec {
    /// Yahoo root symbol (e.g. "CL" for WTI crude)
    root: &'static str,
    /// Yahoo exchange suffix (e.g. ".NYM" for NYMEX)
    exchange: &'static str,
    /// Human name
    name: &'static str,
    /// Unit of measure
    unit: &'static str,
}

fn lookup_commodity(query: &str) -> Option<CommoditySpec> {
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

fn list_commodities() -> Vec<(&'static str, &'static str, &'static str)> {
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
fn generate_futures_tickers(spec: &CommoditySpec, months: usize) -> Vec<(String, String)> {
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
    structure: String, // "backwardation" | "contango" | "mixed" | "insufficient_data"
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
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(());
    }

    let query = args.commodity.as_deref().unwrap_or("oil");
    let spec = lookup_commodity(query).ok_or_else(|| {
        let commodities = list_commodities();
        let names: Vec<&str> = commodities.iter().map(|(a, _, _)| *a).collect();
        anyhow::anyhow!(
            "unknown commodity '{}'. Supported: {}. Use --list to see all.",
            query,
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

    let req = eli_core::finance::TimeseriesRequest {
        tickers: tickers_str.clone(),
        range,
        granularity,
        as_of: None,
        provider: eli_core::finance::ProviderKind::Yahoo,
        max_points_per_ticker: None,
        ibkr: None,
    };

    let resp = eli_core::finance::fetch_timeseries(req, &paths.cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch futures timeseries")?;

    // Extract latest close for each ticker
    let mut contracts: Vec<ContractPoint> = Vec::new();
    for (ticker, label) in &futures {
        if let Some(series) = resp.series.iter().find(|s| &s.ticker == ticker) {
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

    // Determine structure
    let structure = if contracts.len() < 2 {
        "insufficient_data".to_string()
    } else {
        let back_price = contracts.last().unwrap().price;
        if back_price > front_price * 1.005 {
            "contango".to_string()
        } else if back_price < front_price * 0.995 {
            "backwardation".to_string()
        } else {
            "flat".to_string()
        }
    };

    let back_price = contracts.last().map(|c| c.price);
    let spread = back_price.map(|b| b - front_price);
    let spread_pct = back_price.map(|b| (b - front_price) / front_price * 100.0);

    let response = CurveResponse {
        commodity: spec.name.to_string(),
        unit: spec.unit.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        structure,
        front_month_price: Some(front_price),
        back_month_price: back_price,
        spread,
        spread_pct,
        contracts,
    };

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &response,
            "finance.curve",
            &[format!("commodity={}", query)],
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

    Ok(())
}
