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

fn periods_per_year(granularity: Span) -> f64 {
    let n = granularity.n.max(1) as f64;
    match granularity.unit {
        SpanUnit::Minute => (252.0 * 24.0 * 60.0) / n,
        SpanUnit::Hour => (252.0 * 24.0) / n,
        SpanUnit::Day => 252.0 / n,
        SpanUnit::Week => 52.0 / n,
        SpanUnit::Month => 12.0 / n,
        SpanUnit::Year => 1.0 / n,
    }
}

fn default_risk_free_rate_annual() -> f64 {
    0.04
}

pub(crate) fn build_snapshot_analytics(snapshots: &[TickerSnapshot]) -> SnapshotAnalytics {
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

    // Detect when market is likely closed (all returns exactly 0.0)
    let market_note = daily_returns.as_ref().and_then(|dr| {
        if dr.len() >= 2 && dr.values().all(|r| *r == 0.0) {
            Some(
                "market may be closed — all daily returns are 0.0 (current_price == previous_close)"
                    .to_string(),
            )
        } else {
            None
        }
    });

    SnapshotAnalytics {
        market_caps,
        total_market_cap,
        market_cap_weights,
        daily_returns,
        relative_strength,
        market_note,
    }
}

fn build_timeseries_analytics(series: &[TickerSeries], granularity: Span) -> TimeseriesAnalytics {
    let mut dates: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    for s in series {
        for candle in &s.candles {
            dates.insert(candle.t);
        }
    }
    let aligned_dates: Vec<DateTime<Utc>> = dates.into_iter().collect();

    let mut aligned_returns: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    let mut stats: BTreeMap<String, TimeseriesStats> = BTreeMap::new();

    let per_year = periods_per_year(granularity);
    let rf_annual = default_risk_free_rate_annual();
    let rf_per_period = if per_year > 0.0 {
        rf_annual / per_year
    } else {
        0.0
    };

    for s in series {
        let mut price_map: HashMap<DateTime<Utc>, f64> = HashMap::new();
        for candle in &s.candles {
            price_map.insert(candle.t, candle.c);
        }
        let mut prices: Vec<Option<f64>> = Vec::with_capacity(aligned_dates.len());
        for d in &aligned_dates {
            prices.push(price_map.get(d).copied());
        }

        // Align by union of timestamps; forward-fill gaps once the series has started.
        let mut last: Option<f64> = None;
        for p in &mut prices {
            if p.is_some() {
                last = *p;
            } else if let Some(v) = last {
                *p = Some(v);
            }
        }

        let mut returns: Vec<Option<f64>> = Vec::with_capacity(prices.len());
        for i in 0..prices.len() {
            if i == 0 {
                returns.push(None);
                continue;
            }
            match (prices[i], prices[i - 1]) {
                (Some(curr), Some(prev)) if prev != 0.0 => returns.push(Some((curr / prev) - 1.0)),
                _ => returns.push(None),
            }
        }

        let first = prices.iter().find_map(|v| *v);
        let last = prices.iter().rev().find_map(|v| *v);
        let total_return = match (first, last) {
            (Some(f), Some(l)) if f != 0.0 => Some((l / f) - 1.0),
            _ => None,
        };

        let valid_returns: Vec<f64> = returns.iter().filter_map(|v| *v).collect();
        let (annualized_vol, sharpe_ratio) = if valid_returns.len() >= 2 {
            let mean = valid_returns.iter().sum::<f64>() / valid_returns.len() as f64;
            let mut var = 0.0;
            for r in &valid_returns {
                var += (*r - mean) * (*r - mean);
            }
            let denom = (valid_returns.len() as f64 - 1.0).max(1.0);
            let std = (var / denom).sqrt();
            if std > 0.0 {
                let ann_vol = std * per_year.sqrt();
                let sharpe = (mean - rf_per_period) * per_year.sqrt() / std;
                (Some(ann_vol), Some(sharpe))
            } else {
                (Some(0.0), None)
            }
        } else {
            (None, None)
        };

        aligned_returns.insert(s.ticker.clone(), returns);
        stats.insert(
            s.ticker.clone(),
            TimeseriesStats {
                total_return,
                annualized_vol,
                sharpe_ratio,
                relative_strength: None,
            },
        );
    }

    // Relative strength: outperformance vs the mean total return of the request.
    let mean_total_return = {
        let vals: Vec<f64> = stats.values().filter_map(|s| s.total_return).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    };
    if let Some(mean) = mean_total_return {
        for s in stats.values_mut() {
            if let Some(tr) = s.total_return {
                s.relative_strength = Some(tr - mean);
            }
        }
    }

    let mut correlation_matrix: BTreeMap<String, BTreeMap<String, Option<f64>>> = BTreeMap::new();
    let tickers: Vec<String> = aligned_returns.keys().cloned().collect();
    for t1 in &tickers {
        let mut row: BTreeMap<String, Option<f64>> = BTreeMap::new();
        for t2 in &tickers {
            let r1 = aligned_returns.get(t1).cloned().unwrap_or_default();
            let r2 = aligned_returns.get(t2).cloned().unwrap_or_default();
            let mut xs: Vec<f64> = Vec::new();
            let mut ys: Vec<f64> = Vec::new();
            let n = r1.len().min(r2.len());
            for i in 0..n {
                if let (Some(a), Some(b)) = (r1[i], r2[i]) {
                    xs.push(a);
                    ys.push(b);
                }
            }
            row.insert(t2.clone(), correlation(&xs, &ys));
        }
        correlation_matrix.insert(t1.clone(), row);
    }

    TimeseriesAnalytics {
        stats,
        correlation_matrix,
        periods_per_year: per_year,
        risk_free_rate_annual: rf_annual,
    }
}

fn correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 2 || ys.len() < 2 || xs.len() != ys.len() {
        return None;
    }
    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    let mean_y = ys.iter().sum::<f64>() / ys.len() as f64;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    if var_x == 0.0 || var_y == 0.0 {
        return None;
    }
    Some(cov / (var_x.sqrt() * var_y.sqrt()))
}

