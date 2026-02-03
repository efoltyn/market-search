### `eli finance prices`

Live spot prices from Pyth (crypto, commodities, FX, metals).

**Two-step process:**

```bash
# Step 1: Discover feed ID
eli finance prices --query "btc" --asset-type crypto
# Returns candidates with IDs

# Step 2: Fetch price by ID
eli finance prices --ids e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43
```

**Asset types:** `crypto`, `equity`, `fx`, `metal`, `rates`, `commodities`

**Returns:**
- `symbol`, `value` (price), `timestamp`

**When to use:**
- Real-time crypto prices (BTC, ETH, etc.)
- Live commodity prices (gold, oil)
- FX rates

**For stocks:** Use `snapshot` instead - faster, more data.
