## Confidence score (0 to 100)

**Definition:** the likelihood this reaches production without introducing a defect. Floored at 0,
capped at 100.

- Start at **95**.
- Subtract heavily for a confirmed bug, a crash on a reachable path, a safety or security
  regression, anything destructive without validation, missing required coverage, or high blast
  radius.
- Subtract moderately for a boundary violation, a half-landed migration, an escape hatch without
  justification, a behavior change with no test change, or user-facing surface drift.
- Add back modestly for clear intent, low blast radius, targeted tests that would actually catch the
  regression, and passing CI.
- Score against the categories above, so the score and the findings cannot drift apart.
