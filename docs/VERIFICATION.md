# Verification

Use the existing test harness and CI commands to verify the changed behavior.
Run commands from the repository root with the toolchain in `rust-toolchain.toml`.
Read the relevant test before choosing a filter, and check that it actually ran.

## Local checks

`just test` runs `cargo test --features test-fakes`. The feature builds the fake
provider executable needed by integration tests; bare `cargo test` is not equivalent.
Tests use disposable repositories and fake provider CLIs rather than a live account.

Choose the affected integration targets, then expand if shared behavior changed:

| Change | Integration targets under `tests/` |
| --- | --- |
| Worktree ownership, cleanup, navigation | `worktree`, `cleanup`, `navigation` |
| Restack and recovery | `restack`, `worktree`, `repair`, `lock` |
| Stack edits | `insert`, `split`, `absorb`, `rename` |
| Provider operations | `provider`, `gitea`, `submit`, `merge`, `reviews`, `comment` |
| CLI presentation | `list_status`, `list_markdown`, `completions`, `credits`, `demo` |
| Shell setup and lifecycle | `setup`, `upgrade`, `run` |

For example:

```sh
cargo test --features test-fakes --test worktree --test navigation
cargo test --features test-fakes --test worktree new_worktree_creates_the_branch_elsewhere_and_leaves_head_alone -- --exact
```

Check related source/unit tests too. Use `just test` for changes to shared Git,
configuration, stack, or provider machinery where a narrow target is insufficient.

```sh
just test
just lint
```

`just lint` runs formatting checks, Clippy across feature-gated binaries, and
Markdown lint. `just check` also runs `cargo fmt`, so it is not a read-only check.
Lint success is not evidence that a stack operation behaves correctly.

## Reuse the fixtures

`tests/common/mod.rs` provides `TestRepo`, which creates temporary repositories,
sets a test identity, and isolates commands from global/system Git configuration.
This prevents personal options such as `stk.pushOnSubmit` from changing test behavior.
`TestRepo::stack()` uses the binary built from this checkout, not an installed release.

Use `FakeProvider` and `stack_faked` for provider commands. Its invocation log can
prove which calls were made; use failing fallback rules when unexpected calls must
fail the test. Do not swap in real credentials to get a local test passing.

For worktrees, keep every directory inside a fixture-owned temporary parent.
`TestRepo::new_in_subdir` covers commands that create a default sibling worktree.
Follow the patterns in `tests/worktree.rs`; let fixture cleanup own those directories.

Assert observable state, as applicable:

- Command exit status and meaningful output.
- Current branch, parent metadata, and worktree location/ownership.
- Preservation of unrelated dirty files and worktrees.
- Consistent state after a refusal, conflict, continue, or abort.
- Provider calls or their absence when no external operation is expected.

If no test covers the important case, add a regression using these fixtures when
implementation work is authorized. A verification-only request should report the
gap and propose the check before editing. Do not build a parallel smoke-test framework.

## What CI establishes

[CI](../.github/workflows/ci.yml) runs `just lint` on Linux and `just test` on Linux,
macOS, and Windows. Local success proves only the checks run on the local platform;
read the exact commit's CI results before claiming cross-platform success.

[Live E2E](../.github/workflows/e2e.yml) is a separate manual/release gate, excluded
from PR runs. It exercises GitHub across the three operating systems, plus GitLab
and a disposable Gitea instance on Linux. It creates and deletes repositories,
pushes branches, operates on reviews, and changes runner shell setup.

Do not dispatch that workflow or run `git-stk-e2e` as part of ordinary local
verification without approval of the provider, account, and external operations.
The `e2e` feature being included in Clippy does not run the live harness.

For release-specific checks not covered by CI, consult [SMOKE_TEST.md](SMOKE_TEST.md)
and verify each check is still applicable. Installer, upgrade, uninstall, release,
and publishing commands require separate direction; do not run that document wholesale.

## Report evidence

Report exact commands, test counts, pass/fail/skipped outcomes, and the behaviors
they checked. Mention unavailable platforms, provider checks not run, and remaining
manual checks. Existing CI does not verify newer uncommitted code. If a command
fails, report the relevant error without exposing credentials or automatically
changing expected results.
