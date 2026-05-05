#[derive(Debug, Deserialize)]
struct OddsCsvRow {
    source: String,
    ticker: String,
    title: String,
    event_ticker: String,
    yes_price: String,
    volume: String,
    #[serde(default)]
    status: String,
    probability: String,
}

#[derive(Debug, Clone)]
struct MeetingMeta {
    date: chrono::NaiveDate,
    label: String,
}

/// Per-meeting aggregator. Stores VOLUME-WEIGHTED sums so multiple markets
/// pricing the same outcome (e.g., one Polymarket + one Kalshi for "Fed holds in
/// June 2026") collapse into a single liquidity-weighted probability rather than
/// being max-aggregated. Junk thin markets ($1K volume) no longer override deep
/// markets ($17M volume).
#[derive(Debug, Clone, Default)]
struct MeetingAgg {
    /// sum_i (prob_i * volume_i)
    hold_prob_x_vol: f64,
    cut_25bp_prob_x_vol: f64,
    cut_50bp_plus_prob_x_vol: f64,
    hike_prob_x_vol: f64,
    /// sum of volumes from markets that contributed to each bucket
    hold_vol: i64,
    cut_25bp_vol: i64,
    cut_50bp_plus_vol: i64,
    hike_vol: i64,
    /// Total volume across all markets contributing to this meeting
    volume: i64,
    /// Count of distinct markets aggregated (after de-dup)
    n_markets: usize,
}

impl MeetingAgg {
    fn add(&mut self, bucket: FedBucket, prob: f64, vol: i64) {
        let v_f = vol as f64;
        match bucket {
            FedBucket::Hold => {
                self.hold_prob_x_vol += prob * v_f;
                self.hold_vol += vol;
            }
            FedBucket::Cut25 => {
                self.cut_25bp_prob_x_vol += prob * v_f;
                self.cut_25bp_vol += vol;
            }
            FedBucket::Cut50Plus => {
                self.cut_50bp_plus_prob_x_vol += prob * v_f;
                self.cut_50bp_plus_vol += vol;
            }
            FedBucket::Hike => {
                self.hike_prob_x_vol += prob * v_f;
                self.hike_vol += vol;
            }
        }
        self.volume += vol;
        self.n_markets += 1;
    }

    /// Volume-weighted probability per bucket. Returns 0.0 if no volume contributed.
    fn weighted(&self) -> (f64, f64, f64, f64) {
        let safe = |sum: f64, vol: i64| if vol > 0 { sum / vol as f64 } else { 0.0 };
        (
            safe(self.hold_prob_x_vol, self.hold_vol),
            safe(self.cut_25bp_prob_x_vol, self.cut_25bp_vol),
            safe(self.cut_50bp_plus_prob_x_vol, self.cut_50bp_plus_vol),
            safe(self.hike_prob_x_vol, self.hike_vol),
        )
    }
}

/// Skip markets with volume below this threshold. They tend to be thin/junk
/// markets where one trade can pin a 99% probability that doesn't reflect
/// real consensus. $10K USD chosen as a conservative liquidity floor.
const MIN_MARKET_VOLUME: i64 = 10_000;

/// Lower threshold specifically for Kalshi KXFEDDECISION binaries. Each
/// per-meeting event has 5 outcome contracts (H0/H25/H26/C25/C26); individual
/// binary volumes are naturally small even when the meeting-event total is
/// healthy. $500 keeps out one-off junk pins without hiding Sep/Oct/Dec
/// meetings whose binaries trade in the $1-7K range each.
const KALSHI_FED_MIN_MARKET_VOLUME: i64 = 500;

#[derive(Debug, Clone, Copy)]
enum FedBucket {
    Hold,
    Cut25,
    Cut50Plus,
    Hike,
}

