# Critique Strategy

## Objective
Stress-test a lead answer by finding concrete weaknesses, contradictions, and missing data.

## Best Use
- You already have a lead memo.
- You want adversarial pressure before final output.

## Inputs
- `lead_report` path (required)
- `objective` prompt (required)
- optional shared manifest/data paths

## Worker Directive Scaffold
"Read the lead report first. Your job is not to restate it. Find the most consequential flaws. Run at least 2 fresh tool/data fetches. For each critique point, cite the exact output file path used."

## Required Output (per worker)
1. Two evidence-backed critique bullets.
2. One claim likely wrong/overstated in lead report.
3. One concrete fix (data fetch or method change).

## Acceptance Checks
- Every critique bullet cites a real path.
- At least one criticism changes downstream action.
- No purely stylistic complaints.
