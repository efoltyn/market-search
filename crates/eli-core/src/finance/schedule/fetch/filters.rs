fn is_major_us_macro_title(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    let include = [
        "consumer price index",
        "cpi",
        "core pce",
        "personal consumption expenditures",
        "employment situation",
        "nonfarm payroll",
        "unemployment insurance weekly claims report",
        "state unemployment insurance weekly claims report",
        "fomc",
        "federal open market committee",
        "gdp",
        "gross domestic product",
        "retail sales",
        "producer price index",
        "ppi",
        "job openings",
        "jolts",
    ];
    let exclude = [
        "eurostat",
        "sonia",
        "coinbase",
        "ice bofa",
        "international financial statistics",
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
