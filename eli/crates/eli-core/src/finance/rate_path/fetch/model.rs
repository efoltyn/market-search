#[derive(Debug, Deserialize)]
struct OddsCsvRow {
    source: String,
    ticker: String,
    title: String,
    event_ticker: String,
    yes_price: String,
    probability: String,
}

#[derive(Debug, Clone)]
struct MeetingMeta {
    date: chrono::NaiveDate,
    label: String,
}

#[derive(Debug, Clone, Default)]
struct MeetingAgg {
    hold_prob: f64,
    cut_25bp_prob: f64,
    cut_50bp_plus_prob: f64,
    hike_prob: f64,
}

#[derive(Debug, Clone, Copy)]
enum FedBucket {
    Hold,
    Cut25,
    Cut50Plus,
    Hike,
}

