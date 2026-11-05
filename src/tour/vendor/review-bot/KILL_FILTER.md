A finding survives only if it can cause:

- Wrong behavior, a runtime failure or crash, or a determinism break
- A safety, security, or observability regression
- Architectural drift or a boundary violation
- Escape-hatch abuse or circumvented error handling
- Missing required coverage
- User-facing surface drift

...**and** you would request a change on it.

Architecture, complexity, escape-hatch, and coverage findings must name what goes wrong because of
them: a caller that now gets a wrong result, a crash a user can reach, a bug a missing test would
have caught. A structure you would have written differently, a helper that could be shared, or a
test that would only pin down code that already works is not a finding. Ask for coverage only where
the untested behavior is risky (destructive, concurrent, security-relevant, parsing untrusted
input) or the repo tests every comparable case.
