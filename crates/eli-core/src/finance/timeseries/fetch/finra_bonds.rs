/// Individual corporate / agency bond prices and yields from FINRA TRACE
/// (the public data behind finra.org "Fixed Income Data"). No key.
///
/// Ticker grammar (case-insensitive prefix, ID = 9-char CUSIP or FINRA symbol):
///   BOND:21873SAG3        last-sale price (same as :PX)
///   BOND:21873SAG3:PX     last-sale price (percent of par)
///   BOND:21873SAG3:YLD    yield (percent) implied by the last sale
///   BOND:CRWV6364372:YLD  FINRA issue symbol works too
///
/// Two upstream datasets (group FixedIncomeMarket on services-dynarep.ddwa.finra.org):
/// - granularity >= 1d -> `EndOfDayPriceYield` (one row per trade date: last sale
///   price + yield of the day). FINRA's own bond page queries 5 years back; that is
///   the depth we assume. Rows are stamped 00:00 UTC of the trade date. The row for
///   day D only appears after D's session closes (it was absent intraday on D), so a
///   daily candle carries D's closing print; nothing from later days leaks in.
/// - granularity < 1d  -> `CorporateAndAgencyTradeHistory` (every disseminated
///   trade, ~1 year back, ~15 min TRACE dissemination delay). Each trade is stamped at
///   its execution time (US/Eastern -> UTC) and bucketed into OHLC candles; `v` is
///   disseminated par volume. Size is capped upstream ("1MM+" HY / "5MM+" IG), and the
///   cap value is used, so `v` is a lower bound.
///
/// Session: a GET to a public lookup sets an `XSRF-TOKEN` cookie; every POST must echo
/// it in `x-xsrf-token` with finra.org Origin/Referer. One session per fetch call.
///
/// The issuer lookup is not a timeseries: use [`finra_bond_search`] to list an
/// issuer's bonds (CUSIP, symbol, coupon, maturity, last price/yield).

const FINRA_BOND_BASE: &str = "https://services-dynarep.ddwa.finra.org/";
const FINRA_BOND_SESSION_PATH: &str =
    "public/reporting/v2/group/Firm/name/ActiveIndividual/dynamiclookup/examCode";
const FINRA_BOND_DATA_PATH: &str = "public/reporting/v2/data/group/FixedIncomeMarket/name/";
const FINRA_BOND_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36";
const FINRA_BOND_PAGE: u64 = 5000;
/// Hard cap on trade-history pages per bond (100k trades) so a runaway range can't hammer FINRA.
const FINRA_BOND_MAX_PAGES: u64 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinraBondField {
    Price,
    Yield,
}

/// Parse `BOND:<ID>[:PX|:YLD]` -> (upper-cased id, field). None if malformed.
fn finra_bond_parse_ticker(ticker: &str) -> Option<(String, FinraBondField)> {
    let t = ticker.trim();
    if !is_finra_bond_ticker(t) {
        return None;
    }
    let rest = t[5..].trim().to_ascii_uppercase();
    let mut parts = rest.split(':');
    let id = parts.next()?.trim().to_string();
    let field = match parts.next().map(str::trim) {
        None | Some("") | Some("PX") | Some("PRICE") => FinraBondField::Price,
        Some("YLD") | Some("YIELD") => FinraBondField::Yield,
        Some(_) => return None,
    };
    if parts.next().is_some() || id.is_empty() || id == "ISSUER" {
        return None;
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((id, field))
}

/// Returns true if the ticker should route to the FINRA TRACE bond provider.
pub fn is_finra_bond_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    t.len() > 5 && t[..5].eq_ignore_ascii_case("BOND:")
}

/// FINRA returns numbers sometimes as JSON numbers, sometimes as strings.
fn finra_bond_num(v: Option<&serde_json::Value>) -> Option<f64> {
    match v? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|x| x.is_finite())
}

fn finra_bond_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Unwrap `{"status":"success","returnBody":{"headers":{"Record-Total":["N"]},"data":"<json string>"}}`.
/// Returns (rows, record_total).
fn finra_bond_unwrap(body: &serde_json::Value) -> std::result::Result<(Vec<serde_json::Value>, u64), String> {
    if body.get("status").and_then(|s| s.as_str()) != Some("success") {
        let msg = body
            .get("statusMessage")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown failure");
        return Err(format!("finra returned failure: {msg}"));
    }
    let rb = body.get("returnBody").ok_or("finra response missing returnBody")?;
    let rows: Vec<serde_json::Value> = match rb.get("data") {
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str(s).map_err(|e| format!("finra data string not JSON: {e}"))?
        }
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(serde_json::Value::Null) | None => Vec::new(),
        Some(_) => return Err("finra data has unexpected shape".to_string()),
    };
    let total = rb
        .pointer("/headers/Record-Total/0")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(rows.len() as u64);
    Ok((rows, total))
}

/// US/Eastern -> UTC offset hours for a local wall-clock time (2007+ DST rule).
fn finra_bond_eastern_offset(local: chrono::NaiveDateTime) -> i64 {
    use chrono::Datelike;
    let y = local.date().year();
    let nth_sunday = |month: u32, n: u32| -> NaiveDate {
        let first = NaiveDate::from_ymd_opt(y, month, 1).unwrap_or_default();
        let to_sun = (7 - first.weekday().num_days_from_sunday()) % 7;
        first + Duration::days((to_sun + 7 * (n - 1)) as i64)
    };
    let two = chrono::NaiveTime::from_hms_opt(2, 0, 0).unwrap_or_default();
    let dst = local >= nth_sunday(3, 2).and_time(two) && local < nth_sunday(11, 1).and_time(two);
    if dst { 4 } else { 5 }
}

/// "1MM+" -> 1e6, "5MM+" -> 5e6, "500000.00" -> 5e5.
fn finra_bond_volume(raw: Option<&serde_json::Value>) -> Option<f64> {
    let s = finra_bond_str(raw)?;
    let up = s.to_ascii_uppercase();
    if let Some(n) = up.strip_suffix("MM+").or_else(|| up.strip_suffix("MM")) {
        return n.trim().parse::<f64>().ok().map(|x| x * 1e6);
    }
    up.replace(',', "").parse::<f64>().ok()
}

/// One end-of-day observation.
#[derive(Clone, Debug, PartialEq)]
struct FinraBondDaily {
    t: DateTime<Utc>,
    price: Option<f64>,
    yld: Option<f64>,
}

fn finra_bond_parse_eod(rows: &[serde_json::Value]) -> Vec<FinraBondDaily> {
    let mut out: Vec<FinraBondDaily> = rows
        .iter()
        .filter_map(|r| {
            let d = finra_bond_str(r.get("tradeDate"))?;
            let date = NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok()?;
            Some(FinraBondDaily {
                t: date.and_hms_opt(0, 0, 0)?.and_utc(),
                price: finra_bond_num(r.get("lastSalePrice")),
                yld: finra_bond_num(r.get("lastSaleYield")),
            })
        })
        .collect();
    out.sort_by_key(|d| d.t);
    out.dedup_by_key(|d| d.t);
    out
}

/// One disseminated trade.
#[derive(Clone, Debug, PartialEq)]
struct FinraBondTrade {
    t: DateTime<Utc>,
    price: Option<f64>,
    yld: Option<f64>,
    volume: Option<f64>,
}

fn finra_bond_parse_trades(rows: &[serde_json::Value]) -> Vec<FinraBondTrade> {
    let mut out: Vec<FinraBondTrade> = rows
        .iter()
        .filter_map(|r| {
            // Drop explicit cancels if they ever appear in the public feed.
            if finra_bond_str(r.get("tradeStatus")).as_deref() == Some("X") {
                return None;
            }
            let d = finra_bond_str(r.get("tradeExecutionDate"))?;
            let date = NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok()?;
            let time = finra_bond_str(r.get("tradeExecutionTime"))
                .and_then(|s| chrono::NaiveTime::parse_from_str(&s, "%H:%M:%S").ok())
                .unwrap_or_default();
            let local = date.and_time(time);
            let t = (local + Duration::hours(finra_bond_eastern_offset(local))).and_utc();
            Some(FinraBondTrade {
                t,
                price: finra_bond_num(r.get("lastSalePrice")),
                yld: finra_bond_num(r.get("lastSaleYield")),
                volume: finra_bond_volume(r.get("reportedTradeVolume")),
            })
        })
        .collect();
    out.sort_by_key(|x| x.t);
    out
}

/// Bucket trades into OHLC candles of `step` (absolute UTC boundaries, like resample_candles).
fn finra_bond_trades_to_candles(trades: &[FinraBondTrade], field: FinraBondField, step: Duration) -> Vec<Candle> {
    let points: Vec<Candle> = trades
        .iter()
        .filter_map(|tr| {
            let x = match field {
                FinraBondField::Price => tr.price,
                FinraBondField::Yield => tr.yld,
            }?;
            Some(Candle { t: tr.t, o: x, h: x, l: x, c: x, v: tr.volume, kind: None })
        })
        .collect();
    if points.is_empty() {
        return points;
    }
    resample_candles(&points, points[0].t, step)
}

fn finra_bond_daily_to_candles(rows: &[FinraBondDaily], field: FinraBondField) -> Vec<Candle> {
    rows.iter()
        .filter_map(|d| {
            let x = match field {
                FinraBondField::Price => d.price,
                FinraBondField::Yield => d.yld,
            }?;
            Some(Candle { t: d.t, o: x, h: x, l: x, c: x, v: None, kind: Some("point".to_string()) })
        })
        .collect()
}

/// A FINRA session: cookie-store client + XSRF token.
struct FinraBondSession {
    client: reqwest::Client,
    xsrf: String,
}

impl FinraBondSession {
    async fn open() -> std::result::Result<Self, String> {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("finra client build failed: {e}"))?;
        let resp = client
            .get(format!("{FINRA_BOND_BASE}{FINRA_BOND_SESSION_PATH}"))
            .header(reqwest::header::USER_AGENT, FINRA_BOND_USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
            .header("Origin", "https://www.finra.org")
            .header(reqwest::header::REFERER, "https://www.finra.org/")
            .send()
            .await
            .map_err(|e| format!("finra session request failed: {e}"))?;
        let xsrf = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| {
                c.split(';')
                    .next()
                    .and_then(|kv| kv.trim().strip_prefix("XSRF-TOKEN="))
                    .map(str::to_string)
            })
            .ok_or_else(|| format!("finra session did not set XSRF-TOKEN (http {})", resp.status()))?;
        Ok(Self { client, xsrf })
    }

    /// POST a data query with 429/5xx backoff. Returns (rows, record_total).
    async fn query(
        &self,
        dataset: &str,
        body: &serde_json::Value,
    ) -> std::result::Result<(Vec<serde_json::Value>, u64), String> {
        let url = format!("{FINRA_BOND_BASE}{FINRA_BOND_DATA_PATH}{dataset}");
        let mut delay_ms = 1000u64;
        for attempt in 0..4 {
            let resp = self
                .client
                .post(&url)
                .header(reqwest::header::USER_AGENT, FINRA_BOND_USER_AGENT)
                .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
                .header("Origin", "https://www.finra.org")
                .header(reqwest::header::REFERER, "https://www.finra.org/")
                .header("x-xsrf-token", &self.xsrf)
                .json(body)
                .send()
                .await
                .map_err(|e| format!("finra {dataset} request failed: {e}"))?;
            let status = resp.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
                continue;
            }
            let text = resp
                .text()
                .await
                .map_err(|e| format!("finra {dataset} body read failed: {e}"))?;
            if !status.is_success() {
                return Err(format!(
                    "finra {dataset} returned http {status}: {}",
                    text.chars().take(160).collect::<String>()
                ));
            }
            let v: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("finra {dataset} json parse failed: {e}"))?;
            return finra_bond_unwrap(&v);
        }
        Err(format!("finra {dataset} kept rate-limiting (429)"))
    }
}

fn finra_bond_id_filter(id: &str) -> serde_json::Value {
    serde_json::json!([{ "compareFilters": [
        { "fieldName": "cusip", "fieldValue": id, "compareType": "EQUAL" },
        { "fieldName": "issueSymbolIdentifier", "fieldValue": id, "compareType": "EQUAL" }
    ]}])
}

fn finra_bond_eod_body(id: &str, start: DateTime<Utc>, end: DateTime<Utc>, offset: u64) -> serde_json::Value {
    serde_json::json!({
        "fields": ["cusip", "issueSymbolIdentifier", "productType", "lastSalePrice", "lastSaleYield", "tradeDate"],
        "orFilters": finra_bond_id_filter(id),
        "dateRangeFilters": [{
            "startDate": start.format("%Y-%m-%d").to_string(),
            "endDate": end.format("%Y-%m-%d").to_string(),
            "fieldName": "tradeDate"
        }],
        "sortFields": ["tradeDate"],
        "offset": offset,
        "limit": FINRA_BOND_PAGE
    })
}

fn finra_bond_trades_body(id: &str, start: DateTime<Utc>, end: DateTime<Utc>, offset: u64) -> serde_json::Value {
    serde_json::json!({
        "fields": ["cusip", "issueSymbolIdentifier", "tradeExecutionDate", "tradeExecutionTime",
                   "lastSalePrice", "lastSaleYield", "reportedTradeVolume", "tradeStatus"],
        "orFilters": finra_bond_id_filter(id),
        "dateRangeFilters": [{
            // ET dates; widen by a day each side, exact window is applied after parsing.
            "startDate": (start - Duration::days(1)).format("%Y-%m-%d").to_string(),
            "endDate": (end + Duration::days(1)).format("%Y-%m-%d").to_string(),
            "fieldName": "tradeExecutionDate"
        }],
        "sortFields": ["tradeExecutionDate", "tradeExecutionTime"],
        "offset": offset,
        "limit": FINRA_BOND_PAGE
    })
}

/// Page through a dataset until Record-Total is reached (polite: 300ms between pages).
async fn finra_bond_fetch_all(
    session: &FinraBondSession,
    dataset: &str,
    make_body: impl Fn(u64) -> serde_json::Value,
) -> std::result::Result<Vec<serde_json::Value>, String> {
    let mut all = Vec::new();
    let mut offset = 0u64;
    for page in 0..FINRA_BOND_MAX_PAGES {
        if page > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let (rows, total) = session.query(dataset, &make_body(offset)).await?;
        let n = rows.len() as u64;
        all.extend(rows);
        offset += n;
        if n == 0 || offset >= total {
            break;
        }
    }
    Ok(all)
}

/// Fetch FINRA TRACE bond price/yield series. Same conventions as `fetch_govt_series`.
/// Daily+ granularity uses end-of-day prints (point candles, 00:00 UTC of trade date);
/// sub-daily granularity buckets individual trades into OHLC with par volume.
pub(crate) async fn fetch_finra_bond_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let step = granularity.approx_duration();
    let intraday = step < Duration::days(1);
    let mut out = Vec::new();
    let mut errors = Vec::new();

    // Group tickers by bond id so PX and YLD of one bond share one upstream query.
    let mut by_id: Vec<(String, Vec<(String, FinraBondField)>)> = Vec::new();
    for ticker in tickers {
        match finra_bond_parse_ticker(ticker) {
            Some((id, field)) => match by_id.iter_mut().find(|(i, _)| *i == id) {
                Some((_, v)) => v.push((ticker.clone(), field)),
                None => by_id.push((id, vec![(ticker.clone(), field)])),
            },
            None => errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: "finra bond tickers look like BOND:<CUSIP>[:PX|:YLD] (e.g. BOND:21873SAG3:YLD); use the bond-search helper to find CUSIPs by issuer".to_string(),
            }),
        }
    }
    if by_id.is_empty() {
        return Ok((out, errors));
    }

    let session = match FinraBondSession::open().await {
        Ok(s) => s,
        Err(e) => {
            for (_, wanted) in &by_id {
                for (t, _) in wanted {
                    errors.push(TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("fetch".to_string()),
                        message: e.clone(),
                    });
                }
            }
            return Ok((out, errors));
        }
    };

    for (idx, (id, wanted)) in by_id.iter().enumerate() {
        if idx > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let fetched = if intraday {
            finra_bond_fetch_all(&session, "CorporateAndAgencyTradeHistory", |off| {
                finra_bond_trades_body(id, start, end, off)
            })
            .await
            .map(|rows| {
                let mut trades = finra_bond_parse_trades(&rows);
                trades.retain(|t| t.t >= start && t.t <= end);
                (Some(trades), None)
            })
        } else {
            finra_bond_fetch_all(&session, "EndOfDayPriceYield", |off| {
                finra_bond_eod_body(id, start, end, off)
            })
            .await
            .map(|rows| {
                let start_day = start.date_naive().and_hms_opt(0, 0, 0).map(|n| n.and_utc()).unwrap_or(start);
                let mut daily = finra_bond_parse_eod(&rows);
                daily.retain(|d| d.t >= start_day && d.t <= end);
                (None, Some(daily))
            })
        };
        let (trades, daily) = match fetched {
            Ok(x) => x,
            Err(e) => {
                for (t, _) in wanted {
                    errors.push(TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("fetch".to_string()),
                        message: e.clone(),
                    });
                }
                continue;
            }
        };

        for (ticker, field) in wanted {
            let mut candles = match (&trades, &daily) {
                (Some(tr), _) => finra_bond_trades_to_candles(tr, *field, step),
                (_, Some(d)) => finra_bond_daily_to_candles(d, *field),
                _ => Vec::new(),
            };
            if !intraday && step > Duration::days(1) && !candles.is_empty() {
                candles = resample_candles(&candles, start, step);
                for c in &mut candles {
                    c.kind = None;
                }
            }
            if candles.is_empty() {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!(
                        "finra trace has no {} prints for '{id}' in this window (unknown CUSIP/symbol, no trades, or beyond history: ~5y daily, ~1y intraday)",
                        if intraday { "intraday" } else { "end-of-day" }
                    ),
                });
                continue;
            }
            let suffix = match field {
                FinraBondField::Price => "PX",
                FinraBondField::Yield => "YLD",
            };
            out.push(TickerSeries {
                ticker: ticker.clone(),
                candles,
                source: Some("finra_trace".to_string()),
                upstream_id: Some(format!("{id}:{suffix}")),
            });
        }
    }

    Ok((out, errors))
}

/// One bond from an issuer lookup.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BondRef {
    pub cusip: String,
    /// FINRA issue symbol (e.g. CRWV6364372); also accepted as a BOND: id.
    pub symbol: Option<String>,
    pub issuer: Option<String>,
    /// Coupon in percent.
    pub coupon: Option<f64>,
    /// YYYY-MM-DD.
    pub maturity: Option<String>,
    pub last_price: Option<f64>,
    pub last_yield: Option<f64>,
    /// YYYY-MM-DD of the last disseminated trade.
    pub last_trade_date: Option<String>,
}

fn finra_bond_parse_refs(rows: &[serde_json::Value]) -> Vec<BondRef> {
    let mut out: Vec<BondRef> = rows
        .iter()
        .filter_map(|r| {
            Some(BondRef {
                cusip: finra_bond_str(r.get("cusip"))?,
                symbol: finra_bond_str(r.get("issueSymbolIdentifier")),
                issuer: finra_bond_str(r.get("issuerName")),
                coupon: finra_bond_num(r.get("couponRate")),
                maturity: finra_bond_str(r.get("maturityDate")),
                last_price: finra_bond_num(r.get("lastSalePrice")),
                last_yield: finra_bond_num(r.get("lastSaleYield")),
                last_trade_date: finra_bond_str(r.get("lastTradeDate")),
            })
        })
        .collect();
    out.sort_by(|a, b| a.maturity.cmp(&b.maturity).then(a.cusip.cmp(&b.cusip)));
    out
}

/// List an issuer's outstanding (not yet matured) corporate/agency bonds with the last
/// TRACE print, sorted by maturity. FINRA only supports an EXACT issuer-name match
/// (upper-cased here), e.g. "ORACLE CORP", "COREWEAVE INC", "APPLE INC".
/// Not a timeseries: feed the returned `cusip` into `BOND:<CUSIP>:PX|YLD`.
pub async fn finra_bond_search(issuer: &str) -> Result<Vec<BondRef>> {
    let name = issuer.trim().to_ascii_uppercase();
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let mut refs = finra_bond_lookup("issuerName", &name).await?;
    refs.retain(|b| b.maturity.as_deref().is_none_or(|m| m >= today.as_str()));
    Ok(refs)
}

/// Look up one bond by 9-char CUSIP or FINRA issue symbol (e.g. "21873SAG3",
/// "CRWV6364372"). Matured bonds are kept. Empty vec when nothing matches.
pub async fn finra_bond_lookup_id(id: &str) -> Result<Vec<BondRef>> {
    let id = id.trim().to_ascii_uppercase();
    if id.is_empty() {
        return Ok(Vec::new());
    }
    let refs = finra_bond_lookup("cusip", &id).await?;
    if !refs.is_empty() || id.len() == 9 {
        return Ok(refs);
    }
    finra_bond_lookup("issueSymbolIdentifier", &id).await
}

async fn finra_bond_lookup(field: &str, value: &str) -> Result<Vec<BondRef>> {
    let session = FinraBondSession::open().await.map_err(Error::Provider)?;
    let rows = finra_bond_fetch_all(&session, "CorporateAndAgencySecurities", |off| {
        serde_json::json!({
            "fields": ["cusip", "issuerName", "issueSymbolIdentifier", "couponRate", "maturityDate",
                       "lastSalePrice", "lastSaleYield", "lastTradeDate"],
            "compareFilters": [{ "fieldName": field, "fieldValue": value, "compareType": "EQUAL" }],
            "offset": off,
            "limit": FINRA_BOND_PAGE
        })
    })
    .await
    .map_err(Error::Provider)?;
    Ok(finra_bond_parse_refs(&rows))
}

#[cfg(test)]
mod finra_bond_tests {
    use super::*;

    // Real responses captured 2026-10-30 (trimmed).
    const EOD_FIXTURE: &str = r#"{"status": "success", "returnBody": {"statusCode": 200, "headers": {"Record-Total": ["4"]}, "data": "[{\"cusip\":\"21873SAG3\",\"issueSymbolIdentifier\":\"CRWV6364372\",\"tradeDate\":\"2026-10-26\",\"lastSaleYield\":15.38,\"lastSalePrice\":81},{\"cusip\":\"21873SAG3\",\"issueSymbolIdentifier\":\"CRWV6364372\",\"tradeDate\":\"2026-10-27\",\"lastSaleYield\":14.81,\"lastSalePrice\":82.6875},{\"cusip\":\"21873SAG3\",\"issueSymbolIdentifier\":\"CRWV6364372\",\"tradeDate\":\"2026-10-28\",\"lastSaleYield\":14.22,\"lastSalePrice\":84.5},{\"cusip\":\"21873SAG3\",\"issueSymbolIdentifier\":\"CRWV6364372\",\"tradeDate\":\"2026-10-29\",\"lastSaleYield\":14.37,\"lastSalePrice\":84.0625}]"}}"#;
    const TRADES_FIXTURE: &str = r#"{"status": "success", "returnBody": {"headers": {"Record-Total": ["60"]}, "data": "[{\"cusip\": \"21873SAG3\", \"issueSymbolIdentifier\": \"CRWV6364372\", \"tradeExecutionDate\": \"2026-10-30\", \"tradeExecutionTime\": \"10:52:32\", \"lastSalePrice\": \"82.711000\", \"lastSaleYield\": \"14.819840\", \"reportedTradeVolume\": \"500000.00\", \"tradeStatus\": \"M\"}, {\"cusip\": \"21873SAG3\", \"issueSymbolIdentifier\": \"CRWV6364372\", \"tradeExecutionDate\": \"2026-10-30\", \"tradeExecutionTime\": \"10:52:32\", \"lastSalePrice\": \"82.701000\", \"lastSaleYield\": \"14.823154\", \"reportedTradeVolume\": \"500000.00\", \"tradeStatus\": \"M\"}, {\"cusip\": \"21873SAG3\", \"issueSymbolIdentifier\": \"CRWV6364372\", \"tradeExecutionDate\": \"2026-10-30\", \"tradeExecutionTime\": \"11:13:25\", \"lastSalePrice\": \"83.000000\", \"lastSaleYield\": \"14.724268\", \"reportedTradeVolume\": \"1MM+\", \"tradeStatus\": \"M\"}, {\"cusip\": \"21873SAG3\", \"issueSymbolIdentifier\": \"CRWV6364372\", \"tradeExecutionDate\": \"2026-10-30\", \"tradeExecutionTime\": \"11:32:40\", \"lastSalePrice\": \"82.980000\", \"lastSaleYield\": \"14.730868\", \"reportedTradeVolume\": \"1MM+\", \"tradeStatus\": \"M\"}]"}}"#;
    const SEARCH_FIXTURE: &str = r#"{"status": "success", "returnBody": {"headers": {"Record-Total": ["77"]}, "data": "[{\"cusip\": \"68389XEC5\", \"issuerName\": \"ORACLE CORP\", \"issueSymbolIdentifier\": \"ORCL6302044\", \"couponRate\": \"6.8500000000000000000\", \"maturityDate\": \"2066-02-04\", \"lastSalePrice\": \"81.080000\", \"lastSaleYield\": \"8.524515\", \"lastTradeDate\": \"2026-10-30\"}, {\"cusip\": \"68389XAR6\", \"issuerName\": \"ORACLE CORP\", \"issueSymbolIdentifier\": \"ORCL4028185\", \"couponRate\": \"3.0163100000000000000\", \"maturityDate\": \"2019-01-15\", \"lastSalePrice\": \"100.024000\", \"lastSaleYield\": null, \"lastTradeDate\": \"2019-01-08\"}]"}}"#;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn finra_bond_ticker_grammar() {
        assert!(is_finra_bond_ticker("BOND:21873SAG3"));
        assert!(is_finra_bond_ticker("bond:21873sag3:yld"));
        assert!(!is_finra_bond_ticker("BOND:"));
        assert!(!is_finra_bond_ticker("SPY"));
        assert_eq!(
            finra_bond_parse_ticker("bond:21873sag3"),
            Some(("21873SAG3".to_string(), FinraBondField::Price))
        );
        assert_eq!(
            finra_bond_parse_ticker("BOND:21873SAG3:PX"),
            Some(("21873SAG3".to_string(), FinraBondField::Price))
        );
        assert_eq!(
            finra_bond_parse_ticker("BOND:CRWV6364372:YLD"),
            Some(("CRWV6364372".to_string(), FinraBondField::Yield))
        );
        assert_eq!(finra_bond_parse_ticker("BOND:21873SAG3:OAS"), None);
        assert_eq!(finra_bond_parse_ticker("BOND:ISSUER:ORACLE"), None);
    }

    #[test]
    fn finra_bond_parses_eod_fixture() {
        let v: serde_json::Value = serde_json::from_str(EOD_FIXTURE).unwrap();
        let (rows, total) = finra_bond_unwrap(&v).unwrap();
        assert_eq!(total, 4);
        let daily = finra_bond_parse_eod(&rows);
        assert_eq!(daily.len(), 4);
        assert_eq!(daily[0].t, ts("2026-10-26T00:00:00Z"));
        assert_eq!(daily[0].price, Some(81.0));
        let px = finra_bond_daily_to_candles(&daily, FinraBondField::Price);
        let yl = finra_bond_daily_to_candles(&daily, FinraBondField::Yield);
        assert_eq!(px[3].c, 84.0625);
        assert_eq!(yl[3].c, 14.37);
        assert_eq!(yl[3].t, ts("2026-10-29T00:00:00Z"));
    }

    #[test]
    fn finra_bond_parses_trades_fixture() {
        let v: serde_json::Value = serde_json::from_str(TRADES_FIXTURE).unwrap();
        let (rows, total) = finra_bond_unwrap(&v).unwrap();
        assert_eq!(total, 60);
        let trades = finra_bond_parse_trades(&rows);
        assert_eq!(trades.len(), 4);
        // 10:52:32 EDT (Oct 30 is before the Nov 1 DST end) -> 14:52:32 UTC.
        assert_eq!(trades[0].t, ts("2026-10-30T14:52:32Z"));
        assert_eq!(trades[2].volume, Some(1e6));
        let hourly = finra_bond_trades_to_candles(&trades, FinraBondField::Price, Duration::hours(1));
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].t, ts("2026-10-30T14:00:00Z"));
        assert_eq!(hourly[0].o, 82.711);
        assert_eq!(hourly[0].l, 82.701);
        assert_eq!(hourly[0].c, 82.701);
        assert_eq!(hourly[0].v, Some(1e6));
        assert_eq!(hourly[1].h, 83.0);
        assert_eq!(hourly[1].c, 82.98);
    }

    #[test]
    fn finra_bond_parses_search_fixture() {
        let v: serde_json::Value = serde_json::from_str(SEARCH_FIXTURE).unwrap();
        let (rows, _) = finra_bond_unwrap(&v).unwrap();
        let refs = finra_bond_parse_refs(&rows);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].cusip, "68389XAR6"); // sorted by maturity
        assert_eq!(refs[0].last_yield, None);
        assert_eq!(refs[1].symbol.as_deref(), Some("ORCL6302044"));
        assert_eq!(refs[1].coupon, Some(6.85));
        assert_eq!(refs[1].last_price, Some(81.08));
    }

    #[test]
    fn finra_bond_failure_is_error() {
        let v = serde_json::json!({"status": "failure", "statusMessage": "Bad Request."});
        assert!(finra_bond_unwrap(&v).is_err());
    }

    #[test]
    fn finra_bond_eastern_offset_dst_boundary() {
        let d = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(finra_bond_eastern_offset(d("2026-10-30 10:00:00")), 4);
        assert_eq!(finra_bond_eastern_offset(d("2026-11-02 10:00:00")), 5);
        assert_eq!(finra_bond_eastern_offset(d("2026-01-15 10:00:00")), 5);
    }

    /// Live: cargo test -p eli-core finra_bond_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn finra_bond_live() {
        let end = Utc::now();
        let start = end - Duration::days(60);
        let day = Span { n: 1, unit: SpanUnit::Day };
        let tickers = vec!["BOND:21873SAG3:PX".to_string(), "BOND:21873SAG3:YLD".to_string()];
        let (series, errs) = fetch_finra_bond_series(&tickers, start, end, day).await.unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        for s in &series {
            println!("{} ({} candles)", s.ticker, s.candles.len());
            for c in &s.candles {
                println!("  {} {}", c.t.format("%Y-%m-%d"), c.c);
            }
        }
        assert_eq!(series.len(), 2);
        assert!(series[0].candles.len() > 20);

        let hour = Span { n: 1, unit: SpanUnit::Hour };
        let (intra, errs) = fetch_finra_bond_series(
            &["BOND:CRWV6364372".to_string()],
            end - Duration::days(3),
            end,
            hour,
        )
        .await
        .unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        for c in &intra[0].candles {
            println!("  1h {} o{} h{} l{} c{} v{:?}", c.t, c.o, c.h, c.l, c.c, c.v);
        }

        let refs = finra_bond_search("oracle corp").await.unwrap();
        assert!(!refs.is_empty());
        let longest = refs.last().unwrap();
        println!("ORACLE longest: {longest:?} ({} outstanding)", refs.len());
    }
}
