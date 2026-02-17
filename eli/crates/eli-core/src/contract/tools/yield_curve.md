### `eli finance yield-curve`

US Treasury curve in one call with key spreads.

```bash
eli finance yield-curve
eli finance yield-curve --compare 3mo,1y
```

**Returns:**
- `curve[]` across 1mo to 30y
- `spread_2y10y` (optional when required tenors are unavailable)
- `spread_3mo10y` (optional when required tenors are unavailable)
- Optional change fields in **basis points**:
  - `change_3mo_bps`
  - `change_1y_bps`
- `missing_symbols` for degraded but usable partial responses

**Use for:** Recession/rates regime checks without manually stitching many FRED series.
