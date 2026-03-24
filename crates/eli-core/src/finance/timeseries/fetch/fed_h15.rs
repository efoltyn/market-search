/// Fetch Treasury curve data directly from the Federal Reserve H.15 CSV endpoint.
/// This bypasses FRED for the standard DGS* Treasury series while preserving the
/// normal Eli timeseries response contract.
pub(crate) async fn fetch_h15_yield_curve(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let mut url = reqwest::Url::parse("https://www.federalreserve.gov/datadownload/Output.aspx")
        .map_err(|e| Error::Provider(format!("H.15 url build failed: {e}")))?;
    url.query_pairs_mut()
        .append_pair("rel", "H15")
        .append_pair("series", "bf17364827e38702b42a58cf8eaa3f78")
        .append_pair("from", &start.date_naive().format("%Y-%m-%d").to_string())
        .append_pair("to", &end.date_naive().format("%Y-%m-%d").to_string())
        .append_pair("filetype", "csv")
        .append_pair("label", "include")
        .append_pair("layout", "seriescolumn");

    let client = &*crate::finance::shared_client::GENERAL;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("H.15 fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "H.15 fetch failed: HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("H.15 read failed: {e}")))?;

    let parsed = parse_h15_csv(&body)?;
    let step = granularity.approx_duration();
    let mut requested = Vec::with_capacity(tickers.len());
    for ticker in tickers {
        let canonical = ticker.trim().to_ascii_uppercase();
        let Some(idx) = h15_ticker_index(&canonical) else {
            return Err(Error::InvalidInput(format!(
                "H.15 does not support ticker '{ticker}'"
            )));
        };
        requested.push((ticker.clone(), idx));
    }

    let mut out = Vec::with_capacity(requested.len());
    let mut errors = Vec::new();
    for (requested_ticker, idx) in requested {
        let Some(source_series) = parsed.get(idx) else {
            errors.push(TimeseriesError {
                ticker: requested_ticker,
                stage: Some("parse".to_string()),
                message: "H.15 series missing from response".to_string(),
            });
            continue;
        };
        let filtered: Vec<Candle> = source_series
            .candles
            .iter()
            .filter(|c| c.t >= start && c.t <= end)
            .cloned()
            .collect();
        let candles = resample_candles(&filtered, start, step);
        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: requested_ticker,
                stage: Some("parse".to_string()),
                message: "H.15 returned no data points in the requested range".to_string(),
            });
            continue;
        }
        out.push(TickerSeries {
            ticker: requested_ticker,
            candles,
        });
    }

    Ok((out, errors))
}

pub(crate) fn is_h15_yield_curve_request(tickers: &[String]) -> bool {
    !tickers.is_empty()
        && tickers
            .iter()
            .all(|ticker| h15_ticker_index(&ticker.trim().to_ascii_uppercase()).is_some())
}

/// Column order in the H.15 CSV (after the date column):
/// 1mo, 3mo, 6mo, 1yr, 2yr, 3yr, 5yr, 7yr, 10yr, 20yr, 30yr
const H15_TICKERS: &[&str] = &[
    "DGS1MO", "DGS3MO", "DGS6MO", "DGS1", "DGS2", "DGS3",
    "DGS5", "DGS7", "DGS10", "DGS20", "DGS30",
];

fn h15_ticker_index(ticker: &str) -> Option<usize> {
    H15_TICKERS.iter().position(|candidate| *candidate == ticker)
}

fn parse_h15_csv(body: &str) -> Result<Vec<TickerSeries>> {
    let mut series_map: Vec<Vec<Candle>> = (0..H15_TICKERS.len()).map(|_| Vec::new()).collect();

    for line in body.lines() {
        // Data rows start with a date like "2026-03-18"
        if !line.starts_with("20") {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 12 {
            continue;
        }

        let date_str = fields[0].trim();
        let date = match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        let t = date
            .and_hms_opt(0, 0, 0)
            .map(|dt| Utc.from_utc_datetime(&dt))
            .unwrap_or_else(|| Utc::now());

        for (i, field) in fields[1..].iter().enumerate().take(H15_TICKERS.len()) {
            let val: f64 = match field.trim().parse() {
                Ok(v) => v,
                Err(_) => continue, // "ND" or empty = no data for this tenor on this date
            };
            series_map[i].push(Candle {
                t,
                o: val,
                h: val,
                l: val,
                c: val,
                v: None,
            });
        }
    }

    let mut out = Vec::with_capacity(H15_TICKERS.len());
    for (i, ticker_name) in H15_TICKERS.iter().enumerate() {
        if series_map[i].is_empty() {
            continue;
        }
        out.push(TickerSeries {
            ticker: ticker_name.to_string(),
            candles: std::mem::take(&mut series_map[i]),
        });
    }

    Ok(out)
}

#[cfg(test)]
mod fed_h15_tests {
    use super::*;

    #[test]
    fn parses_h15_csv_and_skips_nd_values() {
        let csv = concat!(
            "\"Time Period\",\"RIFLGFCM01_N.B\",\"RIFLGFCM03_N.B\",\"RIFLGFCM06_N.B\",\"RIFLGFCY01_N.B\",\"RIFLGFCY02_N.B\",\"RIFLGFCY03_N.B\",\"RIFLGFCY05_N.B\",\"RIFLGFCY07_N.B\",\"RIFLGFCY10_N.B\",\"RIFLGFCY20_N.B\",\"RIFLGFCY30_N.B\"\n",
            "2025-01-01,ND,ND,ND,ND,ND,ND,ND,ND,ND,ND,ND\n",
            "2025-01-02,4.45,4.36,4.25,4.17,4.25,4.29,4.38,4.47,4.57,4.86,4.79\n",
            "2025-01-03,4.44,4.34,4.25,4.18,4.28,4.32,4.41,4.51,4.60,4.88,4.82\n"
        );
        let parsed = parse_h15_csv(csv).expect("parse h15 csv");
        assert_eq!(parsed.len(), 11);
        assert_eq!(parsed[0].ticker, "DGS1MO");
        assert_eq!(parsed[0].candles.len(), 2);
        assert_eq!(parsed[0].candles[0].c, 4.45);
        assert_eq!(parsed[1].ticker, "DGS3MO");
        assert_eq!(parsed[1].candles.len(), 2);
        assert_eq!(parsed[1].candles[1].c, 4.34);
    }

    #[test]
    fn h15_request_match_is_case_insensitive() {
        assert!(is_h15_yield_curve_request(&["DGS10".to_string(), "dgs2".to_string()]));
        assert!(!is_h15_yield_curve_request(&["DGS10".to_string(), "UNRATE".to_string()]));
    }
}
