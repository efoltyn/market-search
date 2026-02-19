# Eli Behavior Eval (3-Class)

This harness evaluates real Eli session logs against 3 scenario classes:

- `zero_tool_instant`: answer immediately, zero tools.
- `one_tool_instant`: one tool call then done.
- `multi_tool`: more than one tool call across iterative steps.

It also enforces the core invariant:

- `KEEP_WORKING` must not emit `synthesis.answer`.

## Inputs

Use either:

1. Existing session file:

```bash
python3 eli/scripts/eval_sessions.py \
  --session "$HOME/Library/Application Support/dev.eli.eli/data/sessions/<id>.jsonl" \
  --class one_tool_instant --strict
```

2. Run and evaluate in one shot (research mode):

```bash
python3 eli/scripts/eval_sessions.py \
  --run-research "what is the price of intel" \
  --class one_tool_instant --strict
```

Optional flags:

- `--provider <name>`
- `--model <name>`
- `--sessions-dir <path>`

## Output

JSON report with:

- `parsed_steps`, `final_status`
- `tool_calls_total`, `step_tool_calls`
- `keep_working_with_answer`
- `done_without_answer`
- pass/fail + failure reasons
