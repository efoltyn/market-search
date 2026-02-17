### `eli finance dashboard`

Preset multi-tool macro dashboard.

```bash
eli finance dashboard --preset recession
```

**v1 preset:** `recession`

Includes:
- `macro_data`
- `snapshots` (SPY, TLT, HYG, GLD, UUP)
- `odds` searches (`recession`, `unemployment`, `federal reserve`)
- `options` summary (SPY)
- `rate_path`

**Behavior:** Returns partial sections with `warnings` if one provider fails.
