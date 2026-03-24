### `eli finance search`

Find ticker symbols or FRED series IDs.

```bash
eli finance search --query "nvidia"
eli finance search --query "unemployment rate"
eli finance search --query "treasury 10 year"
eli finance search --query "apple options" --provider ibkr --ibkr-account U1234567
```

**Returns:**
- `symbol`, `name`, `exchange`, `asset_type`, `score`
- Includes international listings and ETFs
- Also searches FRED macro series

**Use when:**
- You don't know the exact ticker
- User mentions company name, need symbol
- Looking for FRED series ID
- You want IBKR-native contract discovery for tradable instruments and exchange-aware symbols

**Examples:**
- "nvidia" → NVDA, NVDX (2x ETF), NVD.DE (German listing)
- "unemployment" → UNRATE, UNEMPLOY, etc.
- "gold" → GC=F (futures), GLD (ETF), IAU (ETF)

**Provider notes:**
- Default provider is Yahoo + policy-driven FRED suggestions.
- `--provider ibkr` uses native IBKR symbol matching and is better for exact tradable contract lookup.
