use chrono::Datelike as _;

struct OfficialMajorReleaseSpec {
    pdf_label: &'static str,
    title: &'static str,
    time: &'static str,
    release_id: Option<u32>,
    release_url: &'static str,
}

const OFFICIAL_MAJOR_RELEASES: &[OfficialMajorReleaseSpec] = &[
    OfficialMajorReleaseSpec {
        pdf_label: "The Employment Situation",
        title: "The Employment Situation",
        time: "08:30 ET",
        release_id: None,
        release_url: "https://www.bls.gov/schedule/",
    },
    OfficialMajorReleaseSpec {
        pdf_label: "Producer Price Indexes",
        title: "Producer Price Indexes",
        time: "08:30 ET",
        release_id: Some(46),
        release_url: "https://www.bls.gov/ppi/",
    },
    OfficialMajorReleaseSpec {
        pdf_label: "Consumer Price Index",
        title: "Consumer Price Index",
        time: "08:30 ET",
        release_id: Some(10),
        release_url: "https://www.bls.gov/cpi/",
    },
    OfficialMajorReleaseSpec {
        pdf_label: "Personal Income and Outlays",
        title: "Personal Income and Outlays",
        time: "08:30 ET",
        release_id: None,
        release_url: "https://www.bea.gov/news/schedule",
    },
    OfficialMajorReleaseSpec {
        pdf_label: "Gross Domestic Product",
        title: "Gross Domestic Product",
        time: "08:30 ET",
        release_id: Some(53),
        release_url: "https://www.bea.gov/news/schedule",
    },
    OfficialMajorReleaseSpec {
        pdf_label: "Advance Monthly Sales for Retail",
        title: "Advance Monthly Sales for Retail and Food Services",
        time: "08:30 ET",
        release_id: None,
        release_url: "https://www.census.gov/economic-indicators/calendar-listview.html",
    },
    OfficialMajorReleaseSpec {
        pdf_label: "New Residential Construction",
        title: "New Residential Construction",
        time: "08:30 ET",
        release_id: None,
        release_url: "https://www.census.gov/economic-indicators/calendar-listview.html",
    },
];

fn principal_indicators_pdf_url(year: i32) -> String {
    format!(
        "https://www.census.gov/economic-indicators/econcards/assets/pdf/censusreleaseglance_{year}.pdf"
    )
}

async fn fetch_official_major_macro(
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
) -> Result<(Vec<MacroScheduleEvent>, Vec<MacroScheduleDay>, Vec<String>)> {
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    let years: BTreeSet<i32> = (start_date.year()..=end_date.year()).collect();

    for year in years {
        match fetch_principal_indicators_layout_text(year).await {
            Ok(text) => {
                let (mut parsed, mut parse_warnings) =
                    build_official_major_events_from_text(year, &text, start_date, end_date);
                events.append(&mut parsed);
                warnings.append(&mut parse_warnings);
            }
            Err(err) => warnings.push(format!(
                "official major schedule {year}: {err}"
            )),
        }
    }

    for year in start_date.year()..=end_date.year() {
        events.extend(build_fomc_schedule_events(year, start_date, end_date));
    }

    events.sort_by(|a, b| a.date.cmp(&b.date).then(a.title.cmp(&b.title)));
    events.dedup_by(|a, b| a.date == b.date && a.title == b.title);

    let mut macro_days_map = BTreeMap::new();
    for event in &events {
        *macro_days_map.entry(event.date.clone()).or_insert(0usize) += 1;
    }
    let macro_days = macro_days_map
        .into_iter()
        .map(|(date, release_count)| MacroScheduleDay { date, release_count })
        .collect::<Vec<_>>();

    if events.is_empty() && !warnings.is_empty() {
        return Err(Error::Provider(warnings.join("; ")));
    }

    Ok((events, macro_days, warnings))
}

async fn fetch_principal_indicators_layout_text(year: i32) -> Result<String> {
    let url = principal_indicators_pdf_url(year);
    let resp = super::super::shared_client::GENERAL
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("principal indicators pdf fetch failed: {e}")))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Provider(format!("principal indicators pdf read failed: {e}")))?;
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "principal indicators pdf fetch failed: http {status}"
        )));
    }
    pdf_bytes_to_layout_text(bytes.to_vec()).await
}

async fn pdf_bytes_to_layout_text(pdf_bytes: Vec<u8>) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new("pdftotext")
            .args(["-layout", "-", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Provider(format!("pdftotext launch failed: {e}")))?;

        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::Provider("pdftotext stdin unavailable".to_string()))?;
            use std::io::Write as _;
            stdin
                .write_all(&pdf_bytes)
                .map_err(|e| Error::Provider(format!("pdftotext stdin write failed: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Provider(format!("pdftotext wait failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(Error::Provider(format!(
                "pdftotext failed: {}",
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|e| Error::Provider(format!("pdftotext utf8 decode failed: {e}")))
    })
    .await
    .map_err(|e| Error::Provider(format!("pdftotext task failed: {e}")))?
}

fn build_official_major_events_from_text(
    year: i32,
    text: &str,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
) -> (Vec<MacroScheduleEvent>, Vec<String>) {
    let mut events = Vec::new();
    let mut warnings = Vec::new();

    for spec in OFFICIAL_MAJOR_RELEASES {
        let Some(days) = extract_pdf_row_days(text, spec.pdf_label) else {
            warnings.push(format!(
                "official major schedule parser could not find '{}'",
                spec.title
            ));
            continue;
        };

        for (month_idx, maybe_day) in days.iter().enumerate() {
            let Some(day) = maybe_day else {
                continue;
            };
            let month = (month_idx + 1) as u32;
            let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, *day) else {
                warnings.push(format!(
                    "official major schedule parser produced invalid date for {} {year}-{month:02}-{day:02}",
                    spec.title
                ));
                continue;
            };
            if date < start_date || date > end_date {
                continue;
            }
            events.push(MacroScheduleEvent {
                date: date.to_string(),
                time: Some(spec.time.to_string()),
                title: spec.title.to_string(),
                release_id: spec.release_id,
                release_url: Some(spec.release_url.to_string()),
                source: "official".to_string(),
            });
        }
    }

    (events, warnings)
}

fn extract_pdf_row_days(text: &str, label: &str) -> Option<[Option<u32>; 12]> {
    const LOOKAHEAD_LINES: usize = 8;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|line| line.contains(label))?;
    let mut out = [None; 12];
    let mut idx = 0usize;

    for line in lines.iter().skip(start).take(LOOKAHEAD_LINES) {
        for token in line.split_whitespace() {
            if token == "--" {
                if idx >= out.len() {
                    break;
                }
                out[idx] = None;
                idx += 1;
                continue;
            }
            if token.chars().all(|c| c.is_ascii_digit()) {
                if idx >= out.len() {
                    break;
                }
                let Ok(day) = token.parse::<u32>() else {
                    continue;
                };
                if (1..=31).contains(&day) {
                    out[idx] = Some(day);
                    idx += 1;
                }
            }
        }
        if idx >= out.len() {
            return Some(out);
        }
    }

    None
}

fn build_fomc_schedule_events(
    year: i32,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
) -> Vec<MacroScheduleEvent> {
    let mut events = Vec::new();
    for month in 1..=12 {
        for day in 1..=31 {
            let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) else {
                continue;
            };
            if date < start_date || date > end_date || !is_fomc_decision_day(date) {
                continue;
            }
            events.push(MacroScheduleEvent {
                date: date.to_string(),
                time: Some("14:00 ET".to_string()),
                title: "FOMC Press Release".to_string(),
                release_id: Some(101),
                release_url: Some(
                    "https://www.federalreserve.gov/monetarypolicy/fomccalendars.htm"
                        .to_string(),
                ),
                source: "official".to_string(),
            });
        }
    }
    events
}

#[cfg(test)]
mod official_major_tests {
    use super::*;

    const SAMPLE_TEXT: &str = r#"
DEPT                   AGENCY/INDICATORS                                                     JAN       FEB       MAR         APR       MAY         JUN         JUL       AUG           SEP         OCT       NOV         DEC
     BUREAU OF LABOR STATISTICS
                The Employment Situation                                                     9         6          6          3          8           5          2          7             4          2          6           4
            .Producer Price Indexes                                                          14        12         12         14         13          11         15         13            10         15         13          15
                Consumer Price Index
                                                        (Data are for previous month)
                                                                                             13
                                                                                                   l   11         11
                                                                                                                         I 10           12          10         14         12            11         14         10          10
                   Personal Income and Outlays                                              I    29          26         I    27          30              28          25          30          26           30    I    29              25     I    23
               .Gross Domestic Product                                                           29          26              27          30              28          25          30          26          30          29              25          23
                .Advance Monthly Sales for Retail(Data are for second month previous)
                                                  and Food Services                            15
                                                                                                      I
                                                                                                        17             16
                                                                                                                              l    16         14         17          16         14             16          15         17         16
                .New Residential Construction (Data are for second month previous)             21         18          17          17         19         16          17         18             17          20         18         17
"#;

    #[test]
    fn extracts_pdf_rows_for_single_and_multiline_releases() {
        let jobs = extract_pdf_row_days(SAMPLE_TEXT, "The Employment Situation").unwrap();
        let cpi = extract_pdf_row_days(SAMPLE_TEXT, "Consumer Price Index").unwrap();
        let retail = extract_pdf_row_days(SAMPLE_TEXT, "Advance Monthly Sales for Retail").unwrap();

        assert_eq!(jobs[0], Some(9));
        assert_eq!(jobs[2], Some(6));
        assert_eq!(jobs[11], Some(4));

        assert_eq!(cpi[0], Some(13));
        assert_eq!(cpi[5], Some(10));
        assert_eq!(cpi[11], Some(10));

        assert_eq!(retail[0], Some(15));
        assert_eq!(retail[5], Some(17));
        assert_eq!(retail[11], Some(16));
    }

    #[test]
    fn builds_filtered_events_and_injects_fomc_dates() {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let (mut events, warnings) =
            build_official_major_events_from_text(2026, SAMPLE_TEXT, start, end);
        events.extend(build_fomc_schedule_events(2026, start, end));
        events.sort_by(|a, b| a.title.cmp(&b.title));

        assert!(warnings.is_empty());
        assert!(events.iter().any(|e| e.title == "Consumer Price Index" && e.date == "2026-03-11"));
        assert!(events.iter().any(|e| e.title == "The Employment Situation" && e.date == "2026-03-06"));
        assert!(events.iter().any(|e| e.title == "FOMC Press Release" && e.date == "2026-03-18"));
    }
}
