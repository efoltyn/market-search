# Eli Roadmap: Precision Tools

## Status: IMPLEMENTED ✅

All three systems have been built and are ready for testing.

---

## What Was Built

### 1. System Prompt Update ✅
**File:** `eli-core/src/contract/mod.rs`

Added:
- Data source guidance (prefer structured data over articles)
- `/copy` command documentation for agent self-use
- `eli extract` tool documentation

### 2. /copy Command ✅
**Location:** TUI slash command

```bash
/copy              # Last response → clipboard
/copy all          # Full session → clipboard
/copy all > file   # Full session → file
/copy user         # All user messages
/copy tools        # All tool outputs
/copy last 5       # Last 5 turns
/copy all -data    # Exclude large tool payloads
```

### 3. eli web extract ✅
**Files:**
- `eli-core/src/extraction.rs`
- `eli-core/src/contract/tools/extract.md`

```bash
eli web extract --url <URL> --bullets 10 --focus "revenue"
eli web extract --file article.txt --bullets 5
eli web extract --text "content" --bullets 10
```

---

## How to Test

### Test /copy
1. Start eli: `eli chat`
2. Have a conversation
3. Type `/copy` to copy last response
4. Type `/copy all` to copy full session
5. Type `/copy all > test.md` to save to file
6. Type `/copy tools` to see tool outputs
7. Type `/help` to see all options

### Test extract
```bash
# Extract from URL
eli web extract --url https://example.com --bullets 5

# Extract from text
eli web extract --text "Apple reported revenue of \$94B..." --bullets 3

# Extract with focus
eli web extract --url https://... --focus "guidance" --bullets 10
```

### Test Data Guidance
Start a research session and observe:
- Agent should prefer odds/prices over web search
- Agent should use web tools only when structured data isn't available
- Agent should mention data preference in its reasoning

---

## Files Changed

| File | Change |
|------|--------|
| `eli-core/src/contract/mod.rs` | System prompt + tool docs |
| `eli-core/src/extraction.rs` | Extraction logic (NEW) |
| `eli-core/src/lib.rs` | Export extraction module |
| `eli-core/src/contract/tools/extract.md` | Tool doc (NEW) |
| `eli-cli/src/lib.rs` | /copy command + eli web extract |

---

## Future Improvements (Based on Usage)

- [ ] LLM-powered extraction (subagent) for better quality
- [ ] Extraction caching by URL hash
- [ ] /copy format options (json, compact, markdown)
- [ ] Token count warnings for large /copy
- [ ] Agent self-use of /copy via session file access

---

## Design Principles Followed

1. **Subtraction > Construction** - /copy uses ignore flags, not "only" flags
2. **Opt-in > Automatic** - Extraction is a tool, not middleware
3. **Guidance > Rules** - Data preference is taught, not enforced
4. **Simple First** - Shipped minimal versions, can extend later
