use super::super::timeseries::fetch::fetch_fred_series;
use super::super::*;
use futures::stream::{self, StreamExt};
use tokio::time::{sleep, Duration as TokioDuration};

const CURVE_SERIES: [(&str, &str, u32); 11] = [
    ("DGS1MO", "1mo", 1),
    ("DGS3MO", "3mo", 3),
    ("DGS6MO", "6mo", 6),
    ("DGS1", "1y", 12),
    ("DGS2", "2y", 24),
    ("DGS3", "3y", 36),
    ("DGS5", "5y", 60),
    ("DGS7", "7y", 84),
    ("DGS10", "10y", 120),
    ("DGS20", "20y", 240),
    ("DGS30", "30y", 360),
];

fn round_f64(value: f64, decimals: i32) -> f64 {
    let factor = 10f64.powi(decimals);
    (value * factor).round() / factor
}

fn anchor_value_at_or_before(candles: &[Candle], target: DateTime<Utc>) -> Option<f64> {
    candles.iter().rev().find(|c| c.t <= target).map(|c| c.c)
}

async fn fetch_curve_series_with_retry(
    symbol: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Option<Vec<Candle>> {
    for attempt in 0..3 {
        let tickers = vec![symbol.to_string()];
        if let Ok((mut series, _errors)) =
            fetch_fred_series(&tickers, start, end, granularity).await
        {
            if let Some(s) = series.pop() {
                if !s.candles.is_empty() {
                    return Some(s.candles);
                }
            }
        }
        if attempt < 2 {
            sleep(TokioDuration::from_millis(200 * (attempt + 1) as u64)).await;
        }
    }
    None
}

pub async fn fetch_yield_curve(req: YieldCurveRequest) -> Result<YieldCurveResponse> {
    let now = Utc::now();
    let end = now;
    let lookback_days = if req.compare_1y {
        800
    } else if req.compare_3mo {
        260
    } else {
        90
    };
    let start = end - chrono::Duration::days(lookback_days);

    let granularity = Span {
        n: 1,
        unit: SpanUnit::Day,
    };
    let fetched: Vec<(String, Option<Vec<Candle>>)> =
        stream::iter(CURVE_SERIES.iter().map(|(symbol, _, _)| {
            let symbol = (*symbol).to_string();
            async move {
                let candles = fetch_curve_series_with_retry(&symbol, start, end, granularity).await;
                (symbol, candles)
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut series_map: BTreeMap<String, Vec<Candle>> = BTreeMap::new();
    for (symbol, candles) in fetched {
        if let Some(candles) = candles {
            series_map.insert(symbol, candles);
        }
    }

    // Retry mandatory spread symbols over a wider horizon if needed.
    for symbol in ["DGS10", "DGS2", "DGS3MO"] {
        if series_map.contains_key(symbol) {
            continue;
        }
        let wide_start = end - chrono::Duration::days(3650);
        if let Some(candles) =
            fetch_curve_series_with_retry(symbol, wide_start, end, granularity).await
        {
            series_map.insert(symbol.to_string(), candles);
        }
    }

    let mut curve = Vec::new();
    let mut latest_by_symbol: BTreeMap<String, f64> = BTreeMap::new();
    let mut missing_symbols: Vec<String> = Vec::new();

    for (symbol, maturity, months) in CURVE_SERIES {
        let Some(candles) = series_map.get(symbol) else {
            missing_symbols.push(symbol.to_string());
            continue;
        };
        let Some(latest) = candles.last() else {
            missing_symbols.push(symbol.to_string());
            continue;
        };
        latest_by_symbol.insert(symbol.to_string(), latest.c);

        let target_3mo = latest.t - chrono::Duration::days(90);
        let target_1y = latest.t - chrono::Duration::days(365);
        let change_3mo_bps = if req.compare_3mo {
            anchor_value_at_or_before(candles, target_3mo)
                .map(|v| round_f64((latest.c - v) * 100.0, 2))
        } else {
            None
        };
        let change_1y_bps = if req.compare_1y {
            anchor_value_at_or_before(candles, target_1y)
                .map(|v| round_f64((latest.c - v) * 100.0, 2))
        } else {
            None
        };

        curve.push(YieldCurvePoint {
            maturity: maturity.to_string(),
            maturity_months: months,
            current_yield: round_f64(latest.c, 4),
            change_3mo_bps,
            change_1y_bps,
        });
    }

    curve.sort_by_key(|p| p.maturity_months);
    if req.strict && !missing_symbols.is_empty() {
        return Err(Error::Provider(format!(
            "strict mode: missing yield symbols: {}",
            missing_symbols.join(",")
        )));
    }

    let spread_2y10y = latest_by_symbol
        .get("DGS10")
        .copied()
        .zip(latest_by_symbol.get("DGS2").copied())
        .map(|(y10, y2)| round_f64(y10 - y2, 4));
    let spread_3mo10y = latest_by_symbol
        .get("DGS10")
        .copied()
        .zip(latest_by_symbol.get("DGS3MO").copied())
        .map(|(y10, y3m)| round_f64(y10 - y3m, 4));

    let as_of = curve
        .iter()
        .filter_map(|p| {
            let symbol = match p.maturity.as_str() {
                "1mo" => "DGS1MO",
                "3mo" => "DGS3MO",
                "6mo" => "DGS6MO",
                "1y" => "DGS1",
                "2y" => "DGS2",
                "3y" => "DGS3",
                "5y" => "DGS5",
                "7y" => "DGS7",
                "10y" => "DGS10",
                "20y" => "DGS20",
                "30y" => "DGS30",
                _ => return None,
            };
            series_map.get(symbol).and_then(|c| c.last()).map(|c| c.t)
        })
        .max()
        .unwrap_or(now);
    let age_seconds = (now - as_of).num_seconds().max(0);
    let coverage_ratio = (curve.len() as f64 / CURVE_SERIES.len() as f64).clamp(0.0, 1.0);
    let confidence = (0.6 + (0.35 * coverage_ratio)).clamp(0.0, 0.99);

    Ok(YieldCurveResponse {
        generated_at: now,
        as_of,
        age_seconds,
        curve,
        spread_2y10y,
        spread_3mo10y,
        coverage_ratio,
        confidence,
        missing_symbols,
    })
}
