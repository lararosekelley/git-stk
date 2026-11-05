Check each finding against the code and try to prove it wrong. Drop it only if one of these holds:

- the failure cannot happen: a guard, a caller, or a type rules out the input or branch it needs,
  and you can name that line;
- the behavior is deliberate: a comment, doc, or test at that code says so, or the finding's premise
  misreads the code; or
- acting on it would change no behavior: style, naming, structure, comments, or more tests for code
  that works.

Keep everything else. A failure that is reachable but rare is still a bug: an unusual input, a race,
a missing timeout, an error on a path that seldom runs. Fitting how the rest of the code behaves
does not make a bug intended. When you are unsure, keep the finding.
