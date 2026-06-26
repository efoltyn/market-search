#[derive(Clone, Copy)]
struct FredApiReleaseSpec {
    release_id: u32,
    title: &'static str,
    time_et: &'static str,
}

fn curated_fred_api_release_specs(
    macro_profile: ScheduleMacroProfile,
) -> &'static [FredApiReleaseSpec] {
    const MARKET: &[FredApiReleaseSpec] = &[
        FredApiReleaseSpec {
            release_id: 180,
            title: "Unemployment Insurance Weekly Claims Report",
            time_et: "08:30 ET",
        },
        FredApiReleaseSpec {
            release_id: 9,
            title: "Advance Monthly Sales for Retail and Food Services",
            time_et: "08:30 ET",
        },
        FredApiReleaseSpec {
            release_id: 13,
            title: "Industrial Production and Capacity Utilization",
            time_et: "09:15 ET",
        },
        FredApiReleaseSpec {
            release_id: 192,
            title: "Job Openings and Labor Turnover Survey",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 229,
            title: "Construction Spending",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 291,
            title: "Existing Home Sales",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 97,
            title: "New Residential Sales",
            time_et: "10:00 ET",
        },
    ];
    const BROAD: &[FredApiReleaseSpec] = &[
        FredApiReleaseSpec {
            release_id: 180,
            title: "Unemployment Insurance Weekly Claims Report",
            time_et: "08:30 ET",
        },
        FredApiReleaseSpec {
            release_id: 9,
            title: "Advance Monthly Sales for Retail and Food Services",
            time_et: "08:30 ET",
        },
        FredApiReleaseSpec {
            release_id: 13,
            title: "Industrial Production and Capacity Utilization",
            time_et: "09:15 ET",
        },
        FredApiReleaseSpec {
            release_id: 192,
            title: "Job Openings and Labor Turnover Survey",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 229,
            title: "Construction Spending",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 291,
            title: "Existing Home Sales",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 97,
            title: "New Residential Sales",
            time_et: "10:00 ET",
        },
        FredApiReleaseSpec {
            release_id: 194,
            title: "ADP National Employment Report",
            time_et: "08:15 ET",
        },
    ];
    // Major: Claims (180) and JOLTS (192) are the "true major" labor releases NOT in
    // the Census PDF parsed by fetch_official_major_macro (which covers CPI/PPI/PCE/
    // GDP/Retail/Housing/NFP). Without this list, --major silently drops them — both
    // are top-tier indicators (Claims = weekly labor pulse; JOLTS = Fed's preferred
    // job-market gauge per Powell).
    const MAJOR: &[FredApiReleaseSpec] = &[
        FredApiReleaseSpec {
            release_id: 180,
            title: "Unemployment Insurance Weekly Claims Report",
            time_et: "08:30 ET",
        },
        FredApiReleaseSpec {
            release_id: 192,
            title: "Job Openings and Labor Turnover Survey",
            time_et: "10:00 ET",
        },
    ];
    match macro_profile {
        ScheduleMacroProfile::Broad => BROAD,
        ScheduleMacroProfile::Market => MARKET,
        ScheduleMacroProfile::Major => MAJOR,
    }
}

pub(crate) async fn fetch_fred_macro_api_events(
    client: &reqwest::Client,
    start_date: NaiveDate,
    end_date: NaiveDate,
    macro_profile: ScheduleMacroProfile,
) -> Result<Vec<MacroScheduleEvent>> {
    #[derive(Deserialize)]
    struct FredReleaseDatesResp {
        #[serde(default)]
        release_dates: Vec<FredReleaseDateRow>,
    }

    #[derive(Deserialize)]
    struct FredReleaseDateRow {
        date: String,
    }

    let api_key = crate::finance::credentials::resolve_fred_api_key().map_err(Error::Provider)?;
    let specs = curated_fred_api_release_specs(macro_profile);
    if specs.is_empty() {
        return Ok(Vec::new());
    }

    let start_s = start_date.format("%Y-%m-%d").to_string();
    let end_s = end_date.format("%Y-%m-%d").to_string();

    let futs = specs.iter().map(|spec| {
        let client = client.clone();
        let api_key = api_key.clone();
        let start_s = start_s.clone();
        let end_s = end_s.clone();
        async move {
            let mut url = reqwest::Url::parse("https://api.stlouisfed.org/fred/release/dates")
                .map_err(|e| Error::Provider(format!("fred api url build failed: {e}")))?;
            url.query_pairs_mut()
                .append_pair("api_key", &api_key)
                .append_pair("file_type", "json")
                .append_pair("release_id", &spec.release_id.to_string())
                .append_pair("realtime_start", &start_s)
                .append_pair("realtime_end", &end_s)
                .append_pair("include_release_dates_with_no_data", "true")
                .append_pair("sort_order", "asc")
                .append_pair("limit", "100");

            // Retry on 429 / 5xx with exponential backoff. FRED's release-dates
            // endpoint rate-limits aggressively when several release ids are
            // requested at once; a single transient 429 must not abort the rest.
            let mut body = String::new();
            let mut ok = false;
            let mut last_err: Option<String> = None;
            for attempt in 0..4u32 {
                let resp = match client.get(url.clone()).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        last_err =
                            Some(format!("fred api release dates fetch failed: {e}"));
                        if attempt < 3 {
                            sleep(TokioDuration::from_millis(
                                600u64.saturating_mul(1u64 << attempt),
                            ))
                            .await;
                            continue;
                        }
                        break;
                    }
                };
                let status = resp.status();
                let text = resp.text().await.map_err(|e| {
                    Error::Provider(format!("fred api release dates read failed: {e}"))
                })?;
                let looks_rate_limited =
                    text.to_ascii_lowercase().contains("too many requests");
                let retryable = status.as_u16() == 429 || status.as_u16() >= 500;
                if !status.is_success() && (looks_rate_limited || retryable) {
                    last_err = Some(format!(
                        "fred api release dates fetch failed for rid {}: http {}",
                        spec.release_id, status
                    ));
                    if attempt < 3 {
                        sleep(TokioDuration::from_millis(
                            600u64.saturating_mul(1u64 << attempt),
                        ))
                        .await;
                        continue;
                    }
                    break;
                }
                if !status.is_success() {
                    return Err(Error::Provider(format!(
                        "fred api release dates fetch failed for rid {}: http {}",
                        spec.release_id, status
                    )));
                }
                body = text;
                ok = true;
                break;
            }
            if !ok {
                return Err(Error::Provider(last_err.unwrap_or_else(|| {
                    format!(
                        "fred api release dates fetch failed for rid {}",
                        spec.release_id
                    )
                })));
            }
            let parsed: FredReleaseDatesResp = serde_json::from_str(&body).map_err(|e| {
                Error::Provider(format!(
                    "fred api release dates parse failed for rid {}: {e}",
                    spec.release_id
                ))
            })?;

            Ok::<Vec<MacroScheduleEvent>, Error>(
                parsed
                    .release_dates
                    .into_iter()
                    .map(|row| MacroScheduleEvent {
                        date: row.date,
                        time: Some(spec.time_et.to_string()),
                        title: spec.title.to_string(),
                        release_id: Some(spec.release_id),
                        release_url: Some(format!(
                            "https://fred.stlouisfed.org/release?rid={}",
                            spec.release_id
                        )),
                        source: "fred_api".to_string(),
                    })
                    .collect(),
            )
        }
    });

    let mut out = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    let mut successes = 0usize;
    for result in futures::future::join_all(futs).await {
        match result {
            Ok(events) => {
                successes += 1;
                out.extend(events);
            }
            Err(e) => errs.push(e.to_string()),
        }
    }
    // Resilient aggregation: a single release failing (e.g. a 429 that
    // survived retries) drops only that release, not the whole calendar.
    // An empty calendar is a valid answer when releases simply have no dates
    // in the requested window, so only hard-fail when EVERY release errored.
    if successes == 0 && !errs.is_empty() {
        return Err(Error::Provider(format!(
            "fred api release dates: all {} release(s) failed: {}",
            errs.len(),
            errs.join("; ")
        )));
    }
    Ok(out)
}
