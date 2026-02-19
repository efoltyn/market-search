# Debate Strategy

## Objective
Generate a structured argument among agents, preserve disagreement, then produce a consensus report.

## Best Use
- High-uncertainty forecasts.
- Need both sides represented before decision.

## Inputs
- `question` prompt
- optional lead memo
- worker roles (bull/bear/balanced or pro/con)

## Worker Directive Scaffold
"Argue your assigned side with evidence. Then pre-empt the strongest counterargument. Use tool-backed claims with file paths."

## Required Output (per worker)
1. Two data-backed claims.
2. One direct rebuttal of opposite side.
3. One condition that would make your stance wrong.

## Consensus Pass
A final synthesizer should output:
1. Shared facts.
2. Active disagreements.
3. What data would resolve disagreements fastest.
4. Current best decision under uncertainty.

## Acceptance Checks
- Contradictions are preserved in collab draft.
- Consensus distinguishes facts vs opinions.
- Resolution plan is specific and testable.
