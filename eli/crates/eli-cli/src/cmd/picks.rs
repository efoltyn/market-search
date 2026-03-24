// ── sidecar model ────────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct PickEntry {
    pub symbol: String,
    pub kind: String, // "ticker" | "odds"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_at_report: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prob_at_report: Option<f64>,
    pub logged_at: String,
    // Live fields — populated at query time, not persisted
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_now: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prob_now: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_pp: Option<f64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ReportPicks {
    pub report_file: String,
    pub logged_at: String,
    pub picks: Vec<PickEntry>,
}

// ── command dispatch ──────────────────────────────────────────────────────────

async fn cmd_picks(cmd: PicksCommand) -> Result<()> {
    match cmd {
        PicksCommand::Log(args) => cmd_picks_log(args).await,
    }
}

async fn cmd_picks_log(args: PicksLogArgs) -> Result<()> {
    let report_path = picks_expand_path(&args.report);
    let report_file = report_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let sidecar_path = {
        let name = report_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        report_path.with_file_name(format!("{}.picks.json", name))
    };

    // Load existing sidecar to merge into
    let mut existing: ReportPicks = if sidecar_path.exists() {
        let bytes = std::fs::read(&sidecar_path).context("read sidecar")?;
        serde_json::from_slice(&bytes).unwrap_or_else(|_| ReportPicks {
            report_file: report_file.clone(),
            logged_at: picks_now_iso8601(),
            picks: vec![],
        })
    } else {
        ReportPicks {
            report_file: report_file.clone(),
            logged_at: picks_now_iso8601(),
            picks: vec![],
        }
    };

    // Fetch current prices for tickers
    let tickers: Vec<String> = args
        .ticker
        .iter()
        .map(|t| t.trim().to_uppercase())
        .filter(|t| !t.is_empty())
        .collect();

    let snapshot_prices = if !tickers.is_empty() {
        picks_fetch_snapshot_prices(&tickers).await.unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    // Fetch current probabilities for markets
    let markets: Vec<String> = args
        .market
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();

    let mut market_probs: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for slug in &markets {
        if let Some(prob) = picks_fetch_odds_prob(slug).await {
            market_probs.insert(slug.clone(), prob);
        }
    }

    let now = picks_now_iso8601();

    // Upsert ticker picks
    for ticker in &tickers {
        let price = snapshot_prices.get(ticker).copied();
        if let Some(idx) = existing.picks.iter().position(|p| &p.symbol == ticker) {
            existing.picks[idx].price_at_report = price;
            existing.picks[idx].logged_at = now.clone();
        } else {
            existing.picks.push(PickEntry {
                symbol: ticker.clone(),
                kind: "ticker".into(),
                price_at_report: price,
                prob_at_report: None,
                logged_at: now.clone(),
                price_now: None,
                prob_now: None,
                delta_pct: None,
                delta_pp: None,
            });
        }
    }

    // Upsert market picks
    for slug in &markets {
        let prob = market_probs.get(slug).copied();
        if let Some(idx) = existing.picks.iter().position(|p| &p.symbol == slug) {
            existing.picks[idx].prob_at_report = prob;
            existing.picks[idx].logged_at = now.clone();
        } else {
            existing.picks.push(PickEntry {
                symbol: slug.clone(),
                kind: "odds".into(),
                price_at_report: None,
                prob_at_report: prob,
                logged_at: now.clone(),
                price_now: None,
                prob_now: None,
                delta_pct: None,
                delta_pp: None,
            });
        }
    }

    let json = serde_json::to_string_pretty(&existing).context("serialize picks")?;
    std::fs::write(&sidecar_path, json).context("write sidecar")?;

    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "report": report_file,
            "sidecar": sidecar_path.display().to_string(),
            "picks_logged": existing.picks.len()
        })
    );
    Ok(())
}

// ── helpers used by serve.rs too ─────────────────────────────────────────────

pub fn picks_expand_path(s: &str) -> PathBuf {
    if s.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(format!("{}{}", home, &s[1..]));
        }
    }
    PathBuf::from(s)
}

fn picks_now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub async fn picks_fetch_snapshot_prices(
    tickers: &[String],
) -> anyhow::Result<std::collections::HashMap<String, f64>> {
    let exe = std::env::current_exe()?;
    let output = tokio::process::Command::new(&exe)
        .args(["finance", "snapshot", "--ticker", &tickers.join(",")])
        .output()
        .await?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut map = std::collections::HashMap::new();

    // Current snapshot output shape: { "snapshots": [{ "ticker": "...", "current_price": ... }] }.
    // Keep a top-level-array fallback for compatibility with older payloads.
    let entries = json
        .get("snapshots")
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array());
    if let Some(arr) = entries {
        for snap in arr {
            if let Some(ticker) = snap.get("ticker").and_then(|v| v.as_str()) {
                let price = snap
                    .get("current_price")
                    .and_then(|v| v.as_f64())
                    .or_else(|| snap.get("price").and_then(|v| v.as_f64()));
                if let Some(price) = price {
                    map.insert(ticker.to_string(), price);
                }
            }
        }
    }
    Ok(map)
}

pub async fn picks_fetch_odds_prob(market: &str) -> Option<f64> {
    let exe = std::env::current_exe().ok()?;
    let output = tokio::process::Command::new(&exe)
        .args(["finance", "odds", "--market", market])
        .output()
        .await
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    if let Some(arr) = json["markets"].as_array() {
        if let Some(first) = arr.first() {
            return first["probability_yes"]
                .as_f64()
                .or_else(|| first["yes_price"].as_f64().map(|v| v / 100.0));
        }
    }
    json["probability_yes"]
        .as_f64()
        .or_else(|| json["yes_price"].as_f64().map(|v| v / 100.0))
}

pub async fn picks_load_with_refresh(
    sidecar_path: &std::path::Path,
    refresh: bool,
) -> Option<ReportPicks> {
    let bytes = tokio::fs::read(sidecar_path).await.ok()?;
    let mut picks: ReportPicks = serde_json::from_slice(&bytes).ok()?;

    if refresh {
        let tickers: Vec<String> = picks
            .picks
            .iter()
            .filter(|p| p.kind == "ticker")
            .map(|p| p.symbol.clone())
            .collect();

        let prices = if !tickers.is_empty() {
            picks_fetch_snapshot_prices(&tickers).await.unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };

        for pick in &mut picks.picks {
            if pick.kind == "ticker" {
                let cur = prices.get(&pick.symbol).copied();
                pick.price_now = cur;
                if let (Some(entry), Some(cur)) = (pick.price_at_report, cur) {
                    pick.delta_pct = Some((cur - entry) / entry * 100.0);
                }
            } else if pick.kind == "odds" {
                let cur = picks_fetch_odds_prob(&pick.symbol).await;
                pick.prob_now = cur;
                if let (Some(entry), Some(cur)) = (pick.prob_at_report, cur) {
                    pick.delta_pp = Some((cur - entry) * 100.0);
                }
            }
        }
    }

    Some(picks)
}
