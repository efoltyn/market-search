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

/// Per-meeting aggregator. Within one venue the legs that fall in a bucket
/// are mutually exclusive outcomes (Kalshi H25 + H26 are both "hike"), so
/// their probabilities SUM. Across venues each venue's bucket probability is
/// VOLUME-WEIGHTED, so one Polymarket + one Kalshi view of "Fed holds in June"
/// collapse into a liquidity-weighted probability and thin junk markets ($1K)
/// do not override deep ones ($17M). (Averaging legs inside a bucket used to
/// report Kalshi's 74.5% + 1.5% hike legs as a ~42% hike.)
#[derive(Debug, Clone, Default)]
struct MeetingAgg {
    /// (source, bucket) → (summed leg probability, summed leg volume)
    by_source: std::collections::BTreeMap<(String, u8), (f64, i64)>,
    /// Total volume across all markets contributing to this meeting
    volume: i64,
    /// Count of distinct markets aggregated (after de-dup)
    n_markets: usize,
}

impl MeetingAgg {
    fn add(&mut self, source: &str, bucket: FedBucket, prob: f64, vol: i64) {
        let key = (source.to_string(), bucket as u8);
        let slot = self.by_source.entry(key).or_insert((0.0, 0));
        slot.0 += prob;
        slot.1 += vol;
        self.volume += vol;
        self.n_markets += 1;
    }

    /// Per bucket: each venue's summed probability, volume-weighted across
    /// venues. Returns 0.0 for a bucket no venue priced.
    fn weighted(&self) -> (f64, f64, f64, f64) {
        let bucket = |b: FedBucket| -> f64 {
            let (mut px, mut v) = (0.0_f64, 0_i64);
            for ((_, key), (p, vol)) in &self.by_source {
                if *key == b as u8 {
                    px += p.clamp(0.0, 1.0) * *vol as f64;
                    v += vol;
                }
            }
            if v > 0 { px / v as f64 } else { 0.0 }
        };
        (
            bucket(FedBucket::Hold),
            bucket(FedBucket::Cut25),
            bucket(FedBucket::Cut50Plus),
            bucket(FedBucket::Hike),
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


#[cfg(test)]
mod meeting_agg_tests {
    use super::*;

    #[test]
    fn legs_in_one_bucket_sum_within_a_venue_and_weight_across_venues() {
        let mut agg = MeetingAgg::default();
        // Kalshi Dec: H25 74.5% + H26 1.5% are both "hike".
        agg.add("kalshi", FedBucket::Hike, 0.745, 674_179);
        agg.add("kalshi", FedBucket::Hike, 0.015, 529_666);
        agg.add("kalshi", FedBucket::Hold, 0.245, 892_504);
        let (hold, _, _, hike) = agg.weighted();
        assert!((hike - 0.76).abs() < 1e-9);
        assert!((hold - 0.245).abs() < 1e-9);
        // A second venue is volume-weighted against the first.
        agg.add("polymarket", FedBucket::Hike, 0.70, 1_203_845);
        let (_, _, _, hike) = agg.weighted();
        assert!((hike - 0.73).abs() < 1e-9);
        assert_eq!(agg.n_markets, 4);
    }
}
