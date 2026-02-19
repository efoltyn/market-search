#[derive(Debug, Deserialize)]
struct OddsCsvRow {
    source: String,
    ticker: String,
    title: String,
    event_ticker: String,
    yes_price: String,
    volume: String,
    status: String,
    probability: String,
    category: String,
    topic: String,
}

