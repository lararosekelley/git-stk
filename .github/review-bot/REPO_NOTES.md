# Repo notes - git-stk

Facts the review bot cannot infer from one pass over the diff. The philosophy, the finding
categories, the workflow, the scoring, and the output format all live in the bot's base prompt; this
file only refines them for git-stk.

## The stack

A single Rust crate: a Git-native stacked-branch workflow CLI that integrates with GitHub, GitLab,
and Gitea/Forgejo by shelling out to `gh` / `glab` / `tea`.

Errors are `anyhow::Result` with contextual messages throughout, so a panic is a user-facing crash
rather than an error message. User-facing output goes through `anstream::println!` /
`anstream::eprintln!` with the `style::` helpers; raw git or provider output is shown only on
failure, or under `--verbose`.

## What CI already covers, so never mention it

On every PR: `rustfmt`, `clippy -D warnings` (with the feature-gated binaries' features named),
`markdownlint`, `commitlint`, and the `cargo test` matrix across Linux, macOS, and Windows.

The live e2e suite is **not** a PR check - it runs on `workflow_dispatch` and as a `workflow_call`
from the release, so a failing live run blocks a release rather than a merge.

## What CI cannot cover, so it is yours

- Rebase and fork-point logic that can replay or drop the wrong commits.
- Anything that rewrites history, force-pushes, or deletes a branch or ref. A force-push that loses
  `--force-with-lease`, or a branch delete that no longer confirms the work landed, is a safety
  regression even when the happy path still works.
- Concurrency and TOCTOU hazards around the stack lock and `.git/stack-state`.
- Determinism where output order or rebase order must be stable.
- **Test fakes assert on arguments, not behavior.** They prove git-stk *would* run
  `gh pr edit --title`; they cannot prove `gh` accepts that flag. A new or changed `gh` / `glab` /
  `tea` invocation is only genuinely covered by the live suite in `src/bin/git-stk-e2e.rs`.

## Seams

- **Shelling out:** only `src/git.rs` runs `git`, and only `src/providers/` runs `gh` / `glab` /
  `tea`. A new `std::process::Command` anywhere else is a violation - route it through the existing
  helper, or add one there.
- **Config:** user-facing knobs are `stk.*` git-config keys declared in the `SETTINGS` table in
  `src/settings.rs`. Never read behavior off a bespoke environment variable.
- **Command layout:** subcommands live in `src/commands/<name>.rs` and implement `Run`; stack
  manipulation lives in `src/stack/`. Business logic wired directly into `cli.rs` is misplaced.
- **Provider trait:** behavior shared across forges belongs on the `ReviewProvider` trait in
  `src/providers/mod.rs` and lands for all four implementations (github, gitlab, gitea, demo).
  Uniform on the trait does not mean uniform underneath: a draft is a real flag on GitHub but a
  `WIP:` / `Draft:` title prefix on GitLab and Gitea, and the three CLIs disagree about flag names
  and about what a create does to the branch. Review the per-forge encoding, not just the signature.
- **Review bodies:** the managed sections of a PR/MR body - description, closes, stack overview, and
  the ledger data comment - are assembled in `src/notes/`. A hand-rolled marker string, or a body
  written anywhere but through those helpers, drifts from the parser that has to read it back.
- **Secrets:** an error message must not leak a token or a remote URL with credentials in it.

## Coverage matrix

- New flag, subcommand, provider method, or settings key → an integration test under `tests/`, run
  with the `test-fakes` feature and its `FakeProvider` harness.
- A new `stk.*` setting → the `SETTINGS` table in `src/settings.rs`, the README `[stk]` config
  block, and wherever `docs/COMMANDS.md` describes the behavior. It must be honored everywhere the
  behavior applies.
- A new `ReviewProvider` method → implemented for every forge, demo included.
- A pure helper (parsing, normalizing, prefix handling) → a colocated `#[cfg(test)] mod tests` next
  to the code, not only an end-to-end test that happens to cover it.
- A provider CLI surface nothing else exercises → a case in `src/bin/git-stk-e2e.rs`.
- Any user-facing change → a `CHANGELOG.md` entry. `dist` publishes the `## <version>` section
  verbatim as the GitHub release body, so a missing entry ships a release with empty notes.

## Repo-specific non-findings

- `.unwrap()` / `.expect()` inside `#[cfg(test)]` or `mod tests` is the norm here, and is never a
  finding.
- Shell completions and man pages are generated from clap at release time; they need no edit.
- Comments here are short and present-tense, stating the invariant rather than narrating the code or
  the moment of a fix. Match that.
