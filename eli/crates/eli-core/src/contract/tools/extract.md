### `eli extract`

Purpose: Extract key facts from large content (articles, filings, crawled pages).

Usage:
```
eli extract --url <URL> [--bullets N] [--focus "topic"]
eli extract --file <PATH> [--bullets N] [--focus "topic"]
eli extract --text "content" [--bullets N] [--focus "topic"]
```

Options:
- `--url`: Fetch and extract from URL
- `--file`: Extract from local file
- `--text`: Extract from inline text (use heredoc for large content)
- `--bullets`: Number of bullet points to extract (default: 10)
- `--focus`: Focus extraction on specific topic (e.g., "financial metrics", "guidance")
- `--out`: Write output to file instead of stdout

Output:
Returns JSON with:
- `source`: URL, file path, or "inline"
- `bullets`: Array of extracted facts
- `word_count`: Original content word count
- `extracted_at`: Timestamp

Rules:
- Use when content > 5KB and you need summary
- Skip when user needs exact quotes
- Skip when content is already short
- Each bullet should contain a number, date, or named entity
- Focus on facts, not opinions

Example:
```bash
eli extract --url https://example.com/article --bullets 5 --focus "revenue"
```
