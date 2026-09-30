# git-stk

## Verification

Use [docs/VERIFICATION.md](docs/VERIFICATION.md) to choose checks for the change.
The project-local `verify-git-stk` skill routes to that same guide; it does not
replace CI or introduce another test harness.

- Run commands from the repository root.
- `just test` enables the required `test-fakes` feature. Do not substitute bare `cargo test`.
- `just lint` checks Rust and Markdown, including feature-gated development binaries.
- `just check` runs formatting before lint/tests and can modify files.
- Reuse `tests/common/mod.rs` for disposable repositories and fake providers.
- Do not exercise stack mutations, setup, upgrade, or uninstall against the user's
  actual working repositories or home directory merely to verify a change.
- Live E2E and release tasks publish or mutate external state. Read their workflows
  and obtain explicit approval for the exact actions before running them.

Inspect the diff and preserve unrelated work. Documentation-only edits need
formatting/link checks, not the full integration suite.
