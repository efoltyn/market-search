### `eli finance rate-path`

Implied Fed policy trajectory from local prediction-market cache.

```bash
eli finance sync --max-pages 10
eli finance rate-path
```

**Returns:**
- `current_rate` (Fed funds anchor)
- `meetings[]` with `date`, `label`, and bucketed probabilities:
  - `hold_prob`
  - `cut_25bp_prob`
  - `cut_50bp_plus_prob`
  - `hike_prob`
- `implied_rate` per meeting

**Notes:**
- Uses local CSV cache (`eli finance sync` required)
- Prefers meeting-level contracts; if unavailable, falls back to annual Fed-cuts markets with a warning
- Best for one-call policy-path context before deeper market analysis
