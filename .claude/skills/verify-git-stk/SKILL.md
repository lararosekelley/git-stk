---
name: verify-git-stk
argument-hint: "[change or behavior|help]"
description: Verify git-stk changes with existing local fixtures and CI checks. Use for /verify-git-stk or a verification pass in this repository; do not use for other projects.
---

# Verify git-stk

## Help dispatch

If the entire argument is `help`, `--help`, or `-h`, print Usage and stop before
reading repository state or running checks.

## Usage

`/verify-git-stk [change or behavior|help]`

Select checks from `docs/VERIFICATION.md` for the relevant current change or the
named behavior. Reuse the existing test fixtures; do not create a parallel harness.
Report exactly what ran and what remains unverified. Say `stop` to end the pass.

## Procedure

1. Resolve this repository root and read its `AGENTS.md` and `docs/VERIFICATION.md`.
2. Inspect status and the relevant diff. Use the invocation or forwarded arguments
   to narrow scope; ask if the intended behavior cannot be identified.
3. Select the smallest meaningful checks from the guide. Verify current command
   definitions before executing; the guide is maintained beside the tests and CI.
4. Run approved local checks. Do not mutate the working tree to fix failures or
   accept snapshots without a separate request. Fixtures own their temporary state.
5. Report commands, outcomes, test counts, and limitations. Do not claim live-provider
   or cross-platform coverage from local fixture tests.

This skill does not authorize live E2E, workflow dispatch, releases, pushes, or
provider writes. Respect current user approval rules even if a linked command can
perform those actions. On `stop`, launch no further checks and report any owned
check still running; do not kill unrelated processes or proceed into fixes.
