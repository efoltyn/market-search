use eli_finance_types::OddsListedMarket;
use rusqlite::{params, Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Row returned from FTS5 search.
#[derive(Debug, Clone)]
pub struct MarketRow {
    pub source: String,
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    pub yes_price: Option<i64>,
    pub volume: Option<i64>,
    pub status: Option<String>,
    pub probability: Option<f64>,
    pub category: Option<String>,
    pub slug: Option<String>,
    pub synced_at: String,
    pub fts_rank: f64,
}

/// Filters applied to FTS5 search.
#[derive(Debug, Default)]
pub struct SearchFilters {
    pub category: Option<String>,
    pub min_volume: Option<i64>,
    pub status: Option<String>,
    pub source: Option<String>,
    pub exclude_mentions: bool,
}

/// Default DB path: alongside existing CSV cache.
pub fn default_db_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds").join("markets.db"))
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("eli-odds-cache")
                .join("markets.db")
        })
}

/// Open (or create) the markets database and ensure schema exists.
pub fn open_markets_db(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create cache dir: {e}"))?;
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open DB: {e}"))?;

    // Performance pragmas for local cache DB.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -4000;
         PRAGMA busy_timeout = 3000;",
    )
    .map_err(|e| format!("pragmas: {e}"))?;

    init_schema(&conn)?;
    Ok(conn)
}

/// Open DB read-only (for search). Returns None if DB doesn't exist.
pub fn open_markets_db_readonly(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS markets (
            id           INTEGER PRIMARY KEY,
            source       TEXT NOT NULL,
            ticker       TEXT NOT NULL,
            title        TEXT NOT NULL,
            event_ticker TEXT NOT NULL,
            yes_price    INTEGER,
            volume       INTEGER,
            status       TEXT,
            probability  REAL,
            category     TEXT,
            slug         TEXT,
            clob_token_ids TEXT,
            synced_at    TEXT NOT NULL,
            UNIQUE(source, ticker)
        );

        CREATE INDEX IF NOT EXISTS idx_markets_event ON markets(event_ticker);
        CREATE INDEX IF NOT EXISTS idx_markets_source ON markets(source);
        CREATE INDEX IF NOT EXISTS idx_markets_status ON markets(status);

        CREATE VIRTUAL TABLE IF NOT EXISTS markets_fts USING fts5(
            title,
            ticker,
            event_ticker,
            category,
            content='markets',
            content_rowid='id',
            tokenize='porter unicode61 remove_diacritics 2'
        );

        CREATE TRIGGER IF NOT EXISTS markets_ai AFTER INSERT ON markets BEGIN
            INSERT INTO markets_fts(rowid, title, ticker, event_ticker, category)
            VALUES (new.id, new.title, new.ticker, new.event_ticker, new.category);
        END;

        CREATE TRIGGER IF NOT EXISTS markets_ad AFTER DELETE ON markets BEGIN
            INSERT INTO markets_fts(markets_fts, rowid, title, ticker, event_ticker, category)
            VALUES ('delete', old.id, old.title, old.ticker, old.event_ticker, old.category);
        END;

        CREATE TRIGGER IF NOT EXISTS markets_au AFTER UPDATE ON markets BEGIN
            INSERT INTO markets_fts(markets_fts, rowid, title, ticker, event_ticker, category)
            VALUES ('delete', old.id, old.title, old.ticker, old.event_ticker, old.category);
            INSERT INTO markets_fts(rowid, title, ticker, event_ticker, category)
            VALUES (new.id, new.title, new.ticker, new.event_ticker, new.category);
        END;

        CREATE TABLE IF NOT EXISTS sync_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
    .map_err(|e| format!("init schema: {e}"))?;
    Ok(())
}

/// Upsert a batch of markets from sync. Returns count of rows written.
///
/// When `full_replace` is true, all existing rows for this source are deleted
/// first (full catalog sync). When false, rows are inserted-or-replaced
/// individually (incremental/stream refresh — new markets added, existing
/// ones updated, stale ones left for later pruning).
pub fn upsert_markets(
    conn: &Connection,
    markets: &[OddsListedMarket],
    source: &str,
    synced_at: &str,
    full_replace: bool,
) -> Result<usize, String> {
    if full_replace {
        // Full sync: wipe this source then bulk-insert. FTS triggers fire on DELETE.
        conn.execute("DELETE FROM markets WHERE source = ?1", params![source])
            .map_err(|e| format!("delete old {source}: {e}"))?;
    }

    let mut stmt = conn
        .prepare_cached(
            "INSERT OR REPLACE INTO markets
                (source, ticker, title, event_ticker, yes_price, volume, status,
                 probability, category, slug, clob_token_ids, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .map_err(|e| format!("prepare upsert: {e}"))?;

    let mut count = 0usize;
    for m in markets {
        let clob_json = m
            .clob_token_ids
            .as_ref()
            .map(|ids| serde_json::to_string(ids).unwrap_or_default());
        let category = m
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Uncategorized");

        stmt.execute(params![
            source,
            m.ticker,
            m.title,
            m.event_ticker,
            m.yes_price,
            m.volume,
            m.status.as_deref().unwrap_or(""),
            m.probability_yes,
            category,
            m.slug.as_deref(),
            clob_json,
            synced_at,
        ])
        .map_err(|e| format!("insert market {}: {e}", m.ticker))?;
        count += 1;
    }

    Ok(count)
}

/// Remove markets that haven't been updated since `cutoff` (ISO8601 string).
/// Returns the number of rows pruned.
pub fn prune_stale_markets(conn: &Connection, cutoff: &str) -> Result<usize, String> {
    let deleted = conn
        .execute("DELETE FROM markets WHERE synced_at < ?1", params![cutoff])
        .map_err(|e| format!("prune stale: {e}"))?;
    Ok(deleted)
}

fn tokenize_fts_query(raw: &str) -> Vec<&str> {
    raw.split_whitespace().filter(|t| !t.is_empty()).collect()
}

/// Build a strict phrase query for multi-word search terms.
fn build_phrase_fts_query(raw: &str) -> Option<String> {
    let tokens = tokenize_fts_query(raw);
    if tokens.len() < 2 {
        return None;
    }
    Some(format!("\"{}\"", tokens.join(" ").replace('"', " ")))
}

/// Build a broad fallback FTS5 query from user input.
/// "federal reserve" → "federal AND reserve"
/// "recession" → "recession"
/// Short terms (≤2 chars) are quoted to avoid FTS5 treating them as operators.
fn build_fts_query(raw: &str) -> String {
    let tokens = tokenize_fts_query(raw);
    if tokens.is_empty() {
        return String::new();
    }
    if tokens.len() == 1 {
        // Single term: use prefix matching with *
        let t = tokens[0];
        if t.len() <= 2 {
            return format!("\"{}\"", t);
        }
        return format!("{}*", t);
    }
    // Multiple terms: AND them together, prefix on the final term.
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if t.len() <= 2 {
                format!("\"{}\"", t)
            } else if i == tokens.len() - 1 {
                format!("{}*", t)
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Search markets using FTS5. Returns results ordered by relevance + volume.
pub fn search_markets(
    conn: &Connection,
    query: &str,
    limit: usize,
    filters: &SearchFilters,
) -> Result<Vec<MarketRow>, String> {
    let mut fts_queries = Vec::new();
    if let Some(phrase) = build_phrase_fts_query(query) {
        fts_queries.push(phrase);
    }
    let fallback_query = build_fts_query(query);
    if !fallback_query.is_empty() {
        fts_queries.push(fallback_query);
    }
    if fts_queries.is_empty() {
        return Ok(Vec::new());
    }
    for fts_query in fts_queries {
        let mut conditions = Vec::new();
        let mut run_bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        run_bind_values.push(Box::new(fts_query));
        if let Some(ref status) = filters.status {
            let normalized = status.trim().to_ascii_lowercase();
            if normalized == "open" {
                // Kalshi commonly stores open markets as "active" or "initialized".
                conditions.push(
                    "LOWER(COALESCE(m.status, '')) IN ('', 'open', 'active', 'initialized')"
                        .to_string(),
                );
            } else {
                conditions.push(format!(
                    "LOWER(COALESCE(m.status, '')) = ?{}",
                    run_bind_values.len() + 1
                ));
                run_bind_values.push(Box::new(normalized));
            }
        }
        if let Some(ref category) = filters.category {
            conditions.push(format!(
                "m.category LIKE '%' || ?{} || '%'",
                run_bind_values.len() + 1
            ));
            run_bind_values.push(Box::new(category.clone()));
        }
        if let Some(min_vol) = filters.min_volume {
            conditions.push(format!("m.volume >= ?{}", run_bind_values.len() + 1));
            run_bind_values.push(Box::new(min_vol));
        }
        if let Some(ref src) = filters.source {
            conditions.push(format!("m.source = ?{}", run_bind_values.len() + 1));
            run_bind_values.push(Box::new(src.clone()));
        }
        if filters.exclude_mentions {
            conditions.push(
                "m.event_ticker NOT LIKE '%MENTION%' \
                 AND m.event_ticker NOT LIKE 'KXSOTU%' \
                 AND m.event_ticker NOT LIKE 'KXTWEET%' \
                 AND m.event_ticker NOT LIKE 'KXPRESMENTION%' \
                 AND m.event_ticker NOT LIKE 'KXENTMENTION%'"
                    .to_string(),
            );
        }
        let where_extra = if conditions.is_empty() {
            String::new()
        } else {
            format!(" AND {}", conditions.join(" AND "))
        };
        let sql = format!(
            "SELECT m.source, m.ticker, m.title, m.event_ticker,
                    m.yes_price, m.volume, m.status, m.probability,
                    m.category, m.slug, m.synced_at, fts.rank
             FROM markets_fts fts
             JOIN markets m ON fts.rowid = m.id
             WHERE markets_fts MATCH ?1{where_extra}
             ORDER BY fts.rank, COALESCE(m.volume, 0) DESC
             LIMIT {limit}"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare search: {e}"))?;
        let bind_refs: Vec<&dyn rusqlite::types::ToSql> =
            run_bind_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(bind_refs.as_slice(), |row| {
                Ok(MarketRow {
                    source: row.get(0)?,
                    ticker: row.get(1)?,
                    title: row.get(2)?,
                    event_ticker: row.get(3)?,
                    yes_price: row.get(4)?,
                    volume: row.get(5)?,
                    status: row.get(6)?,
                    probability: row.get(7)?,
                    category: row.get(8)?,
                    slug: row.get(9)?,
                    synced_at: row.get(10)?,
                    fts_rank: row.get(11)?,
                })
            })
            .map_err(|e| format!("query: {e}"))?;

        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(r) => results.push(r),
                Err(e) => eprintln!("[odds_db] row error: {e}"),
            }
        }
        if !results.is_empty() {
            return Ok(results);
        }
    }
    Ok(Vec::new())
}

/// Get distinct event tickers from search results (for hydration grouping).
pub fn get_event_tickers(results: &[MarketRow]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in results {
        let key = (r.source.clone(), r.event_ticker.clone());
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

pub fn set_sync_meta(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO sync_meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map_err(|e| format!("set meta: {e}"))?;
    Ok(())
}

pub fn get_sync_meta(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare("SELECT value FROM sync_meta WHERE key = ?1")
        .map_err(|e| format!("prepare meta: {e}"))?;
    let result = stmt.query_row(params![key], |row| row.get(0)).ok();
    Ok(result)
}

/// Count total markets in DB.
pub fn market_count(conn: &Connection) -> Result<usize, String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM markets", [], |row| row.get(0))
        .map_err(|e| format!("count: {e}"))?;
    Ok(count as usize)
}

/// Count markets by source.
pub fn market_count_by_source(conn: &Connection, source: &str) -> Result<usize, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM markets WHERE source = ?1",
            params![source],
            |row| row.get(0),
        )
        .map_err(|e| format!("count by source: {e}"))?;
    Ok(count as usize)
}

#[cfg(test)]
mod tests {
    use super::{build_fts_query, build_phrase_fts_query};

    #[test]
    fn build_fts_query_uses_prefix_for_single_term() {
        assert_eq!(build_fts_query("recession"), "recession*");
    }

    #[test]
    fn build_phrase_fts_query_for_multiword() {
        assert_eq!(
            build_phrase_fts_query("march madness"),
            Some("\"march madness\"".to_string())
        );
    }
}
