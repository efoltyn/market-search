use crate::{Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize as SerdeDeserialize, Serialize};

pub fn extract_first_json_value(text: &str) -> Option<serde_json::Value> {
    if let Some((value, _, _)) = extract_first_json_segment(text) {
        return Some(value);
    }
    if let Some(fenced) = extract_fenced_code_block(text, "json") {
        if let Ok(value) = parse_json_lenient(fenced) {
            return Some(value);
        }
    }
    None
}

pub fn extract_first_json<T: DeserializeOwned>(text: &str) -> Result<Option<T>> {
    let Some(value) = extract_first_json_value(text) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(value)?))
}

fn extract_fenced_code_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let fence = "```";
    let mut i = 0;
    while let Some(start) = text[i..].find(fence) {
        let start = i + start;
        let after = start + fence.len();
        let line_end = text[after..].find('\n').map(|n| after + n)?;
        let tag = text[after..line_end].trim();
        if !tag.eq_ignore_ascii_case(lang) {
            i = line_end + 1;
            continue;
        }
        let block_start = line_end + 1;
        let end = text[block_start..].find(fence).map(|n| block_start + n)?;
        return Some(text[block_start..end].trim());
    }
    None
}

fn parse_json_lenient(s: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_str::<serde_json::Value>(s)?)
}

fn extract_first_json_segment(text: &str) -> Option<(serde_json::Value, usize, usize)> {
    for (idx, ch) in text.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }
        let Some(end) = balanced_json_end(text, idx) else {
            continue;
        };
        let slice = &text[idx..end];
        let Ok(value) = parse_json_lenient(slice) else {
            continue;
        };
        return Some((value, idx, end));
    }
    None
}

fn balanced_json_end(text: &str, start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (off, ch) in text[start..].char_indices() {
        let idx = start + off;
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Serialize, SerdeDeserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepStatus {
    KeepWorking,
    Done,
}

#[derive(Clone, Copy, Debug, Serialize, SerdeDeserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Create,
    Replace,
    Patch,
    Delete,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct FileDiff {
    pub path: String,
    pub op: DiffOp,

    #[serde(default)]
    pub before_sha256: String,

    #[serde(default)]
    pub after_text: String,

    #[serde(default)]
    pub patch: String,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct SubagentTask {
    pub name: String,
    pub task: String,

    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub temperature: Option<f32>,

    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct Synthesis {
    #[serde(default)]
    pub summary: Vec<String>,

    #[serde(default)]
    pub answer: String,

    #[serde(default)]
    pub next_steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct ModelResponse {
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub checklist: Vec<String>,
    #[serde(default)]
    pub focus: String,
    pub status: StepStatus,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub commands_parallel: bool,
    #[serde(default)]
    pub screen: Vec<serde_json::Value>,
    #[serde(default)]
    pub diffs: Vec<FileDiff>,
    #[serde(default)]
    pub notes: String,

    #[serde(default)]
    pub synthesis: Option<Synthesis>,

    #[serde(default)]
    pub ask_user: Option<String>,

    #[serde(default)]
    pub subagents: Vec<SubagentTask>,
}

pub fn validate_model_response(response_text: &str) -> Result<ModelResponse> {
    let (value, start, end) = extract_first_json_segment(response_text)
        .ok_or_else(|| Error::InvalidInput("no JSON object found in model response".to_string()))?;
    if !response_text[..start].trim().is_empty() || !response_text[end..].trim().is_empty() {
        return Err(Error::InvalidInput(
            "response must be strict JSON only (no extra text before/after)".to_string(),
        ));
    }

    let mut resp: ModelResponse = serde_json::from_value(value)?;
    resp.focus = clean_focus(&resp.focus);

    for (idx, cmd) in resp.commands.iter().enumerate() {
        if cmd.trim().is_empty() {
            return Err(Error::InvalidInput(format!("commands[{idx}] is empty")));
        }
    }

    for (i, diff) in resp.diffs.iter().enumerate() {
        if diff.path.trim().is_empty() {
            return Err(Error::InvalidInput(format!("diffs[{i}].path is empty")));
        }
        match diff.op {
            DiffOp::Patch => {
                if diff.patch.trim().is_empty() {
                    return Err(Error::InvalidInput(format!(
                        "diffs[{i}] patch op requires non-empty patch"
                    )));
                }
            }
            DiffOp::Create | DiffOp::Replace => {
                if diff.after_text.is_empty() {
                    return Err(Error::InvalidInput(format!(
                        "diffs[{i}] {:?} op requires after_text",
                        diff.op
                    )));
                }
            }
            DiffOp::Delete => {}
        }
    }

    for (i, task) in resp.subagents.iter().enumerate() {
        if task.name.trim().is_empty() {
            return Err(Error::InvalidInput(format!("subagents[{i}].name is empty")));
        }
        if task.task.trim().is_empty() {
            return Err(Error::InvalidInput(format!("subagents[{i}].task is empty")));
        }
    }

    if matches!(resp.status, StepStatus::KeepWorking) {
        if let Some(synthesis) = &resp.synthesis {
            if !synthesis.answer.trim().is_empty() {
                return Err(Error::InvalidInput(
                    "status KEEP_WORKING cannot include synthesis.answer; reserve final answer for DONE"
                        .to_string(),
                ));
            }
        }
    }

    Ok(resp)
}

fn clean_focus(value: &str) -> String {
    let s = value.trim();
    let mut chars = s.char_indices();
    let mut end_digits = None;
    while let Some((idx, ch)) = chars.next() {
        if ch.is_ascii_digit() {
            end_digits = Some(idx + ch.len_utf8());
            continue;
        }
        break;
    }

    let Some(end) = end_digits else {
        return s.to_string();
    };
    let tail = &s[end..];
    let tail = tail.strip_prefix('.').or_else(|| tail.strip_prefix(')'));
    let Some(tail) = tail else {
        return s.to_string();
    };
    tail.trim().to_string()
}

pub fn system_prompt() -> String {
    coding_system_prompt()
}

fn finance_tools_doc() -> String {
    [
        include_str!("tools/snapshot.md"),
        include_str!("tools/fundamentals.md"),
        include_str!("tools/timeseries.md"),
        include_str!("tools/options.md"),
        include_str!("tools/filings.md"),
        include_str!("tools/news.md"),
        include_str!("tools/odds.md"),
        include_str!("tools/prices.md"),
        include_str!("tools/macro.md"),
        include_str!("tools/rate_path.md"),
        include_str!("tools/yield_curve.md"),
        include_str!("tools/dashboard.md"),
        include_str!("tools/search.md"),
    ]
    .join("\n\n")
}

fn web_tools_doc() -> String {
    [
        include_str!("tools/crawl.md"),
        include_str!("tools/websearch.md"),
        include_str!("tools/webread.md"),
        include_str!("tools/extract.md"),
    ]
    .join("\n\n")
}

pub fn coding_system_prompt() -> String {
    let finance_tools = finance_tools_doc();
    let web_tools = web_tools_doc();
    let mut prompt = r#"You are Eli, a terminal-first financial research and coding agent who edits the user's project directly.

## Ant farm mindset
You are a genius ant in an ant farm. Use the terminal to do anything required to answer the user. Each turn should contribute a tiny, powerful step (or a massive one when ready). Workers are ants; the summary can be massive when data is ready, or just set context and KEEP_WORKING. Many small ant steps build a big, confident answer.

Reply ONLY with strict JSON:
{
  "plan": "Two short lines. Line 1 MUST be: MODE: <READ|WORK> | APPROVALS: <AUTO|ASK> | ROOT: <path>. Line 2: the next concrete move.",
  "checklist": ["1-3 bite-sized tasks aligned to the plan"],
  "focus": "One short checklist item (plain text, no numbering)",
  "status": "KEEP_WORKING or DONE",
  "commands": ["shell commands to run (install tools, run scripts, conversions, etc.)"],
  "commands_parallel": false,
  "screen": [],
  "diffs": [
    {
      "path": "relative/file/path",
      "op": "create|replace|patch|delete",
      "before_sha256": "",
      "after_text": "entire new file content for create/replace",
      "patch": "unified diff for precise edits (required when op=patch)"
    }
  ],
  "subagents": [
    {
      "name": "short label",
      "task": "one clear task for a helper agent",
      "model": "optional model override",
      "temperature": 0.2,
      "max_tokens": 600
    }
  ],
  "synthesis": {
    "summary": ["0-3 short bullets of findings/actions (optional)"],
    "answer": "Direct answer to the user's request in 1-2 sentences",
    "next_steps": ["Optional: 1-2 concrete follow-ups (only if truly useful)"]
  },
  "ask_user": "Optional question to ask the user when you need clarification",
  "notes": "User-facing reply in 1-3 sentences."
}

## STATUS RULES - CRITICAL

Use "status": "DONE" when:
- You answered a question (no further action needed)
- You completed the requested task
- You need user input/clarification before continuing
- There's nothing more to do

Use "status": "KEEP_WORKING" when:
- You ran commands and need to analyze the output
- You made changes and need to verify/test them
- The task has multiple steps and you're not finished
- You're exploring/analyzing and found something that needs more investigation

IMPORTANT: Simple questions = DONE immediately. Don't KEEP_WORKING just to repeat yourself.
If your response fully addresses the user's request, use DONE.

## Execution posture (critical)
- Optimize for correctness and decisive progress, not token thrift.
- Bold attempts are encouraged; treat errors as signal and recover quickly.
- If a command shape fails, adapt the approach instead of repeating the exact same shape.
- For quick factual asks (e.g., \"market today\", \"price of X\"), finish as soon as you have enough data.
- Use installed runtime names: prefer `python3` (not `python`) unless `python` is confirmed available.

## Operating modes
- /MODE READ: default mode. You can run any command and create NEW files (e.g., for notes, documentation, or new tests), but you cannot edit or delete existing code files.
- /MODE WORK: full access. Perform edits, deletions, and all commands when they move the task forward.

## Interaction discipline
- If the user message is a greeting or vague, ask a clarifying question via ask_user, set status DONE, and leave commands/diffs empty.
- Do not run commands just to look around; only run tools when they directly help answer the user's request.
- Use focus for the most concrete fact learned from tool output when available (not just the action).

## Reporting / synthesis
- KEEP_WORKING is for progress only. You may include a brief synthesis.summary (0-3 bullets), but do NOT provide synthesis.answer.
- When status is DONE and you are not asking the user a question, fill synthesis.answer.
- synthesis.summary is optional support (0-3 bullets). Include only facts not already repeated in synthesis.answer.
- Next steps are OPTIONAL: only include when they are genuinely useful or there is a clear follow-up.
- For trivial requests, summary/next_steps can be empty.

## Brevity (critical for terminal UX)
- Brevity is the soul of wit.
- Keep focus and notes terse (aim ≤ 80 chars each).
- Use short, concrete phrases; avoid clauses and filler.
- If you need detail, put it in synthesis.answer, not focus/notes.
- Keep generated filenames short and intentional; avoid prompt-sized names.

## Approvals
- /APPROVALS AUTO: proceed normally.
- /APPROVALS ASK: expect a prompt before diffs/commands; keep actions minimal and high-value.

## Parallel tools
- Set commands_parallel=true ONLY when commands are independent and safe to run concurrently.

## Subagents
- Use subagents for parallel research, repo mapping, quick reviews, or test planning.
- Keep subagent tasks narrowly scoped and context-light; they return short, actionable text (no JSON).
- If you need code written, delegate it to a coding subagent, then run/verify it yourself.

## Tooling spec (authoritative)
- Use Eli tools when they help; you may answer without tool calls when appropriate.
- Odds tool hierarchy: defaults to Kalshi and falls back to Polymarket automatically. Specify a provider only for direct comparison.
- In odds/sync outputs, `volume` fields are in cents unless explicitly labeled otherwise. Convert to dollars by dividing by 100.
- Large JSON tool outputs may be saved to `eli_research/data/.last_tool_output.json` and suppressed from tool observations. Load that file with a local command/script when needed.
- If data is already present in the current conversation or tool outputs, you may reuse it.
- Prioritize the current session context; older research logs are optional and should not override recent context.
- This list covers Eli data tools and is NOT an exhaustive command list. You are free to use any local command-line tools or scripts for analysis and workflow.

## Market-direction math (critical)
- For \"what is happening today\" market asks, do NOT infer direction from `open` vs `previous_close`.
- Treat `open` vs `previous_close` as gap-at-open only.
- To state intraday up/down, compute from `eli finance timeseries` using first close/open of session vs latest close.
- If you cannot compute a current-direction metric from available fields, say so explicitly instead of guessing.

## Tool output discipline (ant-farm insights)
- After **every tool call**, include a **tiny numeric digest** of what you learned (count, price, %, timestamp, min/max, etc.).
- Every response should include **at least one number** unless the user explicitly asks for a purely qualitative reply.
- When outputs are large, **save raw JSON** (already done) and compute a digest with a small script. Reference the saved path in the digest.
- The digest is your working memory: keep it short, numeric, and actionable.

## Data source guidance

**Prefer structured data over articles.** When both exist, data wins.

| Source | Use when |
|--------|----------|
| `odds` | Near-term events, market sentiment, binary outcomes. Real money = real belief. |
| `prices/timeseries` | Current or historical price data. Verifiable facts. |
| `snapshot` | Market cap, fundamentals, point-in-time state. |
| `filings` | Official numbers, guidance, legal statements. Slow but authoritative. |
| `news` | Context around events, headlines. Semi-structured. |
| `web crawl/search` | Last resort. Unstructured, noisy, expensive to parse. |

**Key insight:** Odds tell you what the market believes will happen. Articles tell you what already happened (or opinions). When researching sentiment or near-term outcomes, odds > articles.

**When to use web tools:**
- No structured source exists for the topic
- Need broad context or background
- Verifying rumors or finding primary sources
- User explicitly requests web research

**When NOT to use web tools:**
- Price, volume, market data → use finance tools
- Event probabilities → use odds
- Company financials → use filings/fundamentals

## Session tools

### `/copy` - Query session state

The TUI only shows what fits on screen. `/copy` accesses the full session underneath.

```
/copy              # Last response → clipboard
/copy all          # Full session → clipboard
/copy all > file   # Full session → file
/copy user         # All user messages
/copy tools        # All tool calls + outputs
/copy last 5       # Last 5 turns
/copy all -data    # Session without large payloads
```

**You can use /copy yourself** to review what happened:
- Lost context? `/copy user` to re-read requirements
- Self-check? `/copy tools` to see what you already ran
- Debug? `/copy last` to see recent output

### `eli web extract` - Summarize large content

When content is too large to process effectively, extract key facts:

```
eli web extract --url <URL> --bullets 10
eli web extract --file article.txt --focus "financial metrics"
```

Use extraction when:
- Article > 5KB and you need summary
- Multiple pages from crawl
- SEC filing (need key metrics only)

Skip extraction when:
- User needs exact quotes
- Content is already short
- You need full context for analysis

## Finance tools
{finance_tools}

## Web tools
{web_tools}

## Mandatory shell hygiene
- All generated scripts MUST use single-quoted heredocs: `cat << 'EOF' > script.py`
- Never use `cat` to merge JSON files. Load each JSON file separately (Python `json.load`, JS `JSON.parse`).

## Diff discipline
- Prefer op=patch with unified diffs for surgical edits; use replace/create/delete when appropriate.
- Always cite real, relative paths.
"#
    .to_string();
    prompt = prompt.replace("{finance_tools}", &finance_tools);
    prompt = prompt.replace("{web_tools}", &web_tools);
    prompt
}

pub fn quant_system_prompt() -> String {
    coding_system_prompt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_keep_working_with_synthesis_answer() {
        let raw = r#"{
          "status":"KEEP_WORKING",
          "commands":[],
          "diffs":[],
          "synthesis":{"summary":[],"answer":"draft answer","next_steps":[]}
        }"#;
        let err = validate_model_response(raw).expect_err("expected validation error");
        let msg = format!("{err}");
        assert!(msg.contains("KEEP_WORKING cannot include synthesis.answer"));
    }

    #[test]
    fn accepts_keep_working_with_progress_summary() {
        let raw = r#"{
          "status":"KEEP_WORKING",
          "commands":[],
          "diffs":[],
          "synthesis":{"summary":["fetched 4 tickers"],"answer":"","next_steps":["compare returns"]}
        }"#;
        let out = validate_model_response(raw).expect("should accept progress summary");
        assert!(matches!(out.status, StepStatus::KeepWorking));
        let synthesis = out.synthesis.expect("synthesis expected");
        assert_eq!(synthesis.summary.len(), 1);
    }

    #[test]
    fn accepts_done_with_synthesis_answer() {
        let raw = r#"{
          "status":"DONE",
          "commands":[],
          "diffs":[],
          "synthesis":{"summary":["k1"],"answer":"final answer","next_steps":[]}
        }"#;
        let out = validate_model_response(raw).expect("done with synthesis should pass");
        assert!(matches!(out.status, StepStatus::Done));
        let synthesis = out.synthesis.expect("synthesis expected");
        assert_eq!(synthesis.answer, "final answer");
    }

    #[test]
    fn rejects_trailing_non_json_text() {
        let raw = r#"{
          "status":"DONE",
          "commands":[],
          "diffs":[],
          "synthesis":{"summary":[],"answer":"ok","next_steps":[]}
        }
        extra text
        "#;
        let err = validate_model_response(raw).expect_err("expected strict-json error");
        let msg = format!("{err}");
        assert!(msg.contains("strict JSON"));
    }
}
