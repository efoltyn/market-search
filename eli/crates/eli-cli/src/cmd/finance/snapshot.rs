async fn cmd_finance_snapshot(args: FinanceSnapshotArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    let return_windows = parse_snapshot_return_windows(&args.returns)?;

    let mut tickers = args.tickers;
    if let Some(path) = args.tickers_file {
        let raw = std::fs::read_to_string(&path).context("read tickers_file")?;
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            tickers.push(t.to_string());
        }
    }

    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "mock" => eli_core::finance::ProviderKind::Mock,
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: mock, yahoo)"),
    };

    let req = eli_core::finance::SnapshotRequest { tickers, provider };
    let mut resp = eli_core::finance::fetch_snapshot(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch snapshot")?;

    if !return_windows.is_empty()
        && matches!(resp.provider, eli_core::finance::ProviderKind::Yahoo)
        && !resp.tickers.is_empty()
    {
        let longest = return_windows
            .iter()
            .max_by_key(|(_, span)| span.approx_duration().num_seconds())
            .map(|(_, span)| *span)
            .unwrap_or(eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Month,
            });
        let fetch_range = padded_snapshot_return_fetch_range(longest);

        let cache_dir = default_finance_cache_dir()?;
        let ts_req = eli_core::finance::TimeseriesRequest {
            tickers: resp.tickers.clone(),
            range: fetch_range,
            granularity: eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Day,
            },
            as_of: Some(resp.generated_at),
            provider: eli_core::finance::ProviderKind::Yahoo,
            max_points_per_ticker: None,
        };
        if let Ok(ts_resp) = eli_core::finance::fetch_timeseries(ts_req, &cache_dir).await {
            let mut trailing: std::collections::BTreeMap<String, std::collections::BTreeMap<String, f64>> =
                std::collections::BTreeMap::new();
            for series in ts_resp.series {
                let Some(latest) = series.candles.last() else {
                    continue;
                };
                if latest.c <= 0.0 {
                    continue;
                }
                let mut per_period = std::collections::BTreeMap::new();
                for (label, span) in &return_windows {
                    let target = latest.t - span.approx_duration();
                    if let Some(anchor) = series.candles.iter().rev().find(|c| c.t <= target) {
                        if anchor.c > 0.0 {
                            per_period.insert(label.clone(), (latest.c / anchor.c) - 1.0);
                        }
                    }
                }
                if !per_period.is_empty() {
                    trailing.insert(series.ticker.clone(), per_period);
                }
            }
            if !trailing.is_empty() {
                resp.trailing_returns = Some(trailing);
            }
        }
    }

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.snapshot",
            &[format!("provider={}", args.provider)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn padded_snapshot_return_fetch_range(longest: eli_core::finance::Span) -> eli_core::finance::Span {
    match longest.unit {
        eli_core::finance::SpanUnit::Year => eli_core::finance::Span {
            // Add one month of padding so 1y anchors can land on a prior trading day.
            n: longest.n.saturating_mul(12).saturating_add(1),
            unit: eli_core::finance::SpanUnit::Month,
        },
        eli_core::finance::SpanUnit::Month => eli_core::finance::Span {
            // One month of lookback padding is enough for month-based trailing windows.
            n: longest.n.saturating_add(1),
            unit: eli_core::finance::SpanUnit::Month,
        },
        _ => longest,
    }
}

fn parse_snapshot_return_windows(
    raw_windows: &[String],
) -> Result<Vec<(String, eli_core::finance::Span)>> {
    let mut out: Vec<(String, eli_core::finance::Span)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for w in raw_windows {
        let normalized = w.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if !seen.insert(normalized.clone()) {
            continue;
        }
        let span = match normalized.as_str() {
            "1mo" => eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "3mo" => eli_core::finance::Span {
                n: 3,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "6mo" => eli_core::finance::Span {
                n: 6,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "1y" => eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Year,
            },
            other => {
                anyhow::bail!(
                    "invalid --returns window '{other}' (supported: 1mo,3mo,6mo,1y)"
                )
            }
        };
        out.push((normalized, span));
    }
    Ok(out)
}

