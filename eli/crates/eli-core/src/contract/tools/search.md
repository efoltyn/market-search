### `eli finance search`

Find ticker symbols or FRED series IDs.

```bash
eli finance search --query "nvidia"
eli finance search --query "unemployment rate"
eli finance search --query "treasury 10 year"
```

**Returns:**
- `symbol`, `name`, `exchange`, `asset_type`, `score`
- Includes international listings and ETFs
- Also searches FRED macro series

**Use when:**
- You don't know the exact ticker
- User mentions company name, need symbol
- Looking for FRED series ID

**Examples:**
- "nvidia" → NVDA, NVDX (2x ETF), NVD.DE (German listing)
- "unemployment" → UNRATE, UNEMPLOY, etc.
- "gold" → GC=F (futures), GLD (ETF), IAU (ETF)
