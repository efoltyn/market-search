fn build_ibkr_connection_config(
    account: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    client_id: Option<i32>,
    market_data_type: Option<i32>,
    timeout_secs: Option<u64>,
) -> eli_core::finance::IbkrConnectionConfig {
    eli_core::finance::IbkrConnectionConfig {
        account,
        host,
        port,
        client_id,
        market_data_type,
        timeout_secs,
    }
}

async fn cmd_finance_ibkr(args: FinanceIbkrArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let ibkr = build_ibkr_connection_config(
        args.account.clone(),
        args.host.clone(),
        args.port,
        args.client_id,
        args.market_data_type,
        args.timeout_secs,
    );

    let output =
        match args.command {
            FinanceIbkrCommandArg::Snapshot => {
                if args.tickers.is_empty() {
                    anyhow::bail!("--tickers is required for --command snapshot");
                }
                let req = eli_core::finance::SnapshotRequest {
                    tickers: args.tickers.clone(),
                    as_of: None,
                    provider: eli_core::finance::ProviderKind::Ibkr,
                    ibkr: Some(ibkr.clone()),
                };
                serde_json::to_value(eli_core::finance::fetch_snapshot(req).await?)?
            }
            FinanceIbkrCommandArg::Timeseries => {
                if args.tickers.is_empty() {
                    anyhow::bail!("--tickers is required for --command timeseries");
                }
                let range = eli_core::finance::Span::parse(&args.range)
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("parse --range")?;
                let granularity = eli_core::finance::Span::parse(&args.granularity)
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("parse --granularity")?;
                let cache_dir = default_finance_cache_dir()?;
                let req = eli_core::finance::TimeseriesRequest {
                    tickers: args.tickers.clone(),
                    range,
                    granularity,
                    as_of: None,
                    provider: eli_core::finance::ProviderKind::Ibkr,
                    max_points_per_ticker: None,
                    ibkr: Some(ibkr.clone()),
                };
                serde_json::to_value(eli_core::finance::fetch_timeseries(req, &cache_dir).await?)?
            }
            FinanceIbkrCommandArg::AccountSummary => {
                let value = eli_core::finance::invoke_ibkr_bridge(
                    "account_summary",
                    json!({
                        "account": args.account,
                        "tags": args.tags,
                    }),
                    Some(&ibkr),
                )
                .await?;
                value
            }
            FinanceIbkrCommandArg::Positions => {
                let value = eli_core::finance::invoke_ibkr_bridge(
                    "positions",
                    json!({
                        "account": args.account,
                    }),
                    Some(&ibkr),
                )
                .await?;
                value
            }
            FinanceIbkrCommandArg::Portfolio => {
                let value = eli_core::finance::invoke_ibkr_bridge(
                    "portfolio",
                    json!({
                        "account": args.account,
                    }),
                    Some(&ibkr),
                )
                .await?;
                value
            }
            FinanceIbkrCommandArg::OpenOrders => {
                let value =
                    eli_core::finance::invoke_ibkr_bridge("open_orders", json!({}), Some(&ibkr))
                        .await?;
                value
            }
            FinanceIbkrCommandArg::PlaceOrder => {
                let symbol = args.symbol.clone().ok_or_else(|| {
                    anyhow::anyhow!("--symbol is required for --command place-order")
                })?;
                let side = args.side.clone().ok_or_else(|| {
                    anyhow::anyhow!("--side is required for --command place-order")
                })?;
                let quantity = args.quantity.ok_or_else(|| {
                    anyhow::anyhow!("--quantity is required for --command place-order")
                })?;
                let value = eli_core::finance::invoke_ibkr_bridge(
                    "place_order",
                    json!({
                        "account": args.account,
                        "side": side,
                        "order_type": args.order_type.unwrap_or_else(|| "MKT".to_string()),
                        "quantity": quantity,
                        "limit_price": args.limit_price,
                        "stop_price": args.stop_price,
                        "tif": args.tif.unwrap_or_else(|| "DAY".to_string()),
                        "contract": {
                            "symbol": symbol,
                            "sec_type": args.sec_type.unwrap_or_else(|| "STK".to_string()),
                            "exchange": args.exchange.unwrap_or_else(|| "SMART".to_string()),
                            "primary_exchange": args.primary_exchange,
                            "currency": args.currency.unwrap_or_else(|| "USD".to_string()),
                            "expiry": args.expiry,
                            "strike": args.strike,
                            "right": args.right,
                            "multiplier": args.multiplier,
                            "trading_class": args.trading_class,
                        }
                    }),
                    Some(&ibkr),
                )
                .await?;
                value
            }
            FinanceIbkrCommandArg::CancelOrder => {
                let order_id = args.order_id.ok_or_else(|| {
                    anyhow::anyhow!("--order-id is required for --command cancel-order")
                })?;
                let value = eli_core::finance::invoke_ibkr_bridge(
                    "cancel_order",
                    json!({
                        "order_id": order_id,
                    }),
                    Some(&ibkr),
                )
                .await?;
                value
            }
        };

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &output,
            "finance.ibkr",
            &[format!("command={:?}", args.command)],
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

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
