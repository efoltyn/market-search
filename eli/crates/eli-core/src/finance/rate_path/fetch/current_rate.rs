fn latest_series_point(
    series: &[TickerSeries],
    ticker: &str,
) -> Option<(f64, DateTime<Utc>)> {
    series
        .iter()
        .find(|row| row.ticker == ticker)
        .and_then(|row| row.candles.last())
        .map(|candle| (candle.c, candle.t))
}

#[derive(Debug, Deserialize)]
struct NyFedEffrResponse {
    #[serde(rename = "refRates")]
    ref_rates: Vec<NyFedEffrPoint>,
}

#[derive(Debug, Clone, Deserialize)]
struct NyFedEffrPoint {
    #[serde(rename = "effectiveDate")]
    effective_date: String,
    #[serde(rename = "percentRate")]
    percent_rate: f64,
    #[serde(rename = "targetRateFrom")]
    target_rate_from: Option<f64>,
    #[serde(rename = "targetRateTo")]
    target_rate_to: Option<f64>,
}

fn nyfed_point_as_of(point: &NyFedEffrPoint) -> Result<DateTime<Utc>> {
    let date = chrono::NaiveDate::parse_from_str(&point.effective_date, "%Y-%m-%d")
        .map_err(|e| Error::Provider(format!("ny fed effr invalid effectiveDate: {e}")))?;
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| Error::Provider("ny fed effr invalid effective date".to_string()))?;
    Ok(Utc.from_utc_datetime(&naive))
}

async fn fetch_latest_nyfed_effr_point() -> Result<(NyFedEffrPoint, DateTime<Utc>)> {
    let end = Utc::now().date_naive();
    let start = end - chrono::Duration::days(90);
    let url = format!(
        "https://markets.newyorkfed.org/api/rates/unsecured/effr/search.json?startDate={start}&endDate={end}&type=rate"
    );
    let resp = super::super::shared_client::GENERAL
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("ny fed effr fetch failed: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("ny fed effr read failed: {e}")))?;
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "ny fed effr fetch failed: http {status}"
        )));
    }
    let parsed: NyFedEffrResponse = serde_json::from_str(&body)
        .map_err(|e| Error::Provider(format!("ny fed effr parse failed: {e}")))?;
    let latest = parsed
        .ref_rates
        .into_iter()
        .max_by(|a, b| a.effective_date.cmp(&b.effective_date))
        .ok_or_else(|| Error::Provider("ny fed effr returned no observations".to_string()))?;
    let as_of = nyfed_point_as_of(&latest)?;
    Ok((latest, as_of))
}

fn assemble_current_fed_rates(
    target_lower: Option<(f64, DateTime<Utc>)>,
    target_upper: Option<(f64, DateTime<Utc>)>,
    effective_daily: Option<(f64, DateTime<Utc>)>,
    effective_monthly: Option<(f64, DateTime<Utc>)>,
) -> Result<RatePathCurrentRates> {
    let (classification_anchor_rate, classification_anchor_basis, target_midpoint, target_range_as_of) =
        match (target_lower, target_upper) {
            (Some((lower, lower_as_of)), Some((upper, upper_as_of))) => {
                let as_of = std::cmp::max(lower_as_of, upper_as_of);
                (
                    (lower + upper) / 2.0,
                    "target_midpoint".to_string(),
                    Some((lower + upper) / 2.0),
                    Some(as_of),
                )
            }
            _ => match effective_daily {
                Some((rate, _as_of)) => (rate, "effective_rate".to_string(), None, None),
                None => match effective_monthly {
                    Some((rate, as_of)) => (
                        rate,
                        "monthly_average_effective_rate".to_string(),
                        None,
                        Some(as_of),
                    ),
                    None => {
                        return Err(Error::Provider(
                            "no current federal funds target/effective rate observations available"
                                .to_string(),
                        ))
                    }
                },
            },
        };

    Ok(RatePathCurrentRates {
        classification_anchor_rate,
        classification_anchor_basis,
        target_lower_bound: target_lower.map(|(value, _)| value),
        target_upper_bound: target_upper.map(|(value, _)| value),
        target_midpoint,
        target_range_as_of,
        effective_rate: effective_daily.map(|(value, _)| value),
        effective_rate_as_of: effective_daily.map(|(_, as_of)| as_of),
        monthly_average_effective_rate: effective_monthly.map(|(value, _)| value),
        monthly_average_effective_rate_as_of: effective_monthly.map(|(_, as_of)| as_of),
    })
}

async fn fetch_current_fed_rates() -> Result<RatePathCurrentRates> {
    let end = Utc::now();
    let monthly_start = end - chrono::Duration::days(800);

    let (monthly_series, _monthly_errors) = fetch_fred_series(
        &["FEDFUNDS".to_string()],
        monthly_start,
        end,
        Span {
            n: 1,
            unit: SpanUnit::Month,
        },
    )
    .await?;

    let effective_monthly = latest_series_point(&monthly_series, "FEDFUNDS");

    if let Ok((latest, as_of)) = fetch_latest_nyfed_effr_point().await {
        return assemble_current_fed_rates(
            latest.target_rate_from.map(|value| (value, as_of)),
            latest.target_rate_to.map(|value| (value, as_of)),
            Some((latest.percent_rate, as_of)),
            effective_monthly,
        );
    }

    let daily_start = end - chrono::Duration::days(90);
    let (daily_series, _daily_errors) = fetch_fred_series(
        &[
            "DFF".to_string(),
            "DFEDTARL".to_string(),
            "DFEDTARU".to_string(),
        ],
        daily_start,
        end,
        Span {
            n: 1,
            unit: SpanUnit::Day,
        },
    )
    .await?;

    assemble_current_fed_rates(
        latest_series_point(&daily_series, "DFEDTARL"),
        latest_series_point(&daily_series, "DFEDTARU"),
        latest_series_point(&daily_series, "DFF"),
        effective_monthly,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_rates_prefer_target_midpoint_when_range_exists() {
        let as_of = Utc
            .with_ymd_and_hms(2026, 3, 19, 0, 0, 0)
            .single()
            .unwrap();
        let rates = assemble_current_fed_rates(
            Some((3.50, as_of)),
            Some((3.75, as_of)),
            Some((3.64, as_of)),
            Some((3.61, as_of)),
        )
        .unwrap();

        assert_eq!(rates.classification_anchor_basis, "target_midpoint");
        assert_eq!(rates.classification_anchor_rate, 3.625);
        assert_eq!(rates.target_midpoint, Some(3.625));
        assert_eq!(rates.effective_rate, Some(3.64));
    }

    #[test]
    fn current_rates_fall_back_to_effective_when_target_range_missing() {
        let as_of = Utc
            .with_ymd_and_hms(2026, 3, 19, 0, 0, 0)
            .single()
            .unwrap();
        let rates =
            assemble_current_fed_rates(None, None, Some((3.64, as_of)), Some((3.61, as_of)))
                .unwrap();

        assert_eq!(rates.classification_anchor_basis, "effective_rate");
        assert_eq!(rates.classification_anchor_rate, 3.64);
        assert_eq!(rates.target_midpoint, None);
        assert_eq!(rates.monthly_average_effective_rate, Some(3.61));
    }
}
