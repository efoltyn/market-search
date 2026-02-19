# Compete Strategy

## Objective
Run independent solution attempts and select the strongest answer by evidence quality.

## Best Use
- Multiple plausible approaches exist.
- You want best answer quickly from diverse agents/models.

## Inputs
- `objective` prompt
- worker roster with model/role

## Worker Directive Scaffold
"You are competing to produce the best answer. Use tools/data directly. Return concise final answer plus evidence list with file paths."

## Optional Rule
- `allow_cheat=true`: workers may read peer outputs and improve.

## Required Output (per worker)
1. Final answer.
2. Evidence bullets with paths.
3. Why this answer is stronger than alternatives.

## Winner Criteria
- Citation validity.
- Novel signal (not obvious from base memo).
- Correctness under quick spot-check.
- Clarity and actionability.
