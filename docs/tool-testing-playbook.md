# Eli Tool Testing Playbook

This is the default testing standard for Eli tools.

## Core Principle
A tool test is not "did commands run".
A tool test is:
1. Pick a concrete target outcome you want.
2. Run the minimum real workflow.
3. Score binary success: did it get what you wanted?
4. Measure quantitative cost.
5. Record qualitative friction and product improvements.

## Required Test Structure
Each test case must include:
1. `target`: one specific thing you want (example: "get live recession odds").
2. `success_criteria_binary`: explicit pass/fail rule.
3. `workflow`: exact commands/tool calls used.
4. `quant_metrics`: call count, errors, elapsed time.
5. `qual_findings`: what felt hard or repetitive.
6. `default_output_gap`: what obvious post-processing should be built into the tool.

## Pass/Fail Rules
- `PASS` only if success criteria is fully met.
- `FAIL` if output is empty, wrong, stale for the goal, or requires hidden manual assumptions.
- Partial relevance is still `FAIL` unless criteria said partial is acceptable.

## Quant Metrics (Mandatory)
For each test case track:
- `tool_calls_total`
- `tool_calls_failed`
- `elapsed_seconds_total`
- `retries`
- `rate_limit_errors` (e.g. 429)
- `bytes_returned` (optional but useful)

For full suite track:
- `success_rate`
- `avg_calls_per_pass`
- `avg_elapsed_per_pass`
- `error_rate`

## Qualitative Rubric (Mandatory)
Capture these in plain language:
1. `discoverability`: could a user guess the right command path?
2. `precision`: did output directly answer the target, or dump broad noise?
3. `stability`: did workflow break on rate limits or pagination quirks?
4. `ergonomics`: did you need custom Python parsing for obvious summaries?
5. `provider_consistency`: do different providers behave similarly for same intent?

## Product Improvement Rule
If test required an obvious post-step script (parse/rank/filter/summarize), file an improvement proposal:
- "Tool should expose this by default"
- include exact proposed field/flag/output

Examples:
- add top-N ranked candidates directly in response
- add summary block: `matches`, `tradable_matches`, `best_contracts`
- add reason codes: why a contract was selected
- add intent mode that does discovery + targeting in one step

## Standard Output Format
Use this JSON shape for all tool test reports:

```json
{
  "generated_at": "2026-02-13T00:00:00Z",
  "tool": "eli finance odds",
  "suite_metrics": {
    "tests": 4,
    "passed": 3,
    "failed": 1,
    "success_rate": 0.75,
    "tool_calls_total": 14,
    "tool_calls_failed": 1,
    "elapsed_seconds_total": 12.4
  },
  "tests": [
    {
      "name": "Recession odds",
      "target": "Get live recession odds",
      "success_criteria_binary": "At least one tradable recession contract with probability field",
      "success": true,
      "quant_metrics": {
        "tool_calls_total": 3,
        "tool_calls_failed": 0,
        "elapsed_seconds_total": 2.6,
        "retries": 0,
        "rate_limit_errors": 0
      },
      "workflow": [
        "eli finance odds ..."
      ],
      "qual_findings": [
        "Series discovery worked; ranking picked active contract quickly"
      ],
      "default_output_gap": [
        "Tool should return top tradable contract summary by default"
      ]
    }
  ]
}
```

## Operating Rules For Future Tool Tests
1. Never stop at a single happy-path command.
2. Always test at least 3 intents for a tool when possible.
3. Include one "unknown intent" discovery test.
4. Include one stress/pathology test (rate limit, sparse data, pagination).
5. Always end with:
   - what failed
   - what to change in tool UX/API
   - what should become default output

## Why This Standard Exists
Goal is not just accurate analysis.
Goal is making every Eli tool intuitive and powerful enough that even small models can get strong results with low cognitive load.
