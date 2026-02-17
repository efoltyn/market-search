### `eli finance macro`

Key economic indicators in one call.

```bash
eli finance macro
eli finance macro --range 6mo
eli finance macro --compare-to 2020-03-01
```

**Returns:**
| Indicator | Symbol | Example |
|-----------|--------|---------|
| CPI (Inflation) | CPIAUCSL | 326.03 (+3% YoY) |
| Unemployment | UNRATE | 4.4% |
| Payrolls | PAYEMS | 159,526K |
| Fed Funds Rate | FEDFUNDS | 3.72% |
| Real GDP | GDPC1 | $24T |
| Yield Curve (10Y-2Y) | T10Y2Y | 0.74% |
| M2 Money Supply | M2SL | $22.4T |
| WTI Oil | DCOILWTICO | $60.46 |

Each indicator includes:
- `category` (`inflation`, `employment`, `gdp`, `rates`, `debt`, `money`, `consumer`, `credit`, `commodities`)
- `current_value`
- `change_1y` (percent change)
- optional compare fields when `--compare-to` is set:
  - `compare_value`
  - `delta_abs`
  - `delta_pct`

**Use for:** Quick macro context, rate environment, inflation trends.
