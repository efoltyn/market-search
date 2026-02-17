use super::super::{
    OddsListedEvent, OddsListedMarket, OddsSyncAnalysis, OddsSyncCategorySummary,
    OddsSyncMarketSummary, OddsSyncProbabilityBucket, OddsSyncSourceAnalytics,
};
use std::collections::{HashMap, HashSet};

pub(crate) struct SyncAnalysisInput {
    pub(crate) source: String,
    pub(crate) events: Vec<OddsListedEvent>,
    pub(crate) markets: Vec<OddsListedMarket>,
}

pub(crate) fn build_sync_source_analytics(markets: &[OddsListedMarket]) -> OddsSyncSourceAnalytics {
    let markets_with_probability = markets
        .iter()
        .filter(|m| m.probability_yes.is_some())
        .count();
    let markets_with_volume = markets.iter().filter(|m| m.volume.is_some()).count();
    let total_volume: i64 = markets.iter().filter_map(|m| m.volume).sum();

    let mut by_category: HashMap<String, (usize, i64)> = HashMap::new();
    for market in markets {
        let category = market
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Uncategorized")
            .to_string();
        let entry = by_category.entry(category).or_insert((0usize, 0i64));
        entry.0 += 1;
        entry.1 += market.volume.unwrap_or(0);
    }

    let mut top_categories: Vec<OddsSyncCategorySummary> = by_category
        .into_iter()
        .map(
            |(category, (markets, volume_sum))| OddsSyncCategorySummary {
                category,
                markets,
                volume_sum,
            },
        )
        .collect();
    top_categories.sort_by(|a, b| {
        b.volume_sum
            .cmp(&a.volume_sum)
            .then_with(|| b.markets.cmp(&a.markets))
            .then_with(|| a.category.cmp(&b.category))
    });
    top_categories.truncate(8);

    OddsSyncSourceAnalytics {
        markets_with_probability,
        markets_with_volume,
        total_volume,
        top_categories,
    }
}

pub(crate) fn build_sync_analysis(inputs: &[SyncAnalysisInput]) -> OddsSyncAnalysis {
    let mut combined_markets: Vec<(String, &OddsListedMarket)> = Vec::new();
    let mut all_events_by_title: HashMap<String, HashSet<String>> = HashMap::new();

    for input in inputs {
        for market in &input.markets {
            combined_markets.push((input.source.clone(), market));
        }
        for event in &input.events {
            let title_key = event.title.trim().to_ascii_lowercase();
            if title_key.is_empty() {
                continue;
            }
            all_events_by_title
                .entry(title_key)
                .or_default()
                .insert(input.source.clone());
        }
    }

    let markets_with_probability = combined_markets
        .iter()
        .filter(|(_, market)| market.probability_yes.is_some())
        .count();
    let markets_with_volume = combined_markets
        .iter()
        .filter(|(_, market)| market.volume.is_some())
        .count();
    let total_volume: i64 = combined_markets
        .iter()
        .map(|(_, market)| market.volume.unwrap_or(0))
        .sum();

    let mut bucket_counts = vec![0usize; 5];
    let mut bucket_volume = vec![0i64; 5];
    let mut zero_yes_with_volume_count = 0usize;
    let mut zero_yes_with_1k_volume_count = 0usize;
    let mut near_even_with_1k_volume_count = 0usize;
    let mut high_confidence_with_10k_volume_count = 0usize;
    let mut extreme_prob_with_1k_volume_count = 0usize;
    let mut informative_prob_with_1k_volume_count = 0usize;
    let mut extreme_prob_volume_sum: i64 = 0;
    let mut informative_prob_volume_sum: i64 = 0;
    let mut top_markets_by_volume: Vec<OddsSyncMarketSummary> = Vec::new();
    let mut top_markets_by_informative_volume: Vec<OddsSyncMarketSummary> = Vec::new();
    let mut anomalous_zero_yes_markets: Vec<OddsSyncMarketSummary> = Vec::new();
    let mut near_even_high_volume_markets: Vec<OddsSyncMarketSummary> = Vec::new();
    let mut high_confidence_high_volume_markets: Vec<OddsSyncMarketSummary> = Vec::new();
    let mut category_rollup: HashMap<String, (usize, i64)> = HashMap::new();

    for (source, market) in &combined_markets {
        let prob = market.probability_yes;
        let volume = market.volume.unwrap_or(0);
        if let Some(p) = prob {
            let bucket_idx = ((p * 100.0).floor() as i32 / 20).clamp(0, 4) as usize;
            bucket_counts[bucket_idx] += 1;
            bucket_volume[bucket_idx] += volume;

            if (p - 0.0).abs() < f64::EPSILON && volume > 0 {
                zero_yes_with_volume_count += 1;
                if volume >= 1_000 {
                    zero_yes_with_1k_volume_count += 1;
                }
            }
            if (0.45..=0.55).contains(&p) && volume >= 1_000 {
                near_even_with_1k_volume_count += 1;
            }
            if (p <= 0.10 || p >= 0.90) && volume >= 10_000 {
                high_confidence_with_10k_volume_count += 1;
            }
            if (p <= 0.05 || p >= 0.95) && volume >= 1_000 {
                extreme_prob_with_1k_volume_count += 1;
            }
            if (0.05..=0.95).contains(&p) && volume >= 1_000 {
                informative_prob_with_1k_volume_count += 1;
            }
            if p <= 0.05 || p >= 0.95 {
                extreme_prob_volume_sum += volume;
            } else {
                informative_prob_volume_sum += volume;
            }
        }

        let category = market
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Uncategorized")
            .to_string();
        let entry = category_rollup.entry(category).or_insert((0usize, 0i64));
        entry.0 += 1;
        entry.1 += volume;

        let summary = OddsSyncMarketSummary {
            source: source.clone(),
            ticker: market.ticker.clone(),
            title: market.title.clone(),
            event_ticker: market.event_ticker.clone(),
            probability_yes: market.probability_yes,
            yes_price: market.yes_price,
            volume: market.volume,
            category: market.category.clone(),
        };
        top_markets_by_volume.push(summary.clone());
        if let Some(p) = market.probability_yes {
            if (0.05..=0.95).contains(&p) {
                top_markets_by_informative_volume.push(summary.clone());
            }
            if (p - 0.0).abs() < f64::EPSILON && volume >= 1_000 {
                anomalous_zero_yes_markets.push(summary.clone());
            }
            if (0.45..=0.55).contains(&p) && volume >= 1_000 {
                near_even_high_volume_markets.push(summary.clone());
            }
            if (p <= 0.10 || p >= 0.90) && volume >= 10_000 {
                high_confidence_high_volume_markets.push(summary.clone());
            }
        }
    }

    top_markets_by_volume.sort_by(|a, b| b.volume.unwrap_or(0).cmp(&a.volume.unwrap_or(0)));
    top_markets_by_volume.truncate(10);
    top_markets_by_informative_volume
        .sort_by(|a, b| b.volume.unwrap_or(0).cmp(&a.volume.unwrap_or(0)));
    top_markets_by_informative_volume.truncate(10);
    anomalous_zero_yes_markets.sort_by(|a, b| b.volume.unwrap_or(0).cmp(&a.volume.unwrap_or(0)));
    anomalous_zero_yes_markets.truncate(10);
    near_even_high_volume_markets.sort_by(|a, b| b.volume.unwrap_or(0).cmp(&a.volume.unwrap_or(0)));
    near_even_high_volume_markets.truncate(10);
    high_confidence_high_volume_markets
        .sort_by(|a, b| b.volume.unwrap_or(0).cmp(&a.volume.unwrap_or(0)));
    high_confidence_high_volume_markets.truncate(10);

    let mut top_categories: Vec<OddsSyncCategorySummary> = category_rollup
        .into_iter()
        .map(
            |(category, (markets, volume_sum))| OddsSyncCategorySummary {
                category,
                markets,
                volume_sum,
            },
        )
        .collect();
    top_categories.sort_by(|a, b| {
        b.volume_sum
            .cmp(&a.volume_sum)
            .then_with(|| b.markets.cmp(&a.markets))
            .then_with(|| a.category.cmp(&b.category))
    });
    top_categories.truncate(12);

    let probability_buckets = vec![
        ("0-20", 0usize),
        ("20-40", 1usize),
        ("40-60", 2usize),
        ("60-80", 3usize),
        ("80-100", 4usize),
    ]
    .into_iter()
    .map(|(range, idx)| OddsSyncProbabilityBucket {
        range: range.to_string(),
        markets: bucket_counts[idx],
        volume_sum: bucket_volume[idx],
    })
    .collect::<Vec<_>>();

    let cross_source_event_overlap_by_title = all_events_by_title
        .values()
        .filter(|sources| sources.len() > 1)
        .count();

    let denom = extreme_prob_volume_sum + informative_prob_volume_sum;
    let extreme_prob_volume_share_pct = if denom > 0 {
        (extreme_prob_volume_sum as f64 / denom as f64) * 100.0
    } else {
        0.0
    };

    OddsSyncAnalysis {
        markets_with_probability,
        markets_with_volume,
        total_volume,
        probability_buckets,
        zero_yes_with_volume_count,
        zero_yes_with_1k_volume_count,
        near_even_with_1k_volume_count,
        high_confidence_with_10k_volume_count,
        extreme_prob_with_1k_volume_count,
        informative_prob_with_1k_volume_count,
        extreme_prob_volume_sum,
        informative_prob_volume_sum,
        extreme_prob_volume_share_pct,
        cross_source_event_overlap_by_title,
        top_categories,
        top_markets_by_volume,
        top_markets_by_informative_volume,
        anomalous_zero_yes_markets,
        near_even_high_volume_markets,
        high_confidence_high_volume_markets,
    }
}
