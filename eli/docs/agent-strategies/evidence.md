# Evidence Strategy

## Objective
Increase confidence by collecting new supporting and disconfirming evidence around a thesis.

## Best Use
- Thesis exists but confidence is low.
- You need broader coverage before deciding.

## Inputs
- `thesis` text or lead report path
- core datasets (optional)

## Worker Directive Scaffold
"Treat the thesis as a hypothesis. Run fresh micro-fetches to either support or reject it. Prefer high-signal data. Return numbers with paths. If evidence is weak, say insufficient evidence."

## Required Output (per worker)
1. Two quantitative findings with file paths.
2. One support signal and one reject signal.
3. Updated confidence (low/medium/high) with reason.

## Acceptance Checks
- Includes both confirming and disconfirming evidence.
- Uses fresh tool outputs, not only inherited artifacts.
- Confidence level is justified by data quality.
