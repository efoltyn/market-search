/// ETF creations/redemptions provider (prefix `ETF:`). No API key.
///
/// Ticker grammar: `ETF:<TICKER>[:<FIELD>]` or `ETF:<FAMILY>:<TICKER>:<FIELD>`
///   FIELD  = SO | NAV | AUM | FLOW | FLOW5 | FLOW20   (default FLOW)
///   FAMILY = SSGA | PROSHARES | ISHARES | DIREXION    (default: auto, tried in that order)
/// Examples: `ETF:JNK:FLOW`, `ETF:SPY:SO`, `ETF:TQQQ:FLOW5`, `ETF:HYG:AUM`,
/// `ETF:DIREXION:SOXL:SO`.
///
/// Units: SO = shares; NAV = $ per share; AUM = total net assets in $mn;
/// FLOW = (SO_t - SO_{t-1}) x NAV_t in $mn (creations positive, redemptions
/// negative); FLOW5 / FLOW20 = rolling sums of FLOW over the last 5 / 20
/// observations (a value only appears once the full window is available).
/// A day whose SO and NAV both jump by the same (>= 1.8x) ratio is treated as a
/// share split and gets no FLOW value rather than a fake multi-billion flow.
///
/// Sources (family auto-detection walks this list; the first hit wins):
/// - SSGA/SPDR (full daily history since inception, ~5,000 rows):
///   https://www.ssga.com/library-content/products/fund-data/etfs/us/navhist-us-en-<t>.xlsx
///   (the /us/en/intermediary/ URL 301-redirects here). Columns Date ("29-Oct-2026"),
///   NAV, Shares Outstanding, Total Net Assets, newest first. Unknown tickers 404.
///   Works for SPY, JNK, SJNK, SPSB, SPIB, SPLB, XLK, XLE, XLF, XLU, GLD, ...
/// - ProShares (full daily history since inception):
///   https://accounts.profunds.com/etfdata/ByFund/<T>-historical_nav.csv
///   Columns Date (MM/DD/YYYY), NAV, Shares Outstanding (000), Assets Under Management.
///   History is split-adjusted. TQQQ, SQQQ, UVXY, SSO, SDS, ...
/// - iShares: SNAPSHOT ONLY. The product-screener JSON lists every US iShares fund
///   with navAmount / totalNetAssetsFund and their as-of dates; SO is derived as
///   TNA / NAV rounded to 100 shares (matches the product page's "Shares
///   Outstanding" 228,000,000 for HYG on 2026-10-29). One observation per call, so FLOW needs accumulated
///   history. The per-fund `...1467271812596.ajax?fileType=xls|csv&dataType=fund`
///   download (which holds the Historical sheet) returns the HTML product page to
///   non-browser clients even with cookies + siteEntryPassthrough=true, so no
///   no-key history path was found.
/// - Direxion: SNAPSHOT ONLY. https://www.direxion.com/holdings/<T>.csv carries a
///   single "Shares Outstanding:" line (no NAV, no history) and holdings rows with a
///   TradeDate; the SO is stamped at that TradeDate. History must be accumulated by
///   polling. No historical NAV/shares file was found (…/<T>-history.csv 404s).
///
/// Timestamps: each observation is stamped 00:00 UTC of its as-of date (the NAV
/// strike date, 4pm ET). Issuers post the file that evening / next morning, the
/// same lag as a Yahoo daily close stamped at 00:00 of the session date.
/// Granularity: native daily; the requested granularity is not resampled because
/// flows must be summed, not last-valued.

const ETF_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36";
const ETF_SSGA_BASE: &str =
    "https://www.ssga.com/library-content/products/fund-data/etfs/us/navhist-us-en-";
const ETF_PROSHARES_BASE: &str = "https://accounts.profunds.com/etfdata/ByFund/";
const ETF_ISHARES_SCREENER: &str = "https://www.ishares.com/us/product-screener/product-screener-v3.1.jsn?dcrPath=/templatedata/config/product-screener-v3/data/en/us-ishares/ishares-product-screener-backend-config&siteEntryPassthrough=true";
const ETF_DIREXION_BASE: &str = "https://www.direxion.com/holdings/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EtfField {
    So,
    Nav,
    Aum,
    Flow,
    Flow5,
    Flow20,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EtfFamily {
    Ssga,
    ProShares,
    IShares,
    Direxion,
}

impl EtfFamily {
    fn name(self) -> &'static str {
        match self {
            EtfFamily::Ssga => "ssga",
            EtfFamily::ProShares => "proshares",
            EtfFamily::IShares => "ishares",
            EtfFamily::Direxion => "direxion",
        }
    }
    fn parse(s: &str) -> Option<EtfFamily> {
        match s.to_ascii_uppercase().as_str() {
            "SSGA" | "SPDR" => Some(EtfFamily::Ssga),
            "PROSHARES" | "PROFUNDS" => Some(EtfFamily::ProShares),
            "ISHARES" | "BLACKROCK" => Some(EtfFamily::IShares),
            "DIREXION" => Some(EtfFamily::Direxion),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct EtfTickerSpec {
    symbol: String,
    field: EtfField,
    family: Option<EtfFamily>,
}

#[derive(Clone, Debug, PartialEq)]
struct EtfObs {
    date: NaiveDate,
    nav: Option<f64>,
    so: Option<f64>,
    aum: Option<f64>,
}

/// Returns true if the ticker should route to the ETF creations/redemptions provider.
pub fn is_etf_flow_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    ticker_has_prefix(t, "ETF:")
}

fn etf_parse_field(s: &str) -> Option<EtfField> {
    match s.to_ascii_uppercase().as_str() {
        "SO" | "SHARES" => Some(EtfField::So),
        "NAV" => Some(EtfField::Nav),
        "AUM" | "TNA" => Some(EtfField::Aum),
        "FLOW" => Some(EtfField::Flow),
        "FLOW5" => Some(EtfField::Flow5),
        "FLOW20" => Some(EtfField::Flow20),
        _ => None,
    }
}

fn etf_parse_ticker(ticker: &str) -> std::result::Result<EtfTickerSpec, String> {
    let t = ticker.trim();
    if !is_etf_flow_ticker(t) {
        return Err(format!("'{t}' is not an ETF: ticker"));
    }
    let parts: Vec<&str> = t[4..].split(':').map(|p| p.trim()).collect();
    let bad = || {
        format!(
            "bad ETF ticker '{t}': use ETF:<TICKER>:<FIELD> (FIELD = SO|NAV|AUM|FLOW|FLOW5|FLOW20), e.g. ETF:JNK:FLOW, or ETF:<FAMILY>:<TICKER>:<FIELD> with FAMILY = SSGA|PROSHARES|ISHARES|DIREXION"
        )
    };
    let (family, symbol, field) = match parts.as_slice() {
        [sym] => (None, *sym, EtfField::Flow),
        [sym, f] => (None, *sym, etf_parse_field(f).ok_or_else(bad)?),
        [fam, sym, f] => (
            Some(EtfFamily::parse(fam).ok_or_else(bad)?),
            *sym,
            etf_parse_field(f).ok_or_else(bad)?,
        ),
        _ => return Err(bad()),
    };
    if symbol.is_empty() || !symbol.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        return Err(bad());
    }
    Ok(EtfTickerSpec {
        symbol: symbol.to_ascii_uppercase(),
        field,
        family,
    })
}

/// Lenient numeric parse: "1,234.5", "6.5074605E7", "$77.37", "".
fn etf_num(s: &str) -> Option<f64> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| !matches!(c, ',' | '$' | '"'))
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn etf_cell_f64(c: &calamine::Data) -> Option<f64> {
    use calamine::{Data, DataType};
    match c {
        Data::Float(f) => Some(*f),
        Data::Int(i) => Some(*i as f64),
        Data::String(s) => etf_num(s),
        other => other.as_f64(),
    }
}

fn etf_cell_str(c: &calamine::Data) -> String {
    use calamine::Data;
    match c {
        Data::String(s) => s.trim().to_string(),
        Data::Empty => String::new(),
        other => other.to_string().trim().to_string(),
    }
}

/// Parse an SSGA navhist xlsx (header rows, then Date/NAV/Shares Outstanding/
/// Total Net Assets, newest first). Returns observations oldest first.
fn etf_parse_ssga_xlsx(bytes: &[u8]) -> std::result::Result<Vec<EtfObs>, String> {
    use calamine::{Reader, Xlsx};
    let mut wb: Xlsx<_> = Xlsx::new(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|e| format!("ssga xlsx open failed: {e}"))?;
    let range = wb
        .worksheet_range_at(0)
        .ok_or_else(|| "ssga xlsx has no sheets".to_string())?
        .map_err(|e| format!("ssga xlsx sheet read failed: {e}"))?;

    let mut cols: Option<(usize, usize, usize, usize)> = None;
    let mut out = Vec::new();
    for row in range.rows() {
        let Some((di, ni, si, ai)) = cols else {
            let names: Vec<String> = row.iter().map(|c| etf_cell_str(c).to_ascii_lowercase()).collect();
            if names.first().map(|s| s == "date").unwrap_or(false) {
                let find = |needle: &str| names.iter().position(|n| n.contains(needle));
                if let (Some(n), Some(s), Some(a)) =
                    (find("nav"), find("shares outstanding"), find("total net assets"))
                {
                    cols = Some((0, n, s, a));
                }
            }
            continue;
        };
        let get = |i: usize| row.get(i);
        let date_s = get(di).map(etf_cell_str).unwrap_or_default();
        if date_s.is_empty() {
            continue;
        }
        let Ok(date) = NaiveDate::parse_from_str(&date_s, "%d-%b-%Y") else {
            continue;
        };
        out.push(EtfObs {
            date,
            nav: get(ni).and_then(etf_cell_f64),
            so: get(si).and_then(etf_cell_f64),
            aum: get(ai).and_then(etf_cell_f64),
        });
    }
    if cols.is_none() {
        return Err("ssga xlsx: no 'Date | NAV | Shares Outstanding | Total Net Assets' header row".into());
    }
    out.sort_by_key(|o| o.date);
    out.dedup_by_key(|o| o.date);
    Ok(out)
}

/// Parse a ProShares historical_nav.csv. Shares Outstanding is in thousands.
fn etf_parse_proshares_csv(body: &str) -> std::result::Result<Vec<EtfObs>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .map_err(|e| format!("proshares csv header: {e}"))?
        .clone();
    let find = |needle: &str| {
        headers
            .iter()
            .position(|h| h.trim().to_ascii_lowercase().starts_with(needle))
    };
    let (Some(di), Some(ni), Some(si), Some(ai)) = (
        find("date"),
        find("nav"),
        find("shares outstanding"),
        find("assets under management"),
    ) else {
        return Err(format!("proshares csv: unexpected header {:?}", headers));
    };
    let so_scale = if headers[si].contains("(000)") { 1000.0 } else { 1.0 };
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        let Some(date) = rec
            .get(di)
            .and_then(|d| NaiveDate::parse_from_str(d.trim(), "%m/%d/%Y").ok())
        else {
            continue;
        };
        out.push(EtfObs {
            date,
            nav: rec.get(ni).and_then(etf_num),
            so: rec.get(si).and_then(etf_num).map(|v| v * so_scale),
            aum: rec.get(ai).and_then(etf_num),
        });
    }
    out.sort_by_key(|o| o.date);
    out.dedup_by_key(|o| o.date);
    Ok(out)
}

/// Pull one fund out of the iShares product-screener JSON (snapshot).
fn etf_parse_ishares_screener(
    body: &serde_json::Value,
    symbol: &str,
) -> std::result::Result<Vec<EtfObs>, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "ishares screener: not a JSON object".to_string())?;
    let fund = obj
        .values()
        .find(|v| {
            v.get("localExchangeTicker")
                .and_then(|t| t.as_str())
                .map(|t| t.eq_ignore_ascii_case(symbol))
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("'{symbol}' is not a US iShares fund"))?;
    let raw = |k: &str| fund.pointer(&format!("/{k}/r")).and_then(|v| v.as_f64());
    let ymd = |k: &str| {
        fund.pointer(&format!("/{k}/r"))
            .and_then(|v| v.as_i64())
            .and_then(|n| NaiveDate::parse_from_str(&n.to_string(), "%Y%m%d").ok())
    };
    let nav = raw("navAmount");
    let nav_date = ymd("navAmountAsOf");
    let tna = raw("totalNetAssetsFund").or_else(|| raw("totalNetAssets"));
    let tna_date = ymd("totalNetAssetsFundAsOf");
    let Some(date) = nav_date.or(tna_date) else {
        return Err(format!("ishares screener: no as-of date for '{symbol}'"));
    };
    // Only derive SO when NAV and TNA are struck on the same day.
    let same_day = nav_date.is_some() && nav_date == tna_date;
    let so = match (nav, tna, same_day) {
        // NAV is published to 6 dp, so TNA/NAV is off by a share or two; iShares
        // creates in 10k-100k blocks, so round to the nearest 100 shares.
        (Some(n), Some(a), true) if n > 0.0 => Some((a / n / 100.0).round() * 100.0),
        _ => None,
    };
    Ok(vec![EtfObs {
        date,
        nav,
        so,
        aum: if tna_date == Some(date) { tna } else { None },
    }])
}

/// Parse a Direxion holdings csv: "Shares Outstanding:<n>" + TradeDate rows (snapshot).
fn etf_parse_direxion_csv(body: &str) -> std::result::Result<Vec<EtfObs>, String> {
    let mut so = None;
    let mut date = None;
    for line in body.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Shares Outstanding:") {
            so = etf_num(rest);
        } else if date.is_none() && l.starts_with('"') && !l.starts_with("\"TradeDate") {
            let first = l.trim_start_matches('"').split('"').next().unwrap_or("");
            let d = first.split_whitespace().next().unwrap_or("");
            date = NaiveDate::parse_from_str(d, "%m/%d/%Y").ok();
        }
    }
    match (so, date) {
        (Some(so), Some(date)) => Ok(vec![EtfObs { date, nav: None, so: Some(so), aum: None }]),
        _ => Err("direxion csv: missing 'Shares Outstanding:' line or TradeDate".to_string()),
    }
}

/// Is SO_t/SO_{t-1} a share split mirrored by NAV_{t-1}/NAV_t?
fn etf_is_split(prev: &EtfObs, cur: &EtfObs) -> bool {
    let (Some(so0), Some(so1), Some(n0), Some(n1)) = (prev.so, cur.so, prev.nav, cur.nav) else {
        return false;
    };
    if so0 <= 0.0 || n1 <= 0.0 {
        return false;
    }
    let so_ratio = so1 / so0;
    let nav_ratio = n0 / n1;
    let big = so_ratio >= 1.8 || so_ratio <= 1.0 / 1.8;
    big && (so_ratio / nav_ratio - 1.0).abs() < 0.1
}

/// Daily flows in $mn: (SO_t - SO_{t-1}) * NAV_t. Needs consecutive observations
/// with SO + NAV; split days are skipped.
fn etf_daily_flows(obs: &[EtfObs]) -> Vec<(NaiveDate, f64)> {
    let mut out = Vec::new();
    for w in obs.windows(2) {
        let (prev, cur) = (&w[0], &w[1]);
        let (Some(so0), Some(so1), Some(nav)) = (prev.so, cur.so, cur.nav) else {
            continue;
        };
        if etf_is_split(prev, cur) {
            continue;
        }
        out.push((cur.date, (so1 - so0) * nav / 1e6));
    }
    out
}

fn etf_rolling_sum(flows: &[(NaiveDate, f64)], window: usize) -> Vec<(NaiveDate, f64)> {
    if window == 0 || flows.len() < window {
        return Vec::new();
    }
    flows
        .windows(window)
        .map(|w| (w[window - 1].0, w.iter().map(|(_, v)| v).sum()))
        .collect()
}

fn etf_field_values(obs: &[EtfObs], field: EtfField) -> Vec<(NaiveDate, f64)> {
    match field {
        EtfField::So => obs.iter().filter_map(|o| o.so.map(|v| (o.date, v))).collect(),
        EtfField::Nav => obs.iter().filter_map(|o| o.nav.map(|v| (o.date, v))).collect(),
        EtfField::Aum => obs.iter().filter_map(|o| o.aum.map(|v| (o.date, v / 1e6))).collect(),
        EtfField::Flow => etf_daily_flows(obs),
        EtfField::Flow5 => etf_rolling_sum(&etf_daily_flows(obs), 5),
        EtfField::Flow20 => etf_rolling_sum(&etf_daily_flows(obs), 20),
    }
}

/// GET with a real UA and one/two backoff retries on 429 / 5xx.
/// Ok(None) = clean 404/403 (ticker not in this family).
async fn etf_get_bytes(url: &str) -> std::result::Result<Option<Vec<u8>>, String> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut delay_ms = 600u64;
    for attempt in 0..3 {
        let resp = client
            .get(url)
            .header(reqwest::header::USER_AGENT, ETF_USER_AGENT)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if status.as_u16() == 429 || status.is_server_error() {
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
                continue;
            }
            return Err(format!("http {status} after retries"));
        }
        if status.as_u16() == 404 || status.as_u16() == 403 || status.as_u16() == 410 {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("http {status}"));
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = resp.bytes().await.map_err(|e| format!("body read failed: {e}"))?;
        // A 200 HTML page is a soft miss (redirect to a landing / not-found page).
        if ctype.starts_with("text/html") {
            return Ok(None);
        }
        return Ok(Some(bytes.to_vec()));
    }
    Err("unreachable retry loop".into())
}

async fn etf_fetch_family(
    family: EtfFamily,
    symbol: &str,
    ishares_cache: &mut Option<std::result::Result<serde_json::Value, String>>,
) -> std::result::Result<Option<Vec<EtfObs>>, String> {
    match family {
        EtfFamily::Ssga => {
            let url = format!("{ETF_SSGA_BASE}{}.xlsx", symbol.to_ascii_lowercase());
            match etf_get_bytes(&url).await? {
                Some(b) => etf_parse_ssga_xlsx(&b).map(Some),
                None => Ok(None),
            }
        }
        EtfFamily::ProShares => {
            let url = format!("{ETF_PROSHARES_BASE}{}-historical_nav.csv", symbol.to_ascii_uppercase());
            match etf_get_bytes(&url).await? {
                Some(b) => etf_parse_proshares_csv(&String::from_utf8_lossy(&b)).map(Some),
                None => Ok(None),
            }
        }
        EtfFamily::IShares => {
            if ishares_cache.is_none() {
                let fetched = match etf_get_bytes(ETF_ISHARES_SCREENER).await {
                    Ok(Some(b)) => serde_json::from_slice::<serde_json::Value>(&b)
                        .map_err(|e| format!("ishares screener json: {e}")),
                    Ok(None) => Err("ishares screener unavailable".to_string()),
                    Err(e) => Err(e),
                };
                *ishares_cache = Some(fetched);
            }
            match ishares_cache.as_ref() {
                Some(Ok(v)) => match etf_parse_ishares_screener(v, symbol) {
                    Ok(obs) => Ok(Some(obs)),
                    Err(_) => Ok(None),
                },
                Some(Err(e)) => Err(e.clone()),
                None => Ok(None),
            }
        }
        EtfFamily::Direxion => {
            let url = format!("{ETF_DIREXION_BASE}{}.csv", symbol.to_ascii_uppercase());
            match etf_get_bytes(&url).await? {
                Some(b) => etf_parse_direxion_csv(&String::from_utf8_lossy(&b)).map(Some),
                None => Ok(None),
            }
        }
    }
}

pub(crate) async fn fetch_etf_flow_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    _granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let err = |ticker: &str, stage: &str, message: String| TimeseriesError {
        ticker: ticker.to_string(),
        stage: Some(stage.to_string()),
        message,
    };

    // One download per (family, symbol) per call, shared across fields.
    let mut cache: HashMap<(Option<EtfFamily>, String), std::result::Result<(EtfFamily, Vec<EtfObs>), String>> =
        HashMap::new();
    let mut ishares_cache: Option<std::result::Result<serde_json::Value, String>> = None;
    let start_day = start.date_naive();

    for ticker in tickers {
        let spec = match etf_parse_ticker(ticker) {
            Ok(s) => s,
            Err(e) => {
                errors.push(err(ticker, "parse", e));
                continue;
            }
        };
        let key = (spec.family, spec.symbol.clone());
        if !cache.contains_key(&key) {
            let families: Vec<EtfFamily> = match spec.family {
                Some(f) => vec![f],
                None => vec![EtfFamily::Ssga, EtfFamily::ProShares, EtfFamily::IShares, EtfFamily::Direxion],
            };
            let mut result: std::result::Result<(EtfFamily, Vec<EtfObs>), String> = Err(format!(
                "'{}' not found at SSGA/SPDR, ProShares, iShares or Direxion",
                spec.symbol
            ));
            let mut transport_errs = Vec::new();
            for fam in families {
                match etf_fetch_family(fam, &spec.symbol, &mut ishares_cache).await {
                    Ok(Some(obs)) if !obs.is_empty() => {
                        result = Ok((fam, obs));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => transport_errs.push(format!("{}: {e}", fam.name())),
                }
            }
            if result.is_err() && !transport_errs.is_empty() {
                result = Err(format!(
                    "'{}' not found; upstream errors: {}",
                    spec.symbol,
                    transport_errs.join("; ")
                ));
            }
            cache.insert(key.clone(), result);
        }
        let (family, obs) = match cache.get(&key) {
            Some(Ok((f, o))) => (*f, o),
            Some(Err(e)) => {
                errors.push(err(ticker, "fetch", format!("etf: {e}")));
                continue;
            }
            None => continue,
        };

        let values = etf_field_values(obs, spec.field);
        let candles: Vec<Candle> = values
            .into_iter()
            .filter_map(|(d, v)| {
                let t = d.and_hms_opt(0, 0, 0)?.and_utc();
                (d >= start_day && t <= end).then_some(Candle {
                    t,
                    o: v,
                    h: v,
                    l: v,
                    c: v,
                    v: None,
                    kind: Some("point".to_string()),
                })
            })
            .collect();

        if candles.is_empty() {
            let snapshot_only = matches!(family, EtfFamily::IShares | EtfFamily::Direxion);
            let msg = if snapshot_only
                && matches!(spec.field, EtfField::Flow | EtfField::Flow5 | EtfField::Flow20)
            {
                format!(
                    "etf ({}) is snapshot-only (one as-of date, no history), so FLOW cannot be computed; use :SO / :AUM and accumulate by polling",
                    family.name()
                )
            } else if snapshot_only && matches!(spec.field, EtfField::Nav | EtfField::Aum) && family == EtfFamily::Direxion {
                "etf (direxion) holdings file only carries Shares Outstanding; use :SO".to_string()
            } else {
                let last = obs.last().map(|o| o.date.to_string()).unwrap_or_default();
                format!(
                    "etf ({}) has no {:?} values in this window (latest observation {last})",
                    family.name(),
                    spec.field
                )
            };
            errors.push(err(ticker, "fetch", msg));
            continue;
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some(format!("etf_{}", family.name())),
            upstream_id: Some(spec.symbol.clone()),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod etf_flows_tests {
    use super::*;

    #[test]
    fn etf_ticker_grammar() {
        assert!(is_etf_flow_ticker("ETF:JNK:FLOW"));
        assert!(is_etf_flow_ticker("etf:jnk"));
        assert!(!is_etf_flow_ticker("ETF:"));
        assert!(!is_etf_flow_ticker("SPY"));
        let s = etf_parse_ticker("etf:jnk:flow5").unwrap();
        assert_eq!(s.symbol, "JNK");
        assert_eq!(s.field, EtfField::Flow5);
        assert_eq!(s.family, None);
        assert_eq!(etf_parse_ticker("ETF:SPY").unwrap().field, EtfField::Flow);
        let d = etf_parse_ticker("ETF:DIREXION:SOXL:SO").unwrap();
        assert_eq!(d.family, Some(EtfFamily::Direxion));
        assert_eq!(d.symbol, "SOXL");
        assert!(etf_parse_ticker("ETF:JNK:BOGUS").is_err());
        assert!(etf_parse_ticker("ETF:NOPE:JNK:SO").is_err());
    }

    #[test]
    fn etf_parse_ssga_fixture() {
        // Real JNK navhist file (2026-10-30), trimmed to the first 8 data rows.
        let bytes = include_bytes!("fixtures/etf_ssga_jnk_head.xlsx");
        let obs = etf_parse_ssga_xlsx(bytes).unwrap();
        assert_eq!(obs.len(), 8);
        let last = obs.last().unwrap();
        assert_eq!(last.date, NaiveDate::from_ymd_opt(2026, 10, 29).unwrap());
        assert!((last.nav.unwrap() - 92.982138).abs() < 1e-9);
        assert_eq!(last.so.unwrap(), 65_074_605.0);
        assert!((last.aum.unwrap() - 6_050_775_871.49).abs() < 1.0);
        // 28-Oct -> 29-Oct: +200,000 shares x 92.982138 = +18.596 $mn
        let flows = etf_daily_flows(&obs);
        let (d, f) = flows.last().unwrap();
        assert_eq!(*d, NaiveDate::from_ymd_opt(2026, 10, 29).unwrap());
        assert!((f - 18.5964276).abs() < 1e-4, "{f}");
        assert_eq!(etf_rolling_sum(&flows, 5).len(), flows.len() - 4);
    }

    #[test]
    fn etf_parse_proshares_fixture() {
        let body = "Date,ProShares Name,Ticker,NAV,Prior NAV,NAV Change (%),NAV Change ($),Shares Outstanding (000),Assets Under Management\n\
10/29/2026,ProShares UltraPro QQQ,TQQQ,69.1933,72.2017,-4.16666,-3.0084,517400,35800613420\n\
10/28/2026,ProShares UltraPro QQQ,TQQQ,72.2017,71.0573,1.61053,1.1444,515300,37205536010\n\
10/27/2026,ProShares UltraPro QQQ,TQQQ,71.0573,68.426,3.84547,2.6313,516650,36711754045\n";
        let obs = etf_parse_proshares_csv(body).unwrap();
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[2].so, Some(517_400_000.0));
        let flows = etf_daily_flows(&obs);
        // 27->28: -1,350,000 x 72.2017 = -97.47 $mn; 28->29: +2,100,000 x 69.1933 = +145.31
        assert!((flows[0].1 + 97.472295).abs() < 1e-4);
        assert!((flows[1].1 - 145.30593).abs() < 1e-4);
        let aum = etf_field_values(&obs, EtfField::Aum);
        assert!((aum[2].1 - 35_800.61342).abs() < 1e-3);
    }

    #[test]
    fn etf_split_day_has_no_flow() {
        let d = |n| NaiveDate::from_ymd_opt(2026, 1, n).unwrap();
        let obs = vec![
            EtfObs { date: d(5), nav: Some(100.0), so: Some(1e6), aum: None },
            EtfObs { date: d(6), nav: Some(50.2), so: Some(2e6), aum: None },
            EtfObs { date: d(7), nav: Some(50.0), so: Some(2.1e6), aum: None },
        ];
        let flows = etf_daily_flows(&obs);
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0, d(7));
        assert!((flows[0].1 - 5.0).abs() < 1e-9);
    }

    #[test]
    fn etf_parse_ishares_fixture() {
        let body: serde_json::Value = serde_json::from_str(r#"{
          "239565": {"localExchangeTicker": "HYG", "fundName": "iShares iBoxx $ High Yield Corporate Bond ETF",
            "navAmount": {"d": "77.37", "r": 77.374511}, "navAmountAsOf": {"d": "Oct 29, 2026", "r": 20261029},
            "totalNetAssetsFund": {"d": "17,641,388,607", "r": 17641388606.74},
            "totalNetAssetsFundAsOf": {"d": "Oct 29, 2026", "r": 20261029}, "portfolioId": 239565},
          "258100": {"localExchangeTicker": "SHYG", "navAmount": {"d": "41.20", "r": 41.197489},
            "navAmountAsOf": {"d": "Oct 29, 2026", "r": 20261029}}
        }"#).unwrap();
        let obs = etf_parse_ishares_screener(&body, "hyg").unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].date, NaiveDate::from_ymd_opt(2026, 10, 29).unwrap());
        // Product page shows Shares Outstanding 228,000,000 for the same date.
        assert_eq!(obs[0].so, Some(228_000_000.0));
        assert!(etf_parse_ishares_screener(&body, "JNK").is_err());
        assert!(etf_daily_flows(&obs).is_empty());
    }

    #[test]
    fn etf_parse_direxion_fixture() {
        let body = "Direxion Daily Semiconductor Bull 3X ETF\nSOXL\nShares Outstanding:237300060\n\n\n\
\"TradeDate\",\"AccountTicker\",\"StockTicker\",\"SecurityDescription\",\"Shares\",\"Price\",\"MarketValue\",\"Cusip\",\"HoldingsPercent\"\n\
\"10/30/2026 12:00:00 AM\",\"SOXL\",\"AMD\",\"ADVANCED MICRO DEVICES\",\"2874061.0000\",\"566.4200\",\"1627925631.6200\",\"007903107\",\"6.6959092136\"\n";
        let obs = etf_parse_direxion_csv(body).unwrap();
        assert_eq!(obs[0].date, NaiveDate::from_ymd_opt(2026, 10, 30).unwrap());
        assert_eq!(obs[0].so, Some(237_300_060.0));
    }

    #[tokio::test]
    #[ignore]
    async fn etf_live_jnk_spy_tqqq_hyg_soxl() {
        let end = Utc::now();
        let start = end - Duration::days(14);
        let tickers: Vec<String> = [
            "ETF:JNK:FLOW", "ETF:SPY:FLOW", "ETF:SPY:SO", "ETF:TQQQ:FLOW", "ETF:GLD:AUM",
            "ETF:HYG:SO", "ETF:SOXL:SO", "ETF:HYG:FLOW",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (series, errors) = fetch_etf_flow_series(&tickers, start, end, Span { n: 1, unit: SpanUnit::Day })
            .await
            .unwrap();
        for s in &series {
            let tail: Vec<String> = s
                .candles
                .iter()
                .rev()
                .take(5)
                .map(|c| format!("{} {:.3}", c.t.date_naive(), c.c))
                .collect();
            println!("{} [{}] {:?}", s.ticker, s.source.clone().unwrap_or_default(), tail);
        }
        for e in &errors {
            println!("ERR {} {}", e.ticker, e.message);
        }
        assert!(series.iter().any(|s| s.ticker == "ETF:JNK:FLOW" && !s.candles.is_empty()));
        assert!(series.iter().any(|s| s.ticker == "ETF:SPY:FLOW" && !s.candles.is_empty()));
    }
}
