use super::super::*;

include!("analytics.rs");
include!("service.rs");
include!("cache.rs");
include!("mock.rs");
include!("yahoo.rs");
include!("fred.rs");
include!("pyth.rs");
include!("fed_h15.rs");
include!("binance.rs");
include!("govt.rs");
include!("finra_bonds.rs");
include!("ust_buyback.rs");
include!("nyfed_ops.rs");
include!("ofr.rs");
include!("etf_flows.rs");
include!("cboe.rs");
include!("hyperliquid.rs");
include!("kraken_futures.rs");
include!("deribit.rs");
include!("secnav.rs");
include!("rng.rs");

/// True when `ticker` starts with `prefix` in any case and has something
/// after it. It never panics: a fixed byte slice (`t[..5]`) does when the
/// index falls inside a multi-byte character, which a pasted ticker with an
/// en dash is enough to cause.
fn ticker_has_prefix(ticker: &str, prefix: &str) -> bool {
    ticker.len() > prefix.len()
        && ticker
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}
