pub(crate) fn sanitize_for_filename(input: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_sep = false;
        } else if !out.is_empty() && !last_sep {
            out.push('_');
            last_sep = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "file".to_string()
    } else {
        out
    }
}

fn truncate_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if input.len() <= max {
        return input.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in input.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    out.push_str("\n… [truncated]");
    out
}

pub(crate) fn best_effort_sec_filing_excerpt(
    text: &str,
    form: &str,
    items: Option<&str>,
    max_chars: usize,
) -> String {
    let max_chars = max_chars.max(256);
    if text.trim().is_empty() {
        return String::new();
    }

    let mut header_candidates: Vec<usize> = Vec::new();
    let mut item_candidates: Vec<usize> = Vec::new();

    // Common SEC filing anchors (prefer starting after any iXBRL/header noise).
    for needle in [
        "SECURITIES AND EXCHANGE COMMISSION",
        "Securities and Exchange Commission",
        "UNITED STATES\n\nSECURITIES",
    ] {
        if let Some(idx) = text.find(needle) {
            header_candidates.push(idx);
        }
    }

    let form = form.trim();
    if !form.is_empty() {
        for needle in [format!("FORM {form}"), format!("Form {form}")] {
            if let Some(idx) = text.find(&needle) {
                header_candidates.push(idx);
            }
        }
    }

    if let Some(raw) = items {
        // SEC "items" can look like "1.01,2.03" or "1.01 2.03"
        for item in raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            for needle in [format!("ITEM {item}"), format!("Item {item}")] {
                if let Some(idx) = text.find(&needle) {
                    item_candidates.push(idx);
                }
            }
        }
    }

    let header_idx = header_candidates.into_iter().min();
    let item_idx = item_candidates.into_iter().min();

    let mut start = match (header_idx, item_idx) {
        (Some(h), Some(i)) => {
            // If the filing cover page is huge, prefer jumping to the first disclosed item.
            let jump_to_item_threshold = 5_000usize;
            if i > h && i.saturating_sub(h) > jump_to_item_threshold {
                i
            } else {
                h.min(i)
            }
        }
        (Some(h), None) => h,
        (None, Some(i)) => i,
        (None, None) => 0,
    };

    // Snap start to a sensible boundary (previous blank line if possible).
    if start > 0 {
        if let Some(boundary) = text[..start].rfind("\n\n") {
            start = boundary + 2;
        } else if let Some(boundary) = text[..start].rfind('\n') {
            start = boundary + 1;
        }
    }

    let excerpt = text[start..].trim_start();
    truncate_chars(excerpt, max_chars)
}

pub(crate) fn html_to_text(raw: &str) -> String {
    // Best-effort HTML -> text. This isn't a full parser, but works well enough for SEC filings.
    let s = raw.replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len().min(128_000));
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut skip_depth = 0usize;

    for ch in s.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_raw = tag_buf.trim();
                let tag = tag_raw.to_ascii_lowercase();
                let mut name = tag.split_whitespace().next().unwrap_or_default();
                let is_end = name.starts_with('/');
                name = name.trim_start_matches('/').trim_end_matches('/');

                let should_skip = matches!(
                    name,
                    "script" | "style" | "head" | "ix:hidden" | "ix:header"
                );
                if should_skip {
                    if is_end {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !tag.ends_with('/') {
                        // Only bump for non-self-closing tags.
                        skip_depth = skip_depth.saturating_add(1);
                    }
                }

                // Newline-ish tags (only when not in a skipped section).
                if skip_depth == 0 {
                    if tag.starts_with("br")
                        || tag.starts_with("/p")
                        || tag.starts_with("p")
                        || tag.starts_with("/div")
                        || tag.starts_with("div")
                        || tag.starts_with("/tr")
                        || tag.starts_with("tr")
                        || tag.starts_with("/li")
                        || tag.starts_with("li")
                        || tag.starts_with("hr")
                    {
                        out.push('\n');
                    }
                }
                tag_buf.clear();
            } else {
                // cap tag buffer to avoid huge memory on malformed input
                if tag_buf.len() < 256 {
                    tag_buf.push(ch);
                }
            }
            continue;
        }

        if ch == '<' {
            in_tag = true;
            continue;
        }
        if skip_depth == 0 {
            out.push(ch);
        }
    }

    // Decode entities and normalize whitespace.
    let decoded = html_escape::decode_html_entities(&out).to_string();
    let mut cleaned = String::with_capacity(decoded.len());
    let mut last_ws = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if ch == '\n' {
                cleaned.push('\n');
                last_ws = false;
            } else if !last_ws {
                cleaned.push(' ');
                last_ws = true;
            }
        } else {
            cleaned.push(ch);
            last_ws = false;
        }
    }

    // Collapse excessive blank lines.
    let mut final_out = String::with_capacity(cleaned.len());
    let mut blank_run = 0usize;
    for line in cleaned.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                final_out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        final_out.push_str(line);
        final_out.push('\n');
    }

    final_out.trim().to_string()
}

