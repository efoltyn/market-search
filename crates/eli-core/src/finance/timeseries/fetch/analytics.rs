fn granularity_suggestion(seconds_per_step: i64) -> String {
    let sec = seconds_per_step.max(60);
    let minute = 60;
    let hour = 60 * minute;
    let day = 24 * hour;
    let week = 7 * day;
    let month = 30 * day;
    let year = 365 * day;

    if sec >= year {
        format!("{}y", ((sec + year - 1) / year).max(1))
    } else if sec >= month {
        format!("{}mo", ((sec + month - 1) / month).max(1))
    } else if sec >= week {
        format!("{}w", ((sec + week - 1) / week).max(1))
    } else if sec >= day {
        format!("{}d", ((sec + day - 1) / day).max(1))
    } else if sec >= hour {
        format!("{}h", ((sec + hour - 1) / hour).max(1))
    } else {
        format!("{}min", ((sec + minute - 1) / minute).max(1))
    }
}


pub fn build_snapshot_analytics(snapshots: &[TickerSnapshot]) -> SnapshotAnalytics {
    let mut market_caps: BTreeMap<String, u64> = BTreeMap::new();
    for snap in snapshots {
        if let Some(cap) = snap.market_cap {
            market_caps.insert(snap.ticker.clone(), cap);
        }
    }
    let total_market_cap = if market_caps.is_empty() {
        None
    } else {
        Some(market_caps.values().sum())
    };
    let mut market_cap_weights: BTreeMap<String, f64> = BTreeMap::new();
    if let Some(total) = total_market_cap {
        if total > 0 {
            let denom = total as f64;
            for (ticker, cap) in &market_caps {
                market_cap_weights.insert(ticker.clone(), (*cap as f64) / denom);
            }
        }
    }

    let mut daily_returns: BTreeMap<String, f64> = BTreeMap::new();
    for snap in snapshots {
        if let (Some(curr), Some(prev)) = (snap.current_price, snap.previous_close) {
            if prev != 0.0 {
                daily_returns.insert(snap.ticker.clone(), (curr / prev) - 1.0);
            }
        }
    }
    let daily_returns = if daily_returns.is_empty() {
        None
    } else {
        Some(daily_returns)
    };

    let relative_strength = daily_returns.as_ref().and_then(|dr| {
        if dr.is_empty() {
            return None;
        }
        let mean = dr.values().sum::<f64>() / dr.len() as f64;
        let mut rs: BTreeMap<String, f64> = BTreeMap::new();
        for (ticker, r) in dr {
            rs.insert(ticker.clone(), *r - mean);
        }
        Some(rs)
    });

    SnapshotAnalytics {
        market_caps,
        total_market_cap,
        market_cap_weights,
        daily_returns,
        relative_strength,
    }
}

pub fn build_timeseries_analytics(series: &[TickerSeries], _granularity: Span) -> TimeseriesAnalytics {
    let mut stats: BTreeMap<String, TimeseriesStats> = BTreeMap::new();

    for s in series {
        let prices: Vec<f64> = s.candles.iter().map(|c| c.c).collect();

        let first = prices.first().copied();
        let last = prices.last().copied();
        let last_at = s.candles.last().map(|c| c.t);
        let total_return = match (first, last) {
            (Some(f), Some(l)) if f != 0.0 => Some((l / f) - 1.0),
            _ => None,
        };
        // Position of the last close inside the window's close range
        // (0 = window low, 1 = window high) — the relative read ("near its
        // 1y high") that otherwise gets recomputed by hand from raw candles
        // on every multi-series pull.
        let range_position = match last {
            Some(l) if !prices.is_empty() => {
                let lo = prices.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = prices.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                if hi > lo {
                    Some((l - lo) / (hi - lo))
                } else {
                    None
                }
            }
            _ => None,
        };

        stats.insert(
            s.ticker.clone(),
            TimeseriesStats {
                total_return,
                last,
                last_at,
                range_position,
            },
        );
    }

    TimeseriesAnalytics {
        stats,
    }
}


