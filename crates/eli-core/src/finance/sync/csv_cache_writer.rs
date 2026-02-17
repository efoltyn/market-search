use super::super::OddsListedMarket;
use std::path::{Path, PathBuf};

pub(crate) fn csv_escape_field(raw: &str) -> String {
    let needs_quote = raw.contains(',') || raw.contains('"') || raw.contains('\n');
    if !needs_quote {
        return raw.to_string();
    }
    let escaped = raw.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

pub(crate) fn write_markets_csv(
    markets: &[OddsListedMarket],
    source: &str,
    cache_dir: &Path,
) -> std::result::Result<PathBuf, String> {
    let path = cache_dir.join(format!("{source}_markets.csv"));
    let mut csv = String::new();
    csv.push_str(
        "source,ticker,title,event_ticker,yes_price,volume,status,probability,category,topic\n",
    );

    for m in markets {
        let prob = m
            .probability_yes
            .map(|p| format!("{:.4}", p))
            .unwrap_or_default();
        let price = m.yes_price.map(|p| p.to_string()).unwrap_or_default();
        let vol = m.volume.map(|v| v.to_string()).unwrap_or_default();
        let status = m.status.as_deref().unwrap_or("");
        let category = m
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Uncategorized");
        let topic = category;

        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            csv_escape_field(source),
            csv_escape_field(&m.ticker),
            csv_escape_field(&m.title),
            csv_escape_field(&m.event_ticker),
            price,
            vol,
            csv_escape_field(status),
            prob,
            csv_escape_field(category),
            csv_escape_field(topic),
        ));
    }

    std::fs::create_dir_all(cache_dir).map_err(|e| format!("Failed to create cache dir: {}", e))?;
    std::fs::write(&path, csv).map_err(|e| format!("Failed to write CSV: {}", e))?;

    Ok(path)
}

pub(crate) fn merge_markets_csv(
    source_paths: &[PathBuf],
    cache_dir: &Path,
) -> std::result::Result<PathBuf, String> {
    let merged_path = cache_dir.join("all_markets.csv");
    let mut merged = String::new();
    merged.push_str(
        "source,ticker,title,event_ticker,yes_price,volume,status,probability,category,topic\n",
    );

    for path in source_paths {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        for line in content.lines().skip(1) {
            if !line.trim().is_empty() {
                merged.push_str(line);
                merged.push('\n');
            }
        }
    }

    std::fs::write(&merged_path, merged)
        .map_err(|e| format!("Failed to write merged CSV: {}", e))?;

    Ok(merged_path)
}
