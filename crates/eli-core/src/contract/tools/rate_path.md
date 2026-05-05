### `eli finance rate-path`

Implied Fed policy trajectory from live Polymarket + Kalshi prediction-market data.

```bash
eli finance rate-path
```

**Returns:**
- `current_rate` (Fed funds anchor) + `current_rate_basis`
- `meetings[]` — per-FOMC-meeting (dates snapped to actual decision day):
  - `date`, `label`
  - `hold_prob`, `cut_prob`, `cut_25bp_prob`, `cut_50bp_plus_prob`, `hike_prob` (volume-weighted across venues)
  - `n_markets`, `volume_total` (so caller can judge depth)
- `year_view` (when cardinality markets exist):
  - `cuts_distribution` — `{0: P(no cuts), 1: P(1 cut), ...}` from "How many cuts in 2026?"
  - `eoy_rate_distribution` — `{"3.75%": 0.55, "3.50%": 0.20, ...}` from "What will rate be at end of 2026?"
  - `cut_by_meeting_distribution` — `{"december": 0.46, "october": 0.20, ...}` from "Cut by which meeting?"
- `compound_paths[]` — joint multi-meeting probabilities ("Pause-Pause-Pause across Mar-Jun"), kept SEPARATE from per-meeting buckets

**Notes:**
- Live API is the hot path; CSV cache is disaster fallback only
- Joint multi-meeting compound markets are excluded from per-meeting buckets (they would drag the marginals)
- Far-future thin pins (single-binary <$5K vol on 2027/2028 dates) are filtered to keep output focused
