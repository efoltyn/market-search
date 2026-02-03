### `eli finance macro`

Key economic indicators in one call.

```bash
eli finance macro
eli finance macro --range 6mo
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

Each indicator shows `current_value` and `change_1y` (percent change).

**Use for:** Quick macro context, rate environment, inflation trends.
