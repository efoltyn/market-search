fn is_major_us_release_id(release_id: Option<u32>) -> bool {
    matches!(
        release_id,
        Some(
            10 |   // Consumer Price Index
            46 |   // Producer Price Index
            50 |   // Employment Situation (NFP)
            51 |   // International Trade in Goods and Services
            53 |   // Gross Domestic Product
            54 |   // Personal Income and Outlays (PCE)
            101 |  // FOMC Press Release
            180 |  // Unemployment Insurance Weekly Claims Report
            192    // Job Openings and Labor Turnover Survey
        )
    )
}

/// Returns true if the given date is an FOMC rate decision day (last day of
/// a scheduled 2-day meeting). FRED lists "FOMC Press Release" on many dates
/// for data refreshes — this filter keeps only actual meeting decisions.
/// Source: federalreserve.gov/monetarypolicy/fomccalendars.htm
fn is_fomc_decision_day(d: chrono::NaiveDate) -> bool {
    use chrono::NaiveDate;
    static FOMC_DECISION_DAYS: &[(i32, u32, u32)] = &[
        // Official Federal Reserve meeting schedules published through January 2028.
        // 2025
        (2025, 1, 29), (2025, 3, 19), (2025, 5, 7), (2025, 6, 18),
        (2025, 7, 30), (2025, 9, 17), (2025, 10, 29), (2025, 12, 10),
        // 2026
        (2026, 1, 28), (2026, 3, 18), (2026, 4, 29), (2026, 6, 17),
        (2026, 7, 29), (2026, 9, 16), (2026, 10, 28), (2026, 12, 9),
        // 2027
        (2027, 1, 27), (2027, 3, 17), (2027, 4, 28), (2027, 6, 9),
        (2027, 7, 28), (2027, 9, 15), (2027, 10, 27), (2027, 12, 8),
        // 2028 (only January date is currently published via the 2027 schedule release)
        (2028, 1, 26),
    ];
    FOMC_DECISION_DAYS.iter().any(|(y, m, day)| {
        NaiveDate::from_ymd_opt(*y, *m, *day).map_or(false, |meeting| meeting == d)
    })
}

fn is_major_us_macro_title(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    let include = [
        "consumer price index",
        "core pce",
        "personal consumption expenditures",
        "personal income and outlays",
        "employment situation",
        "nonfarm payroll",
        "unemployment insurance weekly claims report",
        "fomc",
        "federal open market committee",
        "gross domestic product",
        "advance monthly sales for retail and food services",
        "retail sales",
        "new residential construction",
        "housing starts",
        "producer price index",
        "job openings",
        "jolts",
        "job openings and labor turnover survey",
        "international trade",
        "corporate profits",
    ];
    let exclude = [
        "eurostat",
        "sonia",
        "coinbase",
        "ice bofa",
        "international financial statistics",
        "current median",
        "median cpi",
        "sticky",
        "research consumer price index",
        "consumer price index, australia",
        "consumer price index, japan",
        "monthly state retail sales",
        "state retail sales",
        "state unemployment insurance weekly claims report",
        "median consumer price index",
        "trimmed mean",
        "cleveland fed",
    ];
    include.iter().any(|k| t.contains(k)) && !exclude.iter().any(|k| t.contains(k))
}

fn is_low_signal_macro_title(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    let low_signal = [
        "coinbase cryptocurrencies",
        "ice bofa indices",
        "nasdaq daily index data",
        "sofr averages and index data",
        "secured overnight financing rate data",
        "overnight bank funding rate data",
        "temporary open market operations",
        "tri-party general collateral rate data",
        "standard & poors",
        "recession indicators series",
        "interest rate spreads",
        "key ecb interest rates",
        "historical overnight ameribor unsecured interest rate",
        "h.15 selected interest rates",
    ];
    low_signal.iter().any(|k| t.contains(k))
}

fn compact_schedule_warnings(warnings: Vec<String>) -> Vec<String> {
    if warnings.is_empty() {
        return warnings;
    }

    let mut fred_macro_timeouts = 0usize;
    let mut nasdaq_earnings_timeouts = 0usize;
    let mut fred_macro_403 = 0usize;
    let mut fred_macro_502 = 0usize;
    let mut other = Vec::new();

    for w in warnings {
        if w.starts_with("fred macro ") && w.ends_with("timed out") {
            fred_macro_timeouts += 1;
            continue;
        }
        if w.starts_with("fred macro ")
            && (w.contains("http 403 Forbidden") || w.contains("http 403"))
        {
            fred_macro_403 += 1;
            continue;
        }
        if w.starts_with("fred macro ")
            && (w.contains("http 502 Bad Gateway") || w.contains("http 502"))
        {
            fred_macro_502 += 1;
            continue;
        }
        if w.starts_with("nasdaq earnings ") && w.ends_with("timed out") {
            nasdaq_earnings_timeouts += 1;
            continue;
        }
        other.push(w);
    }

    // Dedupe non-timeout warnings while preserving first-seen order.
    let mut seen = BTreeSet::new();
    other.retain(|w| seen.insert(w.clone()));

    let mut out = Vec::new();
    if fred_macro_timeouts > 0 {
        out.push(format!(
            "fred macro day fetches timed out: {fred_macro_timeouts}"
        ));
    }
    if fred_macro_403 > 0 {
        out.push(format!(
            "fred macro day fetches returned 403: {fred_macro_403}"
        ));
    }
    if fred_macro_502 > 0 {
        out.push(format!(
            "fred macro day fetches returned 502: {fred_macro_502}"
        ));
    }
    if nasdaq_earnings_timeouts > 0 {
        out.push(format!(
            "nasdaq earnings day fetches timed out: {nasdaq_earnings_timeouts}"
        ));
    }
    out.extend(other);

    const MAX_WARNINGS: usize = 10;
    if out.len() > MAX_WARNINGS {
        let omitted = out.len() - MAX_WARNINGS;
        out.truncate(MAX_WARNINGS);
        out.push(format!("additional warnings omitted: {omitted}"));
    }
    out
}
