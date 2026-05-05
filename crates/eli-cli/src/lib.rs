#![forbid(unsafe_code)]

include!("context.rs");
include!("chat/footer.rs");
include!("chat/slash.rs");
include!("args.rs");
include!("dispatch.rs");
include!("cmd/code.rs");
include!("cmd/sentinel.rs");
include!("cmd/mcp.rs");
include!("cmd/agent.rs");
include!("cmd/web.rs");
include!("cmd/finance/macro_schedule.rs");
include!("cmd/finance/odds.rs");
include!("cmd/finance/options.rs");
include!("cmd/finance/paper.rs");
include!("cmd/finance/ibkr.rs");
include!("cmd/finance/misc.rs");
include!("cmd/finance/timeseries.rs");
include!("cmd/finance/curve.rs");
include!("cmd/finance/fed.rs");
include!("cmd/finance/mod.rs");
include!("cmd/picks.rs");
// include!("cmd/serve.rs"); // TODO: fix Send lifetime issues before re-enabling
include!("chat/runtime.rs");
include!("chat/ui.rs");
include!("rust_tools/syn_map.rs");
